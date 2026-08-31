//! asker.rs — режим «Вопросы»: генерация и публикация своих вопросов. Порт asker.js.
//!
//! Заголовок mail.ru ограничивает 120 символами, и лишнее нужно переносить в
//! тело — иначе API отдаёт 400 и вопрос теряется. Журнал `asked_<акк>.ndjson`
//! хранит заголовки: без него режим без нейросети быстро начинает повторяться.

use crate::accounts::Account;
use crate::ai::{apply_markers, AiCfg, AiError, Msg, NO_MARKDOWN};
use crate::answerer::{list_images, ImageMode, Verify};
use crate::api;
use crate::content::{doc_with_image, gallery_from_pool, image_gallery_node};
use crate::http::{HttpError, ReqOpts};
use crate::journals::{self, AskedEntry};
use crate::uniq::{uniquify, UniqMode};
use crate::util::{clip, pick_one, rand_f64, shuffle, Log, Progress, Stop};
use crate::{Core, RunOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Готовые «живые» вопросы для режима без нейросети.
pub const NOAI_QUESTIONS: &[&str] = &[
    "как дела у всех?",
    "что делаете вечером?",
    "посоветуйте фильм на вечер",
    "какую музыку сейчас слушаете?",
    "как побороть лень?",
    "что приготовить на ужин по-быстрому?",
    "кто как просыпается по утрам?",
    "как вы отдыхаете после работы?",
    "какое хобби посоветуете?",
    "что почитать интересного?",
    "как вы относитесь к кофе по утрам?",
    "куда сходить в выходные?",
    "как перестать прокрастинировать?",
    "какой сериал затягивает с первой серии?",
    "что помогает вам уснуть?",
    "как вы боретесь со стрессом?",
    "какое блюдо у вас фирменное?",
    "кто чем занимается на досуге?",
    "как поднять себе настроение?",
    "какая ваша любимая пора года?",
    "что взять с собой в дорогу?",
    "как вы относитесь к раннему подъёму?",
    "посоветуйте игру на телефон",
    "какой напиток любите больше всего?",
    "как провести вечер без интернета?",
    "что подарить другу на день рождения?",
    "как вы относитесь к спорту по утрам?",
    "какое место мечтаете посетить?",
    "что делает день удачным?",
    "как вы выбираете книгу для чтения?",
];

/// Предел заголовка на стороне mail.ru.
const TITLE_LIMIT: usize = 120;

/// Сколько отказов подряд считать «дальше бесполезно».
const MAX_FAILS: i32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AskMode {
    Ai,
    NoAi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskParams {
    pub mode: AskMode,
    /// 0 = без лимита.
    pub limit: i64,
    pub delay_min: f64,
    pub delay_max: f64,
    pub ai: AiCfg,
    pub style: String,
    pub custom_prompt: String,
    pub mention: String,
    /// Свои темы, по одной на строку. Для каждого вопроса берётся случайная;
    /// пусто — случайные темы из styles.json.
    pub topics: Vec<String>,
    pub noai_questions: Vec<String>,
    /// Уникализация текста вопроса.
    pub uniq: UniqMode,
    pub uniq_latin: bool,
    pub image: ImageMode,
    pub image_count: i64,
    /// Проверять, что вопрос остался на сайте, а не снесён автомодерацией.
    pub verify_posted: bool,
    /// Сколько ждать перед проверкой: сразу после публикации вопрос ещё может
    /// не отдаваться по API.
    pub verify_delay_sec: f64,
    pub check_auth: bool,
    /// Живой счётчик для интерфейса.
    #[serde(skip)]
    pub progress: Progress,
}

impl Default for AskParams {
    fn default() -> Self {
        Self {
            mode: AskMode::Ai,
            limit: 5,
            delay_min: 30.0,
            delay_max: 90.0,
            ai: AiCfg { temperature: 0.9, ..AiCfg::preset() },
            style: "Обычный чел".into(),
            custom_prompt: String::new(),
            mention: String::new(),
            topics: vec![],
            noai_questions: vec![],
            uniq: UniqMode::Off,
            uniq_latin: false,
            image: ImageMode::Off,
            image_count: 1,
            verify_posted: true,
            verify_delay_sec: 6.0,
            check_auth: true,
            progress: Progress::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GeneratedQuestion {
    pub title: String,
    pub body: String,
}

/// Разбор ответа модели: первая строка — заголовок, остальное — тело.
/// Слишком длинный заголовок режем по границе слова, хвост уезжает в тело.
pub fn split_question(text: &str) -> GeneratedQuestion {
    let parts: Vec<&str> = text.trim().split("\n\n").collect();
    let mut title = parts.first().unwrap_or(&"").trim().to_string();
    let mut body = if parts.len() > 1 { parts[1..].join("\n\n").trim().to_string() } else { String::new() };

    if title.chars().count() > TITLE_LIMIT {
        let boundary = char_boundary(&title, TITLE_LIMIT);
        let head = &title[..boundary];
        let cut = match head.rfind(' ') {
            Some(i) if head[..i].chars().count() >= 60 => i,
            _ => boundary,
        };
        let tail = title[cut..].trim().to_string();
        let new_title = title[..cut].trim_end_matches([' ', ',', '.', ';', ':', '—', '-']).to_string();
        body = if body.is_empty() { tail } else { format!("{tail}\n{body}") }.trim().to_string();
        title = new_title;
    }
    GeneratedQuestion { title, body }
}

fn char_boundary(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map(|(i, _)| i).unwrap_or(s.len())
}

/// Генерация вопроса нейросетью (у аскера свой промпт, мимо ответчика).
pub async fn generate_question(
    core: &Core,
    ai: &AiCfg,
    topic: &str,
    question_prompt: &str,
    mention: &str,
    log: &Log,
    stop: &Stop,
) -> Result<GeneratedQuestion, AiError> {
    let now = chrono::Local::now();
    let year_info = format!("Сейчас {} год, месяц {}.", now.format("%Y"), now.format("%-m"));
    let mut user_msg = if topic.is_empty() {
        format!("{year_info}\nПридумай любой интересный вопрос")
    } else {
        format!("{year_info}\nПридумай вопрос на тему: {topic}")
    };
    user_msg.push_str(
        "\n\nВАЖНО: заголовок (первая строка) — НЕ ДЛИННЕЕ 120 символов. Если мысль длиннее — переноси её в тело вопроса после пустой строки.",
    );

    let mut sys = if question_prompt.trim().is_empty() {
        "Ты генерируешь вопросы для сайта «Ответы Mail.ru» от лица обычного пользователя. Коротко и по-простому.".to_string()
    } else {
        question_prompt.to_string()
    };
    if !mention.trim().is_empty() {
        sys.push_str(&format!(
            "\n\nОБЯЗАТЕЛЬНО: естественно упомяни «{}» в вопросе (не в лоб, а к месту).",
            mention.trim()
        ));
    }
    sys.push_str(NO_MARKDOWN);
    // Маркеры подставляем здесь: у аскера свой путь генерации.
    let sys = apply_markers(&sys);

    let msgs = vec![Msg::system(sys), Msg::user(user_msg)];
    let text = core.ai.generate(ai, &msgs, log, stop).await?;
    Ok(split_question(&text))
}

/// Публикация вопроса. `author_id` обязателен — без него API отвергает запрос.
pub async fn post_question(
    core: &Core,
    acc: &Account,
    title: &str,
    body: &str,
    image: Option<&Value>,
    log: &Log,
    stop: &Stop,
) -> Result<PostResult, HttpError> {
    let Some(user_id) = acc.user_id else {
        log("   [-] Неизвестен author_id (userId) аккаунта — проверь аккаунт, чтобы задавать вопросы.");
        return Ok(PostResult::NoUser);
    };
    let body_json = json!({
        "content": doc_with_image(body, image),
        "tags": [{ "id": 1883, "name": "other", "title": "другое" }],
        "version": 0,
        "visible_to": 0,
        "title": title,
        "topic_type": 2,
        "author_id": user_id,
        "spaces": [{ "id": 1, "is_prime": true }],
    });
    let r = core
        .http
        .request(
            acc,
            "/api/topic/question",
            // Без ретрая — иначе при потерянном ответе получим два одинаковых
            // вопроса подряд от одного аккаунта.
            ReqOpts::post(body_json).referer("https://otvet.mail.ru/ask").no_retry(),
            stop,
        )
        .await?;
    if r.blocked {
        return Ok(PostResult::Blocked);
    }
    if let Some(id) = r.result().and_then(|res| res.get("id")).and_then(|v| v.as_i64()) {
        return Ok(PostResult::Ok(id));
    }
    // 4xx (кроме антибота) — отказ по содержимому: вопроса не появилось.
    if (400..500).contains(&r.status) {
        return Ok(PostResult::Rejected(crate::answerer::refusal_reason(&r)));
    }
    if !r.ok {
        log(&format!("   [!] Вопрос не прошёл: HTTP {} {}", r.status, r.snippet(120)));
    }
    // Настоящий антибот — только 418/429. Прочие коды (в т.ч. 400 на длинный
    // заголовок) — это один потерянный вопрос, аккаунт не стопаем.
    Ok(PostResult::Failed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostResult {
    Ok(i64),
    Blocked,
    /// Сайт не принял ИМЕННО ЭТОТ текст (4xx, но не антибот): вопрос не создан,
    /// поэтому можно смело сочинять другой.
    Rejected(String),
    /// Прочая неудача: сеть, 5xx, непонятный ответ.
    Failed,
    NoUser,
}

/// Виден ли опубликованный вопрос на сайте.
///
/// Строго наоборот к проверке ответов: там «не нашли среди ответов» — повод
/// заподозрить снос, здесь же снос признаётся ТОЛЬКО по прямому «нет такого
/// вопроса» (404/410). Всё остальное — сеть, антибот, странный ответ сервера —
/// это `Unknown`, и вопрос считается опубликованным. Ошибка в другую сторону
/// стоит дорого: бот задаст второй такой же вопрос от того же аккаунта.
pub async fn verify_question(core: &Core, acc: &Account, topic_id: i64, stop: &Stop) -> Verify {
    let Ok(r) =
        core.http.request(acc, &format!("/api/topic/question/{topic_id}"), ReqOpts::get(), stop).await
    else {
        return Verify::Unknown;
    };
    if r.blocked {
        return Verify::Unknown;
    }
    if r.status == 404 || r.status == 410 {
        return Verify::Missing;
    }
    if !r.ok {
        return Verify::Unknown;
    }
    match r.result().and_then(|res| res.get("id")).and_then(|v| v.as_i64()) {
        Some(id) if id == topic_id => Verify::Present,
        // HTTP 200 без вопроса в теле — так mail.ru отвечает на удалённый.
        None => Verify::Missing,
        _ => Verify::Unknown,
    }
}

/// Пережил ли только что опубликованный вопрос автомодерацию.
///
/// Проверка выключена — считаем, что пережил: не проверяли, значит и повода
/// сомневаться нет.
async fn survived(core: &Core, acc: &Account, p: &AskParams, id: i64, log: &Log, stop: &Stop) -> bool {
    if !p.verify_posted {
        return true;
    }
    let wait = (p.verify_delay_sec.max(0.0) * 1000.0) as u64;
    if wait > 0 && stop.sleep_ms(wait).await {
        return true; // остановили — не наше дело считать это сносом
    }
    match verify_question(core, acc, id, stop).await {
        Verify::Present => true,
        Verify::Unknown => {
            log("   [!] Не смог проверить вопрос на сайте — считаю опубликованным.");
            true
        }
        Verify::Missing => {
            log("   [-] Вопроса на сайте нет — снесла автомодерация. Не засчитываю, беру другой текст.");
            false
        }
    }
}

pub async fn run_asker(core: &Core, acc: &Account, p: &AskParams, log: &Log, stop: &Stop) -> RunOutcome {
    let mut out = RunOutcome::default();
    let img_count = p.image_count.clamp(1, crate::content::MAX_GALLERY as i64);
    let d_min = p.delay_min.max(0.0);
    let d_max = p.delay_max.max(d_min);

    let styles = journals::Styles::load(&core.root);
    let topics: Vec<String> =
        if styles.topics.is_empty() { vec!["жизнь".into()] } else { styles.topics.clone() };
    let custom = p.custom_prompt.trim();
    let question_prompt =
        if custom.is_empty() { styles.prompt(&p.style, "question") } else { custom.to_string() };

    let mut ai = p.ai.clone();
    // Ноль — это ноль (см. комментарий в answerer.rs); «по умолчанию» = отрицательное.
    if ai.temperature < 0.0 {
        ai.temperature = 0.9;
    }

    if p.mode == AskMode::Ai {
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
            "[>] Стиль вопросов: {} | {} | токенов: {} | таймаут: {}с | повторов при ошибке: {}",
            if custom.is_empty() { p.style.as_str() } else { "свой промпт" },
            ai.temperature,
            ai.max_tokens,
            ai.timeout_sec,
            ai.retries
        ));
        match p.topics.len() {
            0 => log(&format!("[>] Темы — случайные из базы ({})", topics.len())),
            1 => log(&format!("[>] Тема: {}", p.topics[0])),
            n => {
                log(&format!("[>] Свои темы ({n}), для каждого вопроса случайная: {}", p.topics.join(" · ")))
            }
        }
    } else {
        log("[>] Режим без AI — готовые вопросы");
    }
    if p.uniq != UniqMode::Off {
        log(&format!("[>] Уникализация текста: {}", p.uniq.ru()));
    }
    if p.verify_posted {
        log(&format!("[>] Проверяю через {:.0} с, что вопрос остался на сайте", p.verify_delay_sec));
    }
    match &p.image {
        ImageMode::Gif { .. } => {
            let n = journals::load_gif_pool(&core.root).len();
            log(&format!("[>] Картинка из пула в каждый вопрос (в пуле: {n})"));
        }
        ImageMode::Upload { dir } => {
            let n = list_images(crate::util::images_dir(&core.root, dir)).len();
            log(&format!("[>] Картинка из {dir} в каждый вопрос (файлов: {n})"));
        }
        ImageMode::Off => {}
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
            log("[x] НЕ авторизован");
            log("Пропускаю аккаунт — не залогинен.");
            out.skipped = true;
            return out;
        } else {
            log("[!] Не удалось проверить авторизацию (ошибка/сеть) — продолжаю, статус не трогаю.");
        }
    }

    let asked_titles = journals::load_asked_titles(&core.root, &acc.name);
    // Пополняется по ходу прогона: журнал читается один раз, и без этого
    // «беру то, чего ещё не задавал» внутри одного запуска не работало —
    // из тридцати готовых вопросов десять публикаций почти наверняка давали
    // повтор, ровно то, ради чего журнал и заведён.
    let mut asked_norm: Vec<String> = asked_titles.iter().map(|t| norm_title(t)).collect();
    let noai_list: Vec<String> = if p.noai_questions.is_empty() {
        NOAI_QUESTIONS.iter().map(|s| s.to_string()).collect()
    } else {
        p.noai_questions.clone()
    };

    let limit_label = if p.limit > 0 { p.limit.to_string() } else { "∞".to_string() };
    let mut fails = 0;

    while p.limit <= 0 || out.done < p.limit {
        if stop.is_stopped() {
            log("\n[x] Остановлено пользователем");
            break;
        }
        if out.blocked {
            log("\n[x] Блокировка mail.ru (418/429). Останавливаю аккаунт.");
            break;
        }

        // 1) текст вопроса
        let mut q = if p.mode == AskMode::Ai {
            // Своих тем может быть несколько — на каждый вопрос берём случайную,
            // иначе двадцать вопросов подряд выйдут об одном и том же.
            let own = !p.topics.is_empty();
            let topic = if own {
                pick_one(&p.topics).cloned().unwrap_or_default()
            } else {
                pick_one(&topics).cloned().unwrap_or_else(|| "жизнь".into())
            };
            log(&format!("\n[>] Генерирую вопрос (тема: {topic})..."));
            // «жизнь» — это отсутствие темы, а не тема.
            let topic_arg = if !own && topic == "жизнь" { String::new() } else { topic };
            match generate_question(core, &ai, &topic_arg, &question_prompt, &p.mention, log, stop).await {
                Ok(q) if !q.title.is_empty() => q,
                Ok(_) => {
                    log("   [-] Пустой вопрос от нейросети");
                    if stop.sleep_ms(4000).await {
                        break;
                    }
                    continue;
                }
                Err(AiError::Aborted) => break,
                Err(e) => {
                    log(&format!("   [-] Нейросеть не ответила: {e}"));
                    if stop.sleep_ms(5000).await {
                        break;
                    }
                    continue;
                }
            }
        } else {
            // Готовые вопросы: сначала те, что этот аккаунт ещё не задавал.
            let pool: Vec<&String> =
                noai_list.iter().filter(|s| !asked_norm.contains(&norm_title(s))).collect();
            let src: Vec<&String> = if pool.is_empty() { noai_list.iter().collect() } else { pool };
            let Some(pick) = pick_one(&src) else { break };
            log("\n[>] Беру готовый вопрос...");
            match pick.split_once('|') {
                Some((t, b)) => GeneratedQuestion { title: t.trim().to_string(), body: b.trim().to_string() },
                None => GeneratedQuestion { title: pick.trim().to_string(), body: String::new() },
            }
        };

        if q.title.is_empty() {
            log("   [!] Пустой заголовок — пропускаю");
            if stop.sleep_ms(1500).await {
                break;
            }
            continue;
        }

        // 2) уникализация. Правим заголовок, только если он остался в
        // пределах лимита сайта, иначе — тело: 400 из-за длинного заголовка
        // стоит целого вопроса.
        if p.uniq != UniqMode::Off {
            let t = uniquify(&q.title, p.uniq, p.uniq_latin);
            if t.chars().count() <= TITLE_LIMIT {
                q.title = t;
            } else if !q.body.is_empty() {
                q.body = uniquify(&q.body, p.uniq, p.uniq_latin);
            }
        }

        log(&format!("   → {}", clip(&q.title, 70)));
        if !q.body.is_empty() {
            log(&format!("   [>] Доп: {}", clip(&q.body, 60)));
        }

        // 3) картинка
        let image = build_image(core, acc, p, img_count, log, stop, &mut out).await;
        if stop.is_stopped() {
            break;
        }
        if out.blocked {
            continue;
        }

        // 4) публикация
        match post_question(core, acc, &q.title, &q.body, image.as_ref(), log, stop).await {
            Err(HttpError::Aborted) => break,
            Err(e) => {
                log(&format!("   [!] Сеть/прокси при отправке ({e}) — пробую дальше."));
                if stop.sleep_ms(5000).await {
                    break;
                }
            }
            Ok(PostResult::NoUser) => break,
            Ok(PostResult::Blocked) => {
                out.blocked = true;
                log("   [x] Антибот при публикации (418/429).");
                break;
            }
            Ok(PostResult::Rejected(why)) => {
                // Текст не понравился сайту — берём другой. Этот запоминаем,
                // чтобы готовый список не подсунул его же следующим кругом.
                asked_norm.push(norm_title(&q.title));
                fails += 1;
                if fails >= MAX_FAILS {
                    log(&format!(
                        "[x] Подряд {MAX_FAILS} отказов при публикации — останавливаю аккаунт (похоже, сайт больше не принимает вопросы)."
                    ));
                    break;
                }
                log(&format!("   [!] Сайт не принял вопрос ({why}) — беру другой текст."));
                let d = d_min + rand_f64() * (d_max - d_min);
                if d > 0.0 && stop.sleep_ms((d * 1000.0) as u64).await {
                    break;
                }
                continue;
            }
            Ok(PostResult::Failed) => {
                // Сайт может отказывать подряд — например, исчерпан дневной
                // лимит вопросов. Без счётчика бот крутился бы вечно, каждый раз
                // заново оплачивая генерацию у нейросети.
                fails += 1;
                if fails >= MAX_FAILS {
                    log(&format!(
                        "[x] Подряд {MAX_FAILS} отказов при публикации — останавливаю аккаунт (похоже, сайт больше не принимает вопросы)."
                    ));
                    break;
                }
                if stop.sleep_ms(4000).await {
                    break;
                }
            }
            Ok(PostResult::Ok(id)) => {
                // Этот заголовок больше не берём в любом случае: если вопрос
                // снесли, повторять ровно его — снова кормить автомодерацию.
                asked_norm.push(norm_title(&q.title));
                if !survived(core, acc, p, id, log, stop).await {
                    // Не засчитываем и в журнал не пишем: вопроса на сайте нет.
                    // Считаем это отказом сайта — иначе бот будет бесконечно
                    // генерировать вопросы в пустоту, каждый раз платя нейросети.
                    fails += 1;
                    if fails >= MAX_FAILS {
                        log(&format!(
                            "[x] Подряд {MAX_FAILS} вопросов не удержались — останавливаю аккаунт."
                        ));
                        break;
                    }
                    // Пауза как между вопросами: снесённый вопрос — не повод
                    // долбить сайт чаще обычного.
                    let d = d_min + rand_f64() * (d_max - d_min);
                    if d > 0.0 && stop.sleep_ms((d * 1000.0) as u64).await {
                        break;
                    }
                    continue;
                }
                fails = 0;
                out.done += 1;
                p.progress.inc();
                journals::append_asked(
                    &core.root,
                    &acc.name,
                    &AskedEntry {
                        title: q.title.clone(),
                        body: q.body.clone(),
                        url: format!("https://otvet.mail.ru/question/{id}"),
                        ts: journals::now_ms(),
                    },
                );
                log(&format!("   [+] Опубликовано [{}/{}] #{id}", out.done, limit_label));
            }
        }

        if (p.limit <= 0 || out.done < p.limit) && !stop.is_stopped() && !out.blocked {
            let d = d_min + rand_f64() * (d_max - d_min);
            if d > 0.0 {
                log(&format!("   [>] Пауза {d:.1} сек..."));
            }
            if stop.sleep_ms((d * 1000.0) as u64).await {
                break;
            }
        }
    }

    let line = "=".repeat(50);
    log(&format!("\n{line}\n[+] Готово! Вопросов задано: {}\n{line}", out.done));
    out
}

/// Ключ дедупа: до разделителя `|`, без регистра и без хвостового «#123456».
///
/// Тег уникальности дописывается уже после выбора вопроса, поэтому без его
/// отсечения журнал сравнивался бы с тегом, а список готовых — без, и фильтр
/// «чего ещё не задавал» ничего бы не отсеивал.
fn norm_title(s: &str) -> String {
    let head = s.split('|').next().unwrap_or("").trim();
    let head = match head.rsplit_once('#') {
        Some((base, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => base.trim_end(),
        _ => head,
    };
    head.to_lowercase()
}

async fn build_image(
    core: &Core,
    acc: &Account,
    p: &AskParams,
    img_count: i64,
    log: &Log,
    stop: &Stop,
    out: &mut RunOutcome,
) -> Option<Value> {
    match &p.image {
        ImageMode::Off => None,
        ImageMode::Gif { selected } => {
            let pool = journals::load_gif_pool(&core.root);
            let mut src: Vec<_> = if selected.is_empty() {
                pool.clone()
            } else {
                pool.iter().filter(|g| selected.contains(&g.hash)).cloned().collect()
            };
            if src.is_empty() {
                src = pool;
            }
            if src.is_empty() {
                log("   [!] Пул пуст — без картинки");
                return None;
            }
            shuffle(&mut src);
            src.truncate(img_count as usize);
            log(&format!("   [>] Картинка из пула ×{}", src.len()));
            gallery_from_pool(&src)
        }
        ImageMode::Upload { dir } => {
            let mut paths = list_images(crate::util::images_dir(&core.root, dir));
            if paths.is_empty() {
                log("   [!] Нет картинок в папке — без картинки");
                return None;
            }
            shuffle(&mut paths);
            paths.truncate(img_count as usize);
            log(&format!("   [>] Заливаю картинку ({} шт.)...", paths.len()));
            let mut uploaded: Vec<(String, i64, i64)> = Vec::new();
            for path in paths {
                match core.http.upload_picture(acc, std::path::Path::new(&path), stop).await {
                    Ok(up) => uploaded.push((up.url, up.width, up.height)),
                    Err(e) if e == "blocked" => {
                        out.blocked = true;
                        log("   [x] Антибот при заливке картинки (418/429).");
                        break;
                    }
                    Err(e) => {
                        if stop.is_stopped() {
                            break;
                        }
                        log(&format!("   [!] Картинка не залилась: {e}"));
                    }
                }
            }
            if uploaded.is_empty() {
                return None;
            }
            log(&format!("   [>] Залито {} шт.", uploaded.len()));
            image_gallery_node(&uploaded)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_title_and_body() {
        let q = split_question("Заголовок\n\nТело вопроса\nвторая строка");
        assert_eq!(q.title, "Заголовок");
        assert_eq!(q.body, "Тело вопроса\nвторая строка");
    }

    #[test]
    fn long_title_moves_tail_to_body() {
        let long = "а".repeat(80) + " " + &"б".repeat(80);
        let q = split_question(&long);
        assert!(q.title.chars().count() <= TITLE_LIMIT, "заголовок {} символов", q.title.chars().count());
        assert!(!q.body.is_empty());
        assert!(q.body.contains('б'));
    }

    /// Ключ дедупа не должен зависеть от тега уникальности и от тела вопроса:
    /// иначе «чего ещё не задавал» сравнивает разные вещи и не отсеивает ничего.
    #[test]
    fn dedup_key_ignores_tag_and_body() {
        assert_eq!(norm_title("Как дела? #123456"), "как дела?");
        assert_eq!(norm_title("Как дела? | и тело"), "как дела?");
        assert_eq!(norm_title("Как дела?"), "как дела?");
        // Решётка в самом тексте — не тег.
        assert_eq!(norm_title("что за #хештег"), "что за #хештег");
    }

    #[test]
    fn noai_pipe_format() {
        let s = "Заголовок | и тело";
        let (t, b) = s.split_once('|').unwrap();
        assert_eq!(t.trim(), "Заголовок");
        assert_eq!(b.trim(), "и тело");
    }
}
