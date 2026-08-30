//! replier.rs — режим «Комменты»: отвечаем тем, кто написал под нашим ответом.
//! Порт replier.js.
//!
//! Контракт уведомлений (снят живьём + с JS-бандла):
//!   GET  /api/notificator/notifications[?limit=20][&before=<id>]
//!        → {result:{unread[],day[],month[],rest[]}}, новейшие сверху, потолок 20.
//!        Листание ТОЛЬКО через `before` — `pos`/`offset`/`page` игнорируются.
//!   POST /api/topic/answers {…, reply_to} — ответ в ветку.
//!   POST /api/notificator/notifications/read — помечает прочитанным ВСЁ разом
//!        (параметр id сервером игнорируется), поэтому по умолчанию выключено.
//!
//! Ответ HTTP 200 с `result: null` — это НЕ «уведомлений нет», а троттлинг
//! аккаунта или IP. Путать нельзя: в первом случае надо ждать, во втором — нет.

use crate::accounts::Account;
use crate::ai::{AiCfg, AiError, Msg, NO_MARKDOWN};
use crate::answerer::with_signature;
use crate::api;
use crate::content::{doc_to_text, text_to_doc};
use crate::http::{HttpError, ReqOpts};
use crate::journals::{self, RepliedEntry};
use crate::uniq::{uniquify, UniqMode};
use crate::util::{clip, pick_one, rand_f64, Log, Progress, Stop};
use crate::{Core, RunOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub const NOAI_REPLIES: &[&str] = &[
    "да лан",
    "ну а чё сразу так",
    "согласен",
    "ну хз",
    "бывает",
    "справедливо",
    "ты прав вообще-то",
    "ага, примерно так",
    "ну тут не поспоришь",
    "сам в шоке",
    "да ладно тебе",
    "ну а чё не так",
    "логично",
    "да, есть такое",
    "хорошая мысль",
    "спасибо",
    "кек",
    "не, ну ты чего",
    "бывает и хуже",
    "ну ты понял",
];

/// Сколько отказов подряд считать «дальше бесполезно». Обычная причина —
/// исчерпан дневной лимит ответов или умер ключ нейросети; каждая следующая
/// цель стоит запроса к сайту и оплаченной генерации, а результат тот же.
const MAX_FAILS: i64 = 5;

pub const TYPE_REPLY: &str = "new_reply_reply";
pub const TYPE_TOPIC: &str = "new_topic_reply";
pub const TYPE_MENTION: &str = "new_reply_mention";

fn type_label(t: &str) -> &'static str {
    match t {
        TYPE_REPLY => "коммент под ответом",
        TYPE_TOPIC => "ответ на мой вопрос",
        TYPE_MENTION => "упоминание",
        _ => "событие",
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NotifTypes {
    pub reply: bool,
    pub topic: bool,
    pub mention: bool,
}

impl Default for NotifTypes {
    fn default() -> Self {
        Self { reply: true, topic: false, mention: false }
    }
}

impl NotifTypes {
    fn wanted(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.reply {
            v.push(TYPE_REPLY);
        }
        if self.topic {
            v.push(TYPE_TOPIC);
        }
        if self.mention {
            v.push(TYPE_MENTION);
        }
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplyMode {
    Ai,
    NoAi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyParams {
    pub mode: ReplyMode,
    /// 0 = без лимита.
    pub limit: i64,
    pub delay_min: f64,
    pub delay_max: f64,
    pub types: NotifTypes,
    /// Не отвечать на комменты своих же аккаунтов.
    pub skip_own: bool,
    /// Не трогать реплики старше N часов (0 = без ограничения).
    pub max_age_hours: f64,
    /// Не больше N наших ответов в одну ветку.
    pub max_per_thread: i64,
    /// Сколько страниц уведомлений просмотреть (по 20).
    pub pages: i64,
    /// Подтягивать текст вопроса в контекст.
    pub use_question: bool,
    /// Собирать полную цепочку разговора.
    pub use_chain: bool,
    pub max_chain: usize,
    pub ai: AiCfg,
    pub style: String,
    pub custom_prompt: String,
    pub mention: String,
    pub noai_replies: Vec<String>,
    /// Уникализация текста реплики.
    pub uniq: UniqMode,
    pub uniq_latin: bool,
    /// Проверять, что реплика осталась в ветке (её мог снести антиспам).
    pub verify_posted: bool,
    pub verify_delay_sec: f64,
    pub signature: String,
    /// Пометить уведомления прочитанными в конце (mail.ru пометит ВСЕ).
    pub mark_read: bool,
    pub check_auth: bool,
    /// Живой счётчик для интерфейса.
    #[serde(skip)]
    pub progress: Progress,
}

impl Default for ReplyParams {
    fn default() -> Self {
        Self {
            mode: ReplyMode::Ai,
            limit: 5,
            delay_min: 25.0,
            delay_max: 60.0,
            types: NotifTypes::default(),
            skip_own: true,
            max_age_hours: 24.0,
            max_per_thread: 1,
            pages: 1,
            use_question: true,
            use_chain: true,
            max_chain: 6,
            ai: AiCfg::preset(),
            style: "Обычный чел".into(),
            custom_prompt: String::new(),
            mention: String::new(),
            noai_replies: vec![],
            uniq: UniqMode::Off,
            uniq_latin: false,
            verify_posted: true,
            verify_delay_sec: 6.0,
            signature: String::new(),
            mark_read: false,
            check_auth: true,
            progress: Progress::new(),
        }
    }
}

/// Цель для ответа, собранная из уведомления.
#[derive(Debug, Clone, Default)]
pub struct Target {
    pub notif_id: String,
    pub kind: String,
    pub topic_id: String,
    pub root_id: String,
    pub root_type: String,
    pub entity_id: String,
    pub author_id: String,
    pub author_name: String,
    pub author_nick: String,
    /// НАШ текст (ответ или заголовок вопроса).
    pub root_text: String,
    /// Текст собеседника.
    pub comment_text: String,
    pub created_at: String,
    pub url: String,
}

impl Target {
    fn who(&self) -> String {
        if !self.author_name.is_empty() {
            format!("@{}", self.author_name)
        } else if !self.author_nick.is_empty() {
            self.author_nick.clone()
        } else {
            "аноним".into()
        }
    }
}

// ─── Время ──────────────────────────────────────────────────────────────────

/// mail.ru отдаёт МОСКОВСКОЕ время с суффиксом «Z», то есть выдаёт его за UTC.
/// Чиним сдвигом на −3 часа: при ошибке в другую сторону реплики считались бы
/// моложе, чем есть, и бот отвечал бы на старое.
const MSK_OFFSET_SEC: i64 = 3 * 3600;

fn norm_notif_time(iso: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(iso) else { return String::new() };
    if iso.ends_with('Z') || iso.ends_with('z') {
        let fixed = t - chrono::Duration::seconds(MSK_OFFSET_SEC);
        fixed.to_rfc3339()
    } else {
        iso.to_string()
    }
}

fn ago(iso: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(iso) else { return String::new() };
    let m = ((chrono::Utc::now().timestamp() - t.timestamp()) / 60).max(0);
    if m < 1 {
        return "только что".into();
    }
    if m < 60 {
        return format!("{m} мин назад");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h} ч назад");
    }
    format!("{} дн назад", h / 24)
}

fn age_hours(iso: &str) -> f64 {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(iso) else { return f64::INFINITY };
    ((chrono::Utc::now().timestamp() - t.timestamp()) as f64 / 3600.0).max(0.0)
}

// ─── Уведомления ────────────────────────────────────────────────────────────

pub struct NotifPage {
    pub items: Vec<Value>,
    pub ok: bool,
    pub blocked: bool,
    /// HTTP 200, но `result: null` — mail.ru душит аккаунт/IP.
    pub throttled: bool,
    pub error: Option<String>,
}

pub async fn fetch_notifications(core: &Core, acc: &Account, before: Option<&str>, stop: &Stop) -> NotifPage {
    let mut url = "/api/notificator/notifications?limit=20".to_string();
    if let Some(b) = before {
        url.push_str(&format!("&before={b}"));
    }
    let r = match core.http.request(acc, &url, ReqOpts::get().referer("https://otvet.mail.ru/"), stop).await {
        Ok(r) => r,
        Err(e) => {
            return NotifPage {
                items: vec![],
                ok: false,
                blocked: false,
                throttled: false,
                error: Some(e.to_string()),
            }
        }
    };
    if r.blocked {
        return NotifPage { items: vec![], ok: false, blocked: true, throttled: false, error: None };
    }
    if !r.ok {
        return NotifPage {
            items: vec![],
            ok: false,
            blocked: false,
            throttled: false,
            error: Some(format!("HTTP {}", r.status)),
        };
    }
    let Some(res) = r.result() else {
        return NotifPage { items: vec![], ok: true, blocked: false, throttled: true, error: None };
    };
    let mut items = Vec::new();
    for bucket in ["unread", "day", "month", "rest"] {
        if let Some(arr) = res.get(bucket).and_then(|b| b.as_array()) {
            items.extend(arr.iter().cloned());
        }
    }
    NotifPage { items, ok: true, blocked: false, throttled: false, error: None }
}

/// Уведомление → цель. `None`, если тип не наш или нет обязательных полей.
pub fn to_target(n: &Value) -> Option<Target> {
    if n.get("entity_type").and_then(|v| v.as_str()) != Some("reply") {
        return None;
    }
    let entity_id = n.get("entity_id").map(val_to_string)?;
    if entity_id.is_empty() {
        return None;
    }
    let page_uri = n.get("page_uri").and_then(|v| v.as_str()).unwrap_or("");
    let topic_id: String =
        page_uri.chars().skip_while(|c| !c.is_ascii_digit()).take_while(|c| c.is_ascii_digit()).collect();
    if topic_id.is_empty() {
        return None;
    }
    let a =
        n.get("authors").and_then(|v| v.as_array()).and_then(|v| v.first()).cloned().unwrap_or(Value::Null);
    Some(Target {
        notif_id: n.get("id").map(val_to_string).unwrap_or_default(),
        kind: n.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        topic_id: topic_id.clone(),
        root_id: n.get("root_id").map(val_to_string).unwrap_or_default(),
        root_type: n.get("root_type").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        entity_id,
        author_id: a.get("id").map(val_to_string).unwrap_or_default(),
        author_name: a.get("username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        author_nick: a.get("nick").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        // title = ТЕКСТ НАШЕГО объекта, body = реплика собеседника (сверено с деревом).
        root_text: n.get("title").and_then(|v| v.as_str()).unwrap_or("").trim().to_string(),
        comment_text: n.get("body").and_then(|v| v.as_str()).unwrap_or("").trim().to_string(),
        created_at: norm_notif_time(n.get("created_at").and_then(|v| v.as_str()).unwrap_or("")),
        url: format!("https://otvet.mail.ru/question/{topic_id}"),
    })
}

fn val_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub struct Located {
    pub found: bool,
    pub entity: Option<Value>,
    pub siblings: Vec<Value>,
    pub blocked: bool,
    pub throttled: bool,
    pub error: Option<String>,
}

/// Найти реплику собеседника в дереве и заодно посмотреть соседей.
pub async fn locate_entity(core: &Core, acc: &Account, t: &Target, stop: &Stop) -> Located {
    let url = if t.root_type == "reply" && !t.root_id.is_empty() {
        format!("/api/topic/answers/{}?reply_id={}", t.topic_id, t.root_id)
    } else {
        format!("/api/topic/answers/{}", t.topic_id)
    };
    let r = match core.http.request(acc, &url, ReqOpts::get(), stop).await {
        Ok(r) => r,
        Err(e) => {
            return Located {
                found: false,
                entity: None,
                siblings: vec![],
                blocked: false,
                throttled: false,
                error: Some(e.to_string()),
            }
        }
    };
    if r.blocked {
        return Located {
            found: false,
            entity: None,
            siblings: vec![],
            blocked: true,
            throttled: false,
            error: None,
        };
    }
    if !r.ok {
        return Located {
            found: false,
            entity: None,
            siblings: vec![],
            blocked: false,
            throttled: false,
            error: Some(format!("HTTP {}", r.status)),
        };
    }
    let Some(res) = r.result() else {
        return Located {
            found: false,
            entity: None,
            siblings: vec![],
            blocked: false,
            throttled: true,
            error: None,
        };
    };
    let siblings = res.get("replies").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let entity =
        siblings.iter().find(|x| x.get("id").map(val_to_string).unwrap_or_default() == t.entity_id).cloned();
    Located { found: entity.is_some(), entity, siblings, blocked: false, throttled: false, error: None }
}

#[derive(Debug, Clone)]
pub struct ChainNode {
    pub id: String,
    pub username: String,
    pub text: String,
    pub mine: bool,
}

/// Полная цепочка разговора сверху вниз.
///
/// mail.ru отдаёт только СПИСОК id (`rootpath`), тексты приходится добирать:
/// содержимое звена лежит в дереве его РОДИТЕЛЯ, а самый верхний ответ — в общем
/// списке ответов вопроса.
pub async fn fetch_chain(
    core: &Core,
    acc: &Account,
    t: &Target,
    my_id: &str,
    max_chain: usize,
    seed_tree: Vec<Value>,
    stop: &Stop,
) -> Option<Vec<ChainNode>> {
    let r = core
        .http
        .request(
            acc,
            &format!("/api/topic/topic/{}/reply/{}/rootpath", t.topic_id, t.entity_id),
            ReqOpts::get(),
            stop,
        )
        .await
        .ok()?;
    if !r.ok || r.blocked {
        return None;
    }
    let ids: Vec<String> = r.result()?.as_array()?.iter().map(val_to_string).collect();
    if ids.len() < 2 {
        return None; // обычный ответ на вопрос — цепочки нет
    }

    let keep = max_chain.max(2);
    let need: HashSet<&String> = ids.iter().skip(ids.len().saturating_sub(keep)).collect();

    let mut tree_cache: HashMap<String, Vec<Value>> = HashMap::new();
    if !t.root_id.is_empty() {
        tree_cache.insert(t.root_id.clone(), seed_tree);
    }
    let mut found: HashMap<String, Value> = HashMap::new();

    for i in 1..ids.len() {
        if !need.contains(&ids[i]) {
            continue;
        }
        let parent = ids[i - 1].clone();
        if !tree_cache.contains_key(&parent) {
            let replies = core
                .http
                .request(
                    acc,
                    &format!("/api/topic/answers/{}?reply_id={parent}", t.topic_id),
                    ReqOpts::get(),
                    stop,
                )
                .await
                .ok()
                .and_then(|rr| {
                    rr.result().and_then(|res| res.get("replies")).and_then(|v| v.as_array()).cloned()
                })
                .unwrap_or_default();
            tree_cache.insert(parent.clone(), replies);
        }
        if let Some(node) = tree_cache.get(&parent).and_then(|tree| {
            tree.iter().find(|x| x.get("id").map(val_to_string).unwrap_or_default() == ids[i])
        }) {
            found.insert(ids[i].clone(), node.clone());
        }
    }

    // Верхнее звено живёт только в общем списке ответов вопроса.
    if need.contains(&ids[0]) {
        if let Ok(top) =
            core.http.request(acc, &format!("/api/topic/answers/{}", t.topic_id), ReqOpts::get(), stop).await
        {
            if let Some(res) = top.result() {
                let mut all: Vec<Value> =
                    res.get("replies").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                match res.get("best_replies") {
                    Some(Value::Array(a)) => all.extend(a.iter().cloned()),
                    Some(v) if !v.is_null() => all.push(v.clone()),
                    _ => {}
                }
                if let Some(node) =
                    all.iter().find(|x| x.get("id").map(val_to_string).unwrap_or_default() == ids[0])
                {
                    found.insert(ids[0].clone(), node.clone());
                }
            }
        }
    }

    let out: Vec<ChainNode> = ids
        .iter()
        .filter_map(|id| {
            let n = found.get(id)?;
            let a = n.get("author").cloned().unwrap_or(Value::Null);
            let author_id = a.get("id").map(val_to_string).unwrap_or_default();
            Some(ChainNode {
                id: id.clone(),
                username: a.get("username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                text: n.get("content").map(doc_to_text).unwrap_or_default(),
                mine: !my_id.is_empty() && author_id == my_id,
            })
        })
        .collect();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub async fn fetch_question_brief(
    core: &Core,
    acc: &Account,
    topic_id: &str,
    stop: &Stop,
) -> Option<(String, String)> {
    let r = core
        .http
        .request(acc, &format!("/api/topic/question/{topic_id}"), ReqOpts::get(), stop)
        .await
        .ok()?;
    if !r.ok || r.blocked {
        return None;
    }
    let res = r.result()?;
    Some((
        res.get("title").and_then(|v| v.as_str()).unwrap_or("").trim().to_string(),
        res.get("content").map(doc_to_text).unwrap_or_default(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostRes {
    Ok(i64),
    /// Антибот mail.ru — аккаунт дальше не идёт.
    Blocked,
    /// Сайт не принял этот текст (4xx, но не антибот): реплики не создано.
    Rejected(String),
    Failed,
}

/// Ответ в ветку: POST /api/topic/answers + `reply_to`.
pub async fn post_reply(
    core: &Core,
    acc: &Account,
    topic_id: &str,
    reply_to: &str,
    text: &str,
    log: &Log,
    stop: &Stop,
) -> Result<PostRes, HttpError> {
    let body = json!({
        "version": 0,
        "visible_to": 0,
        "content": text_to_doc(text),
        "mentions": [],
        "topic_id": topic_id.parse::<i64>().unwrap_or(0),
        "reply_to": reply_to.parse::<i64>().unwrap_or(0),
    });
    let r = match core
        .http
        .request(
            acc,
            "/api/topic/answers",
            // Без ретрая: дубль реплики в ветке виден собеседнику сразу.
            ReqOpts::post(body).referer(format!("https://otvet.mail.ru/question/{topic_id}")).no_retry(),
            stop,
        )
        .await
    {
        Ok(r) => r,
        Err(HttpError::Aborted) => return Err(HttpError::Aborted),
        Err(e) => {
            log(&format!("   [!] Сеть/прокси при отправке ({e}) — не отправлено, продолжаю."));
            return Ok(PostRes::Failed);
        }
    };
    if r.blocked {
        return Ok(PostRes::Blocked);
    }
    if let Some(id) = r.result().and_then(|res| res.get("id")).and_then(|v| v.as_i64()) {
        return Ok(PostRes::Ok(id));
    }
    if (400..500).contains(&r.status) {
        return Ok(PostRes::Rejected(crate::answerer::refusal_reason(&r)));
    }
    // Сюда же попадает исчерпанный дневной лимит ответов mail.ru.
    log(&format!("   [!] Не прошло: HTTP {} {}", r.status, r.snippet(200)));
    Ok(PostRes::Failed)
}

/// Осталась ли наша реплика в ветке. `true` и при сетевом сбое: наказывать за
/// то, что не удалось проверить, нельзя — иначе живой ответ уедет в «не вышло».
pub async fn reply_is_there(core: &Core, acc: &Account, t: &Target, reply_id: i64, stop: &Stop) -> bool {
    let url = format!("/api/topic/answers/{}?reply_id={}", t.topic_id, t.entity_id);
    let Ok(r) = core.http.request(acc, &url, ReqOpts::get(), stop).await else { return true };
    if r.blocked || !r.ok {
        return true;
    }
    let Some(res) = r.result() else { return true };
    let Some(list) = res.get("replies").and_then(|v| v.as_array()) else { return true };
    // Пустая ветка сразу после отправки — это «снесли». А вот полная страница
    // может просто не вместить нашу реплику: считать её пропавшей и слать
    // вторую нельзя.
    if list.is_empty() {
        return false;
    }
    if list.len() >= 20 {
        return true;
    }
    list.iter().any(|x| x.get("id").and_then(|v| v.as_i64()) == Some(reply_id))
}

pub async fn mark_all_read(core: &Core, acc: &Account, stop: &Stop) -> bool {
    core.http
        .request(
            acc,
            "/api/notificator/notifications/read?id=0",
            ReqOpts::post(json!({})).referer("https://otvet.mail.ru/"),
            stop,
        )
        .await
        .map(|r| r.ok)
        .unwrap_or(false)
}

// ─── Промпт ─────────────────────────────────────────────────────────────────

/// Собираем диалог так, чтобы модель понимала: это ОТВЕТ СОБЕСЕДНИКУ в ветке,
/// а не новый ответ на вопрос.
pub fn build_messages(
    t: &Target,
    question: Option<&(String, String)>,
    siblings: &[Value],
    chain: Option<&Vec<ChainNode>>,
    system_prompt: &str,
    mention: &str,
    my_id: &str,
) -> Vec<Msg> {
    let mut sys = if system_prompt.trim().is_empty() {
        "Отвечай как обычный человек, коротко и по-простому.".to_string()
    } else {
        system_prompt.to_string()
    };
    sys.push_str(
        "\n\nСейчас ты не отвечаешь на вопрос, а ОБЩАЕШЬСЯ В КОММЕНТАРИЯХ: человек написал реплику под твоим ответом. \
Ответь именно ему, на его реплику — коротко (1–2 предложения), живо, как в переписке. \
Не здоровайся, не представляйся, не повторяй его слова и не пересказывай вопрос. \
Если он грубит или наезжает — отвечай спокойно и с юмором, без оскорблений.",
    );
    if !mention.trim().is_empty() {
        sys.push_str(&format!("\n\nКогда уместно — естественно упомяни «{}».", mention.trim()));
    }
    sys.push_str(NO_MARKDOWN);

    let who = t.who();
    let when = if t.created_at.is_empty() { String::new() } else { format!(" ({})", ago(&t.created_at)) };
    let mut parts: Vec<String> = Vec::new();

    if let Some((title, body)) = question {
        if !title.is_empty() {
            parts.push(format!("Вопрос на сайте: «{title}»"));
            if !body.is_empty() {
                parts.push(format!("Текст вопроса: {}", clip(body, 600)));
            }
        }
    }

    if t.kind == TYPE_TOPIC {
        parts.push(format!(
            "Это ТВОЙ вопрос{}.",
            if t.root_text.is_empty() { String::new() } else { format!(": «{}»", t.root_text) }
        ));
        parts.push(format!("{who} ответил на него{when}: «{}»", t.comment_text));
    } else if t.kind == TYPE_MENTION {
        parts.push(format!("{who} упомянул тебя{when}: «{}»", t.comment_text));
    } else {
        // Полная цепочка вытесняет обрывочный контекст: в ней уже есть и наш
        // исходный ответ, и все промежуточные реплики по порядку.
        let before: Vec<&ChainNode> = chain
            .map(|c| c.iter().filter(|n| n.id != t.entity_id && !n.text.is_empty()).collect())
            .unwrap_or_default();
        if !before.is_empty() {
            let lines = before
                .iter()
                .map(|n| {
                    format!(
                        "  {}: {}",
                        if n.mine {
                            "ты".to_string()
                        } else {
                            format!("@{}", if n.username.is_empty() { "кто-то" } else { &n.username })
                        },
                        clip(&n.text, 300)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            parts.push(format!("Как шёл разговор (сверху вниз):\n{lines}"));
        } else {
            if !t.root_text.is_empty() {
                parts.push(format!("Твой ответ был: «{}»", t.root_text));
            }
            let others: Vec<String> = siblings
                .iter()
                .filter(|s| {
                    s.get("id").map(val_to_string).unwrap_or_default() != t.entity_id
                        && s.get("author").is_some()
                })
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|s| {
                    let a = s.get("author").cloned().unwrap_or(Value::Null);
                    let uid = a.get("id").map(val_to_string).unwrap_or_default();
                    let name = a.get("username").and_then(|v| v.as_str()).unwrap_or("кто-то");
                    format!(
                        "  {}: {}",
                        if uid == my_id { "ты".to_string() } else { format!("@{name}") },
                        clip(&s.get("content").map(doc_to_text).unwrap_or_default(), 200)
                    )
                })
                .filter(|s| !s.trim().is_empty())
                .collect();
            if !others.is_empty() {
                parts.push(format!("Что уже написано в этой ветке:\n{}", others.join("\n")));
            }
        }
        parts.push(format!("{who} пишет тебе{when}: «{}»", t.comment_text));
    }
    parts.push("Напиши свой ответ ему. Только текст ответа, без кавычек и пояснений.".into());

    vec![Msg::system(sys), Msg::user(parts.join("\n\n"))]
}

// ─── Основной цикл ──────────────────────────────────────────────────────────

pub async fn run_replier(core: &Core, acc: &Account, p: &ReplyParams, log: &Log, stop: &Stop) -> RunOutcome {
    let mut out = RunOutcome::default();

    let wanted = p.types.wanted();
    if wanted.is_empty() {
        log("[-] Не выбрано ни одного вида уведомлений (под моим ответом / на мой вопрос / упоминания).");
        return out;
    }
    let d_min = p.delay_min.max(0.0);
    let d_max = p.delay_max.max(d_min);
    let per_thread = p.max_per_thread.max(1);
    let page_cnt = p.pages.clamp(1, 10);

    let styles = journals::Styles::load(&core.root);
    let mut ai = p.ai.clone();
    let custom = p.custom_prompt.trim();
    let system_prompt = if custom.is_empty() { styles.prompt(&p.style, "reply") } else { custom.to_string() };
    // Ноль — это ноль (см. комментарий в answerer.rs); «из стиля» = отрицательное.
    if ai.temperature < 0.0 {
        ai.temperature = styles.temperature(&p.style).unwrap_or(0.7);
    }

    if p.mode == ReplyMode::Ai {
        log("[>] Проверяю нейросеть...");
        match core.ai.check(&ai, stop).await {
            Ok(m) => log(&format!("[+] {m}")),
            Err(e) => {
                if !stop.is_stopped() {
                    log(&format!("[-] {e}"));
                }
                return out;
            }
        }
        log(&format!(
            "[>] Стиль: {} | {} | токенов: {} | таймаут: {}с | повторов при ошибке: {}",
            if custom.is_empty() { p.style.as_str() } else { "свой промпт" },
            ai.temperature,
            ai.max_tokens,
            ai.timeout_sec,
            ai.retries
        ));
    } else {
        log("[>] Режим без AI — короткие готовые реплики");
    }

    if p.check_auth {
        let v = api::validate_cached(core, acc, stop).await;
        api::persist_validation(core, &acc.name, &v);
        out.karma = v.karma.clone();
        if v.blocked {
            out.blocked = true;
            log("[x] Антибот (418/429) при проверке — статус не меняю.");
        } else if v.banned {
            log("[x] Аккаунт заблокирован сайтом — пропускаю. Сессия жива, но действия молча не проходят.");
            out.skipped = true;
            return out;
        } else if v.alive {
            log("[+] Авторизован");
        } else if v.auth_bad {
            log("[x] НЕ авторизован — пропускаю аккаунт.");
            out.skipped = true;
            return out;
        } else {
            log("[!] Не удалось проверить авторизацию (ошибка/сеть) — продолжаю, статус не трогаю.");
        }
    }

    // Свои аккаунты: их комменты не наши собеседники, а мы сами.
    let my_id = acc.user_id.map(|i| i.to_string()).unwrap_or_default();
    let mut own: HashSet<String> = HashSet::new();
    if p.skip_own {
        for a in core.accounts.all() {
            if let Some(id) = a.user_id {
                own.insert(id.to_string());
            }
        }
    }
    if !my_id.is_empty() {
        own.insert(my_id.clone());
    }

    // Журнал: на что уже отвечали, сколько раз в каждую ветку и какие реплики наши.
    let (seen0, per_root0, mine_replies) = load_replied_state(core, &acc.name);
    let mut seen = seen0;
    let mut per_root = per_root0;

    let limit_label = if p.limit > 0 { p.limit.to_string() } else { "∞".into() };
    log(&format!(
        "[>] Слушаю уведомления: {}",
        wanted.iter().map(|t| type_label(t)).collect::<Vec<_>>().join(", ")
    ));
    log(&format!(
        "[>] Лимит {limit_label} | пауза {d_min}–{d_max}с | не старше {} | не больше {per_thread} в ветку | страниц {page_cnt}{}",
        if p.max_age_hours > 0.0 { format!("{} ч", p.max_age_hours) } else { "∞".into() },
        if p.skip_own { " | свои аккаунты пропускаю" } else { "" }
    ));
    if !seen.is_empty() {
        log(&format!("[>] В журнале уже отвечено: {}", seen.len()));
    }

    // 1) собираем уведомления
    let mut targets: Vec<Target> = Vec::new();
    let mut before: Option<String> = None;
    for page in 0..page_cnt {
        if stop.is_stopped() {
            break;
        }
        let n = fetch_notifications(core, acc, before.as_deref(), stop).await;
        if n.blocked {
            out.blocked = true;
            log("[x] Антибот (418/429) при чтении уведомлений.");
            break;
        }
        if n.throttled {
            log("[!] mail.ru отдал пустой ответ на уведомления (троттлинг аккаунта/IP). Нужна пауза или живой прокси.");
            break;
        }
        if !n.ok {
            log(&format!("[!] Не прочитать уведомления ({}).", n.error.unwrap_or_else(|| "ошибка".into())));
            break;
        }
        if n.items.is_empty() {
            break;
        }
        for raw in &n.items {
            let kind = raw.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if !wanted.contains(&kind) {
                continue;
            }
            if let Some(t) = to_target(raw) {
                targets.push(t);
            }
        }
        before = n.items.last().map(|x| x.get("id").map(val_to_string).unwrap_or_default());
        if page + 1 < page_cnt && stop.sleep_ms(1200 + (rand_f64() * 800.0) as u64).await {
            break;
        }
    }

    if out.blocked {
        return out;
    }
    if targets.is_empty() {
        log("[>] Новых реплик нет.");
        return out;
    }
    log(&format!("[>] Найдено подходящих уведомлений: {}", targets.len()));

    // 2) отсев
    let (mut s_seen, mut s_own, mut s_old, mut s_thread) = (0, 0, 0, 0);
    let mut planned: HashMap<String, i64> = HashMap::new();
    let mut queue: std::collections::VecDeque<Target> = std::collections::VecDeque::new();
    // Одна реплика может прийти дважды: секции ответа (`unread` и `day`)
    // пересекаются, да и курсор `before` у mail.ru инклюзивный. Без этой
    // проверки при «не больше 2 в ветку» бот отвечал бы одному человеку два
    // раза подряд — в журнал entity_id попадает только после отправки.
    let mut queued: HashSet<String> = HashSet::new();
    for t in targets {
        if seen.contains(&t.entity_id) {
            s_seen += 1;
            continue;
        }
        if !queued.insert(t.entity_id.clone()) {
            continue;
        }
        if p.skip_own && !t.author_id.is_empty() && own.contains(&t.author_id) {
            s_own += 1;
            continue;
        }
        if p.max_age_hours > 0.0 && age_hours(&t.created_at) > p.max_age_hours {
            s_old += 1;
            continue;
        }
        let root_key = if t.root_id.is_empty() { t.topic_id.clone() } else { t.root_id.clone() };
        let already =
            per_root.get(&root_key).copied().unwrap_or(0) + planned.get(&root_key).copied().unwrap_or(0);
        if already >= per_thread {
            s_thread += 1;
            continue;
        }
        *planned.entry(root_key).or_insert(0) += 1;
        queue.push_back(t);
    }
    let skips: Vec<String> =
        [(s_seen, "уже отвечено"), (s_own, "свои"), (s_old, "старые"), (s_thread, "лимит ветки")]
            .iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, label)| format!("{label} {n}"))
            .collect();
    if !skips.is_empty() {
        log(&format!("[!] Пропущено: {}", skips.join(", ")));
    }
    if queue.is_empty() {
        log("[>] Отвечать не на что.");
        return out;
    }
    log(&format!("[=] К ответу: {}", queue.len()));

    // 3) отвечаем
    let noai: Vec<String> = if p.noai_replies.is_empty() {
        NOAI_REPLIES.iter().map(|s| s.to_string()).collect()
    } else {
        p.noai_replies.clone()
    };

    // Отказы подряд: сайт перестал принимать ответы (дневной лимит) или умер
    // ключ нейросети. Дальше по очереди идти бессмысленно и дорого.
    let mut fails = 0i64;

    // Сколько раз цель уже возвращали в очередь после отказа по тексту.
    let mut retried: HashSet<String> = HashSet::new();
    while let Some(mut t) = queue.pop_front() {
        if stop.is_stopped() {
            log("\n[x] Остановлено пользователем");
            break;
        }
        if out.blocked {
            log("\n[x] Блокировка mail.ru (418/429) — останавливаю аккаунт.");
            break;
        }
        if p.limit > 0 && out.done >= p.limit {
            log(&format!("\n[=] Достигнут лимит {limit_label}."));
            break;
        }
        if fails >= MAX_FAILS {
            log(&format!(
                "\n[x] Подряд {MAX_FAILS} отказов — останавливаю аккаунт (обычно это дневной лимит ответов или мёртвый ключ нейросети)."
            ));
            break;
        }

        let who = t.who();
        log(&format!(
            "\n[>] {} от {who}{}",
            type_label(&t.kind),
            if t.created_at.is_empty() { String::new() } else { format!(" ({})", ago(&t.created_at)) }
        ));
        log(&format!("   {}", t.url));
        log(&format!("   Он: «{}»", clip(&t.comment_text, 160)));

        let loc = locate_entity(core, acc, &t, stop).await;
        if loc.blocked {
            out.blocked = true;
            log("   [x] Антибот при чтении ветки.");
            break;
        }
        if loc.throttled {
            log("   [!] Пустой ответ от mail.ru (троттлинг) — пропускаю.");
            continue;
        }
        if let Some(e) = loc.error {
            log(&format!("   [!] Не прочитать ветку ({e}) — пропускаю."));
            continue;
        }
        if !loc.found {
            log("   [!] Реплика удалена/недоступна — отмечаю обработанной, пропускаю.");
            journals::append_replied(
                &core.root,
                &acc.name,
                &RepliedEntry {
                    entity: t.entity_id.clone(),
                    root: t.root_id.clone(),
                    topic: t.topic_id.clone(),
                    reply: Value::Null,
                    to: t.author_name.clone(),
                    ts: journals::now_ms(),
                },
            );
            seen.insert(t.entity_id.clone());
            continue;
        }
        // Живой текст точнее, чем поле уведомления (реплику могли отредактировать).
        if let Some(live) = loc.entity.as_ref().and_then(|e| e.get("content")).map(doc_to_text) {
            if !live.is_empty() {
                t.comment_text = live;
            }
        }

        let mut text = if p.mode == ReplyMode::Ai {
            let chain = if p.use_chain && t.kind == TYPE_REPLY && mine_replies.contains(&t.root_id) {
                let c = fetch_chain(core, acc, &t, &my_id, p.max_chain, loc.siblings.clone(), stop).await;
                if let Some(c) = &c {
                    log(&format!("   [>] Цепочка разговора: {} реплик", c.len()));
                }
                c
            } else {
                None
            };
            if stop.is_stopped() {
                break;
            }
            let question =
                if p.use_question { fetch_question_brief(core, acc, &t.topic_id, stop).await } else { None };
            if stop.is_stopped() {
                break;
            }
            let msgs = build_messages(
                &t,
                question.as_ref(),
                &loc.siblings,
                chain.as_ref(),
                &system_prompt,
                &p.mention,
                &my_id,
            );
            match core.ai.generate(&ai, &msgs, log, stop).await {
                Ok(a) if !a.trim().is_empty() => a.trim().to_string(),
                Ok(_) => {
                    fails += 1;
                    log("   [!] Пустой ответ ИИ — пропускаю.");
                    continue;
                }
                Err(AiError::Aborted) => break,
                Err(e) => {
                    fails += 1;
                    log(&format!("   [-] ИИ не ответил ({e}) — пропускаю."));
                    continue;
                }
            }
        } else {
            pick_one(&noai).cloned().unwrap_or_default()
        };

        if p.uniq != UniqMode::Off {
            text = uniquify(&text, p.uniq, p.uniq_latin);
        }
        text = with_signature(&text, &p.signature);
        log(&format!("   Я: «{}»", clip(&text, 160)));

        match post_reply(core, acc, &t.topic_id, &t.entity_id, &text, log, stop).await {
            Err(_) => break,
            Ok(PostRes::Blocked) => {
                out.blocked = true;
                log("   [x] Антибот при отправке — стоп аккаунта.");
                break;
            }
            Ok(PostRes::Rejected(why)) => {
                fails += 1;
                // Отказ по тексту: реплики не появилось, значит можно сочинить
                // другую. Одна попытка на цель — если сайт отказывает и ей, дело
                // не в тексте.
                if retried.insert(t.entity_id.clone()) {
                    log(&format!("   [!] Сайт не принял реплику ({why}) — напишу другую."));
                    queue.push_front(t);
                } else {
                    log(&format!("   [!] Сайт не принял и вторую реплику ({why}) — пропускаю."));
                }
                continue;
            }
            Ok(PostRes::Failed) => {
                fails += 1;
                continue;
            }
            Ok(PostRes::Ok(id)) => {
                // Реплику могли снести за спам сразу после отправки: сайт при
                // этом отвечает «принято» и отдаёт id. Проверяем, что она
                // действительно осталась в ветке.
                if p.verify_posted {
                    let wait = p.verify_delay_sec.clamp(0.0, 120.0);
                    if wait > 0.0 && stop.sleep_ms((wait * 1000.0) as u64).await {
                        break;
                    }
                    if !reply_is_there(core, acc, &t, id, stop).await {
                        fails += 1;
                        log("   [!] Реплики в ветке нет — снесла автомодерация. Не засчитываю.");
                        continue;
                    }
                }
                fails = 0;
                out.done += 1;
                p.progress.inc();
                seen.insert(t.entity_id.clone());
                let root_key = if t.root_id.is_empty() { t.topic_id.clone() } else { t.root_id.clone() };
                *per_root.entry(root_key).or_insert(0) += 1;
                journals::append_replied(
                    &core.root,
                    &acc.name,
                    &RepliedEntry {
                        entity: t.entity_id.clone(),
                        root: t.root_id.clone(),
                        topic: t.topic_id.clone(),
                        reply: json!(id),
                        to: t.author_name.clone(),
                        ts: journals::now_ms(),
                    },
                );
                log(&format!("   [+] Ответ отправлен (reply #{id}) → {who}"));
            }
        }

        if (p.limit <= 0 || out.done < p.limit) && d_max > 0.0 {
            let pause = (d_min + rand_f64() * (d_max - d_min)).round();
            log(&format!("   [>] Пауза {pause:.0}с"));
            if stop.sleep_ms((pause * 1000.0) as u64 + crate::util::rand_range(80, 300) as u64).await {
                break;
            }
        }
    }

    if p.mark_read && out.done > 0 && !stop.is_stopped() {
        let ok = mark_all_read(core, acc, stop).await;
        log(if ok {
            "[+] Уведомления помечены прочитанными (mail.ru помечает все разом)."
        } else {
            "[!] Не удалось пометить уведомления прочитанными."
        });
    }

    log(&format!("\n[=] Ответов в комментариях: {}", out.done));
    out
}

/// Журнал ответов: что видели, сколько в каждой ветке, какие реплики наши.
fn load_replied_state(
    core: &Core,
    account: &str,
) -> (HashSet<String>, HashMap<String, i64>, HashSet<String>) {
    let path = core.root.join(format!("replied_{}.ndjson", crate::util::safe_name(account)));
    let mut seen = HashSet::new();
    let mut per_root: HashMap<String, i64> = HashMap::new();
    let mut mine = HashSet::new();
    if let Ok(txt) = std::fs::read_to_string(&path) {
        for line in txt.lines() {
            let Ok(e) = serde_json::from_str::<RepliedEntry>(line.trim()) else { continue };
            if e.entity.is_empty() {
                continue;
            }
            seen.insert(e.entity.clone());
            if !e.root.is_empty() {
                *per_root.entry(e.root.clone()).or_insert(0) += 1;
            }
            let r = val_to_string(&e.reply);
            if !r.is_empty() && r != "null" {
                mine.insert(r);
            }
        }
    }
    (seen, per_root, mine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_becomes_target() {
        let n = json!({
            "id": 42,
            "type": TYPE_REPLY,
            "entity_type": "reply",
            "entity_id": 777,
            "root_id": 555,
            "root_type": "reply",
            "page_uri": "/question/270332017",
            "title": "мой ответ",
            "body": "его реплика",
            "created_at": "2026-08-01T10:00:00Z",
            "authors": [{ "id": 1, "username": "vasya", "nick": "Вася" }],
        });
        let t = to_target(&n).unwrap();
        assert_eq!(t.topic_id, "270332017");
        assert_eq!(t.entity_id, "777");
        assert_eq!(t.root_id, "555");
        assert_eq!(t.author_name, "vasya");
        assert_eq!(t.root_text, "мой ответ");
        assert_eq!(t.comment_text, "его реплика");
        assert_eq!(t.who(), "@vasya");

        // не «reply» — не наша цель
        let other = json!({ "entity_type": "topic", "entity_id": 1, "page_uri": "/question/1" });
        assert!(to_target(&other).is_none());
    }

    #[test]
    fn moscow_time_is_shifted_back() {
        // mail.ru помечает московское время как UTC — исправляем на −3 часа.
        let fixed = norm_notif_time("2026-08-01T12:00:00Z");
        let t = chrono::DateTime::parse_from_rfc3339(&fixed).unwrap();
        assert_eq!(t.timestamp(), 1785585600 - 3 * 3600);
    }
}
