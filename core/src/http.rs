//! http.rs — HTTP-ядро: запросы к otvet.mail.ru под куками аккаунта.
//!
//! Порт httpclient.js. Браузер для РАБОТЫ бота не нужен — только для входа.
//!
//! Что здесь важно и почему:
//!  · UA/язык/client hints берутся ИЗ ОДНОЙ персоны (см. persona.rs). Антибот
//!    ловит не «редкое» значение, а противоречие между ними.
//!  · UA никогда не содержит «Headless»: mail.ru отдаёт 418 на мутирующие POST
//!    с headless-агентом.
//!  · Порядок заголовков воспроизводит Chrome: `http::HeaderMap` отдаёт их в
//!    порядке вставки, а hyper пишет в этом же порядке.
//!  · Set-Cookie мержится обратно в аккаунт — сессия mail.ru ротируется, и
//!    вечная отправка снятой при входе строки со временем протухает.
//!  · 418/429 = `blocked` (антибот), GET ретраится на 429/5xx.
//!  · Сетевой сбой засчитывается текущему прокси; после порога — ротация на
//!    следующий прокси аккаунта, и ретрай идёт уже через него.
//!
//! Отличие от JS-версии в лучшую сторону: reqwest говорит HTTP/2 по ALPN, как
//! настоящий Chrome (undici в Node здесь оставался на HTTP/1.1 — само по себе
//! противоречие с UA). TLS-отпечаток (JA3) всё равно НЕ браузерный: это стек ОС
//! (schannel), а не BoringSSL. Полное совпадение даёт только запрос из живого
//! браузера.

use crate::accounts::{jar_to_string, parse_cookie_jar, Account, AccountsStore};
use crate::persona::{Persona, PersonaOpts, PersonaStore};
use crate::proxy::{mask_proxy, parse_proxy};
use crate::util::{Log, Stop};
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Боевой адрес сайта.
pub const BASE: &str = "https://otvet.mail.ru";

/// Куда реально ходим. Переопределяется переменной `OTVET_BASE` — это нужно
/// тестам: они поднимают локальный сервер-заглушку и прогоняют по нему те же
/// самые циклы голосования и ленты, что уходят в mail.ru.
pub fn base_url() -> &'static str {
    static B: OnceLock<String> = OnceLock::new();
    B.get_or_init(|| match std::env::var("OTVET_BASE") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_string(),
        _ => BASE.to_string(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    fn as_reqwest(self) -> reqwest::Method {
        match self {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Delete => reqwest::Method::DELETE,
        }
    }
    fn is_mutating(self) -> bool {
        !matches!(self, Method::Get)
    }
}

#[derive(Debug)]
pub enum HttpError {
    /// Сеть/прокси/таймаут — то, что в JS прилетало как `fetch failed`.
    Network(String),
    /// Прогон остановлен пользователем.
    Aborted,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Network(m) => write!(f, "{m}"),
            HttpError::Aborted => write!(f, "остановлено"),
        }
    }
}

impl std::error::Error for HttpError {}

pub type HttpResult = Result<Resp, HttpError>;

/// Ответ сервера в том же виде, что отдавал `hc.request` в JS.
#[derive(Debug, Clone)]
pub struct Resp {
    pub status: u16,
    pub ok: bool,
    /// 418/429 — антибот mail.ru.
    pub blocked: bool,
    pub text: String,
    pub json: Option<Value>,
    /// Финальный URL после редиректов (по нему узнаём ник в `/profile/id<N>`).
    pub url: String,
}

impl Resp {
    pub fn result(&self) -> Option<&Value> {
        self.json.as_ref().and_then(|j| j.get("result"))
    }
    pub fn snippet(&self, n: usize) -> String {
        crate::util::clip(self.text.trim(), n)
    }
}

#[derive(Clone)]
pub struct ReqOpts {
    pub method: Method,
    pub json: Option<Value>,
    pub headers: Vec<(String, String)>,
    pub accept: Option<String>,
    pub referer: Option<String>,
    pub timeout_ms: u64,
    /// Ретраить GET на 429/5xx и один сетевой сбой.
    pub retry: bool,
}

impl Default for ReqOpts {
    fn default() -> Self {
        Self {
            method: Method::Get,
            json: None,
            headers: Vec::new(),
            accept: None,
            referer: None,
            timeout_ms: 30_000,
            retry: true,
        }
    }
}

impl ReqOpts {
    pub fn get() -> Self {
        Self::default()
    }
    pub fn post(json: Value) -> Self {
        Self { method: Method::Post, json: Some(json), ..Default::default() }
    }
    pub fn put(json: Value) -> Self {
        Self { method: Method::Put, json: Some(json), ..Default::default() }
    }
    pub fn delete(json: Value) -> Self {
        Self { method: Method::Delete, json: Some(json), ..Default::default() }
    }
    pub fn referer(mut self, r: impl Into<String>) -> Self {
        self.referer = Some(r.into());
        self
    }
    pub fn accept(mut self, a: impl Into<String>) -> Self {
        self.accept = Some(a.into());
        self
    }
    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }
    pub fn no_retry(mut self) -> Self {
        self.retry = false;
        self
    }
    pub fn html(self) -> Self {
        self.accept("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
    }
}

// Порядки заголовков сняты измерением с Chrome 148 (см. комментарий в httpclient.js).
const CHROME_ORDER_GET: &[&str] = &[
    "sec-ch-ua-platform",
    "user-agent",
    "accept",
    "sec-ch-ua",
    "accept-language",
    "sec-ch-ua-mobile",
    "sec-fetch-site",
    "sec-fetch-mode",
    "sec-fetch-dest",
    "referer",
    "cookie",
];
const CHROME_ORDER_POST: &[&str] = &[
    "sec-ch-ua-platform",
    "authorization",
    "accept-language",
    "sec-ch-ua",
    "sec-ch-ua-mobile",
    "user-agent",
    "accept",
    "content-type",
    "origin",
    "sec-fetch-site",
    "sec-fetch-mode",
    "sec-fetch-dest",
    "referer",
    "cookie",
];

/// HTTP-движок: пул клиентов по прокси + доступ к персонам и хранилищу аккаунтов
/// (нужно, чтобы сохранять ротацию кук).
pub struct Http {
    clients: Mutex<HashMap<String, reqwest::Client>>,
    personas: Arc<PersonaStore>,
    store: Arc<AccountsStore>,
    warned: Mutex<HashSet<String>>,
}

impl Http {
    pub fn new(personas: Arc<PersonaStore>, store: Arc<AccountsStore>) -> Self {
        Self { clients: Mutex::new(HashMap::new()), personas, store, warned: Mutex::new(HashSet::new()) }
    }

    pub fn personas(&self) -> &Arc<PersonaStore> {
        &self.personas
    }

    pub fn store(&self) -> &Arc<AccountsStore> {
        &self.store
    }

    /// Персона аккаунта. Сохранённая в самом аккаунте (`_persona` от JS-версии)
    /// имеет приоритет: под одними куками отпечаток меняться не должен.
    pub fn persona_for(&self, acc: &Account) -> Persona {
        if let Some(p) = acc.cached_persona() {
            return p;
        }
        let p = acc
            .stored_persona()
            .filter(|p| p.v == 2 && !p.ua.is_empty())
            .unwrap_or_else(|| self.personas.get(&acc.name, &PersonaOpts::default()));
        acc.cache_persona(p.clone());
        p
    }

    /// Клиент под конкретный прокси (кэш: один пул соединений на прокси, а не
    /// новый на каждый запрос — иначе в цикле голосов утекают сокеты).
    fn client_for(&self, proxy: Option<&str>, log: Option<&Log>) -> reqwest::Client {
        let key = proxy.unwrap_or("").to_string();
        if let Some(c) = self.clients.lock().get(&key) {
            return c.clone();
        }
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .pool_idle_timeout(Duration::from_secs(90))
            .redirect(reqwest::redirect::Policy::limited(10));
        if !key.is_empty() {
            match parse_proxy(&key) {
                Some(cfg) => match reqwest::Proxy::all(cfg.full_url()) {
                    Ok(p) => b = b.proxy(p),
                    Err(e) => self.warn_once(
                        &key,
                        &format!("⚠️  Прокси {} не принят ({e}). Запросы идут НАПРЯМУЮ.", mask_proxy(&key)),
                        log,
                    ),
                },
                None => self.warn_once(
                    &key,
                    &format!("⚠️  Прокси не распарсился ({}). Запросы идут НАПРЯМУЮ.", mask_proxy(&key)),
                    log,
                ),
            }
        }
        let c = b.build().unwrap_or_else(|_| reqwest::Client::new());
        let mut map = self.clients.lock();
        // Кэш ограничиваем: 50 аккаунтов × свои прокси не должны копиться вечно.
        if map.len() > 64 {
            map.clear();
        }
        map.insert(key, c.clone());
        c
    }

    fn warn_once(&self, key: &str, msg: &str, log: Option<&Log>) {
        if self.warned.lock().insert(key.to_string()) {
            match log {
                Some(l) => l(msg),
                None => eprintln!("{msg}"),
            }
        }
    }

    /// Базовый запрос. Возвращает `Resp` даже на 4xx/5xx; `Err` — только сеть,
    /// таймаут или «Стоп» (как `fetch` в JS: он тоже бросает лишь на сети).
    pub async fn request(&self, acc: &Account, path_or_url: &str, opts: ReqOpts, stop: &Stop) -> HttpResult {
        let url = if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            path_or_url.to_string()
        } else {
            format!("{}{path_or_url}", base_url())
        };

        let res = self.attempt(acc, &url, &opts, stop).await;

        let res = match res {
            Err(HttpError::Network(e)) => {
                // Сбой засчитываем прокси: после порога следующий прокси, и
                // ретрай пойдёт уже через него.
                if let Some((old, new)) = acc.note_proxy_fail() {
                    let msg = format!(
                        "🔄 Прокси не отвечает ×{} → сменил: {} → {}",
                        acc.rotate_threshold(),
                        mask_proxy(&old),
                        mask_proxy(&new)
                    );
                    if let Some(l) = acc.rt.rotate_log.lock().as_ref() {
                        l(&msg);
                    }
                }
                // Чтение повторяем дважды, запись — один раз. Дешёвые прокси
                // отваливаются волнами: наблюдаемая картина — «tunnel error»
                // на несколько попыток подряд, потом снова всё работает. Второй
                // повтор с паузой вытаскивает как раз такие провалы, а для
                // мутирующих запросов лишняя попытка опаснее потерянной.
                let attempts_left = if opts.method == Method::Get { 2 } else { 1 };
                if opts.retry && !stop.is_stopped() {
                    let mut last = HttpError::Network(e);
                    for i in 0..attempts_left {
                        let pause = if opts.method == Method::Get { 600 * (i + 1) } else { 1500 };
                        if stop.sleep_ms(pause).await {
                            return Err(HttpError::Aborted);
                        }
                        match self.attempt(acc, &url, &opts, stop).await {
                            Ok(r) => return Ok(r),
                            Err(HttpError::Aborted) => return Err(HttpError::Aborted),
                            Err(e2) => last = e2,
                        }
                    }
                    Err(last)
                } else {
                    Err(HttpError::Network(e))
                }
            }
            other => other,
        };

        let mut resp = res?;
        // GET на 429/5xx — ещё одна попытка (антибот часто отпускает через паузу).
        if opts.method == Method::Get
            && opts.retry
            && (resp.status == 429 || resp.status >= 500)
            && !stop.is_stopped()
        {
            let pause = 700 + crate::util::rand_range(0, 500) as u64;
            if !stop.sleep_ms(pause).await {
                if let Ok(r2) = self.attempt(acc, &url, &opts, stop).await {
                    resp = r2;
                }
            }
        }
        Ok(resp)
    }

    async fn attempt(&self, acc: &Account, url: &str, opts: &ReqOpts, stop: &Stop) -> HttpResult {
        let persona = self.persona_for(acc);
        let client = self.client_for(acc.active_proxy().as_deref(), acc.rt.rotate_log.lock().as_ref());

        let headers = self.build_headers(acc, &persona, opts);
        let mut rb = client.request(opts.method.as_reqwest(), url).headers(headers);
        if let Some(j) = &opts.json {
            rb = rb.body(serde_json::to_vec(j).unwrap_or_default());
        }

        let dur = Duration::from_millis(opts.timeout_ms.max(1));
        let send = async {
            let resp = rb.send().await.map_err(|e| HttpError::Network(short_err(&e)))?;
            let status = resp.status().as_u16();
            let final_url = resp.url().to_string();
            let set_cookies: Vec<String> = resp
                .headers()
                .get_all(reqwest::header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok().map(|s| s.to_string()))
                .collect();
            let text = resp.text().await.map_err(|e| HttpError::Network(short_err(&e)))?;
            Ok::<_, HttpError>((status, final_url, set_cookies, text))
        };

        let out = tokio::select! {
            biased;
            _ = stop.wait() => return Err(HttpError::Aborted),
            r = tokio::time::timeout(dur, send) => match r {
                Ok(v) => v?,
                Err(_) => return Err(HttpError::Network("таймаут запроса".into())),
            },
        };

        let (status, final_url, set_cookies, text) = out;
        self.merge_set_cookie(acc, &set_cookies);
        let json = if text.is_empty() { None } else { serde_json::from_str::<Value>(&text).ok() };
        Ok(Resp {
            status,
            ok: (200..300).contains(&status),
            blocked: status == 418 || status == 429,
            text,
            json,
            url: final_url,
        })
    }

    fn build_headers(&self, acc: &Account, persona: &Persona, opts: &ReqOpts) -> HeaderMap {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (k, v) in persona.http_headers() {
            pairs.push((k.to_string(), v));
        }
        // account.ua уважаем, только если он согласован с персоной: протухший UA
        // из старого пула молча заменяем персональным.
        if let Some(own) = acc.ua.as_deref() {
            if !own.to_lowercase().contains("headless") && own == persona.ua {
                set_pair(&mut pairs, "user-agent", own);
            }
        }
        set_pair(&mut pairs, "accept", opts.accept.as_deref().unwrap_or("application/json, text/plain, */*"));
        // sec-fetch-* Chrome шлёт на ЛЮБОЙ fetch, не только на мутирующий.
        set_pair(&mut pairs, "sec-fetch-site", "same-origin");
        set_pair(&mut pairs, "sec-fetch-mode", "cors");
        set_pair(&mut pairs, "sec-fetch-dest", "empty");
        if let Some(c) = acc.cookie_header() {
            if !c.is_empty() {
                set_pair(&mut pairs, "cookie", &c);
            }
        }
        if let Some(r) = &opts.referer {
            set_pair(&mut pairs, "referer", r);
            set_pair(&mut pairs, "origin", base_url());
        }
        if opts.method.is_mutating() {
            set_pair(&mut pairs, "content-type", "application/json");
            set_pair(&mut pairs, "authorization", "");
        }
        for (k, v) in &opts.headers {
            set_pair(&mut pairs, &k.to_lowercase(), v);
        }

        // Раскладываем в порядке Chrome; неизвестное дописываем в конец.
        let order = if opts.method.is_mutating() { CHROME_ORDER_POST } else { CHROME_ORDER_GET };
        let mut map = HeaderMap::new();
        let mut push = |name: &str, value: &str| {
            if let (Ok(n), Ok(v)) = (name.parse::<HeaderName>(), HeaderValue::from_str(value)) {
                map.insert(n, v);
            }
        };
        for name in order {
            if let Some((_, v)) = pairs.iter().find(|(k, _)| k == name) {
                push(name, v);
            }
        }
        for (k, v) in &pairs {
            if !order.contains(&k.as_str()) {
                push(k, v);
            }
        }
        map
    }

    /// Ротация сессионных кук: mail.ru обновляет сессию, и без этого мы вечно
    /// слали бы строку, снятую при входе.
    fn merge_set_cookie(&self, acc: &Account, set_cookies: &[String]) {
        if set_cookies.is_empty() {
            return;
        }
        let current = acc.cookie_header().unwrap_or_default();
        let mut jar = parse_cookie_jar(&current);
        let mut changed = false;
        for raw in set_cookies {
            let first = raw.split(';').next().unwrap_or("");
            let (name, value) = match first.split_once('=') {
                Some((n, v)) => (n.trim(), v.trim()),
                None => continue,
            };
            if name.is_empty() {
                continue;
            }
            // Регистр атрибутов не трогаем: дата в Expires приходит как
            // «Wed, 21 Oct 2015 07:28:00 GMT», и в нижнем регистре она уже не
            // разбирается — из-за этого удаление куки по Expires не срабатывало.
            let attrs = &raw[first.len().min(raw.len())..];
            // Удаление куки сервером: Max-Age<=0 либо Expires в прошлом.
            let deleted = max_age_expired(attrs) || expires_in_past(attrs);
            if deleted {
                let before = jar.len();
                jar.retain(|(n, _)| n != name);
                if jar.len() != before {
                    changed = true;
                }
                continue;
            }
            match jar.iter_mut().find(|(n, _)| n == name) {
                Some(slot) => {
                    if slot.1 != value {
                        slot.1 = value.to_string();
                        changed = true;
                    }
                }
                None => {
                    jar.push((name.to_string(), value.to_string()));
                    changed = true;
                }
            }
        }
        if !changed {
            return;
        }
        let merged = jar_to_string(&jar);
        acc.set_cookie_header(&merged);
        // Запись в accounts.json не должна ронять прогон.
        self.store.set_cookies(&acc.name, &merged);
    }

    /// Заливка картинки: POST /api/pictures/images, поле файла — `origin`.
    /// Ответ: `{ result: { size, url:"<хэш>.jpg", dimensions } }`.
    pub async fn upload_picture(
        &self,
        acc: &Account,
        path: &std::path::Path,
        stop: &Stop,
    ) -> Result<UploadedPicture, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("не прочитать файл: {e}"))?;
        let filename =
            path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "image.jpg".into());
        let mime = match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => "image/jpeg",
        };
        let part = reqwest::multipart::Part::bytes(bytes.clone())
            .file_name(filename)
            .mime_str(mime)
            .map_err(|e| e.to_string())?;
        let form = reqwest::multipart::Form::new().part("origin", part);

        let persona = self.persona_for(acc);
        let client = self.client_for(acc.active_proxy().as_deref(), None);
        let mut opts = ReqOpts::post(Value::Null);
        opts.referer = Some(format!("{}/", base_url()));
        let mut headers = self.build_headers(acc, &persona, &opts);
        // content-type ставит multipart-форма сама (с boundary).
        headers.remove("content-type");

        let req = client.post(format!("{}/api/pictures/images", base_url())).headers(headers).multipart(form);
        let fut = async {
            let resp = req.send().await.map_err(|e| short_err(&e))?;
            let status = resp.status().as_u16();
            let text = resp.text().await.map_err(|e| short_err(&e))?;
            Ok::<_, String>((status, text))
        };
        let (status, text) = tokio::select! {
            biased;
            _ = stop.wait() => return Err("остановлено".into()),
            r = tokio::time::timeout(Duration::from_secs(60), fut) => match r {
                Ok(v) => v?,
                Err(_) => return Err("таймаут заливки".into()),
            },
        };
        if status == 418 || status == 429 {
            return Err("blocked".into());
        }
        let json: Option<Value> = serde_json::from_str(&text).ok();
        if let Some(r) = json.as_ref().and_then(|j| j.get("result")) {
            if let Some(u) = r.get("url").and_then(|u| u.as_str()) {
                return Ok(UploadedPicture {
                    url: u.to_string(),
                    width: r.pointer("/dimensions/width").and_then(|v| v.as_i64()).unwrap_or(0),
                    height: r.pointer("/dimensions/height").and_then(|v| v.as_i64()).unwrap_or(0),
                    size: r.get("size").and_then(|v| v.as_i64()).unwrap_or(bytes.len() as i64),
                });
            }
        }
        Err(format!("HTTP {status} {}", crate::util::clip(&text, 140)))
    }
}

#[derive(Debug, Clone)]
pub struct UploadedPicture {
    pub url: String,
    pub width: i64,
    pub height: i64,
    pub size: i64,
}

fn set_pair(pairs: &mut Vec<(String, String)>, key: &str, value: &str) {
    match pairs.iter_mut().find(|(k, _)| k == key) {
        Some(p) => p.1 = value.to_string(),
        None => pairs.push((key.to_string(), value.to_string())),
    }
}

/// Позиция атрибута без учёта регистра (сами значения при этом не портим).
fn find_attr(attrs: &str, name: &str) -> Option<usize> {
    attrs.to_lowercase().find(name)
}

fn max_age_expired(attrs: &str) -> bool {
    if let Some(i) = find_attr(attrs, "max-age") {
        let rest = &attrs[i + 7..];
        let val: String = rest
            .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        if let Ok(n) = val.parse::<i64>() {
            return n <= 0;
        }
    }
    false
}

fn expires_in_past(attrs: &str) -> bool {
    if let Some(i) = find_attr(attrs, "expires") {
        let rest = &attrs[i + 7..];
        let val: String = rest
            .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
            .chars()
            .take_while(|c| *c != ';')
            .collect();
        let val = val.trim();
        // Куки датируются по RFC 1123 («Wed, 21 Oct 2015 07:28:00 GMT»); часть
        // серверов шлёт и вариант с «-» в дате — принимаем оба.
        if let Ok(t) = chrono::DateTime::parse_from_rfc2822(val) {
            return t.timestamp() <= chrono::Utc::now().timestamp();
        }
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(val, "%a, %d-%b-%Y %H:%M:%S GMT") {
            return t.and_utc().timestamp() <= chrono::Utc::now().timestamp();
        }
    }
    false
}

/// Короткое сообщение об ошибке: полный `reqwest::Error` в лог не влезает.
fn short_err(e: &reqwest::Error) -> String {
    // Самая глубокая причина полезнее верхнего слоя: reqwest на всё про всё
    // говорит «error sending request», а внизу лежит «прокси отверг
    // авторизацию» или «сертификат не проверился» — то, что реально чинят.
    let mut cause: Option<String> = None;
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        cause = Some(s.to_string());
        src = std::error::Error::source(s);
    }
    let head = if e.is_timeout() {
        "таймаут"
    } else if e.is_connect() {
        "нет соединения (прокси/сеть)"
    } else if e.is_request() {
        "запрос не ушёл"
    } else {
        "сбой запроса"
    };
    match cause {
        Some(c) => crate::util::clip(&format!("{head}: {c}"), 160),
        None => crate::util::clip(&format!("{head}: {e}"), 160),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cookie_deletion() {
        // Реальные Set-Cookie от mail.ru: регистр смешанный, дата — RFC 1123.
        assert!(max_age_expired("; Max-Age=0; Path=/"));
        assert!(max_age_expired("; max-age=-1"));
        assert!(!max_age_expired("; Max-Age=3600"));

        assert!(expires_in_past("; Expires=Wed, 21 Oct 2015 07:28:00 GMT; Path=/"));
        assert!(expires_in_past("; expires=Thu, 01-Jan-1970 00:00:00 GMT"));
        assert!(!expires_in_past("; Expires=Fri, 01 Jan 2100 00:00:00 GMT"));
        assert!(!expires_in_past("; Path=/"));
    }

    #[test]
    fn chrome_header_order_is_kept() {
        // HeaderMap отдаёт заголовки в порядке вставки, а hyper пишет их в этом
        // же порядке — на этом держится весь порядок «как у Chrome».
        let mut m = HeaderMap::new();
        for name in ["sec-ch-ua-platform", "user-agent", "accept", "cookie"] {
            m.insert(name.parse::<HeaderName>().unwrap(), HeaderValue::from_static("x"));
        }
        let got: Vec<&str> = m.keys().map(|k| k.as_str()).collect();
        assert_eq!(got, vec!["sec-ch-ua-platform", "user-agent", "accept", "cookie"]);
    }
}
