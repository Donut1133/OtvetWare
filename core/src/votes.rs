//! votes.rs — голосование (карма) и список проголосовавших. Порт bot.js.
//!
//! Голос — чистый HTTP: POST `/api/topic/{topics|reply}/{id}/{rt}`, rt=1 плюс,
//! 2 минус. Успех подтверждается полем `result.user_reaction` — сервер умеет
//! молча отклонить голос, и без этой проверки бот радостно врал бы в лог.

use crate::accounts::Account;
use crate::api;
use crate::http::{HttpError, ReqOpts};
use crate::util::{Log, Stop};
use crate::{Core, RunOutcome};
use regex::Regex;
use serde_json::json;
use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vote {
    Plus,
    Minus,
}

impl Vote {
    pub fn rt(self) -> i64 {
        match self {
            Vote::Plus => 1,
            Vote::Minus => 2,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Vote::Plus => "+1 ▲",
            Vote::Minus => "-1 ▼",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Topic,
    Reply,
}

impl Kind {
    fn api_sub(self) -> &'static str {
        match self {
            Kind::Topic => "topics",
            Kind::Reply => "reply",
        }
    }
    fn source(self) -> &'static str {
        match self {
            Kind::Topic => "topics",
            Kind::Reply => "reply",
        }
    }
    pub fn ru(self) -> &'static str {
        match self {
            Kind::Topic => "пост",
            Kind::Reply => "ответ",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Target {
    pub kind: Kind,
    pub id: String,
}

fn re_reply() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[?&#]reply=(\d+)").unwrap())
}

fn re_question() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"/question/(\d+)").unwrap())
}

fn re_profile() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"/profile/([^/?#]+)").unwrap())
}

/// `{kind, id}` из ссылки на вопрос/ответ. `?reply=` важнее — ссылка на ответ
/// всегда содержит и `/question/<id>`.
pub fn parse_target_id(url: &str) -> Option<Target> {
    if let Some(c) = re_reply().captures(url) {
        return Some(Target { kind: Kind::Reply, id: c[1].to_string() });
    }
    if let Some(c) = re_question().captures(url) {
        return Some(Target { kind: Kind::Topic, id: c[1].to_string() });
    }
    None
}

/// Ссылка ведёт на конкретный пост/ответ (а не на профиль)?
pub fn is_single_target(url: &str) -> bool {
    re_question().is_match(url) || re_reply().is_match(url)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteRes {
    Ok,
    Blocked,
    /// Сервер ответил, но голос не зарегистрировался.
    NoReg,
}

/// Один голос. Браузер не нужен.
pub async fn vote_one(
    core: &Core,
    acc: &Account,
    target: &Target,
    vote: Vote,
    referer: &str,
    stop: &Stop,
) -> Result<VoteRes, HttpError> {
    let rt = vote.rt();
    let path = format!("/api/topic/{}/{}/{}", target.kind.api_sub(), target.id, rt);
    let body = json!({
        "reactionType": rt,
        "entityID": target.id.parse::<i64>().unwrap_or(0),
        "reactionSource": target.kind.source(),
    });
    // Ретраи здесь ЗАПРЕЩЕНЫ. Повтор того же голоса сайт понимает как отмену:
    // если первый POST дошёл, а ответ потерялся по дороге (обычное дело на
    // дохлом прокси), вторая попытка снимет только что поставленный голос.
    // Лучше потерять голос и честно написать об этом, чем тихо его отменить.
    let opts = ReqOpts::post(body).referer(referer).no_retry();
    let r = core.http.request(acc, &path, opts, stop).await?;
    if r.blocked {
        return Ok(VoteRes::Blocked);
    }
    let ok = r
        .result()
        .and_then(|res| res.get("user_reaction"))
        .and_then(|v| v.as_i64())
        .map(|v| v == rt)
        .unwrap_or(false);
    Ok(if r.ok && ok { VoteRes::Ok } else { VoteRes::NoReg })
}

/// Голос на ОДИН конкретный пост/ответ по ссылке.
#[allow(clippy::too_many_arguments)]
pub async fn vote_on_single(
    core: &Core,
    acc: &Account,
    url: &str,
    vote: Vote,
    delay: f64,
    log: &Log,
    stop: &Stop,
    blocked: &mut bool,
) -> i64 {
    let Some(target) = parse_target_id(url) else {
        log("❌ Ссылка не похожа на вопрос/ответ (нужен /question/... или ?reply=...).");
        return 0;
    };
    let referer = url.split('#').next().unwrap_or(url).split('?').next().unwrap_or(url).to_string();
    log(&format!("🎯 {} #{} → {}", target.kind.ru(), target.id, vote.label()));
    if stop.is_stopped() {
        log("⛔ Остановлено пользователем");
        return 0;
    }
    if *blocked {
        log("🛑 Блокировка (418/429).");
        return 0;
    }
    if stop.sleep_human(delay).await {
        return 0;
    }
    match vote_one(core, acc, &target, vote, &referer, stop).await {
        Ok(VoteRes::Ok) => {
            log(&format!("✅ Голос ({}) поставлен на {} #{}!", vote.label(), target.kind.ru(), target.id));
            1
        }
        Ok(VoteRes::Blocked) => {
            *blocked = true;
            log(&format!(
                "🛑 Антибот mail.ru (418/429) — голос на {} #{} отклонён.",
                target.kind.ru(),
                target.id
            ));
            0
        }
        Ok(VoteRes::NoReg) => {
            log(&format!(
                "⚠️  Голос НЕ зарегался на {} #{} (сервер молча отклонил).",
                target.kind.ru(),
                target.id
            ));
            0
        }
        Err(HttpError::Aborted) => 0,
        Err(e) => {
            log(&format!("⚠️  Сеть при голосе на {} #{}: {e}", target.kind.ru(), target.id));
            0
        }
    }
}

#[derive(Debug, Clone)]
pub struct Victim {
    pub id: i64,
    pub name: String,
}

/// userId + ник по ссылке на профиль (`/profile/id<N>` либо `/profile/<ник>`).
pub async fn resolve_profile(core: &Core, acc: &Account, profile_url: &str, stop: &Stop) -> Option<Victim> {
    let caps = re_profile().captures(profile_url)?;
    let raw = caps.get(1)?.as_str();
    let name = urlencoding::decode(raw).map(|c| c.into_owned()).unwrap_or_else(|_| raw.to_string());
    if let Some(rest) = name.strip_prefix("id") {
        if let Ok(id) = rest.parse::<i64>() {
            return Some(Victim { id, name });
        }
    }
    let r = core
        .http
        .request(acc, &format!("/api/auth/users/{}", urlencoding::encode(&name)), ReqOpts::get(), stop)
        .await
        .ok()?;
    let j = r.json.as_ref()?;
    let id = j.get("id").and_then(|v| v.as_i64())?;
    let uname = j.get("username").and_then(|v| v.as_str()).unwrap_or(&name).to_string();
    Some(Victim { id, name: uname })
}

/// Голос по ВСЕМ постам (или ответам) профиля жертвы, до `limit`.
///
/// Курсор пагинации у mail.ru инклюзивный: без дедупа тот же пост голосуется
/// дважды, а второй голос — это toggle-off, то есть снятие своего же голоса.
#[allow(clippy::too_many_arguments)]
pub async fn vote_on_profile(
    core: &Core,
    acc: &Account,
    profile_url: &str,
    vote: Vote,
    limit: i64,
    delay: f64,
    log: &Log,
    stop: &Stop,
    blocked: &mut bool,
) -> i64 {
    let Some(victim) = resolve_profile(core, acc, profile_url, stop).await else {
        log("❌ Не нашёл user_id жертвы по ссылке профиля");
        return 0;
    };
    let want_replies = {
        let s = profile_url.trim_end_matches('/');
        s.ends_with("/answers") || profile_url.contains("/answers?") || profile_url.contains("/answers#")
    };
    let kind = if want_replies { Kind::Reply } else { Kind::Topic };
    log(&format!(
        "🎯 Профиль: {} (id {}) → {} на {}",
        victim.name,
        victim.id,
        if vote == Vote::Plus { "👍 плюсы ▲" } else { "👎 минусы ▼" },
        if want_replies { "ответы" } else { "посты" }
    ));

    let (mut success, mut skipped, mut processed) = (0i64, 0i64, 0i64);
    let mut pos: i64 = 0;
    let mut seen: HashSet<i64> = HashSet::new();

    loop {
        if stop.is_stopped() {
            log("\n⛔ Остановлено пользователем");
            break;
        }
        if *blocked {
            log("\n🛑 Блокировка mail.ru (418/429) — останавливаю аккаунт.");
            break;
        }
        if limit > 0 && processed >= limit {
            log(&format!("\n🏁 Достигнут лимит {limit}."));
            break;
        }

        let sub = if want_replies { "replies" } else { "topics" };
        let storage = format!("profile-{}-{}", victim.id, if want_replies { "replies" } else { "posts" });
        let feed = format!(
            "/api/topic/profile/{}/{}?userID={}&dir={}&pos={}&limit=20&storage={}",
            victim.id,
            sub,
            victim.id,
            if want_replies { 0 } else { 1 },
            pos,
            storage
        );
        let r = match core.http.request(acc, &feed, ReqOpts::get(), stop).await {
            Ok(r) => r,
            Err(HttpError::Aborted) => break,
            Err(e) => {
                log(&format!("⚠️  Не загрузить посты жертвы: {e}"));
                break;
            }
        };
        if r.blocked {
            *blocked = true;
            log("🛑 Блокировка при загрузке постов жертвы.");
            break;
        }
        if !r.ok {
            log(&format!("⚠️  Не загрузить посты жертвы (HTTP {}).", r.status));
            break;
        }
        let items = r
            .result()
            .and_then(|res| res.get(if want_replies { "replies" } else { "feed" }))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            log("📄 Посты/ответы кончились.");
            break;
        }

        let mut fresh = 0;
        let mut last_id = pos;
        for it in &items {
            if stop.is_stopped() || *blocked {
                break;
            }
            if limit > 0 && processed >= limit {
                break;
            }
            let Some(id) = it.get("id").and_then(|v| v.as_i64()) else { continue };
            last_id = id;
            if !seen.insert(id) {
                continue; // дубль из перекрытия страниц — второй голос снял бы первый
            }
            fresh += 1;
            let referer = if want_replies {
                format!(
                    "https://otvet.mail.ru/question/{}",
                    it.get("topic_id").and_then(|v| v.as_i64()).unwrap_or(0)
                )
            } else {
                format!("https://otvet.mail.ru/question/{id}")
            };
            let target = Target { kind, id: id.to_string() };
            match vote_one(core, acc, &target, vote, &referer, stop).await {
                Ok(VoteRes::Ok) => {
                    success += 1;
                    log(&format!("  ✅ {} #{id}", kind.ru()));
                }
                Ok(VoteRes::Blocked) => {
                    *blocked = true;
                    log(&format!("🛑 Антибот на {} #{id} — стоп аккаунта.", kind.ru()));
                    break;
                }
                Ok(VoteRes::NoReg) => {
                    skipped += 1;
                    log(&format!("  ⏭️  {} #{id} (не зарегался)", kind.ru()));
                }
                Err(HttpError::Aborted) => break,
                Err(e) => {
                    skipped += 1;
                    log(&format!("  ⚠️  {} #{id}: {e}", kind.ru()));
                }
            }
            processed += 1;
            if stop.sleep_human(delay).await {
                break;
            }
        }

        let limit_hit = limit > 0 && processed >= limit;
        if fresh == 0 && !stop.is_stopped() && !*blocked && !limit_hit {
            log("📄 Новых постов/ответов нет — стоп.");
            break;
        }
        // Пагинация курсором = id последнего элемента страницы.
        if last_id == pos {
            break;
        }
        pos = last_id;
    }

    log(&format!(
        "\n{}\n✅ Готово!  Проголосовано: {success}  |  Пропущено: {skipped}\n{}",
        "=".repeat(50),
        "=".repeat(50)
    ));
    success
}

#[derive(Debug, Clone)]
pub struct VoteParams {
    /// Ссылки: профили и/или конкретные посты/ответы.
    pub targets: Vec<String>,
    pub vote: Vote,
    pub delay: f64,
    /// 0 = без лимита.
    pub limit: i64,
    pub check_auth: bool,
}

impl Default for VoteParams {
    fn default() -> Self {
        Self { targets: vec![], vote: Vote::Plus, delay: 2.0, limit: 0, check_auth: true }
    }
}

/// Точка входа режима «Голоса» для одного аккаунта.
pub async fn run_votes(core: &Core, acc: &Account, p: &VoteParams, log: &Log, stop: &Stop) -> RunOutcome {
    let mut out = RunOutcome::default();
    let targets = crate::util::unique_targets(&p.targets);
    if targets.is_empty() {
        log("❌ Не заданы ссылки.");
        return out;
    }

    if p.check_auth {
        let v = api::validate_account(core, acc, stop).await;
        api::persist_validation(core, &acc.name, &v);
        out.karma = v.karma.clone();
        if v.blocked {
            out.blocked = true;
            log("🛑 Антибот (418/429) при проверке — статус не меняю.");
        } else if v.alive {
            log("✅ Авторизован");
        } else if v.auth_bad {
            log("🔒 НЕ авторизован");
            log("Пропускаю аккаунт — не залогинен. Открой его через «Войти заново».");
            out.skipped = true;
            return out;
        } else {
            log("⚠️  Не удалось проверить авторизацию (ошибка/сеть) — продолжаю, статус не трогаю.");
        }
    }

    let mut blocked = out.blocked;
    let total = targets.len();
    for (i, t) in targets.iter().enumerate() {
        if stop.is_stopped() {
            log("\n⛔ Остановлено пользователем");
            break;
        }
        if blocked {
            log("\n🛑 Блокировка mail.ru (418/429) — пропускаю остальные ссылки.");
            break;
        }
        if total > 1 {
            log(&format!("\n🔗 Ссылка {}/{}: {}", i + 1, total, t));
        }
        out.done += if is_single_target(t) {
            vote_on_single(core, acc, t, p.vote, p.delay, log, stop, &mut blocked).await
        } else {
            vote_on_profile(core, acc, t, p.vote, p.limit, p.delay, log, stop, &mut blocked).await
        };
    }
    out.blocked = blocked;
    out
}

// ─── Кто голосовал ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Voter {
    pub author_id: i64,
    /// Нормализовано к схеме приложения: 1 = плюс, 2 = минус.
    pub reaction: i64,
    pub mine: bool,
    pub created_at: String,
    pub nick: String,
    pub resolved: bool,
}

/// Список проголосовавших на пост/ответ.
///
/// ВНИМАНИЕ: листинг кодирует реакцию ИНАЧЕ, чем эндпоинт голосования — там
/// 1 = плюс, 0 = минус (двойки не бывает). Нормализуем, иначе минусующие
/// потерялись бы при фильтрации по reaction==2.
pub async fn list_voters(
    core: &Core,
    acc: &Account,
    target: &Target,
    stop: &Stop,
) -> Result<Vec<Voter>, String> {
    let path = match target.kind {
        Kind::Reply => format!("/api/topic/reply/{}", target.id),
        Kind::Topic => format!("/api/topic/topics/{}", target.id),
    };
    let r = core.http.request(acc, &path, ReqOpts::get(), stop).await.map_err(|e| e.to_string())?;
    if !r.ok {
        return Err(format!("HTTP {} от {path}", r.status));
    }
    let list = r.result().and_then(|v| v.as_array()).cloned().unwrap_or_default();
    Ok(list
        .iter()
        .map(|v| {
            let id = v.get("author_id").and_then(|x| x.as_i64()).unwrap_or(0);
            Voter {
                author_id: id,
                reaction: if v.get("reaction").and_then(|x| x.as_i64()) == Some(1) { 1 } else { 2 },
                mine: v.get("mine").and_then(|x| x.as_bool()).unwrap_or(false),
                created_at: v.get("created_at").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                nick: format!("id{id}"),
                resolved: false,
            }
        })
        .collect())
}

/// Ник по user_id через редирект `/profile/id<N>` → `/profile/<ник>`.
/// Дорого (каждый заход проходит антибот-челлендж), поэтому вызывается точечно.
pub async fn resolve_nick(core: &Core, acc: &Account, user_id: i64, stop: &Stop) -> Option<String> {
    let r =
        core.http.request(acc, &format!("/profile/id{user_id}"), ReqOpts::get().html(), stop).await.ok()?;
    let caps = re_profile().captures(&r.url)?;
    let raw = caps.get(1)?.as_str();
    let dec = urlencoding::decode(raw).map(|c| c.into_owned()).unwrap_or_else(|_| raw.to_string());
    if dec.starts_with("id") && dec[2..].chars().all(|c| c.is_ascii_digit()) {
        None
    } else {
        Some(dec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets() {
        let t = parse_target_id("https://otvet.mail.ru/question/12345").unwrap();
        assert_eq!(t.kind, Kind::Topic);
        assert_eq!(t.id, "12345");

        let t = parse_target_id("https://otvet.mail.ru/question/12345?reply=678").unwrap();
        assert_eq!(t.kind, Kind::Reply);
        assert_eq!(t.id, "678");

        assert!(parse_target_id("https://otvet.mail.ru/profile/id42").is_none());
        assert!(!is_single_target("https://otvet.mail.ru/profile/vasya"));
    }
}
