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
use crate::uniq::{uniquify, UniqMode};
use crate::util::{clip, pick_one, rand_range, shuffle, Log, Progress, Stop};
use crate::{Core, RunOutcome};
use futures::stream::{FuturesUnordered, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

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
    /// Диапазон номеров вопросов — в том числе ещё не заданных.
    ///
    /// Номера у mail.ru идут подряд, а ответ принимается и на тот вопрос,
    /// которого пока нет: он «дождётся» автора. Текст берётся только готовый —
    /// вопроса ещё не существует, нейросети не о чем писать, а проверять
    /// «остался ли ответ на месте» бессмысленно по той же причине.
    Range,
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

/// Разбор списка слов: по строкам, запятым и точкам с запятой, в нижний
/// регистр, без повторов. Пустые куски выбрасываем — список набирают на ходу.
pub fn parse_keywords(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split([',', '\n', ';']) {
        let w = w.trim().to_lowercase();
        if !w.is_empty() && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// Есть ли в вопросе хоть одно слово из списка. Ищем подстрокой и в заголовке,
/// и в теле: «vpn» найдётся и в «VPN-сервис», и в «нужен впн». Пустой список
/// пропускает всё.
pub fn has_keyword(words: &[String], q: &Question) -> bool {
    if words.is_empty() {
        return true;
    }
    let hay = format!("{} {}", q.title, q.body).to_lowercase();
    words.iter().any(|w| hay.contains(w))
}

/// Общая на весь прогон очередь номеров диапазона.
///
/// Раньше каждый аккаунт шёл по диапазону сам, и десять аккаунтов клали десять
/// ответов под один и тот же будущий вопрос: работа не делилась, а множилась.
/// Здесь номер выдаётся ровно один раз — сколько аккаунтов, во столько раз
/// быстрее разбирается диапазон, хоть по очереди, хоть всеми сразу.
#[derive(Debug, Default)]
pub struct RangeQueue {
    /// Следующий невыданный номер. Ноль — очередь ещё не начата: первый
    /// пришедший ставит начало диапазона.
    next: AtomicI64,
    /// Номера, взятые, но не отработанные: аккаунт словил антибот или уткнулся
    /// в лимит посреди пачки. Без возврата такой номер пропал бы навсегда —
    /// выдан он ровно один раз, а ответа под ним нет.
    back: Mutex<Vec<i64>>,
}

impl RangeQueue {
    /// Занять следующий номер. `None` — диапазон разобран до конца.
    pub fn take(&self, from: i64, to: i64) -> Option<i64> {
        // Возвращённые разбираем первыми: они старше и ждут дольше.
        if let Some(id) = self.back.lock().pop() {
            return Some(id);
        }
        let _ = self.next.compare_exchange(0, from, Ordering::SeqCst, Ordering::SeqCst);
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        (id <= to).then_some(id)
    }

    /// Вернуть номер в очередь: этот аккаунт до него не добрался.
    pub fn give_back(&self, id: i64) {
        self.back.lock().push(id);
    }

    /// Сколько номеров ещё ждёт работы — для лога на старте аккаунта.
    pub fn left(&self, from: i64, to: i64) -> i64 {
        let next = self.next.load(Ordering::SeqCst).max(from);
        (to - next + 1).max(0) + self.back.lock().len() as i64
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerParams {
    pub mode: AnswerMode,
    pub target: TargetMode,
    pub links: Vec<String>,
    /// Границы диапазона номеров (включительно) для `TargetMode::Range`.
    pub range_from: i64,
    pub range_to: i64,
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
    /// Не отвечать на вопросы, заданные своими же аккаунтами.
    pub skip_own_authors: bool,
    /// Показывать нейросети картинки из вопроса. Нужна модель, которая умеет
    /// смотреть; текстовая на такой запрос ответит ошибкой.
    #[serde(default)]
    pub see_images: bool,
    /// Слова-приметы: если список не пуст, бот берёт только те вопросы, где
    /// встретилось хотя бы одно. Текст ответа при этом обычный — нейросетью или
    /// готовыми фразами, как выбрано в режиме.
    pub keywords: Vec<String>,
    /// Уникализация текста вместо старого «#123456».
    pub uniq: UniqMode,
    /// Подменять похожие буквы латиницей (по умолчанию нет — см. uniq.rs).
    pub uniq_latin: bool,
    /// Проверять, что ответ реально появился на сайте.
    pub verify_posted: bool,
    /// Сколько ждать перед проверкой, сек.
    pub verify_delay_sec: f64,
    pub signature: String,
    pub noai_answers: Vec<String>,
    pub image: ImageMode,
    pub image_count: i64,
    pub ai: AiCfg,
    pub style: String,
    pub custom_prompt: String,
    pub mention: String,
    pub check_auth: bool,
    /// Живой счётчик для интерфейса. В файл не пишется.
    #[serde(skip)]
    pub progress: Progress,
    /// Общая очередь номеров диапазона: одна на прогон, у всех аккаунтов та же.
    /// В файл не пишется — живёт только пока идёт работа.
    #[serde(skip)]
    pub range_queue: Arc<RangeQueue>,
}

impl Default for AnswerParams {
    fn default() -> Self {
        Self {
            mode: AnswerMode::Ai,
            target: TargetMode::Feed,
            links: vec![],
            range_from: 0,
            range_to: 0,
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
            skip_own_authors: true,
            see_images: false,
            keywords: vec![],
            uniq: UniqMode::Off,
            uniq_latin: false,
            verify_posted: true,
            verify_delay_sec: 6.0,
            signature: String::new(),
            noai_answers: vec![],
            image: ImageMode::Off,
            image_count: 1,
            ai: AiCfg::preset(),
            style: "Обычный чел".into(),
            custom_prompt: String::new(),
            mention: String::new(),
            check_auth: true,
            progress: Progress::new(),
            range_queue: Arc::new(RangeQueue::default()),
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
    /// Картинки вопроса — `src` из галерей, без адреса CDN. Половина вопросов
    /// на сайте это фото с подписью «как вам?», и без них текст бессмысленный.
    pub images: Vec<String>,
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
    mine: &HashSet<String>,
    recent: i64,
    stop: &Stop,
) -> Result<Vec<Question>, HttpError> {
    let r = core
        .http
        // Больше 20 сайт за раз не отдаёт, сколько ни проси (проверено живьём):
        // `limit=50` возвращает те же 20. Интерфейс поэтому и не даёт больше.
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
        // Свой же вопрос отвечать незачем: два своих аккаунта в одной ветке —
        // готовая связка для модерации, да и карма от этого не растёт.
        if is_mine(&a, mine) {
            continue;
        }
        out.push(Question {
            id: id.to_string(),
            norm,
            title,
            body: it.get("content").map(doc_to_text).unwrap_or_default(),
            images: it.get("content").map(crate::content::doc_images).unwrap_or_default(),
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

/// Автор вопроса — один из наших аккаунтов? Сверяем и по id, и по нику:
/// в ленте приходит то одно, то другое.
pub fn is_mine(author: &Value, mine: &HashSet<String>) -> bool {
    if mine.is_empty() {
        return false;
    }
    let id = author.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string());
    let user = author.get("username").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
    id.map(|i| mine.contains(&i)).unwrap_or(false) || user.map(|u| mine.contains(&u)).unwrap_or(false)
}

/// Ключи своих аккаунтов: id и ники в нижнем регистре.
pub fn my_keys(core: &Core) -> HashSet<String> {
    let mut out = HashSet::new();
    for a in core.accounts.all() {
        if let Some(id) = a.user_id {
            out.insert(id.to_string());
        }
        if let Some(u) = a.username.as_deref() {
            if !u.is_empty() {
                out.insert(u.to_lowercase());
            }
        }
    }
    out
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
        images: obj.get("content").map(crate::content::doc_images).unwrap_or_default(),
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
    // Регулярка одна на процесс: собирать её на каждую ссылку — это заметные
    // сотни микросекунд там, где ссылок тысячи.
    let topic = crate::votes::topic_id_from_url(s)?;
    let reply = match q.kind {
        crate::votes::Kind::Reply => Some(q.id),
        crate::votes::Kind::Topic => None,
    };
    Some((topic, reply))
}

/// Чем кончилась попытка отправить.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostRes {
    Ok(i64),
    /// Антибот mail.ru: дальше по этому аккаунту идти нельзя.
    Blocked,
    /// Сайт отказался принимать ИМЕННО ЭТОТ текст: 4xx, но не антибот. Ответ
    /// при этом не создан, поэтому можно спокойно пробовать другим текстом —
    /// в отличие от сетевого сбоя, где неизвестно, дошло или нет.
    Rejected(String),
    /// Прочая неудача: сеть, 5xx, непонятный ответ. Дошло или нет — неизвестно,
    /// поэтому повторять тот же шаг нельзя.
    Failed,
}

/// Короткая причина отказа: в логе должно быть видно, ЧЕМ текст не понравился,
/// а не только «HTTP 400».
pub fn refusal_reason(r: &crate::http::Resp) -> String {
    let from_json = r.json.as_ref().and_then(|j| {
        for key in ["error", "message", "detail", "description", "reason"] {
            match j.get(key) {
                Some(Value::String(s)) if !s.trim().is_empty() => return Some(s.trim().to_string()),
                Some(Value::Object(o)) => {
                    for inner in ["message", "description", "text"] {
                        if let Some(Value::String(s)) = o.get(inner) {
                            if !s.trim().is_empty() {
                                return Some(s.trim().to_string());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        None
    });
    match from_json {
        Some(m) => format!("HTTP {}: {}", r.status, crate::util::clip(&m, 90)),
        None if r.text.trim().is_empty() => format!("HTTP {}", r.status),
        None => format!("HTTP {}: {}", r.status, r.snippet(90)),
    }
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
        // ЧИСЛО, а не строка. mail.ru разбирает тело строго по типам и на
        // `"topic_id":"270370769"` отвечает 400 с «expected=int64, got=string» —
        // то есть ни один ответ не уходит вообще.
        "topic_id": topic_id.parse::<i64>().unwrap_or(0),
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
    // 4xx (кроме антибота) — отказ по содержимому: сайт ничего не создал, и
    // повторять с ДРУГИМ текстом безопасно.
    if (400..500).contains(&r.status) {
        return Ok(PostRes::Rejected(refusal_reason(&r)));
    }
    if !r.ok {
        log(&format!("   [!] Ответ не прошёл: HTTP {} {}", r.status, r.snippet(120)));
    }
    Ok(PostRes::Failed)
}

/// Сколько ответов сайт отдаёт одной страницей. Точное число неважно: важно не
/// принять «страница кончилась» за «ответа нет».
const PAGE_GUESS: usize = 20;

/// Виден ли отправленный ответ на сайте.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    /// Ответ на месте.
    Present,
    /// Ответа нет — снесла автомодерация.
    Missing,
    /// Проверить не вышло (сеть, антибот) — наказывать не за что.
    Unknown,
}

/// Проверить, что ответ действительно опубликован.
///
/// mail.ru отвечает «принято» и на то, что через несколько секунд удалит
/// автомодерация: id в ответе есть, а самого ответа под вопросом нет. Без этой
/// проверки бот считает такие ответы отправленными и идёт дальше, а вопрос
/// остаётся без ответа — и в журнале записан как отвеченный.
pub async fn verify_answer(core: &Core, acc: &Account, topic_id: &str, reply_id: i64, stop: &Stop) -> Verify {
    let Ok(r) = core.http.request(acc, &format!("/api/topic/answers/{topic_id}"), ReqOpts::get(), stop).await
    else {
        return Verify::Unknown;
    };
    if r.blocked || !r.ok {
        return Verify::Unknown;
    }
    let Some(res) = r.result() else { return Verify::Unknown };
    let mut all: Vec<&Value> =
        res.get("replies").and_then(|v| v.as_array()).map(|a| a.iter().collect()).unwrap_or_default();
    match res.get("best_replies") {
        Some(Value::Array(a)) => all.extend(a.iter()),
        Some(v) if !v.is_null() => all.push(v),
        _ => {}
    }
    // Пустой список ответов на только что отвеченный вопрос — это не «нет
    // ответа», а подозрительный ответ сервера: считаем неизвестным.
    if all.is_empty() {
        return Verify::Unknown;
    }
    // Ответы приходят страницей. Если страница выглядит полной, нашего ответа
    // могло просто не хватить места — и «не нашли» тут НЕ значит «снесли».
    // Ошибиться в эту сторону дорого: бот отправит второй ответ, и под вопросом
    // окажутся два от одного аккаунта.
    if all.len() >= PAGE_GUESS {
        return Verify::Unknown;
    }
    if all.iter().any(|x| x.get("id").and_then(|v| v.as_i64()) == Some(reply_id)) {
        Verify::Present
    } else {
        Verify::Missing
    }
}

// ─── Текст ответа ───────────────────────────────────────────────────────────

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
    /// Занятые места под лимитом: отправленные ответы ПЛЮС те, что прямо сейчас
    /// уходят на сайт. Без этого счётчика параллельная пачка пробивает лимит:
    /// все задачи успевают увидеть `count == 0` до первой отправки и постят
    /// разом — при лимите 5 и пачке 10 уходит десять ответов.
    reserved: AtomicI64,
    blocked: AtomicBool,
    /// Отказов подряд при отправке.
    fails: AtomicI64,
    /// Диапазон пройден до конца, и отвечать в нём больше не на что.
    exhausted: AtomicBool,
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
        limit > 0 && self.reserved.load(Ordering::SeqCst) >= limit
    }
    /// Занять место под лимитом перед отправкой. `false` — мест не осталось.
    fn reserve(&self, limit: i64) -> bool {
        if limit <= 0 {
            return true;
        }
        let mut cur = self.reserved.load(Ordering::SeqCst);
        loop {
            if cur >= limit {
                return false;
            }
            match self.reserved.compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(now) => cur = now,
            }
        }
    }
    /// Сколько ответов ещё можно отправить (при лимите 0 — сколько угодно).
    fn remaining(&self, limit: i64) -> i64 {
        if limit <= 0 {
            return i64::MAX;
        }
        (limit - self.reserved.load(Ordering::SeqCst)).max(0)
    }
    /// Ответ не ушёл — место возвращаем, иначе неудачные попытки съедали бы лимит.
    fn release(&self, limit: i64) {
        if limit > 0 {
            self.reserved.fetch_sub(1, Ordering::SeqCst);
        }
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
    // В диапазоне вопросов ещё нет. Нейросети не о чем писать, а проверка
    // «остался ли ответ на месте» упирается в несуществующую страницу и всегда
    // отвечает «не знаю». Поэтому и текст, и проверку задаём здесь сами — что бы
    // ни стояло в настройках.
    let patched: AnswerParams;
    let p = if p.target == TargetMode::Range {
        patched = AnswerParams { mode: AnswerMode::NoAi, verify_posted: false, ..p.clone() };
        &patched
    } else {
        p
    };
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
        // Отрицательная температура = «взять из стиля». Ноль — это НОЛЬ:
        // сухие предсказуемые ответы, а не «значение не задано». Раньше 0 молча
        // превращался в 0.7, и настройка «живость 0» просто не работала.
        if ai.temperature < 0.0 {
            ai.temperature = styles.temperature(&p.style).unwrap_or(0.7);
        }
        log("[>] Проверяю нейросеть...");
        match core.ai.check(&ai, stop).await {
            Ok(msg) => log(&format!("[+] {msg}")),
            Err(e) => {
                if stop.is_stopped() {
                    return out;
                }
                log(&format!("[-] {e}"));
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
        if convo_mode {
            convo_system = build_convo_system(&system_prompt, &p.mention);
            log(&format!(
                "[>] Диалоговый режим: единый чат с памятью. {}. Ответы — последовательно.",
                if p.convo_budget_k > 0.0 {
                    format!("Сжатие контекста после ~{}k символов", p.convo_budget_k)
                } else {
                    "Без сжатия — держу весь чат".into()
                }
            ));
        }
    } else if p.mode == AnswerMode::Mangle {
        log("[>] Режим коверканья — перемешиваю слова вопроса");
    } else {
        log("[>] Режим без AI — короткие готовые реплики");
    }

    // Диалоговый режим = ОБЩАЯ история на все аккаунты → работает один за раз.
    if convo_mode {
        if let Err(owner) = core.convo.try_acquire(&acc.name) {
            log(&format!("[!] Диалоговый режим занят аккаунтом «{owner}» — пропускаю"));
            out.skipped = true;
            return out;
        }
    }

    match &p.image {
        ImageMode::Gif { .. } => {
            let n = journals::load_gif_pool(&core.root).len();
            log(&format!(
                "[>] Картинка из пула в каждый ответ (в пуле: {n}){}",
                if n == 0 { " — пул пуст" } else { "" }
            ));
        }
        ImageMode::Upload { dir } => {
            let n = list_images(crate::util::images_dir(&core.root, dir)).len();
            log(&format!(
                "[>] Картинка из {dir} в каждый ответ (файлов: {n}){}",
                if n == 0 { " — папка пуста" } else { "" }
            ));
        }
        ImageMode::Off => {}
    }

    if !p.keywords.is_empty() {
        log(&format!("[>] Беру только вопросы со словами: {}", p.keywords.join(", ")));
    }

    if p.check_auth {
        let v = api::validate_cached(core, acc, stop).await;
        api::persist_validation(core, &acc.name, &v);
        out.karma = v.karma.clone();
        if v.blocked {
            log("[x] Антибот (418/429) при проверке — статус не меняю.");
            out.blocked = true;
        } else if v.banned {
            log("[x] Аккаунт заблокирован сайтом — пропускаю. Сессия жива, но действия молча не проходят.");
            if convo_mode {
                core.convo.release(&acc.name);
            }
            out.skipped = true;
            return out;
        } else if v.alive {
            log("[+] Авторизован");
        } else if v.auth_bad {
            log("[x] НЕ авторизован");
            log("Пропускаю аккаунт — не залогинен.");
            if convo_mode {
                core.convo.release(&acc.name);
            }
            out.skipped = true;
            return out;
        } else {
            log("[!] Не удалось проверить авторизацию (ошибка/сеть) — продолжаю, статус не трогаю.");
        }
    }

    let answered = journals::load_answered(&core.root, &acc.name);
    let exclude: HashSet<String> = if p.skip_others {
        let all = journals::load_all_answered(&core.root);
        log(&format!("[>] Пропускаю вопросы, отвеченные другими аккаунтами (в базе: {})", all.len()));
        all
    } else {
        answered
    };

    let sh = Shared {
        count: AtomicI64::new(0),
        reserved: AtomicI64::new(0),
        blocked: AtomicBool::new(out.blocked),
        fails: AtomicI64::new(0),
        exhausted: AtomicBool::new(false),
        ai_fails: AtomicI64::new(0),
        exclude: Mutex::new(exclude),
        tried: Mutex::new(HashSet::new()),
    };

    let mine = if p.skip_own_authors { my_keys(core) } else { HashSet::new() };
    if !mine.is_empty() {
        log(&format!("[>] Вопросы своих аккаунтов пропускаю (своих в базе: {})", core.accounts.len()));
    }
    // Пул картинок читаем один раз на прогон, а не на каждый ответ.
    let pool = match &p.image {
        ImageMode::Gif { .. } => journals::load_gif_pool(&core.root),
        _ => Vec::new(),
    };

    let ctx = RunCtx {
        core,
        acc,
        p,
        mine: &mine,
        pool: &pool,
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
        TargetMode::Range => answer_range(&ctx, batch_per, go_parallel).await,
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
    out.exhausted = sh.exhausted.load(Ordering::SeqCst);
    let line = "=".repeat(50);
    if stop.is_stopped() {
        log(&format!("\n{line}\n[x] Остановлено пользователем. Ответов отправлено: {}\n{line}", out.done));
    } else {
        log(&format!("\n{line}\n[+] Готово! Ответов отправлено: {}\n{line}", out.done));
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

/// Чем закончилась одна попытка ответить.
enum Step {
    /// Ответ ушёл и виден на сайте.
    Posted,
    /// Отправился, но на сайте его нет — можно попробовать другим текстом.
    Vanished,
    /// Сайт не принял текст. Тоже повод перегенерировать: ответа не появилось.
    Rejected,
    /// Дальше по этому вопросу (а иногда и по аккаунту) не идём.
    Stop,
}

/// Всё, что нужно обработчику одного вопроса. Ссылками, чтобы не копировать
/// параметры на каждый ответ.
struct RunCtx<'a> {
    core: &'a Core,
    acc: &'a Account,
    p: &'a AnswerParams,
    /// id и ники своих аккаунтов — их вопросы пропускаем.
    mine: &'a HashSet<String>,
    /// Пул картинок, прочитанный один раз на прогон.
    pool: &'a [PoolImage],
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
    /// Отказов подряд набралось столько, что дальше идти незачем.
    ///
    /// Причину уточняем одним запросом: протухшая сессия и дневной лимит дают
    /// на отправке один и тот же отказ, и раньше в лог всегда уходил лимит —
    /// даже когда аккаунт был просто разлогинен.
    async fn report_fail_stop(&self) {
        let v = api::validate_account(self.core, self.acc, self.stop).await;
        api::persist_validation(self.core, &self.acc.name, &v);
        if v.auth_bad {
            (self.log)(&format!(
                "[x] Подряд {MAX_FAILS} отказов при отправке — аккаунт разлогинен, нужен новый вход."
            ));
        } else {
            (self.log)(&format!("[x] Подряд {MAX_FAILS} отказов при отправке — останавливаю аккаунт."));
        }
    }

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
            (self.log)(&format!("   [>] Пауза {d:.1} сек..."));
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
                "[x] Нейросеть не отвечает {MAX_FAILS} раз подряд — останавливаю аккаунт (проверь ключ и баланс)."
            ));
        }
    }

    /// Обработка одного вопроса: `repeat_per` ответов, каждый — с проверкой,
    /// что он действительно появился на сайте.
    async fn handle_one(&self, q: &Question) -> bool {
        let mut posted_any = false;
        for rep in 0..self.repeat_per {
            if self.done() {
                break;
            }
            // Две попытки на ответ. Вторая нужна, когда сайт не принял текст
            // или снёс его автомодерацией: и то и другое лечится НОВЫМ текстом,
            // тот же самый отклонят снова. Когда всё хорошо, вторая попытка не
            // тратится вовсе.
            const TRIES: i64 = 2;
            let mut ok = false;
            for attempt in 0..TRIES {
                match self.answer_once(q, rep, attempt).await {
                    Step::Posted => {
                        ok = true;
                        posted_any = true;
                        break;
                    }
                    Step::Vanished | Step::Rejected => continue,
                    Step::Stop => return posted_any,
                }
            }
            if !ok {
                break;
            }
            if rep + 1 < self.repeat_per && !self.done() && self.pause_between().await {
                break;
            }
        }
        posted_any
    }

    /// Подходит ли вопрос под список слов. Пустой список пропускает любой.
    fn passes_keywords(&self, q: &Question) -> bool {
        has_keyword(&self.p.keywords, q)
    }

    /// Картинки вопроса, готовые к показу модели.
    async fn question_images(&self, q: &Question) -> Vec<String> {
        if !self.p.see_images {
            return Vec::new();
        }
        api::fetch_images(self.core, self.acc, &q.images, self.log, self.stop).await
    }

    /// Одна попытка ответить: текст → картинка → отправка → проверка.
    async fn answer_once(&self, q: &Question, rep: i64, attempt: i64) -> Step {
        // 1) текст
        let mut ask_msg = None;
        let mut ai_raw = String::new();
        let mut answer = match self.p.mode {
            AnswerMode::Ai => {
                (self.log)(&match (self.repeat_per > 1, attempt > 0) {
                    (_, true) => "   [>] Прежний ответ не прошёл — генерирую другой...".to_string(),
                    (true, _) => format!("   [>] Генерирую ответ {}/{}...", rep + 1, self.repeat_per),
                    _ => "   [>] Генерирую ответ...".to_string(),
                });
                let pics = self.question_images(q).await;
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
                    msgs.push(Msg::user(m.clone()).with_images(pics));
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
                    vec![Msg::system(sys), Msg::user(prompt).with_images(pics)]
                };
                match self.core.ai.generate(self.ai, &msgs, self.log, self.stop).await {
                    Ok(a) if !a.is_empty() => {
                        self.sh.ai_fails.store(0, Ordering::SeqCst);
                        ai_raw = a.clone();
                        a
                    }
                    Ok(_) => {
                        self.note_ai_fail("   [-] Пустой ответ от нейросети");
                        self.stop.sleep_ms(3000).await;
                        return Step::Stop;
                    }
                    Err(crate::ai::AiError::Aborted) => return Step::Stop,
                    Err(e) => {
                        self.note_ai_fail(&format!("   [-] Нейросеть не ответила: {e}"));
                        self.stop.sleep_ms(3000).await;
                        return Step::Stop;
                    }
                }
            }
            AnswerMode::Mangle => {
                let a = scramble_question(&q.title);
                if a.is_empty() {
                    (self.log)("   [!] Вопрос слишком короткий для коверканья — пропускаю");
                    return Step::Stop;
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
            return Step::Stop;
        }
        // Уникализация — до подписи: подпись у всех ответов и так одинаковая,
        // трогать её незачем.
        answer = uniquify(&answer, self.p.uniq, self.p.uniq_latin);
        answer = with_signature(&answer, &self.p.signature);
        (self.log)(&format!("   [>] {}", clip(&answer, 90)));

        // 2) картинка
        let image = self.build_image().await;
        if self.stop.is_stopped() {
            return Step::Stop;
        }
        if self.sh.blocked() {
            (self.log)("   [x] Пропускаю постинг (блокировка).");
            return Step::Stop;
        }

        // 3) постинг
        //
        // Место под лимитом занимаем ПЕРЕД отправкой: при параллельной пачке
        // соседи уже готовы постить, и проверка счётчика в начале круга
        // ничего не гарантирует.
        if !self.sh.reserve(self.p.limit) {
            return Step::Stop;
        }
        let posted_id =
            match post_answer(self.core, self.acc, &q.id, &answer, image.as_ref(), self.log, self.stop).await
            {
                Err(_) => {
                    self.sh.release(self.p.limit);
                    return Step::Stop;
                }
                Ok(PostRes::Blocked) => {
                    self.sh.release(self.p.limit);
                    // Антибот: без этого флага бот продолжал бы долбиться в
                    // закрытую дверь и получал бы 418 на каждый следующий ответ.
                    self.sh.blocked.store(true, Ordering::SeqCst);
                    (self.log)("   [x] Антибот mail.ru при отправке (418/429) — стоп аккаунта.");
                    return Step::Stop;
                }
                Ok(PostRes::Rejected(why)) => {
                    // Отказали по тексту, а не по аккаунту: место под лимитом
                    // возвращаем и пробуем другим текстом. Счётчик отказов при этом
                    // ведём общий — если сайт отказывает подряд (дневной лимит), с
                    // перегенерацией это стоило бы денег за нейросеть на пустом месте.
                    self.sh.release(self.p.limit);
                    let fails = self.sh.fails.fetch_add(1, Ordering::SeqCst) + 1;
                    if fails >= MAX_FAILS {
                        self.report_fail_stop().await;
                        return Step::Stop;
                    }
                    (self.log)(&format!("   [!] Сайт не принял ответ ({why})"));
                    return Step::Rejected;
                }
                Ok(PostRes::Failed) => {
                    // Не антибот, а разовый сбой: ждём подольше, чтобы не
                    // молотить сайт без передышки.
                    self.sh.release(self.p.limit);
                    let fails = self.sh.fails.fetch_add(1, Ordering::SeqCst) + 1;
                    if fails >= MAX_FAILS {
                        self.report_fail_stop().await;
                        return Step::Stop;
                    }
                    (self.log)("   [!] Не отправлено — пауза 10 сек, дальше следующий вопрос.");
                    self.stop.sleep_ms(10_000).await;
                    return Step::Stop;
                }
                Ok(PostRes::Ok(id)) => id,
            };

        // 4) проверка, что ответ реально виден
        //
        // Сайт отвечает «принято» и на то, что через секунду снесёт
        // автомодерация. Без проверки бот считал такие ответы отправленными,
        // а на деле их не было.
        if self.p.verify_posted {
            let wait = self.p.verify_delay_sec.clamp(0.0, 120.0);
            if wait > 0.0 && self.stop.sleep_ms((wait * 1000.0) as u64).await {
                return Step::Stop;
            }
            match verify_answer(self.core, self.acc, &q.id, posted_id, self.stop).await {
                Verify::Missing => {
                    self.sh.release(self.p.limit);
                    (self.log)(&format!(
                        "   [!] Ответ #{posted_id} на сайте не появился (снесла автомодерация){}",
                        if attempt == 0 {
                            " — пробую другим текстом"
                        } else {
                            " и со второго раза"
                        }
                    ));
                    self.sh.fails.fetch_add(1, Ordering::SeqCst);
                    return Step::Vanished;
                }
                Verify::Unknown => {
                    (self.log)("   [!] Проверить ответ не вышло (сеть) — считаю отправленным");
                }
                Verify::Present => {}
            }
        }

        // 5) засчитываем
        self.sh.fails.store(0, Ordering::SeqCst);
        let n = self.sh.count.fetch_add(1, Ordering::SeqCst) + 1;
        self.p.progress.inc();
        if self.convo_mode {
            if let Some(m) = &ask_msg {
                self.core.convo.push_turn(
                    Msg::user(m.clone()),
                    Msg::assistant(if ai_raw.is_empty() { answer.clone() } else { ai_raw.clone() }),
                );
                self.compress_convo().await;
            }
        }
        (self.log)(&format!(
            "   [+] Отправлено [{n}/{}]{}",
            self.limit_label(),
            if self.repeat_per > 1 {
                format!(" (на этот вопрос {}/{})", rep + 1, self.repeat_per)
            } else {
                String::new()
            }
        ));
        Step::Posted
    }

    /// Картинка к ответу: из пула (хэш уже на CDN) либо заливкой файлов.
    async fn build_image(&self) -> Option<Value> {
        match &self.p.image {
            ImageMode::Off => None,
            ImageMode::Gif { selected } => {
                let gifs = pick_n_gifs(self.pool, selected, self.img_count as usize);
                if gifs.is_empty() {
                    (self.log)("   [!] Пул пуст — без картинки");
                    return None;
                }
                (self.log)(&format!("   [>] Картинка из пула ×{}", gifs.len()));
                gallery_from_pool(&gifs)
            }
            ImageMode::Upload { dir } => {
                let paths =
                    pick_n_images(crate::util::images_dir(&self.core.root, dir), self.img_count as usize);
                if paths.is_empty() {
                    (self.log)("   [!] Нет картинок в папке — без картинки");
                    return None;
                }
                (self.log)(&format!("   [>] Заливаю картинку ({} шт.)...", paths.len()));
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
                            (self.log)("   [x] Антибот при заливке картинки (418/429).");
                            break;
                        }
                        Err(e) => {
                            if self.stop.is_stopped() {
                                break;
                            }
                            (self.log)(&format!("   [!] Картинка не залилась: {e}"));
                        }
                    }
                }
                if uploaded.is_empty() {
                    return None;
                }
                (self.log)(&format!("   [>] Залито {} шт.", uploaded.len()));
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
                (self.log)("   [>] Контекст разговора сжат (обновил summary).");
            }
            _ => {
                self.core.convo.set_compressed(convo.summary.clone(), keep);
                (self.log)("   [>] Контекст урезан (старые сообщения отброшены).");
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

/// Ответы по диапазону номеров: от и до включительно.
///
/// Журнал уважаем как везде: перезапуск того же диапазона не наделает вторых
/// ответов под теми же вопросами.
async fn answer_range(ctx: &RunCtx<'_>, batch_per: i64, go_parallel: bool) {
    let (from, to) = (ctx.p.range_from.min(ctx.p.range_to), ctx.p.range_from.max(ctx.p.range_to));
    if from <= 0 || to <= 0 {
        (ctx.log)("[-] Не задан диапазон номеров.");
        return;
    }
    (ctx.log)(&format!("[>] Диапазон: {from}–{to} ({} шт.), текст готовыми фразами.", to - from + 1));
    (ctx.log)(&format!(
        "[>] Номера общие на прогон: каждый достаётся одному аккаунту. Ждут работы: {}",
        ctx.p.range_queue.left(from, to)
    ));

    // Один номер = одна попытка на весь прогон, поэтому взятое, но не
    // отработанное надо вернуть — иначе под ним так и не будет ответа.
    let queue = &ctx.p.range_queue;
    let give_back = |ids: &[i64]| ids.iter().for_each(|id| queue.give_back(*id));

    // Ушли ли раньше конца — по лимиту, «Стопу» или паузе. От этого зависит,
    // есть ли смысл в следующем круге.
    let mut left = false;
    loop {
        if ctx.done() {
            left = true;
            break;
        }
        // Берём пачку номеров сразу, но не больше, чем осталось места под
        // лимитом: лишнее пришлось бы возвращать.
        let room = ctx.sh.remaining(ctx.p.limit).min(if go_parallel { batch_per } else { 1 });
        if room <= 0 {
            left = true;
            break;
        }
        let mut batch: Vec<i64> = Vec::new();
        while (batch.len() as i64) < room {
            let Some(id) = queue.take(from, to) else { break };
            // Уже отвечено — в журнале аккаунта или (если включено) чужом.
            // Такой номер не возвращаем: работа по нему сделана.
            if ctx.sh.exclude.lock().contains(&format!("https://otvet.mail.ru/question/{id}")) {
                continue;
            }
            batch.push(id);
        }
        if batch.is_empty() {
            // Очередь пуста: диапазон разобран до конца, и следующий круг по
            // тем же номерам не сделает ничего.
            break;
        }

        if go_parallel && batch.len() > 1 {
            (ctx.log)(&format!("\n[>] Отвечаю на {} номер(ов) ПАРАЛЛЕЛЬНО (разом)...", batch.len()));
            let mut tasks = FuturesUnordered::new();
            for id in &batch {
                let id = *id;
                tasks.push(async move {
                    if ctx.done() {
                        queue.give_back(id);
                        return;
                    }
                    (ctx.log)(&format!("\n→ Вопрос #{id}"));
                    let q = one_range_question(id);
                    let posted = ctx.handle_one(&q).await;
                    ctx.after_one(&q, posted);
                    // Антибот убил аккаунт — номер тут ни при чём, пусть его
                    // возьмёт другой.
                    if !posted && ctx.sh.blocked() {
                        queue.give_back(id);
                    }
                });
            }
            while tasks.next().await.is_some() {}
            // Пауза действует МЕЖДУ пачками, а не между ответами.
            if !ctx.done() && ctx.pause_between().await {
                left = true;
                break;
            }
        } else {
            for (k, id) in batch.iter().enumerate() {
                let id = *id;
                if ctx.done() {
                    give_back(&batch[k..]);
                    left = true;
                    break;
                }
                (ctx.log)(&format!("\n→ Вопрос #{id}"));
                let q = one_range_question(id);
                let posted = ctx.handle_one(&q).await;
                ctx.after_one(&q, posted);
                if !posted && ctx.sh.blocked() {
                    queue.give_back(id);
                }
                if !ctx.done() && ctx.pause_between().await {
                    give_back(&batch[k + 1..]);
                    left = true;
                    break;
                }
            }
            if left {
                break;
            }
        }
    }
    if !left {
        ctx.sh.exhausted.store(true, Ordering::SeqCst);
    }
}

/// Вопрос-заглушка под номер диапазона: текста у него ещё нет и быть не может.
fn one_range_question(id: i64) -> Question {
    Question {
        id: id.to_string(),
        norm: format!("https://otvet.mail.ru/question/{id}"),
        ..Default::default()
    }
}

async fn answer_links(ctx: &RunCtx<'_>) {
    let mut targets: Vec<(String, Option<String>)> = Vec::new();
    for u in &ctx.p.links {
        match parse_answer_target(u) {
            Some(t) => targets.push(t),
            None if !u.trim().is_empty() => (ctx.log)(&format!(
                "[!] Не похоже на ссылку вопроса/ответа — пропускаю: {}",
                clip(u.trim(), 80)
            )),
            None => {}
        }
    }
    if targets.is_empty() {
        (ctx.log)("[-] Нет распознанных ссылок — нечего отвечать.");
        return;
    }
    (ctx.log)(&format!("[>] Режим по ссылкам: целей {} (по {} на каждую).", targets.len(), ctx.repeat_per));

    // Ушли ли раньше конца списка. Если прошли его целиком, следующий круг по
    // тем же ссылкам не сделает ничего: журнал отбросит их все. Без этого
    // «Ответы по ссылке» с включёнными кругами крутились впустую до «Стоп».
    let mut left = false;
    for (topic_id, reply_id) in targets {
        if ctx.done() {
            left = true;
            break;
        }
        let norm = format!("https://otvet.mail.ru/question/{topic_id}");
        if ctx.sh.exclude.lock().contains(&norm) {
            (ctx.log)(&format!("[!] Уже отвечал на {norm} — пропускаю"));
            continue;
        }
        // Текст вопроса не нужен только если и реплика из набора, и правил нет:
        // правилам как раз нужно, по чему искать слова.
        let q = if ctx.p.mode == AnswerMode::NoAi && ctx.p.keywords.is_empty() {
            Question { id: topic_id.clone(), norm: norm.clone(), ..Default::default() }
        } else {
            (ctx.log)(&format!("   [>] Читаю вопрос #{topic_id}..."));
            match fetch_question(ctx.core, ctx.acc, &topic_id, ctx.stop).await {
                Some(mut q) => {
                    q.norm = norm.clone();
                    q
                }
                None => {
                    if ctx.stop.is_stopped() {
                        left = true;
                        break;
                    }
                    (ctx.log)(&format!("   [!] Не удалось прочитать вопрос #{topic_id} — пропускаю"));
                    ctx.sh.tried.lock().insert(norm);
                    continue;
                }
            }
        };
        if !ctx.passes_keywords(&q) {
            (ctx.log)(&format!("[!] #{topic_id} — нужных слов в вопросе нет, пропускаю"));
            ctx.sh.tried.lock().insert(norm);
            continue;
        }
        let title = if q.title.is_empty() { format!("Вопрос #{topic_id}") } else { q.title.clone() };
        (ctx.log)(&format!(
            "\n→ {}{}",
            clip(&title, 70),
            reply_id.map(|r| format!(" (в ответ на #{r})")).unwrap_or_default()
        ));
        let posted = ctx.handle_one(&q).await;
        ctx.after_one(&q, posted);
        if !ctx.done() && ctx.pause_between().await {
            left = true;
            break;
        }
    }
    if !left {
        ctx.sh.exhausted.store(true, Ordering::SeqCst);
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
            "[>] Беру по {batch_per} вопрос(ов) за один проход ленты{}.",
            if go_parallel {
                " и отвечаю на них ПАРАЛЛЕЛЬНО (риск антибота!)"
            } else {
                " — успеваю при наплыве"
            }
        ));
    }
    loop {
        if ctx.stop.is_stopped() {
            (ctx.log)("\n[x] Остановлено пользователем");
            break;
        }
        if ctx.sh.blocked() {
            (ctx.log)("\n[x] Блокировка mail.ru (418/429). Останавливаю аккаунт.");
            break;
        }
        if ctx.sh.limit_hit(ctx.p.limit) || ctx.sh.too_many_fails() {
            break;
        }

        let questions = {
            let exclude = ctx.sh.exclude.lock().clone();
            let tried = ctx.sh.tried.lock().clone();
            match collect_questions(ctx.core, ctx.acc, &exclude, &tried, ctx.mine, recent, ctx.stop).await {
                Ok(q) => q,
                Err(HttpError::Aborted) => break,
                Err(e) => {
                    let wait = rand_f(f_min.max(8.0), f_max.max(8.0));
                    (ctx.log)(&format!(
                        "   [!] Сеть/прокси при загрузке ленты ({e}) — повторю через {wait:.0} сек."
                    ));
                    if ctx.stop.sleep_ms((wait * 1000.0) as u64).await {
                        break;
                    }
                    continue;
                }
            }
        };
        // При «только по триггерам» всё остальное в ленте нам неинтересно:
        // отсеиваем сразу, чтобы не тратить на них ни пачку, ни лимит.
        let questions: Vec<Question> = questions.into_iter().filter(|q| ctx.passes_keywords(q)).collect();
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

        // Пачку урезаем по ОСТАТКУ лимита. Иначе бот с лимитом 20, ответив на
        // 12, брал ещё 15 вопросов, генерировал на них текст и упирался в
        // дневной лимит сайта — платили за генерацию, получали отказы.
        let room = ctx.sh.remaining(ctx.p.limit).min(batch_per);
        if room <= 0 {
            break;
        }
        let batch: Vec<Question> = questions.into_iter().take(room as usize).collect();
        if batch_per > 1 && !go_parallel {
            (ctx.log)(&format!("\n[>] За этот проход беру {} вопрос(ов) из ленты", batch.len()));
        }

        if go_parallel {
            (ctx.log)(&format!("\n[>] Отвечаю на {} вопрос(ов) ПАРАЛЛЕЛЬНО (разом)...", batch.len()));
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
        "[>] Непрерывное обновление ленты: каждый новый вопрос — сразу в обработку (макс. {max_active} одновременно)."
    ));
    let mut queued: HashSet<String> = HashSet::new();
    let mut tasks = FuturesUnordered::new();

    loop {
        if ctx.done() && tasks.is_empty() {
            break;
        }

        // Подливаем новые вопросы, пока есть свободные слоты И место под лимитом:
        // брать вопрос, на который уже нельзя ответить, — потраченный запрос.
        let room = ctx.sh.remaining(ctx.p.limit) - tasks.len() as i64;
        if !ctx.done() && tasks.len() < max_active && room > 0 {
            let fresh = {
                let exclude = ctx.sh.exclude.lock().clone();
                let tried = ctx.sh.tried.lock().clone();
                collect_questions(ctx.core, ctx.acc, &exclude, &tried, ctx.mine, recent, ctx.stop)
                    .await
                    .unwrap_or_default()
            };
            let mut room = room;
            for q in fresh {
                if !ctx.passes_keywords(&q) {
                    continue;
                }
                if tasks.len() >= max_active || ctx.done() || room <= 0 {
                    break;
                }
                if !queued.insert(q.norm.clone()) {
                    continue;
                }
                room -= 1;
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

pub fn list_images(dir: impl AsRef<std::path::Path>) -> Vec<String> {
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

fn pick_n_images(dir: impl AsRef<std::path::Path>, n: usize) -> Vec<String> {
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

    /// Разбор списка: строки, запятые, регистр и повторы.
    #[test]
    fn keywords_are_parsed_into_a_clean_list() {
        let w = parse_keywords("VPN, впн\nобход блокировок ,, vpn\n\n");
        assert_eq!(w, vec!["vpn", "впн", "обход блокировок"], "список разобран неверно: {w:?}");
        assert!(parse_keywords("   ,\n ").is_empty(), "из пустого списка получились слова");
    }

    /// Слово ищется и в заголовке, и в теле, без оглядки на регистр. Пустой
    /// список пропускает всё — иначе бот молча перестал бы отвечать.
    #[test]
    fn keyword_matches_title_and_body() {
        let words = parse_keywords("vpn, блокировк");
        let q = |title: &str, body: &str| Question {
            title: title.into(),
            body: body.into(),
            ..Default::default()
        };

        assert!(has_keyword(&words, &q("Какой VPN выбрать?", "")), "не нашли в заголовке");
        assert!(has_keyword(&words, &q("Вопрос", "как обойти блокировки")), "не нашли в теле");
        assert!(has_keyword(&words, &q("VPN-сервис", "")), "слово внутри слова тоже считается");
        assert!(!has_keyword(&words, &q("что приготовить на ужин", "")), "подошло лишнее");
        assert!(has_keyword(&[], &q("что угодно", "")), "пустой список должен пропускать всё");
    }

    #[test]
    fn scramble_keeps_words_and_tail() {
        let src = "почему небо синее?";
        let out = scramble_question(src);
        // Хвостовая пунктуация клеится к СЛУЧАЙНОМУ слову,
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
    fn signature_is_separated_by_a_blank_line() {
        assert_eq!(with_signature("текст", "  "), "текст");
        assert_eq!(with_signature("текст", "подпись"), "текст\n\nподпись");
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

    /// Очередь номеров: каждый выдаётся один раз, возвращённый идёт первым.
    #[test]
    fn range_queue_hands_out_each_number_once() {
        let q = RangeQueue::default();
        assert_eq!(q.left(10, 12), 3);
        assert_eq!(q.take(10, 12), Some(10));
        assert_eq!(q.take(10, 12), Some(11));
        assert_eq!(q.left(10, 12), 1, "два номера уже разобраны");

        // Аккаунт не справился с 11 — вернул его в общую кучу.
        q.give_back(11);
        assert_eq!(q.left(10, 12), 2);
        assert_eq!(q.take(10, 12), Some(11), "возвращённый должен уйти первым, он ждёт дольше");

        assert_eq!(q.take(10, 12), Some(12));
        assert_eq!(q.take(10, 12), None, "за концом диапазона номеров нет");
        assert_eq!(q.left(10, 12), 0);
    }

    /// Отмеченные картинки — это «бери только их». Пустой список означает
    /// «любая из пула», а отметки на давно удалённые хэши не должны оставлять
    /// пост вовсе без картинки: тогда лучше взять любую.
    #[test]
    fn chosen_images_narrow_the_pool() {
        let pool: Vec<PoolImage> = ["a", "b", "c"]
            .iter()
            .map(|h| PoolImage { hash: (*h).into(), width: 0, height: 0, tag: String::new() })
            .collect();

        let any = pick_n_gifs(&pool, &[], 3);
        assert_eq!(any.len(), 3, "без отметок годится любая");

        let only_b = pick_n_gifs(&pool, &["b".to_string()], 3);
        assert_eq!(only_b.iter().map(|g| g.hash.as_str()).collect::<Vec<_>>(), vec!["b"]);

        let two = pick_n_gifs(&pool, &["a".to_string(), "c".to_string()], 1);
        assert_eq!(two.len(), 1, "просили одну");
        assert!(matches!(two[0].hash.as_str(), "a" | "c"), "взяли не из отмеченных: {}", two[0].hash);

        let stale = pick_n_gifs(&pool, &["нет-такого".to_string()], 1);
        assert_eq!(stale.len(), 1, "отметка на удалённую картинку оставила пост без картинки");
    }
}
