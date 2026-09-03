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
use otvet_core::{answerer, asker, complain, replier};
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
    /// Все тела запросов по пути, содержащему `needle`.
    pub fn bodies_matching(&self, needle: &str) -> Vec<serde_json::Value> {
        self.bodies
            .lock()
            .iter()
            .filter(|(p, _)| p.contains(needle))
            .filter_map(|(_, b)| serde_json::from_str(b).ok())
            .collect()
    }

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
        &Default::default(),
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
        &Default::default(),
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
        &Default::default(),
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
        &Default::default(),
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
        &Default::default(),
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
        &Default::default(),
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
        ..Default::default()
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

/// Картинки вопроса показываются нейросети.
///
/// Половина вопросов на сайте — это фото с подписью «как вам?»: без картинки
/// текст пустой, и модель отвечала вслепую. Проверяем, что картинка скачивается
/// и уезжает в запрос, а без галки не скачивается вовсе.
#[tokio::test]
async fn question_images_are_shown_to_the_ai() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("aisee");
    let acc = account("see-acc");

    route_ai(m, "симпатичная");
    m.route(|r| {
        if r.path.starts_with("/api/topic/question/") {
            // Текста нет — только картинка, как в живом вопросе-фотографии.
            return Some(Res::json(
                r#"{"result":{"title":"Ну как вам такое фото?","content":{"type":"doc","content":[
                    {"type":"imageGallery","attrs":{"gallery":[{"src":"pic.jpg?size=origin"}]}},
                    {"type":"paragraph"}]},"author":{"username":"vasya"}}}"#,
            ));
        }
        if r.path.starts_with("/api/pictures/images/") {
            return Some(Res::json("FAKEJPEG").with_header("content-type", "image/jpeg"));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":777}}"#));
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::Ai,
        target: answerer::TargetMode::Links,
        links: vec![format!("{}/question/900001", m.base)],
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        check_auth: false,
        see_images: true,
        // Проверка отправки тут ни при чём, а ждёт она секундами.
        verify_posted: false,
        ai: ai_cfg(&m.base),
        style: "Обычный чел".into(),
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 1, "ответ должен уйти");

    // Картинку скачали с сайта — по адресу из галереи.
    assert!(m.hits_matching("/api/pictures/images/pic.jpg").len() == 1, "картинку вопроса не скачали");

    // И вложили в запрос к модели рядом с текстом.
    let body = m.last_body("/v1/chat/completions").expect("запрос к нейросети записан");
    let user = body["messages"].as_array().and_then(|a| a.last()).cloned().expect("сообщение пользователя");
    let parts = user["content"].as_array().expect("содержимое должно быть частями: {user}");
    assert!(
        parts.iter().any(|x| x["text"].as_str().unwrap_or("").contains("Ну как вам такое фото")),
        "текст вопроса пропал: {user}"
    );
    let img = parts
        .iter()
        .find_map(|x| x.pointer("/image_url/url").and_then(|u| u.as_str()))
        .expect("картинки в запросе нет");
    // «FAKEJPEG» в base64 — ровно это и должно доехать до модели.
    assert_eq!(img, "data:image/jpeg;base64,RkFLRUpQRUc=", "картинка доехала не той");

    // А без галки её не должно быть вовсе — ни запроса за файлом, ни частей.
    let (core2, dir2) = temp_core("aisee-off");
    let p2 = answerer::AnswerParams { see_images: false, ..p.clone() };
    answerer::run_answerer(&core2, &account("see-off"), &p2, &no_log(), &Stop::new()).await;
    assert_eq!(m.hits_matching("/api/pictures/images/pic.jpg").len(), 1, "картинку скачали зря");
    let body = m.last_body("/v1/chat/completions").expect("запрос к нейросети записан");
    let user = body["messages"].as_array().and_then(|a| a.last()).cloned().unwrap();
    assert!(user["content"].is_string(), "без галки содержимое должно остаться строкой: {user}");

    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(dir2);
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
        verify_delay_sec: 0.0,
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
        verify_delay_sec: 0.0,
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
        verify_delay_sec: 0.0,
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

// ─── Лимиты, дедуп и отказы ────────────────────────────────────────────────

/// Лимит ответов обязан держаться и когда пачка уходит РАЗОМ. Раньше все задачи
/// пачки успевали увидеть «сделано 0» до первой отправки, и при лимите 2 и
/// пачке 6 уходило шесть ответов.
#[tokio::test]
async fn parallel_batch_never_exceeds_the_limit() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("parlimit");
    let acc = account("par-acc");

    m.route(|r| {
        if r.path.starts_with("/api/topic/feed") {
            return Some(Res::json(
                r#"{"result":{"feed":[
                    {"id":901,"title":"Первый вопрос про жизнь"},
                    {"id":902,"title":"Второй вопрос про жизнь"},
                    {"id":903,"title":"Третий вопрос про жизнь"},
                    {"id":904,"title":"Четвёртый вопрос про жизнь"},
                    {"id":905,"title":"Пятый вопрос про жизнь"},
                    {"id":906,"title":"Шестой вопрос про жизнь"}
                ]}}"#,
            ));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":1}}"#));
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::NoAi,
        target: answerer::TargetMode::Feed,
        limit: 2,
        batch_size: 6,
        parallel: true,
        delay_min: 0.0,
        delay_max: 0.0,
        feed_min: 0.0,
        feed_max: 0.0,
        check_auth: false,
        verify_delay_sec: 0.0,
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;

    let posts = m.hits_matching("POST /api/topic/answers").len();
    assert_eq!(posts, 2, "отправлено ответов сверх лимита: {posts}");
    assert_eq!(out.done, 2);
    let _ = std::fs::remove_dir_all(dir);
}

/// Лимит подписок обязан обрывать список, а не проходить его целиком.
/// Интерфейс его больше не задаёт (список ссылок и есть лимит), но у режима это
/// часть договора: раз ограничение передали — оно должно сработать.
#[tokio::test]
async fn subscriptions_respect_the_limit() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("sublimit");
    let acc = account("sub-acc");

    m.route(|r| {
        if r.path == "/api/topic/subscription" {
            return Some(Res::json(r#"{"result":"Ok"}"#));
        }
        None
    });

    let p = otvet_core::subscribe::SubParams {
        profiles: (1..=5).map(|i| format!("https://otvet.mail.ru/profile/id{i}")).collect(),
        action: otvet_core::subscribe::SubAction::Subscribe,
        delay: 0.0,
        limit: 2,
        check_auth: false,
        progress: Default::default(),
    };
    let out = otvet_core::subscribe::run_subscriber(&core, &acc, &p, &no_log(), &Stop::new()).await;

    assert_eq!(out.done, 2, "лимит подписок не сработал");
    assert_eq!(m.hits_matching("/api/topic/subscription").len(), 2);
}

/// Готовые вопросы не должны повторяться внутри одного прогона: журнал
/// читается один раз, поэтому список уже заданного нужно пополнять на лету.
#[tokio::test]
async fn ready_made_questions_do_not_repeat_in_one_run() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("askrepeat");
    let acc = account("ask-acc");

    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/question" {
            static N: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
            let id = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Some(Res::json(format!(r#"{{"result":{{"id":{id}}}}}"#)));
        }
        // Проверка публикации: все вопросы на месте.
        if let Some(id) = r.path.strip_prefix("/api/topic/question/") {
            return Some(Res::json(format!(r#"{{"result":{{"id":{id}}}}}"#)));
        }
        None
    });

    // Шесть вопросов и шесть публикаций: со сломанным дедупом совпадение
    // «все шесть разные» случайно выпадает в полутора случаях из ста.
    let p = asker::AskParams {
        mode: asker::AskMode::NoAi,
        limit: 6,
        delay_min: 0.0,
        delay_max: 0.0,
        verify_delay_sec: 0.0,
        check_auth: false,
        noai_questions: vec![
            "первый готовый вопрос".into(),
            "второй готовый вопрос".into(),
            "третий готовый вопрос".into(),
            "четвёртый готовый вопрос".into(),
            "пятый готовый вопрос".into(),
            "шестой готовый вопрос".into(),
        ],
        ..Default::default()
    };
    let out = asker::run_asker(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 6);

    let titles = otvet_core::journals::load_asked_titles(&dir, "ask-acc");
    let uniq: std::collections::HashSet<&String> = titles.iter().collect();
    assert_eq!(uniq.len(), titles.len(), "вопрос задан дважды за один прогон: {titles:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Одно и то же уведомление приходит в разных секциях ответа (`unread` и
/// `day`). Отвечать на него дважды нельзя: в журнал id попадает только после
/// отправки, поэтому дедуп нужен прямо в очереди.
#[tokio::test]
async fn duplicate_notification_is_answered_once() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("notifdup");
    let acc = account("notif-acc");

    m.route(|r| {
        if r.path.starts_with("/api/notificator/notifications") {
            let one = r#"{"id":42,"type":"new_reply_reply","entity_type":"reply","entity_id":777,
                "page_uri":"/question/500","title":"мой ответ","body":"его реплика",
                "created_at":"2026-08-01T10:00:00Z","authors":[{"id":5,"username":"vasya"}]}"#;
            return Some(Res::json(format!(r#"{{"result":{{"unread":[{one}],"day":[{one}]}}}}"#)));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":9001}}"#));
        }
        if r.path.starts_with("/api/topic/answers/") {
            // В ветке лежит и реплика собеседника, и наш ответ на неё: по
            // второму бот проверяет, что отправленное не снесли.
            return Some(Res::json(
                r#"{"result":{"replies":[
                    {"id":777,"content":{"type":"doc","content":[]},"author":{"id":5,"username":"vasya"}},
                    {"id":9001,"content":{"type":"doc","content":[]},"author":{"id":1000,"username":"me"}}
                ]}}"#,
            ));
        }
        None
    });

    let p = replier::ReplyParams {
        mode: replier::ReplyMode::NoAi,
        limit: 0,
        delay_min: 0.0,
        delay_max: 0.0,
        max_age_hours: 0.0,
        max_per_thread: 2,
        pages: 1,
        skip_own: false,
        use_question: false,
        use_chain: false,
        check_auth: false,
        verify_delay_sec: 0.0,
        ..Default::default()
    };
    let out = replier::run_replier(&core, &acc, &p, &no_log(), &Stop::new()).await;

    assert_eq!(out.done, 1, "на одну реплику ушло больше одного ответа");
    assert_eq!(m.hits_matching("POST /api/topic/answers").len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

/// В комментах картинка из реплики собеседника тоже показывается нейросети.
///
/// Мемом отвечают не реже, чем словами: у такой реплики текста нет вовсе, и без
/// картинки модель отвечала вслепую. Уведомление картинок не содержит — они
/// видны только в самой реплике, которую бот дочитывает живьём.
#[tokio::test]
async fn reply_images_are_shown_to_the_ai() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("cmsee");
    let acc = account("cmsee-acc");

    route_ai(m, "ну ты даёшь");
    m.route(|r| {
        if r.path.starts_with("/api/notificator/notifications") {
            return Some(Res::json(
                r#"{"result":{"unread":[{"id":42,"type":"new_reply_reply","entity_type":"reply",
                    "entity_id":777,"page_uri":"/question/500","title":"мой ответ","body":"",
                    "created_at":"2026-08-01T10:00:00Z","authors":[{"id":5,"username":"vasya"}]}]}}"#,
            ));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":9001}}"#));
        }
        if r.path.starts_with("/api/topic/answers/") {
            // Реплика собеседника — одна картинка без единого слова.
            return Some(Res::json(
                r#"{"result":{"replies":[
                    {"id":777,"content":{"type":"doc","content":[
                        {"type":"imageGallery","attrs":{"gallery":[{"src":"mem.png?size=origin"}]}},
                        {"type":"paragraph"}]},"author":{"id":5,"username":"vasya"}},
                    {"id":9001,"content":{"type":"doc","content":[]},"author":{"id":1000,"username":"me"}}
                ]}}"#,
            ));
        }
        if r.path.starts_with("/api/pictures/images/") {
            return Some(Res::json("FAKEPNG").with_header("content-type", "image/png"));
        }
        None
    });

    let p = replier::ReplyParams {
        mode: replier::ReplyMode::Ai,
        limit: 0,
        delay_min: 0.0,
        delay_max: 0.0,
        max_age_hours: 0.0,
        max_per_thread: 2,
        pages: 1,
        skip_own: false,
        use_question: false,
        use_chain: false,
        see_images: true,
        check_auth: false,
        verify_posted: false,
        verify_delay_sec: 0.0,
        ai: ai_cfg(&m.base),
        style: "Обычный чел".into(),
        ..Default::default()
    };
    let out = replier::run_replier(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 1, "реплика должна уйти");

    assert_eq!(m.hits_matching("/api/pictures/images/mem.png").len(), 1, "картинку реплики не скачали");
    let body = m.last_body("/v1/chat/completions").expect("запрос к нейросети записан");
    let user = body["messages"].as_array().and_then(|a| a.last()).cloned().expect("сообщение");
    let parts = user["content"].as_array().expect("содержимое должно быть частями");
    let img = parts
        .iter()
        .find_map(|x| x.pointer("/image_url/url").and_then(|u| u.as_str()))
        .expect("картинки в запросе нет");
    // Тип берётся из имени файла, а не угадывается: .png так .png.
    assert_eq!(img, "data:image/png;base64,RkFLRVBORw==", "картинка доехала не той");
    let _ = std::fs::remove_dir_all(dir);
}

/// Сайт перестал принимать ответы (обычно — дневной лимит). Режим «Комменты»
/// обязан остановиться после нескольких отказов подряд, а не перебирать всю
/// очередь, оплачивая генерацию на каждую цель.
#[tokio::test]
async fn dead_daily_limit_stops_the_replier() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("replierfails");
    let acc = account("fail-acc");

    m.route(|r| {
        if r.path.starts_with("/api/notificator/notifications") {
            let items: Vec<String> = (0..10)
                .map(|i| {
                    format!(
                        r#"{{"id":{},"type":"new_reply_reply","entity_type":"reply","entity_id":{},
                        "page_uri":"/question/{}","title":"мой ответ","body":"реплика",
                        "created_at":"2026-08-01T10:00:00Z","authors":[{{"id":5,"username":"vasya"}}]}}"#,
                        100 + i,
                        701 + i,
                        600 + i
                    )
                })
                .collect();
            return Some(Res::json(format!(r#"{{"result":{{"unread":[{}]}}}}"#, items.join(","))));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res { status: 400, body: r#"{"message":"limit"}"#.into(), headers: vec![] });
        }
        if r.path.starts_with("/api/topic/answers/") {
            let replies: Vec<String> = (0..10)
                .map(|i| {
                    format!(
                        r#"{{"id":{},"content":{{"type":"doc","content":[]}},"author":{{"id":5,"username":"vasya"}}}}"#,
                        701 + i
                    )
                })
                .collect();
            return Some(Res::json(format!(r#"{{"result":{{"replies":[{}]}}}}"#, replies.join(","))));
        }
        None
    });

    let p = replier::ReplyParams {
        mode: replier::ReplyMode::NoAi,
        limit: 0,
        delay_min: 0.0,
        delay_max: 0.0,
        max_age_hours: 0.0,
        max_per_thread: 1,
        pages: 1,
        skip_own: false,
        use_question: false,
        use_chain: false,
        check_auth: false,
        ..Default::default()
    };
    let out = replier::run_replier(&core, &acc, &p, &no_log(), &Stop::new()).await;

    assert_eq!(out.done, 0);
    let posts = m.hits_matching("POST /api/topic/answers").len();
    assert!(posts <= 5, "после пяти отказов подряд нужно остановиться, а попыток было {posts}");
    assert!(posts >= 5, "остановились слишком рано: {posts}");
    let _ = std::fs::remove_dir_all(dir);
}

/// «Живость 0» — это ноль, а не «значение не задано»: раньше его молча
/// подменяли на 0.7, и настройка не работала вообще.
#[tokio::test]
async fn zero_temperature_reaches_the_api() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("temp0");
    let acc = account("temp-acc");

    route_ai(m, "ответ");
    m.route(|r| {
        if r.path.starts_with("/api/topic/question/") {
            return Some(Res::json(
                r#"{"result":{"title":"Вопрос про жизнь","content":{"type":"doc","content":[]}}}"#,
            ));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":1}}"#));
        }
        None
    });

    let mut ai = ai_cfg(&m.base);
    ai.temperature = 0.0;
    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::Ai,
        target: answerer::TargetMode::Links,
        links: vec![format!("{}/question/999", m.base)],
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        check_auth: false,
        verify_delay_sec: 0.0,
        ai,
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 1);

    let body = m.last_body("/v1/chat/completions").expect("запрос к нейросети");
    assert_eq!(body["temperature"].as_f64(), Some(0.0), "температуру подменили: {body}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Сайт принял ответ, а через секунду его снесла автомодерация. Такой ответ
/// нельзя ни засчитывать, ни записывать в журнал: вопрос остался без ответа.
#[tokio::test]
async fn vanished_answer_is_not_counted() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("vanished");
    let acc = account("vanish-acc");

    m.route(|r| {
        if r.path.starts_with("/api/topic/question/") {
            return Some(Res::json(
                r#"{"result":{"title":"Вопрос про жизнь","content":{"type":"doc","content":[]}}}"#,
            ));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":4242}}"#));
        }
        // В ветке нашего ответа нет — только чужой.
        if r.path.starts_with("/api/topic/answers/") {
            return Some(Res::json(
                r#"{"result":{"replies":[{"id":777,"content":{"type":"doc","content":[]}}]}}"#,
            ));
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::NoAi,
        target: answerer::TargetMode::Links,
        links: vec![format!("{}/question/555", m.base)],
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        check_auth: false,
        verify_posted: true,
        verify_delay_sec: 0.0,
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;

    assert_eq!(out.done, 0, "пропавший ответ засчитан как отправленный");
    // Одна перегенерация: второй раз тем же текстом отправлять бессмысленно,
    // а бесконечно долбиться — тем более.
    let posts = m.hits_matching("POST /api/topic/answers").len();
    assert_eq!(posts, 2, "ожидалась ровно одна повторная попытка, было {posts}");
    let answered = otvet_core::journals::load_answered(&dir, "vanish-acc");
    assert!(answered.is_empty(), "в журнал попал ответ, которого нет на сайте");
    let _ = std::fs::remove_dir_all(dir);
}

/// Ответ на месте — засчитываем и записываем в журнал.
#[tokio::test]
async fn present_answer_is_counted_once() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("present");
    let acc = account("present-acc");

    m.route(|r| {
        if r.path.starts_with("/api/topic/question/") {
            return Some(Res::json(
                r#"{"result":{"title":"Вопрос про жизнь","content":{"type":"doc","content":[]}}}"#,
            ));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":4242}}"#));
        }
        if r.path.starts_with("/api/topic/answers/") {
            return Some(Res::json(
                r#"{"result":{"replies":[{"id":4242,"content":{"type":"doc","content":[]}}]}}"#,
            ));
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::NoAi,
        target: answerer::TargetMode::Links,
        links: vec![format!("{}/question/556", m.base)],
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        check_auth: false,
        verify_posted: true,
        verify_delay_sec: 0.0,
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 1);
    assert_eq!(m.hits_matching("POST /api/topic/answers").len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

/// Вопросы, заданные своими же аккаунтами, в работу не берутся.
#[tokio::test]
async fn own_questions_are_skipped() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("ownq");
    let acc = account("own-acc");
    // Свой второй аккаунт: id 4242 и ник «мойник».
    let mut other = Account::new("другой мой");
    other.user_id = Some(4242);
    other.username = Some("мойник".into());
    core.accounts.add(other).unwrap();

    m.route(|r| {
        if r.path.starts_with("/api/topic/feed") {
            return Some(Res::json(
                r#"{"result":{"feed":[
                    {"id":1,"title":"Вопрос от чужого человека","author":{"id":7,"username":"vasya"}},
                    {"id":2,"title":"Вопрос от моего аккаунта","author":{"id":4242,"username":"мойник"}},
                    {"id":3,"title":"Ещё один мой вопрос","author":{"username":"МойНик"}}
                ]}}"#,
            ));
        }
        None
    });

    let mine = otvet_core::answerer::my_keys(&core);
    let qs = otvet_core::answerer::collect_questions(
        &core,
        &acc,
        &std::collections::HashSet::new(),
        &std::collections::HashSet::new(),
        &mine,
        10,
        &Stop::new(),
    )
    .await
    .unwrap();

    let ids: Vec<&str> = qs.iter().map(|q| q.id.as_str()).collect();
    assert_eq!(ids, vec!["1"], "свои вопросы попали в работу: {ids:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Пачка не должна превышать остаток лимита: иначе бот генерирует текст на
/// вопросы, ответить на которые уже нельзя, и упирается в дневной лимит сайта.
#[tokio::test]
async fn batch_is_capped_by_the_remaining_limit() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("batchcap");
    let acc = account("cap-acc");

    route_ai(m, "ну такое");
    m.route(|r| {
        if r.path.starts_with("/api/topic/feed") {
            let items: Vec<String> = (1..=20)
                .map(|i| format!(r#"{{"id":{},"title":"Вопрос номер {i} про жизнь"}}"#, 300 + i))
                .collect();
            return Some(Res::json(format!(r#"{{"result":{{"feed":[{}]}}}}"#, items.join(","))));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":1}}"#));
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::Ai,
        target: answerer::TargetMode::Feed,
        limit: 3,
        batch_size: 15,
        // Пачкой разом: именно тут лишние вопросы стоят денег — каждый успевает
        // сходить в нейросеть до того, как лимит закончится.
        parallel: true,
        delay_min: 0.0,
        delay_max: 0.0,
        feed_min: 0.0,
        feed_max: 0.0,
        check_auth: false,
        verify_posted: false,
        ai: ai_cfg(&m.base),
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;

    assert_eq!(out.done, 3);
    assert_eq!(m.hits_matching("POST /api/topic/answers").len(), 3);
    // Главное: генераций не больше, чем ответов (+1 на проверку ключа).
    // Иначе бот платит за текст, который отправить уже нельзя.
    let gens = m.hits_matching("/v1/chat/completions").len();
    assert!(gens <= 4, "лишние обращения к нейросети: {gens}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Под вопросом целая страница ответов — нашего в ней может не быть просто
/// потому, что он не поместился. Считать его снесённым и слать второй нельзя:
/// под вопросом окажутся два ответа от одного аккаунта.
#[tokio::test]
async fn full_page_of_answers_is_not_treated_as_vanished() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("fullpage");
    let acc = account("page-acc");

    m.route(|r| {
        if r.path.starts_with("/api/topic/question/") {
            return Some(Res::json(
                r#"{"result":{"title":"Популярный вопрос про жизнь","content":{"type":"doc","content":[]}}}"#,
            ));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":4242}}"#));
        }
        if r.path.starts_with("/api/topic/answers/") {
            // Ровно страница чужих ответов, нашего нет.
            let items: Vec<String> = (1..=20)
                .map(|i| format!(r#"{{"id":{i},"content":{{"type":"doc","content":[]}}}}"#))
                .collect();
            return Some(Res::json(format!(r#"{{"result":{{"replies":[{}]}}}}"#, items.join(","))));
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::NoAi,
        target: answerer::TargetMode::Links,
        links: vec![format!("{}/question/557", m.base)],
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        check_auth: false,
        verify_posted: true,
        verify_delay_sec: 0.0,
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;

    assert_eq!(out.done, 1, "ответ засчитан не был");
    assert_eq!(m.hits_matching("POST /api/topic/answers").len(), 1, "ушёл второй ответ на тот же вопрос");
    let _ = std::fs::remove_dir_all(dir);
}

/// Прогрев следующего аккаунта: раннер проверяет его, пока работает текущий,
/// и режим стартует без трёх запросов на разогрев. Но ровно один раз — второй
/// раз тот же результат отдавать нельзя, аккаунт мог разлогиниться.
#[tokio::test]
async fn warmed_validation_is_used_once() {
    let (m, _lock) = exclusive().await;
    let (core, _dir) = temp_core("warm");
    let acc = account("warm-acc");

    m.route(|r| {
        if r.path == "/api/auth/user" {
            return Some(Res::json(r#"{"id":1000,"username":"botik"}"#));
        }
        if r.path.starts_with("/api/karma/score/") {
            return Some(Res::json(
                r#"{"result":{"total_score":5,"score":{"history":1,"knowledge":2,"discussion":2}}}"#,
            ));
        }
        None
    });

    otvet_core::api::warm_account(&core, &acc, &Stop::new()).await;
    let after_warm = m.hits_matching("/api/auth/user").len();
    assert_eq!(after_warm, 1, "прогрев не сходил на сайт");

    let v = otvet_core::api::validate_cached(&core, &acc, &Stop::new()).await;
    assert!(v.alive);
    assert_eq!(
        m.hits_matching("/api/auth/user").len(),
        after_warm,
        "прогретая проверка всё равно полезла в сеть"
    );

    // Второй раз — уже честная проверка.
    let _ = otvet_core::api::validate_cached(&core, &acc, &Stop::new()).await;
    assert_eq!(
        m.hits_matching("/api/auth/user").len(),
        after_warm + 1,
        "прогретый результат отдан повторно — так можно проспать разлогин"
    );
}

/// Вопрос, который снесла автомодерация, засчитывать нельзя: иначе «задано 5»
/// при пустом профиле, а лимит на аккаунт съеден впустую.
#[tokio::test]
async fn vanished_question_is_not_counted() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("askvanish");
    let acc = account("ask-vanish");

    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/question" {
            return Some(Res::json(r#"{"result":{"id":501}}"#));
        }
        // Сайт принял вопрос и тут же его снёс.
        if r.path.starts_with("/api/topic/question/") {
            return Some(Res::status(404));
        }
        None
    });

    let p = asker::AskParams {
        mode: asker::AskMode::NoAi,
        limit: 3,
        delay_min: 0.0,
        delay_max: 0.0,
        verify_delay_sec: 0.0,
        check_auth: false,
        noai_questions: vec!["первый".into(), "второй".into(), "третий".into(), "четвёртый".into()],
        ..Default::default()
    };
    let out = asker::run_asker(&core, &acc, &p, &no_log(), &Stop::new()).await;

    assert_eq!(out.done, 0, "снесённый вопрос попал в счёт");
    assert!(
        otvet_core::journals::load_asked_titles(&dir, "ask-vanish").is_empty(),
        "снесённый вопрос попал в журнал — потом его не повторят, хотя он не публиковался"
    );
    // Пять отказов подряд — и аккаунт останавливается, а не крутится вечно.
    assert_eq!(m.hits_matching("POST /api/topic/question").len(), 5, "бот не остановился после отказов");
    let _ = std::fs::remove_dir_all(dir);
}

/// Не смогли проверить — считаем опубликованным. Ошибка в другую сторону
/// стоит второго такого же вопроса от того же аккаунта.
#[tokio::test]
async fn unverifiable_question_still_counts() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("askunknown");
    let acc = account("ask-unknown");

    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/question" {
            return Some(Res::json(r#"{"result":{"id":777}}"#));
        }
        if r.path.starts_with("/api/topic/question/") {
            return Some(Res::status(500));
        }
        None
    });

    let p = asker::AskParams {
        mode: asker::AskMode::NoAi,
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        verify_delay_sec: 0.0,
        check_auth: false,
        noai_questions: vec!["единственный".into()],
        ..Default::default()
    };
    let out = asker::run_asker(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 1);
    assert_eq!(otvet_core::journals::load_asked_titles(&dir, "ask-unknown").len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

/// Выключенная проверка не должна стоить ни одного лишнего запроса.
#[tokio::test]
async fn verification_off_does_not_check_questions() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("asknocheck");
    let acc = account("ask-nocheck");

    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/question" {
            return Some(Res::json(r#"{"result":{"id":888}}"#));
        }
        None
    });

    let p = asker::AskParams {
        mode: asker::AskMode::NoAi,
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        verify_posted: false,
        check_auth: false,
        noai_questions: vec!["без проверки".into()],
        ..Default::default()
    };
    let out = asker::run_asker(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 1);
    assert!(
        m.hits_matching("GET /api/topic/question/").is_empty(),
        "проверка выключена, а запрос всё равно ушёл"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Забаненный аккаунт сайт отдаёт как ЖИВОЙ: 200, профиль на месте, куки
/// рабочие. Отличие одно — `user_status: -1`. Без него бот числил такой аккаунт
/// живым и каждый прогон тратил на него проход: голоса «не регистрируются»,
/// ответы не появляются, а в логе бодрое «Авторизован».
#[tokio::test]
async fn banned_account_is_seen_and_skipped() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("banned");
    core.accounts.add(account("banned-acc")).unwrap();
    let acc = core.accounts.get("banned-acc").unwrap();

    m.route(|r| {
        if r.path == "/api/auth/user" {
            return Some(Res::json(r#"{"id":100200301,"username":"botik","user_status":-1}"#));
        }
        if r.path.starts_with("/api/karma/score/") {
            return Some(Res::json(
                r#"{"result":{"total_score":-39,"score":{"history":0,"knowledge":-4,"discussion":-35}}}"#,
            ));
        }
        None
    });

    let v = otvet_core::api::validate_account(&core, &acc, &Stop::new()).await;
    assert!(v.alive, "сессия и правда живая — этим бан и коварен");
    assert!(v.banned, "бан не распознан");
    otvet_core::api::persist_validation(&core, "banned-acc", &v);
    assert_eq!(
        core.accounts.get("banned-acc").and_then(|a| a.banned),
        Some(true),
        "бан не сохранился в accounts.json"
    );

    // И режим обязан такой аккаунт пропустить, а не тратить на него проход.
    let p = votes::VoteParams {
        targets: vec!["https://otvet.mail.ru/question/12345".into()],
        vote: Vote::Plus,
        delay: 0.0,
        limit: 0,
        check_auth: true,
        progress: Default::default(),
    };
    let out = votes::run_votes(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert!(out.skipped, "заблокированный аккаунт должен пропускаться");
    assert_eq!(out.done, 0);
    assert!(
        m.hits_matching("POST /api/topic/topics").is_empty(),
        "в бан улетел запрос — проход потрачен впустую"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Обычный аккаунт (`user_status: 0`) баном считаться не должен, а снятый бан
/// обязан сняться и в файле.
#[tokio::test]
async fn normal_status_clears_the_ban_flag() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("unbanned");
    let mut a = Account::new("unban-acc");
    a.cookies = Some("Mpop=x".into());
    a.banned = Some(true);
    core.accounts.add(a).unwrap();
    let acc = core.accounts.get("unban-acc").unwrap();

    m.route(|r| {
        if r.path == "/api/auth/user" {
            return Some(Res::json(r#"{"id":1,"username":"botik","user_status":0}"#));
        }
        None
    });

    let v = otvet_core::api::validate_account(&core, &acc, &Stop::new()).await;
    assert!(!v.banned);
    otvet_core::api::persist_validation(&core, "unban-acc", &v);
    assert_eq!(core.accounts.get("unban-acc").and_then(|a| a.banned), Some(false), "бан не снялся");
    let _ = std::fs::remove_dir_all(dir);
}

/// Пауза должна останавливать прогон целиком, а не «на словах»: пока она стоит,
/// на сайт не уходит ни одного запроса, а после «Продолжить» работа идёт с того
/// же места, не теряя ни цели, ни счёта.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_freezes_the_run_and_resume_finishes_it() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("pause");
    let acc = account("pause-acc");

    m.route(|r| {
        if r.method == "POST" && r.path.starts_with("/api/topic/topics/") {
            return Some(Res::json(r#"{"result":{"user_reaction":1}}"#));
        }
        None
    });

    let stop = Stop::new();
    let p = votes::VoteParams {
        targets: (1..=4).map(|i| format!("https://otvet.mail.ru/question/100{i}")).collect(),
        vote: Vote::Plus,
        delay: 0.2,
        limit: 0,
        check_auth: false,
        progress: Default::default(),
    };
    let task = {
        let (core, acc, stop) = (core.clone(), acc.clone(), stop.clone());
        tokio::spawn(async move { votes::run_votes(&core, &acc, &p, &no_log(), &stop).await })
    };

    // Ждём первый голос, затем встаём на паузу.
    for _ in 0..100 {
        if !m.hits_matching("POST /api/topic/topics/").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    stop.pause();
    let frozen = m.hits_matching("POST /api/topic/topics/").len();
    assert!(frozen >= 1, "прогон не начался — тест ни о чём");

    // Пауза 0.2 с между голосами: за полсекунды без паузы ушло бы ещё два.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(
        m.hits_matching("POST /api/topic/topics/").len(),
        frozen,
        "на паузе бот всё равно ходил на сайт"
    );
    assert!(!task.is_finished(), "на паузе прогон завершился сам");

    stop.resume();
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("после «Продолжить» прогон не ожил")
        .unwrap();
    assert_eq!(out.done, 4, "после паузы прогон обязан доделать оставшееся");
    let _ = std::fs::remove_dir_all(dir);
}

/// Своих тем может быть несколько, и каждая — отдельная тема, а не один длинный
/// текст. Раньше поле было однострочным, и весь список уехал бы в промпт целиком.
#[tokio::test]
async fn every_question_takes_one_topic_from_the_list() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("topics");
    let acc = account("topic-acc");

    route_ai(m, "Что посмотреть вечером?");
    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/question" {
            return Some(Res::json(r#"{"result":{"id":321}}"#));
        }
        if let Some(id) = r.path.strip_prefix("/api/topic/question/") {
            return Some(Res::json(format!(r#"{{"result":{{"id":{id}}}}}"#)));
        }
        None
    });

    let topics = vec!["кино".to_string(), "еда".to_string(), "работа".to_string()];
    let p = asker::AskParams {
        mode: asker::AskMode::Ai,
        limit: 5,
        delay_min: 0.0,
        delay_max: 0.0,
        verify_delay_sec: 0.0,
        check_auth: false,
        topics: topics.clone(),
        ai: ai_cfg(&m.base),
        ..Default::default()
    };
    let out = asker::run_asker(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 5);

    // Первый запрос к нейросети — проверка ключа, темы в нём нет.
    let asked: Vec<String> = m
        .bodies_matching("/v1/chat/completions")
        .iter()
        .filter_map(|body| {
            let user = body["messages"].as_array()?.iter().find(|x| x["role"] == "user")?["content"]
                .as_str()?
                .to_string();
            // После темы в сообщении идёт ещё абзац с требованием к заголовку.
            user.split("на тему: ").nth(1).map(|t| t.lines().next().unwrap_or_default().trim().to_string())
        })
        .collect();
    assert_eq!(asked.len(), 5, "тема должна уходить в каждый запрос: {asked:?}");
    for t in &asked {
        assert!(topics.contains(t), "нейросети ушла не тема из списка, а «{t}» — весь список целиком?");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Сайт может отказаться принимать конкретный текст (ссылка в ответе, слишком
/// коротко, спам-фильтр). Ответа при этом не создаётся, поэтому правильный ход —
/// сочинить другой текст и отправить ещё раз, а не терять вопрос целиком.
#[tokio::test]
async fn refused_answer_is_retried_with_another_text() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("refused");
    let acc = account("refuse-acc");

    m.route(|r| {
        if r.path.starts_with("/api/topic/question/") {
            return Some(Res::json(
                r#"{"result":{"title":"Вопрос про жизнь","content":{"type":"doc","content":[]}}}"#,
            ));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            static N: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
            // Первый текст сайт отвергает, второй принимает.
            return Some(if N.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Res {
                    status: 400, ..Res::json(r#"{"error":{"message":"текст не прошёл модерацию"}}"#)
                }
            } else {
                Res::json(r#"{"result":{"id":4242}}"#)
            });
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::NoAi,
        target: answerer::TargetMode::Links,
        links: vec![format!("{}/question/555", m.base)],
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        verify_posted: false,
        check_auth: false,
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;

    assert_eq!(out.done, 1, "после отказа ответ так и не ушёл");
    assert_eq!(
        m.hits_matching("POST /api/topic/answers").len(),
        2,
        "второй попытки не было — вопрос потеряли"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// То же для вопросов: отказ по тексту — повод сочинить другой, а не считать
/// его неудачей и не идти дальше с тем же самым.
#[tokio::test]
async fn refused_question_is_replaced_by_another() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("refusedq");
    let acc = account("refuseq-acc");

    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/question" {
            static N: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
            return Some(if N.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Res {
                    status: 422, ..Res::json(r#"{"error":"нельзя такое спрашивать"}"#)
                }
            } else {
                Res::json(r#"{"result":{"id":900}}"#)
            });
        }
        if let Some(id) = r.path.strip_prefix("/api/topic/question/") {
            return Some(Res::json(format!(r#"{{"result":{{"id":{id}}}}}"#)));
        }
        None
    });

    let p = asker::AskParams {
        mode: asker::AskMode::NoAi,
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        verify_delay_sec: 0.0,
        check_auth: false,
        noai_questions: vec!["первый готовый".into(), "второй готовый".into()],
        ..Default::default()
    };
    let out = asker::run_asker(&core, &acc, &p, &no_log(), &Stop::new()).await;

    assert_eq!(out.done, 1, "после отказа вопрос так и не опубликовался");
    assert_eq!(m.hits_matching("POST /api/topic/question").len(), 2, "второй попытки не было");
    // Отвергнутый заголовок не должен попасть в журнал: его на сайте нет.
    let titles = otvet_core::journals::load_asked_titles(&dir, "refuseq-acc");
    assert_eq!(titles.len(), 1, "в журнал попал и отвергнутый вопрос: {titles:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// И в «Комментах»: отвергнутая реплика возвращается в очередь и пишется
/// заново. Одна попытка на цель — если сайт отказывает и второй, дело не в тексте.
#[tokio::test]
async fn refused_reply_is_written_again_once() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("refusedr");
    let acc = account("refuser-acc");

    m.route(|r| {
        if r.path.starts_with("/api/notificator/notifications") {
            let one = r#"{"id":7,"type":"new_reply_reply","entity_type":"reply","entity_id":700,
                "page_uri":"/question/600","title":"мой ответ","body":"его реплика",
                "created_at":"2026-08-01T10:00:00Z","authors":[{"id":42,"username":"vasya"}]}"#;
            return Some(Res::json(format!(r#"{{"result":{{"unread":[{one}]}}}}"#)));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            static N: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
            return Some(if N.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Res { status: 400, ..Res::json(r#"{"error":"нельзя"}"#) }
            } else {
                Res::json(r#"{"result":{"id":808}}"#)
            });
        }
        if r.path.starts_with("/api/topic/answers/") {
            return Some(Res::json(
                r#"{"result":{"replies":[
                    {"id":700,"content":{"type":"doc","content":[]},"author":{"id":42,"username":"vasya"}}
                ]}}"#,
            ));
        }
        None
    });

    let p = replier::ReplyParams {
        mode: replier::ReplyMode::NoAi,
        limit: 1,
        delay_min: 0.0,
        delay_max: 0.0,
        max_age_hours: 0.0,
        max_per_thread: 2,
        pages: 1,
        skip_own: false,
        use_question: false,
        use_chain: false,
        verify_posted: false,
        check_auth: false,
        noai_replies: vec!["ага".into(), "ну да".into()],
        ..Default::default()
    };
    let out = replier::run_replier(&core, &acc, &p, &no_log(), &Stop::new()).await;

    assert_eq!(out.done, 1, "после отказа реплика так и не ушла");
    assert_eq!(m.hits_matching("POST /api/topic/answers").len(), 2, "второй попытки не было");
    let _ = std::fs::remove_dir_all(dir);
}

/// Ответы по диапазону номеров — в том числе на вопросы, которых ещё нет.
/// Номера у mail.ru идут подряд, и сайт принимает ответ авансом. Проверяем, что
/// бот идёт по диапазону подряд, уважает лимит и не отвечает дважды при
/// повторном запуске того же диапазона.
#[tokio::test]
async fn range_answers_go_in_order_and_respect_the_journal() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("range");
    let acc = account("range-acc");

    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/answers" {
            static N: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
            let id = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Some(Res::json(format!(r#"{{"result":{{"id":{id}}}}}"#)));
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::Ai, // ядро обязано само перевести это в готовые фразы
        target: answerer::TargetMode::Range,
        range_from: 100,
        range_to: 104,
        limit: 3,
        delay_min: 0.0,
        delay_max: 0.0,
        check_auth: false,
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 3, "лимит в диапазоне не сработал");
    assert!(!out.exhausted, "диапазон не пройден — упёрлись в лимит, работа осталась");

    let ids: Vec<i64> =
        m.bodies_matching("/api/topic/answers").iter().filter_map(|b| b["topic_id"].as_i64()).collect();
    assert_eq!(ids, vec![100, 101, 102], "диапазон прошли не по порядку");
    // Нейросеть не спрашивали: вопроса ещё нет, писать не о чем.
    assert!(m.hits_matching("/v1/chat/completions").is_empty(), "в диапазоне позвали нейросеть");
    // И страницу вопроса не читали — её тоже ещё нет.
    assert!(m.hits_matching("GET /api/topic/question/").is_empty(), "лишний запрос за текстом вопроса");

    // Второй запуск того же диапазона продолжает с того места, где остановились.
    // Очередь номеров берём чистую — как при новом нажатии «Запустить»: тогда
    // повтор ловится журналом, а не тем, что очередь уже прокручена.
    let p = answerer::AnswerParams { range_queue: Default::default(), ..p.clone() };
    let out2 = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out2.done, 2, "повторный запуск не дошёл до хвоста диапазона");
    assert!(out2.exhausted, "диапазон пройден целиком — прогон должен об этом сказать");
    let ids: Vec<i64> =
        m.bodies_matching("/api/topic/answers").iter().filter_map(|b| b["topic_id"].as_i64()).collect();
    assert_eq!(ids, vec![100, 101, 102, 103, 104], "по одному из вопросов ответили дважды");
    let _ = std::fs::remove_dir_all(dir);
}

/// Диапазон делится между аккаунтами, а не проходится каждым целиком.
///
/// Очередь номеров одна на прогон, поэтому десять тысяч номеров разбираются во
/// столько раз быстрее, сколько аккаунтов работает. Раньше каждый аккаунт шёл
/// по всему диапазону сам, и под одним будущим вопросом оказывалось столько
/// ответов, сколько аккаунтов запустили.
#[tokio::test]
async fn range_is_shared_between_accounts() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("range-share");

    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/answers" {
            static N: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
            let id = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Some(Res::json(format!(r#"{{"result":{{"id":{id}}}}}"#)));
        }
        None
    });

    let p = answerer::AnswerParams {
        target: answerer::TargetMode::Range,
        range_from: 100,
        range_to: 109,
        limit: 0,
        delay_min: 0.0,
        delay_max: 0.0,
        check_auth: false,
        ..Default::default()
    };

    // Три аккаунта с ОДНИМИ параметрами — так их и раздаёт интерфейс: параметры
    // собираются один раз на прогон и клонируются под каждый аккаунт.
    let mut done = Vec::new();
    for name in ["share-1", "share-2", "share-3"] {
        let out = answerer::run_answerer(&core, &account(name), &p.clone(), &no_log(), &Stop::new()).await;
        done.push(out.done);
    }

    let mut ids: Vec<i64> =
        m.bodies_matching("/api/topic/answers").iter().filter_map(|b| b["topic_id"].as_i64()).collect();
    ids.sort_unstable();
    assert_eq!(ids, (100..=109).collect::<Vec<_>>(), "диапазон разобран не ровно по разу");
    assert_eq!(done.iter().sum::<i64>(), 10, "сумма по аккаунтам разошлась с числом ответов");
    // Первый забрал всё, потому что шёл без лимита и без пауз, — но остальным
    // очередь уже ничего не выдала, и второго ответа под теми же номерами нет.
    assert_eq!(done[0], 10, "первый аккаунт не разобрал очередь: {done:?}");
    assert_eq!(&done[1..], &[0, 0], "очередь выдала номера повторно: {done:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Номер, взятый аккаунтом, но не отработанный, возвращается в очередь.
///
/// Выдаётся он ровно один раз, поэтому без возврата антибот посреди диапазона
/// оставлял бы дыру: аккаунт умер, а под номером так и нет ответа, и никто
/// больше его не возьмёт.
#[tokio::test]
async fn a_blocked_account_returns_its_numbers() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("range-return");

    // Четвёртый ответ отбиваем антиботом, дальше снова пускаем.
    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/answers" {
            static N: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 3 {
                return Some(Res { status: 429, ..Res::json(r#"{"error":"antibot"}"#) });
            }
            return Some(Res::json(format!(r#"{{"result":{{"id":{}}}}}"#, 500 + n)));
        }
        None
    });

    let p = answerer::AnswerParams {
        target: answerer::TargetMode::Range,
        range_from: 100,
        range_to: 104,
        limit: 0,
        delay_min: 0.0,
        delay_max: 0.0,
        check_auth: false,
        ..Default::default()
    };

    let first = answerer::run_answerer(&core, &account("ret-1"), &p.clone(), &no_log(), &Stop::new()).await;
    assert!(first.blocked, "антибот не остановил аккаунт");
    assert_eq!(first.done, 3, "до антибота должно было уйти три ответа");

    // Второй аккаунт добирает и возвращённый номер, и остаток диапазона.
    let second = answerer::run_answerer(&core, &account("ret-2"), &p.clone(), &no_log(), &Stop::new()).await;
    assert_eq!(second.done, 2, "второй аккаунт не добрал остаток: {second:?}");

    let mut ok: Vec<i64> =
        m.bodies_matching("/api/topic/answers").iter().filter_map(|b| b["topic_id"].as_i64()).collect();
    ok.sort_unstable();
    ok.dedup();
    assert_eq!(ok, (100..=104).collect::<Vec<_>>(), "в диапазоне осталась дыра: {ok:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// По ссылкам работа кончается вместе со списком.
///
/// Иначе «Ответы по ссылке» с включёнными кругами крутились впустую до «Стоп»:
/// журнал отбрасывал все ссылки, круг делал ноль ответов, и следующий начинался
/// снова. Прогон обязан сказать «отвечать больше не на что».
#[tokio::test]
async fn links_are_exhausted_when_the_list_is_done() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("linksdone");
    let acc = account("links-acc");

    m.route(|r| {
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":42}}"#));
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::NoAi,
        target: answerer::TargetMode::Links,
        links: vec![format!("{}/question/111", m.base), format!("{}/question/222", m.base)],
        limit: 0,
        delay_min: 0.0,
        delay_max: 0.0,
        check_auth: false,
        verify_posted: false,
        ..Default::default()
    };

    let first = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(first.done, 2, "обе ссылки должны быть отвечены");
    assert!(first.exhausted, "список пройден целиком — работы больше нет");

    // Второй круг: журнал отбрасывает обе ссылки, отвечать не на что.
    let second = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(second.done, 0, "по журналу второй раз отвечать нельзя");
    assert!(second.exhausted, "пустой круг обязан сказать, что работы нет");
    assert_eq!(m.hits_matching("POST /api/topic/answers").len(), 2, "на ссылку ушло больше одного ответа");

    // А вот прерванный лимитом проход работой не считается: остаток списка
    // ждёт следующего круга.
    let (core2, dir2) = temp_core("linkscut");
    let p2 = answerer::AnswerParams { limit: 1, ..p.clone() };
    let cut = answerer::run_answerer(&core2, &account("links-cut"), &p2, &no_log(), &Stop::new()).await;
    assert_eq!(cut.done, 1);
    assert!(!cut.exhausted, "упёрлись в лимит — список не дошли, круги нужны");

    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(dir2);
}

/// Жалобы: тела запросов и лимит.
///
/// Единственный режим, который нельзя проверить живьём — жалоба летит в чужой
/// аккаунт. Поэтому контракт проверяем на заглушке: id должен уходить ЧИСЛОМ и
/// в свой эндпоинт для профиля и для поста, а лимит — считать отправленные.
#[tokio::test]
async fn complaints_hit_the_right_endpoints() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("complain");
    let acc = account("cmp-acc");

    m.route(|r| {
        if r.path.starts_with("/api/antispam/report_") {
            return Some(Res::json(r#"{"result":{}}"#));
        }
        if r.path.starts_with("/api/topic/profile/") {
            // Две страницы постов профиля: курсор инклюзивный, как у сайта.
            let pos = r.path.split("pos=").nth(1).and_then(|t| t.split('&').next()).unwrap_or("0");
            if pos == "0" {
                return Some(Res::json(r#"{"result":{"feed":[{"id":11},{"id":12},{"id":13}]}}"#));
            }
            return Some(Res::json(r#"{"result":{"feed":[]}}"#));
        }
        None
    });

    // 1) Жалоба на сам профиль.
    let p = complain::ComplainParams {
        targets: vec![format!("{}/profile/id777/", m.base)],
        target: complain::ComplainTarget::User,
        reason: "spam".into(),
        delay: 0.0,
        limit: 0,
        check_auth: false,
        ..Default::default()
    };
    let out = complain::run_complainer(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 1, "жалоба на профиль не ушла");
    let body = m.last_body("/api/antispam/report_user").expect("запрос на профиль");
    assert_eq!(body["id"].as_i64(), Some(777), "id профиля ушёл не числом: {body}");
    assert_eq!(body["report_type"].as_str(), Some("spam"));

    // 2) Перебор постов профиля — со своим эндпоинтом и своим лимитом.
    let p = complain::ComplainParams {
        targets: vec![format!("{}/profile/id777/", m.base)],
        target: complain::ComplainTarget::Topics,
        reason: "flood".into(),
        delay: 0.0,
        limit: 2,
        check_auth: false,
        ..Default::default()
    };
    let out = complain::run_complainer(&core, &acc, &p, &no_log(), &Stop::new()).await;
    assert_eq!(out.done, 2, "лимит жалоб не сработал");
    let ids: Vec<i64> =
        m.bodies_matching("/api/antispam/report_topic").iter().filter_map(|b| b["id"].as_i64()).collect();
    assert_eq!(ids, vec![11, 12], "посты обошли не по порядку или не остановились на лимите");
    assert_eq!(
        m.last_body("/api/antispam/report_topic").unwrap()["report_type"].as_str(),
        Some("flood"),
        "причина жалобы потерялась"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Слова-приметы: из ленты берутся только вопросы, где они встретились.
/// Остальные не берутся вовсе — ни лимита, ни запроса к нейросети на них не
/// тратим, а текст ответа остаётся обычным, от нейросети.
#[tokio::test]
async fn keywords_filter_the_feed() {
    let (m, _lock) = exclusive().await;
    let (core, dir) = temp_core("trigger");
    let acc = account("trig-acc");

    route_ai(m, "ответ от нейросети");
    m.route(|r| {
        if r.path.starts_with("/api/topic/feed") {
            return Some(Res::json(
                r#"{"result":{"feed":[
                    {"id":900,"title":"Какой VPN сейчас работает?","content":{"type":"doc","content":[]},
                     "author":{"id":7,"nick":"вася","username":"vasya"}},
                    {"id":901,"title":"Что приготовить на ужин по-быстрому?","content":{"type":"doc","content":[]},
                     "author":{"id":8,"nick":"петя","username":"petya"}}
                ]}}"#,
            ));
        }
        if r.method == "POST" && r.path == "/api/topic/answers" {
            return Some(Res::json(r#"{"result":{"id":555}}"#));
        }
        None
    });

    let p = answerer::AnswerParams {
        mode: answerer::AnswerMode::Ai,
        target: answerer::TargetMode::Feed,
        // Ровно один: лента в заглушке не меняется, и без лимита прогон стоял бы
        // и ждал новых вопросов — так он и должен себя вести.
        limit: 1,
        recent_scan: 10,
        delay_min: 0.0,
        delay_max: 0.0,
        feed_min: 0.0,
        feed_max: 0.0,
        verify_posted: false,
        check_auth: false,
        skip_own_authors: false,
        keywords: answerer::parse_keywords("vpn, впн"),
        ai: ai_cfg(&m.base),
        ..Default::default()
    };
    let out = answerer::run_answerer(&core, &acc, &p, &no_log(), &Stop::new()).await;

    assert_eq!(out.done, 1, "ответить надо ровно на один вопрос — про VPN");
    let body = m.last_body("/api/topic/answers").expect("ответ ушёл");
    assert_eq!(body["topic_id"].as_i64(), Some(900), "ответили не на тот вопрос");
    assert!(body.to_string().contains("ответ от нейросети"), "текст должен быть от нейросети: {body}");
    // Про ужин нейросеть не спрашивали: этот вопрос из ленты вообще не берётся.
    let about_dinner =
        m.bodies_matching("/v1/chat/completions").iter().filter(|b| b.to_string().contains("ужин")).count();
    assert_eq!(about_dinner, 0, "лишний вопрос уехал в нейросеть — это деньги на ветер");
    let _ = std::fs::remove_dir_all(dir);
}
