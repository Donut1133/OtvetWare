//! api.rs — прикладные вызовы otvet.mail.ru: кто я, жив ли аккаунт, карма,
//! профиль, смена имени/аватара. Порт соответствующих функций httpclient.js.

use crate::accounts::{Account, Karma};
use crate::http::{base_url, HttpError, Method, ReqOpts, Resp};
use crate::util::Stop;
use crate::Core;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub user_id: Option<i64>,
    pub username: Option<String>,
}

/// Итог проверки аккаунта.
///
/// `auth_bad` = ТОЧНО не залогинен (401/403). Антибот (418/429) и сетевые сбои
/// не дают ни `alive`, ни `auth_bad` — красить аккаунт в этом случае нельзя.
#[derive(Debug, Clone, Default)]
pub struct Validation {
    pub alive: bool,
    pub blocked: bool,
    pub auth_bad: bool,
    pub karma: Option<Karma>,
    pub user_id: Option<i64>,
    pub username: Option<String>,
    /// Сеть/прокси не дали ответа — статус трогать нельзя.
    pub error: Option<String>,
}

fn re_karma_url() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"/api/karma/score/(\d+)").unwrap())
}

fn re_profile_path() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"/profile/([^/?#]+)").unwrap())
}

fn re_default_nick() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^id\d+$").unwrap())
}

fn re_nuxt() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?s)<script[^>]*id="__NUXT_DATA__"[^>]*>(.*?)</script>"#).unwrap())
}

/// Ник из финального URL профиля (`/profile/id123` → редирект на `/profile/vasya`).
fn nick_from_url(url: &str) -> Option<String> {
    let caps = re_profile_path().captures(url)?;
    let raw = caps.get(1)?.as_str();
    let dec = urlencoding::decode(raw).map(|c| c.into_owned()).unwrap_or_else(|_| raw.to_string());
    if re_default_nick().is_match(&dec) {
        None
    } else {
        Some(dec)
    }
}

/// Узнать свой id/ник по кукам, без браузера (нужно при импорте кук).
///
/// Порядок важен: сначала `/api/auth/user` — единственный надёжный источник.
/// В HTML-фолбэке id берём ТОЛЬКО из карма-виджета: первый юзер в `__NUXT_DATA__`
/// это юзер ИЗ ЛЕНТЫ, и раньше именно так в аккаунт попадал чужой userId.
pub async fn resolve_me(core: &Core, acc: &Account, stop: &Stop) -> Option<Identity> {
    if let Ok(r) = core.http.request(acc, "/api/auth/user", ReqOpts::get().timeout_ms(10_000), stop).await {
        if r.ok {
            if let Some(j) = &r.json {
                if let Some(id) = j.get("id").and_then(|v| v.as_i64()) {
                    let username = j
                        .get("username")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty() && !re_default_nick().is_match(s))
                        .map(|s| s.to_string());
                    // Ник может быть дефолтным (idN) — это не повод терять id:
                    // без ника жить можно, без id нельзя (он уходит в author_id).
                    return Some(Identity { user_id: Some(id), username });
                }
            }
        }
    }

    let r = core.http.request(acc, "/", ReqOpts::get().html(), stop).await.ok()?;
    if !r.ok && r.text.is_empty() {
        return None;
    }
    let html = &r.text;
    let user_id =
        re_karma_url().captures(html).and_then(|c| c.get(1)).and_then(|m| m.as_str().parse::<i64>().ok())?;

    let mut username: Option<String> = None;
    if let Some(caps) = re_nuxt().captures(html) {
        if let Ok(arr) = serde_json::from_str::<Value>(caps.get(1).map(|m| m.as_str()).unwrap_or("[]")) {
            if let Some(items) = arr.as_array() {
                for x in items {
                    let (Some(id), Some(u)) =
                        (x.get("id").and_then(|v| v.as_i64()), x.get("username").and_then(|v| v.as_str()))
                    else {
                        continue;
                    };
                    // Ник подтверждаем ТОЛЬКО по уже известному id.
                    if id == user_id && !u.is_empty() && !re_default_nick().is_match(u) {
                        username = Some(u.to_string());
                        break;
                    }
                }
            }
        }
    }
    if username.is_none() {
        if let Ok(rr) =
            core.http.request(acc, &format!("/profile/id{user_id}"), ReqOpts::get().html(), stop).await
        {
            username = nick_from_url(&rr.url);
        }
    }
    Some(Identity { user_id: Some(user_id), username })
}

/// Карма пользователя. Эндпоинт ПУБЛИЧНЫЙ — для проверки авторизации не годится.
pub async fn fetch_karma(core: &Core, acc: &Account, user_id: i64, stop: &Stop) -> Option<Karma> {
    let r =
        core.http.request(acc, &format!("/api/karma/score/{user_id}"), ReqOpts::get(), stop).await.ok()?;
    let res = r.result()?;
    let score = res.get("score")?;
    Some(Karma {
        total: res.get("total_score").and_then(|v| v.as_i64()).unwrap_or(0),
        history: score.get("history").and_then(|v| v.as_i64()).unwrap_or(0),
        knowledge: score.get("knowledge").and_then(|v| v.as_i64()).unwrap_or(0),
        discussion: score.get("discussion").and_then(|v| v.as_i64()).unwrap_or(0),
    })
}

/// Проверка аккаунта без браузера. Заодно освежает userId и ник.
///
/// Пробуем `/api/auth/user`: без кук он отдаёт 403, а с куками — актуальные
/// id и ник одним запросом. Карма для проверки не годится в принципе:
/// `/api/karma/score` публичный и отвечает 200 даже разлогиненному.
///
/// Ник ОБЯЗАТЕЛЬНО перечитываем каждый раз. Раньше он брался из базы, только
/// если там пусто, и переименование на сайте не подхватывалось никогда: в
/// accounts.json годами лежал старый ник, а ссылка на свой профиль отдавала 404.
pub async fn validate_account(core: &Core, acc: &Account, stop: &Stop) -> Validation {
    let mut out = Validation { user_id: acc.user_id, username: acc.username.clone(), ..Default::default() };

    let probe = core.http.request(acc, "/api/auth/user", ReqOpts::get(), stop).await;
    match probe {
        Ok(p) => {
            out.blocked = p.blocked;
            out.auth_bad = !p.ok && !p.blocked && (p.status == 401 || p.status == 403);
            out.alive = p.ok;
            if let Some(j) = p.json.as_ref().filter(|_| p.ok) {
                if let Some(id) = j.get("id").and_then(|v| v.as_i64()) {
                    out.user_id = Some(id);
                }
                if let Some(u) = j
                    .get("username")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty() && !re_default_nick().is_match(s))
                {
                    out.username = Some(u.to_string());
                }
            }
        }
        Err(HttpError::Aborted) => {
            out.error = Some("остановлено".into());
            return out;
        }
        Err(e) => {
            out.error = Some(e.to_string());
            return out;
        }
    }

    // Аккаунт жив, но id так и не узнали — добираем окольным путём.
    if out.alive && out.user_id.is_none() {
        if let Some(me) = resolve_me(core, acc, stop).await {
            out.user_id = out.user_id.or(me.user_id);
            out.username = out.username.or(me.username);
        }
    }

    if let (Some(id), false) = (out.user_id, out.blocked) {
        out.karma = fetch_karma(core, acc, id, stop).await;
    }
    out
}

/// Применить итог проверки к хранилищу: красим ТОЛЬКО при явном 401/403,
/// зелёным — при `alive`. Антибот и сетевые ошибки статус не трогают.
pub fn persist_validation(core: &Core, name: &str, v: &Validation) {
    if v.alive {
        core.accounts.set_auth(name, true);
    } else if v.auth_bad {
        core.accounts.set_auth(name, false);
    }
    if let Some(k) = &v.karma {
        core.accounts.set_karma(name, k.clone());
    }
    if v.user_id.is_some() || v.username.is_some() {
        core.accounts.set_ident(name, v.user_id, v.username.clone());
    }
}

// ─── Профиль: имя и аватар ──────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct ProfileInfo {
    pub username: String,
    pub nick: String,
}

pub async fn get_profile(core: &Core, acc: &Account, stop: &Stop) -> Result<ProfileInfo, String> {
    let r = core
        .http
        .request(acc, "/api/auth/user", ReqOpts::get().timeout_ms(12_000), stop)
        .await
        .map_err(|e| e.to_string())?;
    let j = r
        .json
        .as_ref()
        .filter(|_| r.status == 200)
        .ok_or_else(|| format!("не удалось получить профиль: {}", r.status))?;
    Ok(ProfileInfo {
        username: j.get("username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        nick: j.get("nick").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    })
}

/// Смена username/nick.
///
/// Гейт «с 50 кармы» — фронтовый, API его не проверяет. PUT требует ВСЕ поля
/// профиля, поэтому читаем текущие и подменяем только заданные.
/// username — латиница (цифры не первыми), должен быть свободен; nick — буквы и
/// пробелы, без цифр, уникальность не нужна.
pub async fn change_name(
    core: &Core,
    acc: &Account,
    username: Option<&str>,
    nick: Option<&str>,
    stop: &Stop,
) -> Result<ProfileInfo, String> {
    let cur = core
        .http
        .request(acc, "/api/auth/user", ReqOpts::get().timeout_ms(10_000), stop)
        .await
        .map_err(|e| e.to_string())?;
    let u = cur
        .json
        .as_ref()
        .filter(|_| cur.status == 200)
        .ok_or_else(|| format!("не удалось получить данные пользователя: {}", cur.status))?;

    let take = |v: Option<&str>, fallback: &str| -> String {
        match v {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => fallback.to_string(),
        }
    };
    let body = json!({
        "username": take(username, u.get("username").and_then(|v| v.as_str()).unwrap_or("")),
        "nick": take(nick, u.get("nick").and_then(|v| v.as_str()).unwrap_or("")),
        "avatar": u.get("avatar").and_then(|v| v.as_str()).unwrap_or(""),
        "bio": u.get("bio").and_then(|v| v.as_str()).unwrap_or(""),
        "banner": u.get("banner").and_then(|v| v.as_str()).unwrap_or(""),
    });

    let mut opts = ReqOpts::put(body);
    opts.referer = Some(format!("{}/settings", base_url()));
    opts.timeout_ms = 15_000;
    let r = core.http.request(acc, "/api/auth/user", opts, stop).await.map_err(|e| e.to_string())?;
    if r.blocked {
        return Err("антибот mail.ru (418/429)".into());
    }
    if r.status == 200 {
        if let Some(j) = &r.json {
            return Ok(ProfileInfo {
                username: j.get("username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                nick: j.get("nick").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            });
        }
    }
    Err(format!("API не принял: {}", api_err_text(&r)))
}

fn re_cdn() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)/([a-f0-9]{50,}|[a-f0-9]+_[a-f0-9]+)\.(?:jpg|gif)").unwrap())
}

/// Хэш картинки из CDN-URL (`/api/pictures/images/<hash>.jpg`).
pub fn extract_cdn_hash(url: &str) -> Option<String> {
    re_cdn().captures(url).and_then(|c| c.get(1)).map(|m| m.as_str().to_string())
}

/// Смена аватара: заливаем файл (или берём готовый хэш из CDN-ссылки) и
/// прописываем его в профиль через PUT /api/auth/user.
pub async fn change_avatar(
    core: &Core,
    acc: &Account,
    file_or_url: &str,
    stop: &Stop,
) -> Result<String, String> {
    let hash = match extract_cdn_hash(file_or_url) {
        Some(h) => h,
        None => {
            let path = std::path::PathBuf::from(file_or_url);
            let up = core.http.upload_picture(acc, &path, stop).await?;
            extract_cdn_hash(&up.url)
                .or(Some(up.url.trim_end_matches(".jpg").trim_end_matches(".gif").to_string()))
                .ok_or_else(|| format!("не извлечь хэш из url: {}", up.url))?
        }
    };

    let cur = core
        .http
        .request(acc, "/api/auth/user", ReqOpts::get().timeout_ms(10_000), stop)
        .await
        .map_err(|e| e.to_string())?;
    let u = cur
        .json
        .as_ref()
        .filter(|_| cur.status == 200)
        .ok_or_else(|| format!("не удалось получить данные пользователя: {}", cur.status))?;

    let body = json!({
        "username": u.get("username").and_then(|v| v.as_str()).unwrap_or(""),
        "nick": u.get("nick").and_then(|v| v.as_str()).unwrap_or(""),
        "avatar": format!("/api/pictures/images/{hash}.jpg?size=origin"),
        "bio": u.get("bio").and_then(|v| v.as_str()).unwrap_or(""),
        "banner": u.get("banner").and_then(|v| v.as_str()).unwrap_or(""),
    });
    let mut opts = ReqOpts::put(body);
    opts.timeout_ms = 15_000;
    let r = core.http.request(acc, "/api/auth/user", opts, stop).await.map_err(|e| e.to_string())?;
    if r.status == 200 && r.json.is_some() {
        return Ok(hash);
    }
    Err(format!("API не принял: {}", api_err_text(&r)))
}

/// Текст ошибки из ответа API: сначала `message`, иначе кусок тела.
pub fn api_err_text(r: &Resp) -> String {
    if let Some(j) = &r.json {
        if let Some(m) = j.get("message").and_then(|v| v.as_str()) {
            return m.to_string();
        }
        return crate::util::clip(&j.to_string(), 160);
    }
    if r.text.trim().is_empty() {
        format!("HTTP {}", r.status)
    } else {
        format!("HTTP {} {}", r.status, r.snippet(140))
    }
}

/// Быстрая проверка доступности сайта под аккаунтом (для теста прокси).
pub async fn ping(core: &Core, acc: &Account, stop: &Stop) -> Result<u16, String> {
    let mut opts = ReqOpts::get().timeout_ms(15_000);
    opts.method = Method::Get;
    core.http
        .request(acc, "/api/notificator/notifications/unread", opts, stop)
        .await
        .map(|r| r.status)
        .map_err(|e| e.to_string())
}
