//! answerer.rs — режим «Ответы»: лента вопросов → текст → постинг. Порт answerer.js.
//!
//! Три способа получить текст: нейросеть, готовые короткие реплики и «коверканье»
//! (перемешанные слова вопроса). Цели тоже две: лента свежих вопросов или список
//! прямых ссылок.
//!
//! Журнал `answered_<акк>.ndjson` не даёт отвечать дважды на один вопрос — это
//! важнее, чем кажется: повторный ответ от того же аккаунта модерация видит сразу.

use crate::accounts::Account;
use crate::ai::{AiCfg, Msg, NO_MARKDOWN};
use crate::api;
use crate::content::{doc_to_text, doc_with_image, gallery_from_pool, image_gallery_node};
use crate::http::{HttpError, ReqOpts};
use crate::journals::{self, PoolImage};
use crate::util::{clip, pick_one, rand_range, shuffle, Log, Stop};
use crate::{Core, RunOutcome};
use futures::stream::{FuturesUnordered, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// Короткие «живые» реплики для режима без нейросети.
pub const NOAI_ANSWERS: &[&str] = &[
    "лол",
    "бывает",
    "согласен",
    "ну такое",
    "это нормально",
    "жиза",
    "плюсую",
    "не знаю даже",
    "хз честно",
    "а смысл",
    "ну ты дал",
    "класс",
    "красава",
    "держись",
    "всё будет норм",
    "забей",
    "не парься",
    "мда",
    "ну и ну",
    "интересно",
    "поддерживаю",
    "верно говоришь",
    "логично",
    "факт",
    "согласен на все 100",
    "так и есть",
    "ну да",
    "походу так",
    "кек",
    "жесть конечно",
    "бывает и хуже",
    "не переживай",
    "всё пройдёт",
    "мудро",
    "респект",
    "топ",
    "нормас",
    "ну норм",
    "окай",
    "понял принял",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnswerMode {
    Ai,
    NoAi,
    Mangle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetMode {
    /// Свежие вопросы из ленты.
    Feed,
    /// Конкретные ссылки.
    Links,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ImageMode {
    #[default]
    Off,
    /// Готовые картинки из пула (перезаливать не нужно — хэш уже на CDN).
    Gif { selected: Vec<String> },
    /// Файлы из папки: заливаются на каждый пост.
    Upload { dir: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerParams {
    pub mode: AnswerMode,
    pub target: TargetMode,
    pub links: Vec<String>,
    /// Лимит ответов на аккаунт за проход (0 = без лимита).
    pub limit: i64,
    pub delay_min: f64,
    pub delay_max: f64,
    /// Пауза между обновлениями ленты, если отвечать не на что.
    pub feed_min: f64,
    pub feed_max: f64,
    /// Сколько самых свежих вопросов смотреть.
    pub recent_scan: i64,
    /// Сколько ответов на один вопрос.
    pub repeat_per_question: i64,
    /// Сколько вопросов брать за один проход ленты.
    pub batch_size: i64,
    /// Отвечать на пачку разом (сильный сигнал антиботу — по умолчанию выкл.).
    pub parallel: bool,
    /// Непрерывно тянуть ленту, не дожидаясь конца пачки.
    pub continuous_feed: bool,
    /// Единый чат с памятью (только для режима с нейросетью).
    pub conversational: bool,
    /// Порог сжатия истории, тыс. символов (0 = не сжимать).
    pub convo_budget_k: f64,
    /// Не отвечать на то, что уже отвечали ДРУГИЕ аккаунты.
    pub skip_others: bool,
    /// Дописывать «#123456» для уникальности.
    pub random_tag: bool,
    pub signature: String,
    pub noai_answers: Vec<String>,
    pub image: ImageMode,
    pub image_count: i64,
    pub ai: AiCfg,
    pub style: String,
    pub custom_prompt: String,
    pub mention: String,
    pub check_auth: bool,
}

impl Default for AnswerParams {
    fn default() -> Self {
        Self {
            mode: AnswerMode::Ai,
            target: TargetMode::Feed,
            links: vec![],
            limit: 5,
            delay_min: 20.0,
            delay_max: 45.0,
            feed_min: 10.0,
            feed_max: 20.0,
            recent_scan: 10,
            repeat_per_question: 1,
            batch_size: 1,
            parallel: false,
            continuous_feed: false,
            conversational: false,
            convo_budget_k: 60.0,
            skip_others: false,
            random_tag: false,
            signature: String::new(),
            noai_answers: vec![],
            image: ImageMode::Off,
            image_count: 1,
            ai: AiCfg::preset(),
            style: "Обычный чел".into(),
            custom_prompt: String::new(),
            mention: String::new(),
            check_auth: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Question {
    pub id: String,
    /// Канонический URL — ключ журнала.
    pub norm: String,
    pub title: String,
    pub body: String,
    pub author: String,
    pub author_user: String,
    pub date: String,
}

// ─── Чтение ленты и вопросов ────────────────────────────────────────────────

/// Свежие вопросы: `sort=id&dir=0` — самые новые сверху.
///
/// Смотрим РОВНО `recent` последних и среди них ищем отвечаемые. Раньше грузилось
/// 50 и сканировалось до набора нужного числа — бот залезал в старые вопросы, а
/// поле «сканировать последних» почти ни на что не влияло.
pub async fn collect_questions(
    core: &Core,
    acc: &Account,
    exclude: &HashSet<String>,
    tried: &HashSet<String>,
    recent: i64,
    stop: &Stop,
) -> Result<Vec<Question>, HttpError> {
    let r = core
        .http
        .request(acc, &format!("/api/topic/feed?limit={recent}&pos=0&dir=0&sort=id"), ReqOpts::get(), stop)
        .await?;
    let feed =
        r.result().and_then(|res| res.get("feed")).and_then(|f| f.as_array()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for it in feed.iter().take(recent.max(1) as usize) {
        let Some(id) = it.get("id").and_then(|v| v.as_i64()) else { continue };
        let norm = format!("https://otvet.mail.ru/question/{id}");
        if exclude.contains(&norm) || tried.contains(&norm) {
            continue;
        }
        let title = it.get("title").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if title.chars().count() < 5 {
            continue;
        }
        let a = it.get("author").cloned().unwrap_or(Value::Null);
        out.push(Question {
            id: id.to_string(),
            norm,
            title,
            body: it.get("content").map(doc_to_text).unwrap_or_default(),
            author: a
                .get("nick")
                .and_then(|v| v.as_str())
                .or_else(|| a.get("username").and_then(|v| v.as_str()))
                .unwrap_or("")
                .trim()
                .to_string(),
            author_user: a.get("username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            date: it.get("created_at").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        });
    }
    Ok(out)
}

/// Один вопрос по id — нужен, когда отвечаем по прямой ссылке.
pub async fn fetch_question(core: &Core, acc: &Account, topic_id: &str, stop: &Stop) -> Option<Question> {
    let r = core
        .http
        .request(acc, &format!("/api/topic/question/{topic_id}"), ReqOpts::get(), stop)
        .await
        .ok()?;
    if !r.ok || r.blocked {
        return None;
    }
    let j = r.json.as_ref()?;
    let obj = j.get("result").unwrap_or(j);
    let a = obj.get("author").cloned().unwrap_or(Value::Null);
    Some(Question {
        id: topic_id.to_string(),
        norm: format!("https://otvet.mail.ru/question/{topic_id}"),
        title: obj.get("title").and_then(|v| v.as_str()).unwrap_or("").trim().to_string(),
        body: obj.get("content").map(doc_to_text).unwrap_or_default(),
        author: a
            .get("nick")
            .and_then(|v| v.as_str())
            .or_else(|| a.get("username").and_then(|v| v.as_str()))
            .unwrap_or("")
            .trim()
            .to_string(),
        author_user: a.get("username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        date: obj.get("created_at").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    })
}

/// Ссылка вопроса/ответа → (topic_id, reply_id). `/question/<id>` есть всегда,
/// в том числе в ссылке на конкретный ответ.
pub fn parse_answer_target(raw: &str) -> Option<(String, Option<String>)> {
    let s = raw.trim();
    let q = crate::votes::parse_target_id(s)?;
    let topic = regex_capture(s, r"/question/(\d+)")?;
    let reply = match q.kind {
        crate::votes::Kind::Reply => Some(q.id),
        crate::votes::Kind::Topic => None,
    };
    Some((topic, reply))
}

fn regex_capture(s: &str, pat: &str) -> Option<String> {
    regex::Regex::new(pat).ok()?.captures(s)?.get(1).map(|m| m.as_str().to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostRes {
    Ok(i64),
    /// Антибот mail.ru: дальше по этому аккаунту идти нельзя.
    Blocked,
    Failed,
}

/// Отправка ответа: POST /api/topic/answers.
pub async fn post_answer(
    core: &Core,
    acc: &Account,
    topic_id: &str,
    text: &str,
    image: Option<&Value>,
    log: &Log,
    stop: &Stop,
) -> Result<PostRes, HttpError> {
    let body = json!({
        "version": 0,
        "visible_to": 0,
        "content": doc_with_image(text, image),
        "mentions": [],
        "topic_id": topic_id,
    });
    let r = match core
        .http
        .request(
            acc,
            "/api/topic/answers",
            // Без ретрая: если ответ дошёл, а подтверждение потерялось,
            // повтор оставит под вопросом ДВА одинаковых ответа от одного
            // аккаунта. Потерять ответ дешевле — просто возьмём следующий вопрос.
            ReqOpts::post(body).referer(format!("https://otvet.mail.ru/question/{topic_id}")).no_retry(),
            stop,
        )
        .await
    {
        Ok(r) => r,
        Err(HttpError::Aborted) => return Err(HttpError::Aborted),
        Err(e) => {
            // Транзитная сеть/прокси не должна валить весь аккаунт: считаем
            // ответ неотправленным и идём дальше.
            log(&format!("   ⚠️  Сеть/прокси при отправке ({e}) — не отправлено, продолжаю."));
            return Ok(PostRes::Failed);
        }
    };
    if r.blocked {
        return Ok(PostRes::Blocked);
    }
    if let Some(id) = r.result().and_then(|res| res.get("id")).and_then(|v| v.as_i64()) {
        return Ok(PostRes::Ok(id));
    }
    if !r.ok {
        log(&format!("   ⚠️  Ответ не прошёл: HTTP {} {}", r.status, r.snippet(120)));
    }
    Ok(PostRes::Failed)
}

// ─── Текст ответа ───────────────────────────────────────────────────────────

pub fn with_random_tag(text: &str) -> String {
    format!("{text} #{}", rand_range(100_000, 999_999))
}

/// Подпись отделяем пустой строкой, тройные переводы схлопываем.
pub fn with_signature(text: &str, signature: &str) -> String {
    let sig = signature.trim();
    if sig.is_empty() {
        return text.to_string();
    }
    let mut cleaned = sig.to_string();
    while cleaned.contains("\n\n\n") {
        cleaned = cleaned.replace("\n\n\n", "\n\n");
    }
    format!("{text}\n\n{cleaned}")
}

/// «Коверканье»: перемешиваем слова вопроса, хвостовую пунктуацию клеим к
/// случайному слову.
pub fn scramble_question(text: &str) -> String {
    let src = text.trim();
    if src.is_empty() {
        return String::new();
    }
    let tail: String = src
        .chars()
        .rev()
        .take_while(|c| matches!(c, '?' | '!' | '.' | '…'))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let core_part = src[..src.len() - tail.len()].trim();
    let words: Vec<&str> = core_part.split_whitespace().collect();
    if words.len() <= 1 {
        return src.to_string();
    }
    let orig = words.join(" ");
    let mut out: Vec<String> = words.iter().map(|s| s.to_string()).collect();
    for _ in 0..8 {
        shuffle(&mut out);
        if out.join(" ") != orig {
            break;
        }
    }
    if !tail.is_empty() {
        let k = rand_range(0, out.len() as i64 - 1) as usize;
        out[k] = format!("{}{}", out[k], tail);
    }
    out.join(" ")
}

/// «45 мин назад» / «2 ч 5 мин назад» / «3 дн 2 ч назад».
pub fn exact_time(iso: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(iso) else { return String::new() };
    let diff = (chrono::Utc::now().timestamp() - t.timestamp()).max(0);
    let m = diff / 60;
    if m < 1 {
        return "только что".into();
    }
    if m < 60 {
        return format!("{m} мин назад");
    }
    let (h, mm) = (m / 60, m % 60);
    if h < 24 {
        return format!("{h} ч {mm} мин назад");
    }
    let (d, hh) = (h / 24, h % 24);
    format!("{d} дн {hh} ч назад")
}

/// Реплика для единого чата: кто, когда и о чём спрашивает. Так модель видит,
/// что каждое сообщение — от РАЗНОГО человека.
pub fn build_ask_msg(q: &Question) -> String {
    let who = if !q.author_user.is_empty() {
        format!("@{}", q.author_user)
    } else if !q.author.is_empty() {
        format!("«{}»", q.author)
    } else {
        "аноним".to_string()
    };
    let when = exact_time(&q.date);
    let head = if when.is_empty() {
        format!("[{who}] спрашивает:")
    } else {
        format!("[{who}, {when}] спрашивает:")
    };
    let mut s = format!("{head}\n{}", q.title);
    if !q.body.is_empty() {
        s.push_str(&format!("\n\n{}", q.body));
    }
    s.trim().to_string()
}

// ─── Состояние прогона ──────────────────────────────────────────────────────

/// Сколько отказов подряд считать «сайт больше не принимает ответы».
/// Обычная причина — исчерпан дневной лимит; крутиться дальше бессмысленно,
/// а каждый круг стоит запроса к нейросети (то есть денег).
const MAX_FAILS: i64 = 5;

struct Shared {
    count: AtomicI64,
    blocked: AtomicBool,
    /// Отказов подряд при отправке.
    fails: AtomicI64,
    /// Подряд неудачных обращений к нейросети. Ключ может умереть посреди
    /// прогона (кончился баланс), и без счётчика бот крутил бы ленту вечно,
    /// каждый раз получая отказ.
    ai_fails: AtomicI64,
    /// Что уже отвечено (журнал аккаунта + чужие, если включено).
    exclude: Mutex<HashSet<String>>,
    /// Пробовали в этой сессии, но не вышло — второй раз не берём.
    tried: Mutex<HashSet<String>>,
}

impl Shared {
    fn count(&self) -> i64 {
        self.count.load(Ordering::SeqCst)
    }
    fn blocked(&self) -> bool {
        self.blocked.load(Ordering::SeqCst)
    }
    fn limit_hit(&self, limit: i64) -> bool {
        limit > 0 && self.count() >= limit
    }
    fn too_many_fails(&self) -> bool {
        self.fails.load(Ordering::SeqCst) >= MAX_FAILS || self.ai_fails.load(Ordering::SeqCst) >= MAX_FAILS
    }
}

// ─── Точка входа ────────────────────────────────────────────────────────────

pub async fn run_answerer(
    core: &Core,
    acc: &Account,
    p: &AnswerParams,
    log: &Log,
    stop: &Stop,
) -> RunOutcome {
    let mut out = RunOutcome::default();

    let repeat_per = p.repeat_per_question.clamp(1, 20);
    let batch_per = p.batch_size.clamp(1, 50);
    let recent = p.recent_scan.clamp(1, 50);
    let img_count = p.image_count.clamp(1, crate::content::MAX_GALLERY as i64);
    let d_min = p.delay_min.max(0.0);
    let d_max = p.delay_max.max(d_min);
    let f_min = p.feed_min.max(0.0);
    let f_max = p.feed_max.max(f_min);
    let convo_mode = p.conversational && p.mode == AnswerMode::Ai;
    // Единый чат отвечает строго по очереди: параллельный постинг ломает историю.
    let go_parallel = p.parallel && batch_per > 1 && !convo_mode;

    let styles = journals::Styles::load(&core.root);
    let mut ai = p.ai.clone();
    let mut system_prompt = String::new();
    let mut convo_system = String::new();

    if p.mode == AnswerMode::Ai {
        let custom = p.custom_prompt.trim();
        system_prompt =
            if custom.is_empty() { styles.prompt(&p.style, "answer") } else { custom.to_string() };
        if ai.temperature <= 0.0 {
            ai.temperature = styles.temperature(&p.style).unwrap_or(0.7);
        }
        log("🔌 Проверяю API...");
        match core.ai.check(&ai, stop).await {
            Ok(msg) => log(&format!("✅ {msg}")),
            Err(e) => {
                if stop.is_stopped() {
                    return out;
                }
                log(&format!("❌ {e}"));
                return out;
            }
        }
        log(&format!(
            "🎨 Стиль: {} | 🌡 {} | 🎟 токенов: {} | ⏱ таймаут: {}с | 🔁 повторов при ошибке: {}",
            if custom.is_empty() { p.style.as_str() } else { "свой промпт" },
            ai.temperature,
            ai.max_tokens,
            ai.timeout_sec,
            ai.retries
        ));
        if convo_mode {
            convo_system = build_convo_system(&system_prompt, &p.mention);
            log(&format!(
                "🧠 Диалоговый режим: единый чат с памятью. {}. Ответы — последовательно.",
                if p.convo_budget_k > 0.0 {
                    format!("Сжатие контекста после ~{}k символов", p.convo_budget_k)
                } else {
                    "Без сжатия — держу весь чат".into()
                }
            ));
        }
    } else if p.mode == AnswerMode::Mangle {
        log("🤪 Режим коверканья — перемешиваю слова вопроса");
    } else {
        log("💬 Режим без AI — короткие готовые реплики");
    }

    // Диалоговый режим = ОБЩАЯ история на все аккаунты → работает один за раз.
    if convo_mode {
        if let Err(owner) = core.convo.try_acquire(&acc.name) {
            log(&format!("⏭️  Диалоговый режим занят аккаунтом «{owner}» — пропускаю"));
            out.skipped = true;
            return out;
        }
    }

    match &p.image {
        ImageMode::Gif { .. } => {
            let n = journals::load_gif_pool(&core.root).len();
            log(&format!(
                "🎞  Картинка из пула в каждый ответ (в пуле: {n}){}",
                if n == 0 { " — ⚠️ пул пуст" } else { "" }
            ));
        }
        ImageMode::Upload { dir } => {
            let n = list_images(dir).len();
            log(&format!(
                "🖼  Картинка из {dir} в каждый ответ (файлов: {n}){}",
                if n == 0 { " — ⚠️ папка пуста" } else { "" }
            ));
        }
        ImageMode::Off => {}
    }

    if p.check_auth {
        let v = api::validate_account(core, acc, stop).await;
        api::persist_validation(core, &acc.name, &v);
        out.karma = v.karma.clone();
        if v.blocked {
            log("🛑 Антибот (418/429) при проверке — статус не меняю.");
            out.blocked = true;
        } else if v.alive {
            log("✅ Авторизован");
        } else if v.auth_bad {
            log("🔒 НЕ авторизован");
            log("Пропускаю аккаунт — не залогинен.");
            if convo_mode {
                core.convo.release(&acc.name);
            }
            out.skipped = true;
            return out;
        } else {
            log("⚠️  Не удалось проверить авторизацию (ошибка/сеть) — продолжаю, статус не трогаю.");
        }
    }

    let answered = journals::load_answered(&core.root, &acc.name);
    let exclude: HashSet<String> = if p.skip_others {
        let all = journals::load_all_answered(&core.root);
        log(&format!("🚫 Пропускаю вопросы, отвеченные другими аккаунтами (в базе: {})", all.len()));
        all
    } else {
        answered
    };

    let sh = Shared {
        count: AtomicI64::new(0),
        blocked: AtomicBool::new(out.blocked),
        fails: AtomicI64::new(0),
        ai_fails: AtomicI64::new(0),
        exclude: Mutex::new(exclude),
        tried: Mutex::new(HashSet::new()),
    };

    let ctx = RunCtx {
        core,
        acc,
        p,
        ai: &ai,
        system_prompt: &system_prompt,
        convo_system: &convo_system,
        convo_mode,
        repeat_per,
        img_count,
        d_min,
        d_max,
        log,
        stop,
        sh: &sh,
    };

    match p.target {
        TargetMode::Links => answer_links(&ctx).await,
        TargetMode::Feed => {
            if p.continuous_feed {
                answer_feed_continuous(&ctx, recent, batch_per, f_min, f_max).await;
            } else {
                answer_feed(&ctx, recent, batch_per, go_parallel, f_min, f_max).await;
            }
        }
    }

    if convo_mode {
        core.convo.release(&acc.name);
    }

    out.done = sh.count();
    out.blocked = sh.blocked();
    let line = "=".repeat(50);
    if stop.is_stopped() {
        log(&format!("\n{line}\n⛔ Остановлено пользователем. Ответов отправлено: {}\n{line}", out.done));
    } else {
        log(&format!("\n{line}\n✅ Готово!  Ответов отправлено: {}\n{line}", out.done));
    }
    out
}

fn build_convo_system(style_prompt: &str, mention: &str) -> String {
    let mut s = String::from(
        "Ты — постоянный житель сайта «Ответы Mail.ru»: целый день переписываешься с разными людьми и отвечаешь на их вопросы. \
Это ОДИН непрерывный разговор — ты помнишь предыдущие вопросы и свои ответы, держишь единый характер, настроение и манеру. \
Каждое новое сообщение в формате «[ник, когда] спрашивает: …» — это НОВЫЙ вопрос от ДРУГОГО человека. \
Отвечай ТОЛЬКО на последний вопрос, живо и по-простому, в духе Ответов Mail.ru, без приветствий-шаблонов и без повторения вопроса.",
    );
    if !style_prompt.trim().is_empty() {
        s.push_str(&format!("\n\nТвоя роль/стиль: {style_prompt}"));
    }
    if !mention.trim().is_empty() {
        s.push_str(&format!("\n\nКогда уместно — естественно упоминай «{}».", mention.trim()));
    }
    s.push_str(NO_MARKDOWN);
    s
}

/// Всё, что нужно обработчику одного вопроса. Ссылками, чтобы не копировать
/// параметры на каждый ответ.
struct RunCtx<'a> {
    core: &'a Core,
    acc: &'a Account,
    p: &'a AnswerParams,
    ai: &'a AiCfg,
    system_prompt: &'a str,
    convo_system: &'a str,
    convo_mode: bool,
    repeat_per: i64,
    img_count: i64,
    d_min: f64,
    d_max: f64,
    log: &'a Log,
    stop: &'a Stop,
    sh: &'a Shared,
}

impl RunCtx<'_> {
    fn done(&self) -> bool {
        self.stop.is_stopped()
            || self.sh.blocked()
            || self.sh.limit_hit(self.p.limit)
            || self.sh.too_many_fails()
    }

    fn limit_label(&self) -> String {
        if self.p.limit > 0 {
            self.p.limit.to_string()
        } else {
            "∞".into()
        }
    }

    async fn pause_between(&self) -> bool {
        let d = rand_f(self.d_min, self.d_max);
        if d > 0.0 {
            (self.log)(&format!("   ⏳ Пауза {d:.1} сек..."));
        }
        self.stop.sleep_ms((d * 1000.0) as u64).await
    }

    /// Записать неудачу нейросети и, если их подряд слишком много, сказать об
    /// этом прямо: дальше крутиться бессмысленно и дорого.
    fn note_ai_fail(&self, msg: &str) {
        (self.log)(msg);
        let n = self.sh.ai_fails.fetch_add(1, Ordering::SeqCst) + 1;
        if n >= MAX_FAILS {
            (self.log)(&format!(
                "🛑 Нейросеть не отвечает {MAX_FAILS} раз подряд — останавливаю аккаунт (проверь ключ и баланс)."
            ));
        }
    }

    /// Обработка одного вопроса: текст → картинка → постинг (repeat_per раз).
    async fn handle_one(&self, q: &Question) -> bool {
        let mut posted_any = false;
        for rep in 0..self.repeat_per {
            if self.done() {
                break;
            }

            // 1) текст
            let mut ask_msg = None;
            let mut ai_raw = String::new();
            let mut answer = match self.p.mode {
                AnswerMode::Ai => {
                    (self.log)(&if self.repeat_per > 1 {
                        format!("   🤖 Генерирую ответ {}/{}...", rep + 1, self.repeat_per)
                    } else {
                        "   🤖 Генерирую ответ...".to_string()
                    });
                    let msgs = if self.convo_mode {
                        let m = build_ask_msg(q);
                        let convo = self.core.convo.snapshot();
                        let mut msgs = vec![Msg::system(self.convo_system)];
                        if !convo.summary.is_empty() {
                            msgs.push(Msg::system(format!(
                                "Что было раньше в этом разговоре (сжато): {}",
                                convo.summary
                            )));
                        }
                        msgs.extend(convo.turns.clone());
                        msgs.push(Msg::user(m.clone()));
                        ask_msg = Some(m);
                        msgs
                    } else {
                        let mut prompt = q.title.clone();
                        if !q.body.is_empty() {
                            prompt.push_str(&format!("\n\nДополнение: {}", q.body));
                        }
                        let mut sys = if self.system_prompt.trim().is_empty() {
                            "Отвечай как обычный человек, коротко и по-простому.".to_string()
                        } else {
                            self.system_prompt.to_string()
                        };
                        if !self.p.mention.trim().is_empty() {
                            sys.push_str(&format!(
                                "\n\nОБЯЗАТЕЛЬНО: естественно упомяни «{}» в ответе (не в лоб, а к месту).",
                                self.p.mention.trim()
                            ));
                        }
                        sys.push_str(NO_MARKDOWN);
                        vec![Msg::system(sys), Msg::user(prompt)]
                    };
                    match self.core.ai.generate(self.ai, &msgs, self.log, self.stop).await {
                        Ok(a) if !a.is_empty() => {
                            self.sh.ai_fails.store(0, Ordering::SeqCst);
                            ai_raw = a.clone();
                            a
                        }
                        Ok(_) => {
                            self.note_ai_fail("   ❌ Пустой ответ от нейросети");
                            self.stop.sleep_ms(3000).await;
                            break;
                        }
                        Err(crate::ai::AiError::Aborted) => break,
                        Err(e) => {
                            self.note_ai_fail(&format!("   ❌ Нейросеть не ответила: {e}"));
                            self.stop.sleep_ms(3000).await;
                            break;
                        }
                    }
                }
                AnswerMode::Mangle => {
                    let a = scramble_question(&q.title);
                    if a.is_empty() {
                        (self.log)("   ⚠️  Вопрос слишком короткий для коверканья — пропускаю");
                        break;
                    }
                    a
                }
                AnswerMode::NoAi => {
                    let list: Vec<&str> = if self.p.noai_answers.is_empty() {
                        NOAI_ANSWERS.to_vec()
                    } else {
                        self.p.noai_answers.iter().map(|s| s.as_str()).collect()
                    };
                    pick_one(&list).map(|s| s.to_string()).unwrap_or_default()
                }
            };

            if self.stop.is_stopped() {
                break;
            }
            if self.p.random_tag {
                answer = with_random_tag(&answer);
            }
            answer = with_signature(&answer, &self.p.signature);
            (self.log)(&format!("   💬 {}", clip(&answer, 90)));

            // 2) картинка
            let image = self.build_image().await;
            if self.stop.is_stopped() {
                break;
            }
            if self.sh.blocked() {
                (self.log)("   🛑 Пропускаю постинг (блокировка).");
                break;
            }

            // 3) постинг
            match post_answer(self.core, self.acc, &q.id, &answer, image.as_ref(), self.log, self.stop).await
            {
                Err(_) => break,
                Ok(PostRes::Blocked) => {
                    // Антибот: без этого флага бот продолжал бы долбиться в
                    // закрытую дверь и получал бы 418 на каждый следующий ответ.
                    self.sh.blocked.store(true, Ordering::SeqCst);
                    (self.log)("   🛑 Антибот mail.ru при отправке (418/429) — стоп аккаунта.");
                    break;
                }
                Ok(PostRes::Ok(_id)) => {
                    posted_any = true;
                    self.sh.fails.store(0, Ordering::SeqCst);
                    let n = self.sh.count.fetch_add(1, Ordering::SeqCst) + 1;
                    if self.convo_mode {
                        if let Some(m) = &ask_msg {
                            self.core.convo.push_turn(
                                Msg::user(m.clone()),
                                Msg::assistant(if ai_raw.is_empty() {
                                    answer.clone()
                                } else {
                                    ai_raw.clone()
                                }),
                            );
                            self.compress_convo().await;
                        }
                    }
                    (self.log)(&format!(
                        "   ✅ Отправлено [{n}/{}]{}",
                        self.limit_label(),
                        if self.repeat_per > 1 {
                            format!(" (на этот вопрос {}/{})", rep + 1, self.repeat_per)
                        } else {
                            String::new()
                        }
                    ));
                }
                Ok(PostRes::Failed) => {
                    // Не антибот, а разовый сбой: ждём подольше, чтобы не
                    // молотить сайт без передышки.
                    let fails = self.sh.fails.fetch_add(1, Ordering::SeqCst) + 1;
                    if fails >= MAX_FAILS {
                        (self.log)(&format!(
                            "🛑 Подряд {MAX_FAILS} отказов при отправке — останавливаю аккаунт (обычно это дневной лимит ответов)."
                        ));
                        break;
                    }
                    (self.log)("   ⏳ Не отправлено — пауза 10 сек перед следующей попыткой...");
                    self.stop.sleep_ms(10_000).await;
                    break;
                }
            }

            if rep + 1 < self.repeat_per && !self.done() && self.pause_between().await {
                break;
            }
        }
        posted_any
    }

    /// Картинка к ответу: из пула (хэш уже на CDN) либо заливкой файлов.
    async fn build_image(&self) -> Option<Value> {
        match &self.p.image {
            ImageMode::Off => None,
            ImageMode::Gif { selected } => {
                let pool = journals::load_gif_pool(&self.core.root);
                let gifs = pick_n_gifs(&pool, selected, self.img_count as usize);
                if gifs.is_empty() {
                    (self.log)("   ⚠️  Пул пуст — без картинки");
                    return None;
                }
                (self.log)(&format!("   🎞  Картинка из пула ×{}", gifs.len()));
                gallery_from_pool(&gifs)
            }
            ImageMode::Upload { dir } => {
                let paths = pick_n_images(dir, self.img_count as usize);
                if paths.is_empty() {
                    (self.log)("   ⚠️  Нет картинок в папке — без картинки");
                    return None;
                }
                (self.log)(&format!("   🖼  Заливаю картинку ({} шт.)...", paths.len()));
                let mut uploaded: Vec<(String, i64, i64)> = Vec::new();
                for path in paths {
                    match self
                        .core
                        .http
                        .upload_picture(self.acc, std::path::Path::new(&path), self.stop)
                        .await
                    {
                        Ok(up) => uploaded.push((up.url, up.width, up.height)),
                        Err(e) if e == "blocked" => {
                            self.sh.blocked.store(true, Ordering::SeqCst);
                            (self.log)("   🛑 Антибот при заливке картинки (418/429).");
                            break;
                        }
                        Err(e) => {
                            if self.stop.is_stopped() {
                                break;
                            }
                            (self.log)(&format!("   ⚠️  Картинка не залилась: {e}"));
                        }
                    }
                }
                if uploaded.is_empty() {
                    return None;
                }
                (self.log)(&format!("   🖼  Залито {} шт.", uploaded.len()));
                image_gallery_node(&uploaded)
            }
        }
    }

    /// Сжатие единого чата: старые ходы суммируем тем же API, последние
    /// оставляем как есть. Если сжать не вышло — просто отбрасываем старое окно.
    async fn compress_convo(&self) {
        const KEEP_TURNS: usize = 8;
        if self.p.convo_budget_k <= 0.0 {
            return;
        }
        let threshold = (self.p.convo_budget_k * 1000.0) as usize;
        let convo = self.core.convo.snapshot();
        let chars: usize = convo.summary.chars().count()
            + convo.turns.iter().map(|t| t.content.chars().count()).sum::<usize>();
        if chars < threshold || convo.turns.len() <= KEEP_TURNS {
            return;
        }
        let split = convo.turns.len() - KEEP_TURNS;
        let keep: Vec<Msg> = convo.turns[split..].to_vec();
        let transcript = convo.turns[..split]
            .iter()
            .map(|t| format!("{}: {}", if t.role == "user" { "Вопрос" } else { "Мой ответ" }, t.content))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "{}Сожми этот кусок моего разговора на Ответах Mail.ru в краткое резюме (4–6 предложений): какие темы обсуждали, как я отвечал, мой характер и манера. Только суть, без воды.\n\n{transcript}",
            if convo.summary.is_empty() { String::new() } else { format!("Прошлое резюме: {}\n\n", convo.summary) }
        );
        let msgs = vec![Msg::user(prompt)];
        match self.core.ai.generate_with(self.ai, &msgs, 0.3, 400, self.log, self.stop).await {
            Ok(sum) if !sum.trim().is_empty() => {
                self.core.convo.set_compressed(sum.trim().to_string(), keep);
                (self.log)("   ♻️ Контекст разговора сжат (обновил summary).");
            }
            _ => {
                self.core.convo.set_compressed(convo.summary.clone(), keep);
                (self.log)("   ♻️ Контекст урезан (старые сообщения отброшены).");
            }
        }
    }

    /// Отметка в журнале: ответили — в `answered`, не вышло — в «пробовали».
    fn after_one(&self, q: &Question, posted_any: bool) {
        if posted_any {
            journals::append_answered(&self.core.root, &self.acc.name, &q.norm);
            self.sh.exclude.lock().insert(q.norm.clone());
        } else if !self.sh.blocked() {
            self.sh.tried.lock().insert(q.norm.clone());
        }
    }
}

// ─── Сценарии ───────────────────────────────────────────────────────────────

async fn answer_links(ctx: &RunCtx<'_>) {
    let mut targets: Vec<(String, Option<String>)> = Vec::new();
    for u in &ctx.p.links {
        match parse_answer_target(u) {
            Some(t) => targets.push(t),
            None if !u.trim().is_empty() => (ctx.log)(&format!(
                "⚠️  Не похоже на ссылку вопроса/ответа — пропускаю: {}",
                clip(u.trim(), 80)
            )),
            None => {}
        }
    }
    if targets.is_empty() {
        (ctx.log)("❌ Нет распознанных ссылок — нечего отвечать.");
        return;
    }
    (ctx.log)(&format!("🔗 Режим по ссылкам: целей {} (по {} на каждую).", targets.len(), ctx.repeat_per));

    for (topic_id, reply_id) in targets {
        if ctx.done() {
            break;
        }
        let norm = format!("https://otvet.mail.ru/question/{topic_id}");
        if ctx.sh.exclude.lock().contains(&norm) {
            (ctx.log)(&format!("⏭️  Уже отвечал на {norm} — пропускаю"));
            continue;
        }
        let q = if ctx.p.mode == AnswerMode::NoAi {
            // текст вопроса не нужен — реплика берётся из набора
            Question { id: topic_id.clone(), norm: norm.clone(), ..Default::default() }
        } else {
            (ctx.log)(&format!("   🔎 Читаю вопрос #{topic_id}..."));
            match fetch_question(ctx.core, ctx.acc, &topic_id, ctx.stop).await {
                Some(mut q) => {
                    q.norm = norm.clone();
                    q
                }
                None => {
                    if ctx.stop.is_stopped() {
                        break;
                    }
                    (ctx.log)(&format!("   ⚠️  Не удалось прочитать вопрос #{topic_id} — пропускаю"));
                    ctx.sh.tried.lock().insert(norm);
                    continue;
                }
            }
        };
        let title = if q.title.is_empty() { format!("Вопрос #{topic_id}") } else { q.title.clone() };
        (ctx.log)(&format!(
            "\n→ {}{}",
            clip(&title, 70),
            reply_id.map(|r| format!(" (в ответ на #{r})")).unwrap_or_default()
        ));
        let posted = ctx.handle_one(&q).await;
        ctx.after_one(&q, posted);
        if !ctx.done() && ctx.pause_between().await {
            break;
        }
    }
}

async fn answer_feed(
    ctx: &RunCtx<'_>,
    recent: i64,
    batch_per: i64,
    go_parallel: bool,
    f_min: f64,
    f_max: f64,
) {
    if batch_per > 1 {
        (ctx.log)(&format!(
            "📦 Беру по {batch_per} вопрос(ов) за один проход ленты{}.",
            if go_parallel {
                " и отвечаю на них ПАРАЛЛЕЛЬНО (риск антибота!)"
            } else {
                " — успеваю при наплыве"
            }
        ));
    }
    loop {
        if ctx.stop.is_stopped() {
            (ctx.log)("\n⛔ Остановлено пользователем");
            break;
        }
        if ctx.sh.blocked() {
            (ctx.log)("\n🛑 Блокировка mail.ru (418/429). Останавливаю аккаунт.");
            break;
        }
        if ctx.sh.limit_hit(ctx.p.limit) || ctx.sh.too_many_fails() {
            break;
        }

        let questions = {
            let exclude = ctx.sh.exclude.lock().clone();
            let tried = ctx.sh.tried.lock().clone();
            match collect_questions(ctx.core, ctx.acc, &exclude, &tried, recent, ctx.stop).await {
                Ok(q) => q,
                Err(HttpError::Aborted) => break,
                Err(e) => {
                    let wait = rand_f(f_min.max(8.0), f_max.max(8.0));
                    (ctx.log)(&format!(
                        "   ⚠️  Сеть/прокси при загрузке ленты ({e}) — повторю через {wait:.0} сек."
                    ));
                    if ctx.stop.sleep_ms((wait * 1000.0) as u64).await {
                        break;
                    }
                    continue;
                }
            }
        };
        if questions.is_empty() {
            let wait = rand_f(f_min, f_max);
            (ctx.log)(&format!(
                "   Среди {recent} последних новых нет отвечаемых — обновлю ленту через {wait:.1} сек..."
            ));
            if ctx.stop.sleep_ms((wait * 1000.0) as u64).await {
                break;
            }
            continue;
        }

        let batch: Vec<Question> = questions.into_iter().take(batch_per as usize).collect();
        if batch_per > 1 && !go_parallel {
            (ctx.log)(&format!("\n📥 За этот проход беру {} вопрос(ов) из ленты", batch.len()));
        }

        if go_parallel {
            (ctx.log)(&format!("\n⚡ Отвечаю на {} вопрос(ов) ПАРАЛЛЕЛЬНО (разом)...", batch.len()));
            let mut tasks = FuturesUnordered::new();
            for q in &batch {
                tasks.push(async move {
                    if ctx.done() {
                        return;
                    }
                    (ctx.log)(&format!("\n→ {}", clip(&q.title, 70)));
                    let posted = ctx.handle_one(q).await;
                    ctx.after_one(q, posted);
                });
            }
            while tasks.next().await.is_some() {}
            // Пауза действует МЕЖДУ пачками, а не между ответами.
            if !ctx.done() && ctx.pause_between().await {
                break;
            }
        } else {
            for q in &batch {
                if ctx.done() {
                    break;
                }
                (ctx.log)(&format!("\n→ {}", clip(&q.title, 70)));
                let posted = ctx.handle_one(q).await;
                ctx.after_one(q, posted);
                if !ctx.done() && ctx.pause_between().await {
                    break;
                }
            }
        }
    }
}

/// Непрерывная лента: держим до `max_active` ответов «в полёте» и подливаем
/// свежие вопросы, не дожидаясь конца пачки.
async fn answer_feed_continuous(ctx: &RunCtx<'_>, recent: i64, batch_per: i64, f_min: f64, f_max: f64) {
    let max_active = batch_per.max(1) as usize;
    (ctx.log)(&format!(
        "♻️  Непрерывное обновление ленты: каждый новый вопрос — сразу в обработку (макс. {max_active} одновременно)."
    ));
    let mut queued: HashSet<String> = HashSet::new();
    let mut tasks = FuturesUnordered::new();

    loop {
        if ctx.done() && tasks.is_empty() {
            break;
        }

        // Подливаем новые вопросы, пока есть свободные слоты.
        if !ctx.done() && tasks.len() < max_active {
            let fresh = {
                let exclude = ctx.sh.exclude.lock().clone();
                let tried = ctx.sh.tried.lock().clone();
                collect_questions(ctx.core, ctx.acc, &exclude, &tried, recent, ctx.stop)
                    .await
                    .unwrap_or_default()
            };
            for q in fresh {
                if tasks.len() >= max_active || ctx.done() {
                    break;
                }
                if !queued.insert(q.norm.clone()) {
                    continue;
                }
                tasks.push(async move {
                    (ctx.log)(&format!("\n→ {}", clip(&q.title, 70)));
                    let posted = ctx.handle_one(&q).await;
                    ctx.after_one(&q, posted);
                });
            }
        }

        if tasks.is_empty() {
            if ctx.done() {
                break;
            }
            let wait = rand_f(f_min.max(0.5), f_max.max(0.5));
            if ctx.stop.sleep_ms((wait * 1000.0) as u64).await {
                break;
            }
        } else {
            // Ждём завершения одной задачи и сразу подливаем следующую.
            if tasks.next().await.is_none() {
                break;
            }
        }
    }
}

// ─── Мелочи ─────────────────────────────────────────────────────────────────

fn rand_f(min: f64, max: f64) -> f64 {
    if max <= min {
        return min.max(0.0);
    }
    min + crate::util::rand_f64() * (max - min)
}

const IMAGE_EXTS: [&str; 5] = ["jpg", "jpeg", "png", "gif", "webp"];

pub fn list_images(dir: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut out: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| IMAGE_EXTS.contains(&x.to_lowercase().as_str()))
                .unwrap_or(false)
        })
        .map(|e| e.path().to_string_lossy().to_string())
        .collect();
    out.sort();
    out
}

fn pick_n_images(dir: &str, n: usize) -> Vec<String> {
    let mut list = list_images(dir);
    if list.len() <= n {
        return list;
    }
    shuffle(&mut list);
    list.truncate(n);
    list
}

/// N случайных РАЗНЫХ картинок из пула. Если что-то выбрано вручную — берём из
/// выбранного, иначе из всего пула.
fn pick_n_gifs(pool: &[PoolImage], selected: &[String], n: usize) -> Vec<PoolImage> {
    let from: Vec<PoolImage> = if selected.is_empty() {
        vec![]
    } else {
        pool.iter().filter(|g| selected.contains(&g.hash)).cloned().collect()
    };
    let mut src = if from.is_empty() { pool.to_vec() } else { from };
    if src.len() <= n {
        return src;
    }
    shuffle(&mut src);
    src.truncate(n);
    src
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scramble_keeps_words_and_tail() {
        let src = "почему небо синее?";
        let out = scramble_question(src);
        // Хвостовая пунктуация клеится к СЛУЧАЙНОМУ слову (как в JS-версии),
        // поэтому проверяем её наличие, а не позицию.
        assert_eq!(out.matches('?').count(), 1);
        let mut a: Vec<String> = src.replace('?', "").split_whitespace().map(|s| s.to_string()).collect();
        let mut b: Vec<String> = out.replace('?', "").split_whitespace().map(|s| s.to_string()).collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);
        // одно слово перемешивать нечего
        assert_eq!(scramble_question("привет"), "привет");
    }

    #[test]
    fn signature_and_tag() {
        assert_eq!(with_signature("текст", "  "), "текст");
        assert_eq!(with_signature("текст", "подпись"), "текст\n\nподпись");
        let t = with_random_tag("ответ");
        assert!(t.starts_with("ответ #"));
    }

    #[test]
    fn parses_links() {
        let (t, r) = parse_answer_target("https://otvet.mail.ru/question/123?reply=456").unwrap();
        assert_eq!(t, "123");
        assert_eq!(r.as_deref(), Some("456"));
        let (t, r) = parse_answer_target("https://otvet.mail.ru/question/123").unwrap();
        assert_eq!(t, "123");
        assert!(r.is_none());
        assert!(parse_answer_target("мусор").is_none());
    }
}
