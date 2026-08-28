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

    fn reset(&self) {
        self.routes.lock().handlers.clear();
        self.hits.lock().clear();
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
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let r2 = routes.clone();
        let h2 = hits.clone();
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
                    tokio::spawn(async move {
                        let _ = serve(sock, routes, hits).await;
                    });
                }
            });
        });
        let addr = rx.recv().unwrap();
        let base = format!("http://{addr}");
        std::env::set_var("OTVET_BASE", &base);
        Mock { base, routes, hits }
    })
}

async fn serve(
    mut sock: tokio::net::TcpStream,
    routes: Arc<Mutex<Routes>>,
    hits: Arc<Mutex<Vec<String>>>,
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
        // Тело дочитываем, иначе клиент подвиснет на следующем запросе.
        let mut body_read = buf.len() - head_end;
        while body_read < content_length {
            let n = sock.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body_read += n;
        }

        hits.lock().push(format!("{method} {path}"));
        let req = Req { method, path, cookie };
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
