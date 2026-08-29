//! Интеграционные тесты против локального сервера-заглушки.
//!
//! Здесь проверяются не отдельные функции, а БОЕВЫЕ ЦИКЛЫ: голосование по ленте
//! профиля, реакция на антибот, ротация кук, ретраи. Именно в них живут ошибки,
//! которые дорого стоят на настоящем сайте (лишний голос снимает предыдущий,
//! бот долбится в 418, аккаунт теряет сессию).
//!
//! Адрес сайта переопределяется переменной `OTVET_BASE`, поэтому тот же код,
//! что ходит в mail.ru, ходит и сюда.

use otvet_core::accounts::Account;
use otvet_core::util::{no_log, Stop};
use otvet_core::votes::{self, Vote};
use otvet_core::Core;
use otvet_core::{answerer, asker, replier};
use parking_lot::Mutex;
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ─── Заглушка сайта ─────────────────────────────────────────────────────────

pub struct Req {
    pub method: String,
    /// Путь вместе с query.
    pub path: String,
    pub cookie: String,
    pub body: String,
}

pub struct Res {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

impl Res {
    fn json(body: impl Into<String>) -> Self {
        Self { status: 200, body: body.into(), headers: vec![] }
    }
    fn status(code: u16) -> Self {
        Self { status: code, body: String::new(), headers: vec![] }
    }
    fn with_header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
}

type Handler = Box<dyn Fn(&Req) -> Option<Res> + Send + Sync>;

#[derive(Default)]
struct Routes {
    handlers: Vec<Handler>,
}

pub struct Mock {
    pub base: String,
    routes: Arc<Mutex<Routes>>,
    hits: Arc<Mutex<Vec<String>>>,
    /// Тела запросов: (путь, тело). По ним проверяются контракты API —
    /// mail.ru разбирает поля строго по типам.
    bodies: Arc<Mutex<Vec<(String, String)>>>,
}

impl Mock {
    /// Добавить обработчик. Обходятся в порядке добавления, первый ответивший
    /// `Some` — выигрывает.
    pub fn route(&self, f: impl Fn(&Req) -> Option<Res> + Send + Sync + 'static) {
        self.routes.lock().handlers.push(Box::new(f));
    }

    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().clone()
    }

    pub fn hits_matching(&self, needle: &str) -> Vec<String> {
        self.hits().into_iter().filter(|h| h.contains(needle)).collect()
    }

    /// Тело последнего запроса по пути, содержащему `needle`.
    pub fn last_body(&self, needle: &str) -> Option<serde_json::Value> {
        self.bodies
            .lock()
            .iter()
            .rev()
            .find(|(p, _)| p.contains(needle))
            .and_then(|(_, b)| serde_json::from_str(b).ok())
    }

    fn reset(&self) {
        self.routes.lock().handlers.clear();
        self.hits.lock().clear();
        self.bodies.lock().clear();
    }
}

/// Сервер один на все тесты, поэтому маршруты и журнал запросов — общий ресурс.
/// Берём его монопольно: иначе соседний тест отвечает на твой запрос, и падение
/// выглядит как баг в боте, хотя бот ни при чём.
pub async fn exclusive() -> (&'static Mock, tokio::sync::MutexGuard<'static, ()>) {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let guard = LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    let m = mock();
    m.reset();
    (m, guard)
}

/// Один сервер на весь тестовый бинарник: `OTVET_BASE` читается один раз.
pub fn mock() -> &'static Mock {
    static M: OnceLock<Mock> = OnceLock::new();
    M.get_or_init(|| {
        let routes: Arc<Mutex<Routes>> = Arc::new(Mutex::new(Routes::default()));
        let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let r2 = routes.clone();
        let h2 = hits.clone();
        let b2 = bodies.clone();
        // Отдельный поток со своим рантаймом: тесты сами по себе асинхронные,
        // а сервер должен жить дольше любого из них.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap().to_string()).unwrap();
                loop {
                    let Ok((sock, _)) = listener.accept().await else { break };
                    let routes = r2.clone();
                    let hits = h2.clone();
                    let bodies = b2.clone();
                    tokio::spawn(async move {
                        let _ = serve(sock, routes, hits, bodies).await;
                    });
                }
            });
        });
        let addr = rx.recv().unwrap();
        let base = format!("http://{addr}");
        std::env::set_var("OTVET_BASE", &base);
        Mock { base, routes, hits, bodies }
    })
}

async fn serve(
    mut sock: tokio::net::TcpStream,
    routes: Arc<Mutex<Routes>>,
    hits: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<Vec<(String, String)>>>,
) -> std::io::Result<()> {
    loop {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        // Заголовки
        let head_end = loop {
            let n = sock.read(&mut tmp).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.lines();
        let first = lines.next().unwrap_or("").to_string();
        let mut parts = first.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("/").to_string();

        let mut cookie = String::new();
        let mut content_length = 0usize;
        for l in head.lines().skip(1) {
            let low = l.to_ascii_lowercase();
            if let Some(v) = low.strip_prefix("cookie:") {
                cookie = v.trim().to_string();
            }
            if let Some(v) = low.strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        // Тело дочитываем целиком: и чтобы клиент не подвис на следующем
        // запросе, и чтобы проверять контракты (типы полей у mail.ru строгие).
        let mut body_bytes: Vec<u8> = buf[head_end..].to_vec();
        while body_bytes.len() < content_length {
            let n = sock.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body_bytes.extend_from_slice(&tmp[..n]);
        }
        let body = String::from_utf8_lossy(&body_bytes).to_string();

        hits.lock().push(format!("{method} {path}"));
        if !body.is_empty() {
            bodies.lock().push((path.clone(), body.clone()));
        }
        let req = Req { method, path, cookie, body };
        let res = {
            let routes = routes.lock();
            routes.handlers.iter().find_map(|h| h(&req))
        }
        .unwrap_or_else(|| Res::status(404));

        // 599 — договорённость тестов: «ответ потерялся», рвём соединение.
        // Так выглядит дохлый прокси, который принял запрос и умолк.
        if res.status == 599 {
            return Ok(());
        }

        let mut head = format!(
            "HTTP/1.1 {} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
            res.status,
            res.body.len()
        );
        for (k, v) in &res.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        sock.write_all(head.as_bytes()).await?;
        sock.write_all(res.body.as_bytes()).await?;
        sock.flush().await?;
    }
}

// ─── Обвязка ────────────────────────────────────────────────────────────────

fn temp_core(name: &str) -> (Arc<Core>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("otvetware-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    (Core::open(&dir), dir)
}

fn account(name: &str) -> Account {
    let mut a = Account::new(name);
    a.cookies = Some("Mpop=test; oid=1".into());
    a.user_id = Some(1000);
    a
}

/// Лента профиля: страница из 20 элементов с ИНКЛЮЗИВНЫМ курсором, как у
/// mail.ru (последний элемент страницы повторяется первым на следующей).
fn profile_feed(ids: &[i64], pos: i64) -> String {
    let start = if pos == 0 { 0 } else { ids.iter().position(|x| *x == pos).unwrap_or(ids.len()) };
    let page: Vec<String> = ids[start..].iter().take(3).map(|id| format!(r#"{{"id":{id}}}"#)).collect();
    format!(r#"{{"result":{{"feed":[{}]}}}}"#, page.join(","))
}

// ─── Тесты ──────────────────────────────────────────────────────────────────

/// Главный тест голосования: инклюзивный курсор не должен приводить к
/// повторному голосу (он же — отмена собственного голоса).
#[tokio::test]
async fn votes_never_hit_the_same_post_twice() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("votes");
    let acc = account("voter");

    m.route(|r| {
        if r.path.starts_with("/api/auth/users/") {
            return Some(Res::json(r#"{"id":555,"username":"victim"}"#));
        }
        None
    });
    m.route(|r| {
        if !r.path.starts_with("/api/topic/profile/555/topics") {
            return None;
        }
        let pos: i64 = r
            .path
            .split("pos=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Some(Res::json(profile_feed(&[10, 11, 12, 13, 14], pos)))
    });
    m.route(|r| {
        if r.method == "POST" && r.path.starts_with("/api/topic/topics/") {
            return Some(Res::json(r#"{"result":{"user_reaction":1}}"#));
        }
        None
    });

    let mut blocked = false;
    let voted = votes::vote_on_profile(
        &core,
        &acc,
        &format!("{}/profile/id555", m.base),
        Vote::Plus,
        0,
        0.0,
        &no_log(),
        &Stop::new(),
        &mut blocked,
    )
    .await;

    assert_eq!(voted, 5, "должны проголосовать за каждый пост ровно один раз");
    let mut votes_sent: Vec<String> = m
        .hits_matching("POST /api/topic/topics/")
        .into_iter()
        .map(|h| h.trim_start_matches("POST /api/topic/topics/").to_string())
        .collect();
    votes_sent.sort();
    votes_sent.dedup();
    assert_eq!(votes_sent.len(), 5, "повторный голос по тому же посту снял бы предыдущий");
    assert!(!blocked);
}

/// 418 — это антибот: аккаунт обязан остановиться, а не долбиться дальше.
#[tokio::test]
async fn antibot_stops_the_account() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("blocked");
    let acc = account("blocked-acc");

    m.route(|r| {
        if r.method == "POST" && r.path.starts_with("/api/topic/topics/900") {
            return Some(Res::status(418));
        }
        None
    });

    let mut blocked = false;
    let voted = votes::vote_on_single(
        &core,
        &acc,
        &format!("{}/question/900", m.base),
        Vote::Plus,
        0.0,
        &no_log(),
        &Stop::new(),
        &mut blocked,
    )
    .await;
    assert_eq!(voted, 0);
    assert!(blocked, "418 обязан поднимать флаг блокировки");
}

/// Сервер молча не зарегистрировал голос — считать это успехом нельзя.
#[tokio::test]
async fn silently_rejected_vote_is_not_counted() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("noreg");
    let acc = account("noreg-acc");

    m.route(|r| {
        if r.method == "POST" && r.path.starts_with("/api/topic/topics/901") {
            // user_reaction не совпал с запрошенным — голос не прошёл
            return Some(Res::json(r#"{"result":{"user_reaction":0}}"#));
        }
        None
    });

    let mut blocked = false;
    let voted = votes::vote_on_single(
        &core,
        &acc,
        &format!("{}/question/901", m.base),
        Vote::Plus,
        0.0,
        &no_log(),
        &Stop::new(),
        &mut blocked,
    )
    .await;
    assert_eq!(voted, 0, "молча отклонённый голос не должен считаться поставленным");
}

/// Set-Cookie от сайта обязан попасть и в аккаунт, и в файл: иначе сессия
/// со временем протухает, хотя сайт её продлевал.
#[tokio::test]
async fn session_cookies_are_rotated_and_saved() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("cookies");
    let mut acc = account("cookie-acc");
    acc.name = "cookie-acc".into();
    core.accounts.add(acc.clone()).unwrap();
    let acc = core.accounts.get("cookie-acc").unwrap();

    m.route(|r| {
        if r.path == "/api/karma/score/1000" {
            return Some(
                Res::json(
                    r#"{"result":{"total_score":7,"score":{"history":1,"knowledge":2,"discussion":4}}}"#,
                )
                .with_header("set-cookie", "Mpop=REFRESHED; Path=/"),
            );
        }
        None
    });

    let karma = otvet_core::api::fetch_karma(&core, &acc, 1000, &Stop::new()).await;
    assert_eq!(karma.map(|k| k.total), Some(7));

    let live = acc.cookie_header().unwrap_or_default();
    assert!(live.contains("Mpop=REFRESHED"), "куки в памяти не обновились: {live}");
    let saved = core.accounts.get("cookie-acc").unwrap().cookies.unwrap_or_default();
    assert!(saved.contains("Mpop=REFRESHED"), "куки не сохранились в accounts.json: {saved}");
    assert!(saved.contains("oid=1"), "остальные куки не должны теряться: {saved}");
}

/// 429 на GET — не приговор: один ретрай, и если сайт отпустил, работаем дальше.
#[tokio::test]
async fn get_retries_after_429() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("retry");
    let acc = account("retry-acc");

    static N: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    m.route(|r| {
        if r.path != "/api/auth/users/retrytest" {
            return None;
        }
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 {
            Some(Res::status(429))
        } else {
            Some(Res::json(r#"{"id":777,"username":"retrytest"}"#))
        }
    });

    let who =
        votes::resolve_profile(&core, &acc, &format!("{}/profile/retrytest", m.base), &Stop::new()).await;
    assert_eq!(who.map(|w| w.id), Some(777), "после 429 должен пройти повторный запрос");
    assert_eq!(N.load(std::sync::atomic::Ordering::SeqCst), 2, "ровно один ретрай");
}

/// Лента ответов: короткие заголовки и уже отвеченное не попадают в работу.
#[tokio::test]
async fn feed_skips_answered_and_too_short() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("feed");
    let acc = account("feed-acc");

    m.route(|r| {
        if !r.path.starts_with("/api/topic/feed") {
            return None;
        }
        Some(Res::json(
            r#"{"result":{"feed":[
                {"id":1,"title":"Нормальный вопрос про жизнь","created_at":"2026-08-01T10:00:00Z"},
                {"id":2,"title":"аб","created_at":"2026-08-01T10:00:00Z"},
                {"id":3,"title":"Уже отвечали на этот вопрос","created_at":"2026-08-01T10:00:00Z"}
            ]}}"#,
        ))
    });

    let mut answered = std::collections::HashSet::new();
    answered.insert("https://otvet.mail.ru/question/3".to_string());
    let qs = otvet_core::answerer::collect_questions(
        &core,
        &acc,
        &answered,
        &std::collections::HashSet::new(),
        10,
        &Stop::new(),
    )
    .await
    .unwrap();

    let ids: Vec<&str> = qs.iter().map(|q| q.id.as_str()).collect();
    assert_eq!(ids, vec!["1"], "остаться должен только пригодный вопрос");
}

/// «Стоп» обязан рвать работу немедленно, а не после завершения цикла.
#[tokio::test]
async fn stop_interrupts_immediately() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("stop");
    let acc = account("stop-acc");

    m.route(|r| {
        if r.path.starts_with("/api/auth/users/") {
            return Some(Res::json(r#"{"id":556,"username":"slowvictim"}"#));
        }
        if r.path.starts_with("/api/topic/profile/556/topics") {
            return Some(Res::json(profile_feed(&[20, 21, 22, 23, 24], 0)));
        }
        if r.method == "POST" && r.path.starts_with("/api/topic/topics/") {
            return Some(Res::json(r#"{"result":{"user_reaction":1}}"#));
        }
        None
    });

    let stop = Stop::new();
    let s2 = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        s2.stop();
    });

    let t = std::time::Instant::now();
    let mut blocked = false;
    // Пауза 30 секунд между голосами: без честной реакции на «Стоп» тест
    // висел бы минуты.
    let _ = votes::vote_on_profile(
        &core,
        &acc,
        &format!("{}/profile/id556", m.base),
        Vote::Plus,
        0,
        30.0,
        &no_log(),
        &stop,
        &mut blocked,
    )
    .await;
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "«Стоп» не прервал паузу: {:?}", t.elapsed());
}

/// Ротация прокси: после порога сбоев аккаунт переключается на следующий адрес.
#[tokio::test]
async fn proxy_rotates_after_failures() {
    let mut a = Account::new("rotator");
    a.proxy = Some("http://127.0.0.1:9|http://127.0.0.1:10".into());
    a.reset_proxy_state(2);
    assert_eq!(a.active_proxy().as_deref(), Some("http://127.0.0.1:9"));
    assert!(a.note_proxy_fail().is_none(), "одного сбоя мало для смены");
    let (old, new) = a.note_proxy_fail().expect("после второго сбоя прокси меняется");
    assert_eq!(old, "http://127.0.0.1:9");
    assert_eq!(new, "http://127.0.0.1:10");
    // По кругу: следующий заход возвращает к первому.
    a.note_proxy_fail();
    let (_, back) = a.note_proxy_fail().unwrap();
    assert_eq!(back, "http://127.0.0.1:9");
}

/// Дубликаты ссылок в списке целей должны схлопываться: второй голос по тому же
/// профилю снял бы накрученное.
#[test]
fn duplicate_targets_collapse() {
    let out = otvet_core::util::unique_targets([
        "https://otvet.mail.ru/profile/id1/",
        "  https://otvet.mail.ru/profile/id1  ",
        "https://OTVET.mail.ru/profile/ID1",
        "",
        "https://otvet.mail.ru/profile/id2",
    ]);
    assert_eq!(out.len(), 2, "остались дубли: {out:?}");
    assert!(out[0].contains("id1"));
    assert!(out[1].contains("id2"));
}

/// Заглушка недоступна — код обязан вернуть сетевую ошибку, а не паниковать.
#[tokio::test]
async fn network_failure_is_reported_not_panicked() {
    let (core, _dir) = temp_core("neterr");
    let acc = account("net-acc");
    // Порт 9 гарантированно ничего не слушает.
    let r = core
        .http
        .request(
            &acc,
            "http://127.0.0.1:9/api/auth/user",
            otvet_core::http::ReqOpts::get().timeout_ms(800),
            &Stop::new(),
        )
        .await;
    assert!(matches!(r, Err(otvet_core::http::HttpError::Network(_))), "ожидали сетевую ошибку");
}

/// Голос НЕ должен уходить повторно после обрыва связи: сайт понимает второй
/// такой же голос как отмену первого.
#[tokio::test]
async fn lost_response_does_not_resend_the_vote() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("norepeat");
    let acc = account("norepeat-acc");

    m.route(|r| {
        if r.method == "POST" && r.path.starts_with("/api/topic/topics/902") {
            // Ответ «теряется»: сокет закрывается без ответа.
            return Some(Res::status(599));
        }
        None
    });

    let mut blocked = false;
    let voted = votes::vote_on_single(
        &core,
        &acc,
        &format!("{}/question/902", m.base),
        Vote::Plus,
        0.0,
        &no_log(),
        &Stop::new(),
        &mut blocked,
    )
    .await;

    assert_eq!(voted, 0, "потерянный ответ не считается поставленным голосом");
    let posts = m.hits_matching("POST /api/topic/topics/902");
    assert_eq!(posts.len(), 1, "голос ушёл повторно и снял бы первый: {posts:?}");
}

// ─── Контракты тел запросов ────────────────────────────────────────────────
//
// mail.ru разбирает тело строго по типам. `"topic_id":"270370769"` строкой —
// это HTTP 400 «expected=int64, got=string», то есть НИ ОДИН ответ не уходит.
// Такое ловится только живым запросом или вот такой проверкой.

#[tokio::test]
async fn answer_sends_numeric_topic_id() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("answerbody");
    let acc = account("answer-acc");

    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":4242}}"#));
        }
        None
    });

    let res = answerer::post_answer(&core, &acc, "270370769", "текст", None, &no_log(), &Stop::new())
        .await
        .expect("запрос ушёл");
    assert!(matches!(res, answerer::PostRes::Ok(4242)));

    let body = m.last_body("/api/topic/answers").expect("тело запроса записано");
    assert!(body["topic_id"].is_number(), "topic_id обязан быть числом: {}", body["topic_id"]);
    assert_eq!(body["topic_id"].as_i64(), Some(270370769));
}

#[tokio::test]
async fn reply_sends_numeric_ids() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("replybody");
    let acc = account("reply-acc");

    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":77}}"#));
        }
        None
    });

    let res = replier::post_reply(&core, &acc, "111", "222", "ага", &no_log(), &Stop::new())
        .await
        .expect("запрос ушёл");
    assert!(matches!(res, replier::PostRes::Ok(77)));
    let body = m.last_body("/api/topic/answers").expect("тело записано");
    assert!(
        body["topic_id"].is_number() && body["reply_to"].is_number(),
        "оба id должны быть числами: {body}"
    );
}

#[tokio::test]
async fn question_body_matches_contract() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("askbody");
    let mut acc = account("ask-acc");
    acc.user_id = Some(100200300);

    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/question" {
            return Some(Res::json(r#"{"result":{"id":99}}"#));
        }
        None
    });

    let res = asker::post_question(&core, &acc, "Заголовок", "тело", None, &no_log(), &Stop::new())
        .await
        .expect("запрос ушёл");
    assert!(matches!(res, asker::PostResult::Ok(99)));
    let body = m.last_body("/api/topic/question").expect("тело записано");
    assert_eq!(body["author_id"].as_i64(), Some(100200300), "author_id обязан быть числом");
    assert_eq!(body["title"].as_str(), Some("Заголовок"));
    assert!(body["tags"].is_array() && body["spaces"].is_array(), "теги и spaces обязательны: {body}");
}

#[tokio::test]
async fn vote_body_matches_contract() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("votebody");
    let acc = account("vote-body-acc");

    m.route(|r| {
        if r.method == "POST" && r.path.starts_with("/api/topic/topics/903/2") {
            return Some(Res::json(r#"{"result":{"user_reaction":2}}"#));
        }
        None
    });

    let mut blocked = false;
    let voted = votes::vote_on_single(
        &core,
        &acc,
        &format!("{}/question/903", m.base),
        Vote::Minus,
        0.0,
        &no_log(),
        &Stop::new(),
        &mut blocked,
    )
    .await;
    assert_eq!(voted, 1);
    let body = m.last_body("/api/topic/topics/903").expect("тело записано");
    assert_eq!(body["entityID"].as_i64(), Some(903), "entityID числом");
    assert_eq!(body["reactionType"].as_i64(), Some(2), "минус = 2");
    assert_eq!(body["reactionSource"].as_str(), Some("topics"));
}

// ─── Резолв профиля ────────────────────────────────────────────────────────

/// Служебный эндпоинт отдал 404 (так и случилось на живом сайте) — id всё
/// равно обязан находиться: он есть в разметке страницы профиля.
#[tokio::test]
async fn profile_id_falls_back_to_page_when_api_is_gone() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("resolve404");
    let acc = account("resolve-acc");

    m.route(|r| {
        if r.path.starts_with("/api/auth/users/") {
            return Some(Res { status: 404, body: r#"{"message":"Not Found"}"#.into(), headers: vec![] });
        }
        if r.path.starts_with("/profile/vasya") {
            // Кусок РЕАЛЬНОЙ разметки профиля: Nuxt держит id в ключе состояния.
            return Some(Res::json(
                r#"<html><body>{"$slist-requests-count-profile-100200300-posts":16}</body></html>"#,
            ));
        }
        None
    });

    let who = votes::resolve_profile_result(&core, &acc, &format!("{}/profile/vasya", m.base), &Stop::new())
        .await
        .expect("id должен найтись по странице профиля");
    assert_eq!(who.id, 100200300);
    assert_eq!(who.name, "vasya");
}

/// `/profile/id<N>` разбирается вообще без запросов — лишний трафик ни к чему.
#[tokio::test]
async fn profile_by_id_needs_no_requests() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("resolveid");
    let acc = account("resolve-id-acc");

    let who = votes::resolve_profile_result(&core, &acc, "https://otvet.mail.ru/profile/id777", &Stop::new())
        .await
        .expect("id прямо в ссылке");
    assert_eq!(who.id, 777);
    assert!(m.hits().is_empty(), "запросов быть не должно: {:?}", m.hits());
}

/// Сетевой сбой и «профиль не найден» — разные сообщения: чинятся по-разному.
#[tokio::test]
async fn resolve_errors_are_distinguishable() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("resolveerr");
    let acc = account("resolve-err-acc");

    // 1) мусор вместо ссылки
    let e = votes::resolve_profile_result(&core, &acc, "просто текст", &Stop::new()).await.unwrap_err();
    assert!(e.contains("не похоже на ссылку профиля"), "{e}");

    // Порядок важен: обработчики обходятся сверху вниз, поэтому «оборванное
    // соединение» для одного ника регистрируем ДО общего 404.
    m.route(|r| {
        if r.path.contains("lost") {
            return Some(Res::status(599)); // соединение рвётся без ответа
        }
        None
    });

    // 2) сайт отвечает, но id нигде нет
    m.route(|r| {
        if r.path.starts_with("/api/auth/users/") || r.path.starts_with("/profile/") {
            return Some(Res { status: 404, body: "нет такого".into(), headers: vec![] });
        }
        None
    });
    let e = votes::resolve_profile_result(&core, &acc, &format!("{}/profile/ghost", m.base), &Stop::new())
        .await
        .unwrap_err();
    assert!(e.contains("не нашёл id профиля"), "{e}");

    // 3) сеть рвётся — сообщение про сеть, а не про ссылку.
    // Важно: путь к API строится от базового адреса, а хост из ссылки на профиль
    // не используется вовсе, поэтому «сломать сеть» можно только ответом сервера.
    let e = votes::resolve_profile_result(&core, &acc, &format!("{}/profile/lost", m.base), &Stop::new())
        .await
        .unwrap_err();
    assert!(e.contains("сеть/прокси"), "{e}");
}

// ─── Личность аккаунта ─────────────────────────────────────────────────────

/// Ник на сайте меняется (аккаунт переименовали) — бот обязан подхватить новый.
/// Раньше он читал ник из базы, только если там пусто, и годами ходил со
/// старым: ссылки на собственный профиль отдавали 404.
#[tokio::test]
async fn validation_refreshes_stale_username() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("ident");
    let mut acc = Account::new("stale");
    acc.cookies = Some("Mpop=x".into());
    acc.user_id = Some(100200300);
    acc.username = Some("старый_ник".into());
    core.accounts.add(acc).unwrap();
    let acc = core.accounts.get("stale").unwrap();

    m.route(|r| {
        if r.path == "/api/auth/user" {
            return Some(Res::json(r#"{"id":100200300,"username":"новый_ник","nick":"Ботик"}"#));
        }
        if r.path.starts_with("/api/karma/score/") {
            return Some(Res::json(
                r#"{"result":{"total_score":3,"score":{"history":1,"knowledge":1,"discussion":1}}}"#,
            ));
        }
        None
    });

    let v = otvet_core::api::validate_account(&core, &acc, &Stop::new()).await;
    assert!(v.alive, "аккаунт жив");
    assert_eq!(v.username.as_deref(), Some("новый_ник"), "ник обязан обновиться");
    otvet_core::api::persist_validation(&core, "stale", &v);
    assert_eq!(
        core.accounts.get("stale").and_then(|a| a.username).as_deref(),
        Some("новый_ник"),
        "новый ник должен сохраниться в accounts.json"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// 403 без кук — это «не залогинен», и красить аккаунт можно. 418 — антибот,
/// и трогать статус нельзя: иначе мёртвый прокси «разлогинит» живые аккаунты.
#[tokio::test]
async fn auth_states_are_distinguished() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("authstates");
    let acc = account("auth-acc");

    static MODE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(403);
    m.route(|r| {
        if r.path == "/api/auth/user" {
            return Some(Res::status(MODE.load(std::sync::atomic::Ordering::SeqCst) as u16));
        }
        None
    });

    let v = otvet_core::api::validate_account(&core, &acc, &Stop::new()).await;
    assert!(v.auth_bad && !v.alive && !v.blocked, "403 = разлогин: {v:?}");

    MODE.store(418, std::sync::atomic::Ordering::SeqCst);
    let v = otvet_core::api::validate_account(&core, &acc, &Stop::new()).await;
    assert!(v.blocked && !v.auth_bad && !v.alive, "418 = антибот, статус не трогаем: {v:?}");
}

// ─── Режим с нейросетью ────────────────────────────────────────────────────
//
// Ключа в тестах нет, поэтому нейросеть тоже заглушка: тот же мок-сервер
// отвечает и за mail.ru, и за OpenAI-совместимый эндпоинт. Так проверяется
// весь путь — проверка ключа, генерация, подстановка маркеров, отправка,
// запись в журнал — без единого живого запроса.

fn ai_cfg(base: &str) -> otvet_core::ai::AiCfg {
    otvet_core::ai::AiCfg {
        url: format!("{base}/v1/chat/completions"),
        model: "test-model".into(),
        api_key: "sk-test".into(),
        temperature: 0.7,
        max_tokens: 100,
        timeout_sec: 10,
        retries: 1,
    }
}

/// Заглушка нейросети: отвечает фиксированным текстом и запоминает запрос.
fn route_ai(m: &Mock, answer: &'static str) {
    m.route(move |r| {
        if r.path.starts_with("/v1/chat/completions") {
            let body =
                format!(r#"{{"choices":[{{"message":{{"role":"assistant","content":"{answer}"}}}}]}}"#);
            return Some(Res::json(body));
        }
        None
    });
}

#[tokio::test]
async fn ai_answer_goes_through_the_whole_path() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("aianswer");
    let acc = account("ai-acc");

    route_ai(m, "ну такое, конечно");
    m.route(|r| {
        if r.path == "/api/auth/user" {
            return Some(Res::json(r#"{"id":1000,"username":"botik"}"#));
        }
        if r.path.starts_with("/api/karma/score/") {
            return Some(Res::json(
                r#"{"result":{"total_score":1,"score":{"history":0,"knowledge":0,"discussion":1}}}"#,
            ));
        }
        if r.path.starts_with("/api/topic/question/") {
            return Some(Res::json(
                r#"{"result":{"title":"Как варить пельмени?","content":{"type":"doc","content":[]},"author":{"username":"vasya"}}}"#,
            ));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":555}}"#));
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::Ai,
        target: answerer::TargetMode::Links,
        links: vec![format!("{}/question/12345", m.base)],
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        check_auth: true,
        ai: ai_cfg(&m.base),
        style: "Обычный чел".into(),
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;

    assert_eq!(out.done, 1, "ответ должен уйти");
    assert!(!out.blocked);

    // В нейросеть ушёл текст вопроса, а на сайт — ответ нейросети.
    let ai_body = m.last_body("/v1/chat/completions").expect("запрос к нейросети записан");
    let msgs = ai_body["messages"].as_array().expect("messages");
    assert!(
        msgs.iter().any(|x| x["content"].as_str().unwrap_or("").contains("Как варить пельмени")),
        "вопрос не попал в промпт: {ai_body}"
    );
    assert_eq!(ai_body["model"].as_str(), Some("test-model"));

    let post = m.last_body("/api/topic/answers").expect("ответ отправлен");
    let text = post["content"]["content"][0]["content"][0]["text"].as_str().unwrap_or("");
    assert_eq!(text, "ну такое, конечно", "на сайт ушёл не текст нейросети: {post}");

    // И вопрос записан в журнал — второй раз бот на него не полезет.
    let answered = otvet_core::journals::load_answered(&dir, "ai-acc");
    assert_eq!(answered.len(), 1, "журнал не пополнился: {answered:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Нейросеть молчит (кончился баланс — 402): аккаунт обязан остановиться, а не
/// крутить ленту вечно, оплачивая каждый отказ.
#[tokio::test]
async fn dead_ai_key_stops_the_account() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("aidead");
    let acc = account("ai-dead-acc");

    m.route(|r| {
        if r.path.starts_with("/v1/chat/completions") {
            // Первый запрос — проверка ключа — проходит, дальше «нет денег».
            static N: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Some(if n == 0 {
                Res::json(r#"{"choices":[{"message":{"content":"ок"}}]}"#)
            } else {
                Res {
                    status: 402,
                    body: r#"{"error":{"message":"Insufficient credits"}}"#.into(),
                    headers: vec![],
                }
            });
        }
        if r.path == "/api/auth/user" {
            return Some(Res::json(r#"{"id":1000,"username":"botik"}"#));
        }
        if r.path.starts_with("/api/topic/feed") {
            return Some(Res::json(
                r#"{"result":{"feed":[
                    {"id":501,"title":"Вопрос номер один про жизнь"},
                    {"id":502,"title":"Вопрос номер два про жизнь"},
                    {"id":503,"title":"Вопрос номер три про жизнь"},
                    {"id":504,"title":"Вопрос номер четыре про жизнь"},
                    {"id":505,"title":"Вопрос номер пять про жизнь"},
                    {"id":506,"title":"Вопрос номер шесть про жизнь"},
                    {"id":507,"title":"Вопрос номер семь про жизнь"}
                ]}}"#,
            ));
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::Ai,
        target: answerer::TargetMode::Feed,
        limit: 0, // без лимита — остановить должен именно счётчик отказов
        delay_min: 0.0,
        delay_max: 0.0,
        feed_min: 0.0,
        feed_max: 0.0,
        check_auth: false,
        ai: ai_cfg(&m.base),
        ..Default::default()
    };

    let out = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()),
    )
    .await
    .expect("прогон обязан закончиться сам, а не крутиться вечно");

    assert_eq!(out.done, 0, "при мёртвом ключе ничего не отправляется");
    let calls = m.hits_matching("/v1/chat/completions").len();
    assert!(calls <= 12, "слишком много попыток к нейросети: {calls}");
}

// ─── Картинки и память разговора ───────────────────────────────────────────

/// Картинка из пула прикладывается к ответу как галерея. Пул — это готовые
/// хэши на CDN, перезаливать их не нужно: в теле поста должен быть
/// imageGallery, а лишних запросов на заливку — ноль.
#[tokio::test]
async fn pool_image_is_attached_without_upload() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("poolimg");
    let acc = account("img-acc");

    std::fs::write(dir.join("gif-pool.json"), r#"[{"hash":"abc123def","width":320,"height":240}]"#).unwrap();

    m.route(|r| {
        if r.path.starts_with("/api/topic/question/") {
            return Some(Res::json(
                r#"{"result":{"title":"Вопрос с картинкой","content":{"type":"doc","content":[]}}}"#,
            ));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":1}}"#));
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::NoAi,
        target: answerer::TargetMode::Links,
        links: vec![format!("{}/question/700", m.base)],
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        check_auth: false,
        image: answerer::ImageMode::Gif { selected: vec![] },
        image_count: 1,
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 1);

    let body = m.last_body("/api/topic/answers").expect("ответ отправлен");
    let nodes = body["content"]["content"].as_array().expect("узлы документа");
    let gallery = nodes.iter().find(|n| n["type"] == "imageGallery").expect("картинки в посте нет");
    assert_eq!(
        gallery["attrs"]["gallery"][0]["src"].as_str(),
        Some("abc123def.jpg?size=origin"),
        "неверная ссылка на картинку: {gallery}"
    );
    // Последним узлом обязан идти пустой абзац, иначе редактор считает
    // документ невалидным.
    assert_eq!(nodes.last().unwrap()["type"], "paragraph");
    assert!(m.hits_matching("/api/pictures/images").is_empty(), "пул не должен перезаливаться");
    let _ = std::fs::remove_dir_all(dir);
}

/// Диалоговый режим: вопрос и ответ попадают в общий чат, а следующий запрос к
/// нейросети уже несёт эту историю. На этом держится «единый характер».
#[tokio::test]
async fn conversation_memory_is_kept_and_sent_back() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("convo");
    let acc = account("convo-acc");

    route_ai(m, "ага, бывает");
    m.route(|r| {
        if r.path.starts_with("/api/topic/question/") {
            return Some(Res::json(
                r#"{"result":{"title":"Первый вопрос про жизнь","content":{"type":"doc","content":[]},"author":{"username":"petya"}}}"#,
            ));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":2}}"#));
        }
        None
    });

    let mut p = answerer::AnswerParams {
        mode: answerer::AnswerMode::Ai,
        target: answerer::TargetMode::Links,
        links: vec![format!("{}/question/801", m.base)],
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        check_auth: false,
        conversational: true,
        convo_budget_k: 0.0, // без сжатия
        ai: ai_cfg(&m.base),
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 1);

    let convo = core.convo.snapshot();
    assert_eq!(convo.turns.len(), 2, "в чат должны лечь вопрос и ответ: {:?}", convo.turns);
    assert_eq!(convo.turns[1].content, "ага, бывает");
    assert!(convo.turns[0].content.contains("Первый вопрос"), "вопрос не записан: {:?}", convo.turns[0]);

    // Второй прогон: история обязана уехать в запрос к нейросети.
    p.links = vec![format!("{}/question/802", m.base)];
    let out2 = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out2.done, 1);
    let ai_body = m.last_body("/v1/chat/completions").expect("запрос к нейросети");
    let msgs = ai_body["messages"].as_array().unwrap();
    assert!(
        msgs.iter().any(|x| x["content"].as_str().unwrap_or("") == "ага, бывает"),
        "прошлый ответ не попал в историю: {ai_body}"
    );
    let _ = std::fs::remove_dir_all(dir);
}
