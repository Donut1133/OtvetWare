//! cdp.rs — вход в аккаунт через настоящий браузер и съём кук по CDP.
//!
//! Работать боту браузер не нужен — только войти. Схема:
//!   1. поднимаем Chromium из `browsers/` со своим профилем и `--remote-debugging-port`;
//!   2. человек логинится руками (капчу и СМС никто за него не решает);
//!   3. как только в куках появляются `Mpop`/`Auth-Token`, снимаем их через
//!      `Storage.getCookies` и закрываем окно.
//!
//! Про прокси честно: с логином и паролем Chromium работать по-человечески не
//! умеет. SOCKS5 с авторизацией он не поддерживает вовсе («Browser does not
//! support socks5 proxy authentication»), а на HTTP-прокси показывает окно
//! «укажите имя пользователя и пароль» — на каждый запуск и на каждый аккаунт.
//! Поэтому для любого прокси с логином мы поднимаем локальный мост: браузер
//! ходит на `127.0.0.1:<порт>` без авторизации, а логин подставляет мост.
//!
//! Без прокси вход шёл бы с реального IP, а работа потом — с прокси: сессия,
//! снятая с одного IP и используемая с другого, для антифрода mail.ru выглядит
//! именно так, как выглядит угон аккаунта.

use crate::accounts::{jar_to_string, looks_logged_in};
use crate::persona::Persona;
use crate::proxy::{parse_proxy, ProxyCfg};
use crate::util::{Log, Stop};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Результат входа.
pub struct Harvest {
    pub cookies: String,
    pub ua: String,
}

/// Страница входа в почту — с неё начинается добыча кук.
const LOGIN_URL: &str = "https://account.mail.ru/login";
/// Куда уводит вторая вкладка после входа — туда, где аккаунт и будет работать.
const SITE_URL: &str = "https://otvet.mail.ru/";

pub fn chrome_path(root: &Path) -> Option<PathBuf> {
    // Явный путь важнее всего: так подключают портативную сборку или браузер,
    // установленный не туда, куда принято.
    if let Ok(p) = std::env::var("OTVET_BROWSER") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    // Сначала — сборка, лежащая рядом (та же, чью версию обещает персона).
    // Смотрим и в папку данных, и рядом с самой программой: при портативной
    // раскладке данные лежат в `accounts`, а тяжёлые `browsers` обычно остаются
    // рядом с .exe.
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let cwd = std::env::current_dir().ok();
    for dir in [
        Some(root.join("browsers")),
        exe_dir.map(|d| d.join("browsers")),
        // Текущая папка — это запуск из клона: `cargo run` кладёт .exe в
        // target/release, а браузер человек ставит в корень проекта.
        cwd.map(|d| d.join("browsers")),
    ]
    .into_iter()
    .flatten()
    {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            for rel in [
                "chrome-win64/chrome.exe",
                "chrome-win/chrome.exe",
                "chrome-linux/chrome",
                "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
            ] {
                let p = e.path().join(rel);
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }
    // Системный браузер намеренно НЕ ищем. Он обновляется сам, и его версия
    // разъезжается с той, которой представляется персона: UA говорит «148», а
    // движок оказывается 151-м — это ловится перебором возможностей. Плюс
    // обычный Chrome не прячет следы автоматизации, ради которых и берётся
    // патченная сборка. Лучше внятно сказать «браузера нет», чем тихо работать
    // с приметным.
    None
}

/// Файл, в который браузер пишет выбранный им порт отладчика.
fn devtools_port_file(profile_dir: &Path) -> PathBuf {
    profile_dir.join("DevToolsActivePort")
}

/// Адрес отладчика ИМЕННО ЭТОГО окна.
///
/// Порт выбирает сам браузер (`--remote-debugging-port=0`) и пишет его в
/// `DevToolsActivePort` в своём профиле: первая строка — порт, вторая — путь
/// сокета.
///
/// Раньше порт подбирали мы: биндили `127.0.0.1:0`, отпускали сокет и отдавали
/// номер флагом. Между «отпустили» и «браузер занял» тот же номер успевал
/// достаться следующему запуску — и второе окно подключалось не к себе, а к
/// первому, уже работающему. Оттуда брались чужие вкладки в окне «другого
/// аккаунта», куки уходили не в тот браузер, а когда то окно закрывали —
/// сыпалось «Session with given id not found».
async fn wait_devtools(profile_dir: &Path, child: &mut tokio::process::Child) -> anyhow::Result<String> {
    let file = devtools_port_file(profile_dir);
    for _ in 0..160 {
        if let Some(url) = read_devtools_url(&file) {
            return Ok(url);
        }
        // Браузер с уже занятым профилем не поднимает своё окно: он передаёт
        // запрос работающей копии и сразу выходит. Порта от него не дождаться.
        if matches!(child.try_wait(), Ok(Some(_))) {
            anyhow::bail!("окно этого аккаунта уже открыто — закрой его и попробуй снова");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("браузер не поднял отладочный порт")
}

fn read_devtools_url(file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    if port == 0 || !path.starts_with('/') {
        return None;
    }
    Some(format!("ws://127.0.0.1:{port}{path}"))
}

/// Локальный мост до прокси С ЛОГИНОМ — и SOCKS5, и HTTP.
///
/// Браузер ходит на `127.0.0.1:<порт>` без всякой авторизации, а логин с паролем
/// подставляет мост. Иначе Chromium либо не умеет вовсе (SOCKS с паролем), либо
/// спрашивает пароль у человека отдельным окном (HTTP) — каждый раз, на каждый
/// запуск, по каждому аккаунту.
///
/// Возвращает адрес `127.0.0.1:port`; живёт, пока не сработает `stop`.
async fn auth_bridge(cfg: ProxyCfg, stop: Stop) -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let accept = tokio::select! {
                _ = stop.wait() => break,
                r = listener.accept() => r,
            };
            let Ok((client, _)) = accept else { break };
            let cfg = cfg.clone();
            tokio::spawn(async move {
                let _ = bridge_one(client, cfg).await;
            });
        }
    });
    Ok(addr.to_string())
}

async fn bridge_one(mut client: TcpStream, cfg: ProxyCfg) -> std::io::Result<()> {
    // Читаем заголовок запроса браузера целиком (до пустой строки).
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    let head_end = loop {
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = find_headers_end(&buf) {
            break i;
        }
        if buf.len() > 64 * 1024 {
            return Ok(());
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    let (host, port, is_connect) = if method.eq_ignore_ascii_case("CONNECT") {
        let (h, p) = target.rsplit_once(':').unwrap_or((target, "443"));
        (h.to_string(), p.parse::<u16>().unwrap_or(443), true)
    } else {
        // Обычный HTTP: адрес в absolute-URI или в заголовке Host.
        let hostport = target
            .strip_prefix("http://")
            .map(|r| r.split('/').next().unwrap_or("").to_string())
            .or_else(|| {
                head.lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("host:"))
                    .map(|l| l[5..].trim().to_string())
            })
            .unwrap_or_default();
        if hostport.is_empty() {
            return Ok(());
        }
        let (h, p) = hostport.rsplit_once(':').unwrap_or((hostport.as_str(), "80"));
        (h.to_string(), p.parse::<u16>().unwrap_or(80), false)
    };

    let mut upstream = if cfg.is_socks() {
        socks5_connect(&cfg, &host, port).await?
    } else {
        // HTTP-прокси: туннель поднимаем сами, логин подставляем в заголовок.
        http_connect(&cfg, &host, port).await?
    };

    if is_connect {
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
    } else if cfg.is_socks() {
        // Через SOCKS запрос уходит как есть — он адресован уже самому сайту.
        upstream.write_all(&buf).await?;
    } else {
        // Через HTTP-прокси тот же запрос нужно снабдить авторизацией, иначе
        // прокси ответит 407 и браузер снова покажет окно с паролем.
        upstream.write_all(&with_proxy_auth(&buf, head_end, &cfg)).await?;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

/// `Proxy-Authorization: Basic …` для HTTP-прокси с логином.
fn basic_auth(cfg: &ProxyCfg) -> Option<String> {
    let user = cfg.username.clone()?;
    let pass = cfg.password.clone().unwrap_or_default();
    Some(format!("Basic {}", base64(format!("{user}:{pass}").as_bytes())))
}

/// Base64 по учебнику. Отдельная зависимость ради одной строки заголовка —
/// перебор, а спрятанных краевых случаев здесь нет.
fn base64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { A[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Вставить `Proxy-Authorization` сразу после строки запроса.
fn with_proxy_auth(buf: &[u8], head_end: usize, cfg: &ProxyCfg) -> Vec<u8> {
    let Some(auth) = basic_auth(cfg) else { return buf.to_vec() };
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let Some(line_end) = head.find("\r\n") else { return buf.to_vec() };
    let mut out = Vec::with_capacity(buf.len() + auth.len() + 24);
    out.extend_from_slice(&buf[..line_end + 2]);
    out.extend_from_slice(format!("Proxy-Authorization: {auth}\r\n").as_bytes());
    out.extend_from_slice(&buf[line_end + 2..]);
    out
}

/// Туннель через HTTP-прокси: свой CONNECT с авторизацией.
///
/// Ради этого всё и затевалось: Chromium умеет ходить через HTTP-прокси с
/// логином, но пароль спрашивает у человека отдельным окном — каждый раз.
async fn http_connect(cfg: &ProxyCfg, host: &str, port: u16) -> std::io::Result<TcpStream> {
    let hostport = cfg.server.split("://").nth(1).unwrap_or(&cfg.server);
    let mut s = TcpStream::connect(hostport).await?;
    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some(auth) = basic_auth(cfg) {
        req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    req.push_str("Proxy-Connection: Keep-Alive\r\n\r\n");
    s.write_all(req.as_bytes()).await?;

    // Ответ прокси читаем до пустой строки — тело у 200 на CONNECT отсутствует.
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 512];
    loop {
        let n = s.read(&mut tmp).await?;
        if n == 0 {
            return Err(std::io::Error::other("прокси закрыл соединение на CONNECT"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if find_headers_end(&buf).is_some() {
            break;
        }
        if buf.len() > 8192 {
            return Err(std::io::Error::other("прокси ответил чем-то очень длинным"));
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let code = head.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        return Err(std::io::Error::other(format!(
            "прокси отказал на CONNECT: {}",
            head.lines().next().unwrap_or("").trim()
        )));
    }
    Ok(s)
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Минимальный SOCKS5-клиент: приветствие, при необходимости логин/пароль, CONNECT.
async fn socks5_connect(cfg: &ProxyCfg, host: &str, port: u16) -> std::io::Result<TcpStream> {
    let hostport = cfg.server.split("://").nth(1).unwrap_or(&cfg.server);
    let mut s = TcpStream::connect(hostport).await?;
    let has_auth = cfg.username.is_some();

    // greeting
    if has_auth {
        s.write_all(&[0x05, 0x02, 0x00, 0x02]).await?;
    } else {
        s.write_all(&[0x05, 0x01, 0x00]).await?;
    }
    let mut r = [0u8; 2];
    s.read_exact(&mut r).await?;
    if r[0] != 0x05 {
        return Err(std::io::Error::other("не SOCKS5-прокси"));
    }
    if r[1] == 0x02 {
        let u = cfg.username.clone().unwrap_or_default();
        let p = cfg.password.clone().unwrap_or_default();
        let mut req = vec![0x01, u.len() as u8];
        req.extend_from_slice(u.as_bytes());
        req.push(p.len() as u8);
        req.extend_from_slice(p.as_bytes());
        s.write_all(&req).await?;
        let mut a = [0u8; 2];
        s.read_exact(&mut a).await?;
        if a[1] != 0x00 {
            return Err(std::io::Error::other("SOCKS5: логин/пароль отвергнуты"));
        }
    } else if r[1] != 0x00 {
        return Err(std::io::Error::other("SOCKS5: метод авторизации не поддержан"));
    }

    // connect по доменному имени — DNS резолвит прокси (socks5h)
    let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await?;

    let mut head = [0u8; 4];
    s.read_exact(&mut head).await?;
    if head[1] != 0x00 {
        return Err(std::io::Error::other(format!("SOCKS5: отказ {}", head[1])));
    }
    match head[3] {
        0x01 => {
            let mut skip = [0u8; 6];
            s.read_exact(&mut skip).await?;
        }
        0x03 => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len).await?;
            let mut skip = vec![0u8; len[0] as usize + 2];
            s.read_exact(&mut skip).await?;
        }
        0x04 => {
            let mut skip = [0u8; 18];
            s.read_exact(&mut skip).await?;
        }
        _ => return Err(std::io::Error::other("SOCKS5: неизвестный тип адреса")),
    }
    Ok(s)
}

/// Аргумент `--proxy-server` для браузера (при необходимости подняв мост).
async fn browser_proxy_arg(proxy: Option<&str>, stop: &Stop, log: &Log) -> Option<String> {
    let cfg = parse_proxy(proxy?)?;
    // Прокси без логина браузер съедает сам.
    if cfg.username.is_none() {
        return Some(cfg.server.clone());
    }
    // К прокси, до которого сам нужен TLS, мост не подключиться: у нас тут
    // голый TCP. Такие отдаём браузеру как есть.
    if cfg.scheme() == "https" {
        log("[!] HTTPS-прокси с логином: браузер спросит логин и пароль отдельным окном.");
        return Some(cfg.server.clone());
    }
    match auth_bridge(cfg.clone(), stop.clone()).await {
        Ok(addr) => {
            log(&format!(
                "[>] Прокси с логином ({}) — поднял локальный мост {addr}, пароль браузер не спросит",
                cfg.scheme()
            ));
            Some(format!("http://{addr}"))
        }
        Err(e) => {
            log(&format!("[!] Не поднять мост до прокси ({e}) — браузер пойдёт НАПРЯМУЮ"));
            None
        }
    }
}

// ─── CDP ────────────────────────────────────────────────────────────────────

/// Сколько ждём ответа на управляющую команду. Столько браузер не думает
/// никогда — потолок нужен ровно на случай, когда он не ответит вовсе.
const CDP_WAIT: Duration = Duration::from_secs(10);

struct Cdp {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    next_id: i64,
}

impl Cdp {
    async fn connect(ws_url: &str) -> anyhow::Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url).await?;
        Ok(Self { ws, next_id: 1 })
    }

    async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.send(method, params, None, CDP_WAIT).await
    }

    /// Команда в конкретную вкладку. Домены `Page` и `Emulation` живут не в
    /// браузере целиком, а в сессии страницы — без `sessionId` они просто не
    /// адресуются, и подмена отпечатка молча никуда не применяется.
    async fn call_in(&mut self, session: &str, method: &str, params: Value) -> anyhow::Result<Value> {
        self.send(method, params, Some(session), CDP_WAIT).await
    }

    /// Перейти на страницу. Ответ приходит, когда переход НАЧАЛСЯ, а не когда
    /// страница догрузилась, — но по медленному прокси и это небыстро, поэтому
    /// ждём дольше обычного, а молчание считаем «идёт, просто медленно»: окно
    /// уже открыто, и убивать его из-за неответа нечестно.
    async fn navigate(&mut self, session: &str, url: &str) -> anyhow::Result<Option<String>> {
        let params = serde_json::json!({ "url": url });
        match self.send("Page.navigate", params, Some(session), Duration::from_secs(45)).await {
            // `errorText` в ответе — это «переход не состоялся»: не разрешилось
            // имя, не поднялся туннель через прокси, оборвалось соединение.
            // Раньше мы его не читали и говорили «браузер открыт» над окном,
            // в котором висела страница ошибки.
            Ok(v) => {
                Ok(v.get("errorText").and_then(|e| e.as_str()).filter(|e| !e.is_empty()).map(String::from))
            }
            Err(e) if e.to_string().contains("не ответил") => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn send(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
        wait_for: Duration,
    ) -> anyhow::Result<Value> {
        use futures::{SinkExt, StreamExt};
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = serde_json::json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session {
            msg["sessionId"] = Value::String(sid.to_string());
        }
        self.ws.send(tokio_tungstenite::tungstenite::Message::Text(msg.to_string())).await?;
        // Ответы перемешаны с событиями — ждём свой id. С потолком по времени:
        // на некоторые команды сборка браузера может не ответить вовсе, и без
        // него вход зависал бы навсегда вместо того, чтобы пойти дальше.
        let wait = async {
            while let Some(m) = self.ws.next().await {
                let m = m?;
                let Ok(txt) = m.into_text() else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&txt) else { continue };
                if v.get("id").and_then(|x| x.as_i64()) == Some(id) {
                    if let Some(err) = v.get("error") {
                        anyhow::bail!("CDP {method}: {err}");
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            anyhow::bail!("CDP: соединение закрылось до ответа")
        };
        match tokio::time::timeout(wait_for, wait).await {
            Ok(r) => r,
            Err(_) => anyhow::bail!("CDP {method}: браузер не ответил"),
        }
    }

    /// Подключиться к первой вкладке-странице. Возвращает `sessionId`, которым
    /// дальше адресуются команды страницы.
    async fn attach_page(&mut self) -> anyhow::Result<(String, String)> {
        // Вкладку ждём, а не спрашиваем один раз: отладочный порт браузер
        // публикует РАНЬШЕ, чем создаёт первую вкладку, и на быстрой машине
        // первый же вопрос попадает в эту щель — «браузер не открыл ни одной
        // вкладки» на ровном месте.
        let mut target = None;
        for _ in 0..40 {
            let list = self.call("Target.getTargets", serde_json::json!({})).await?;
            target = list
                .get("targetInfos")
                .and_then(|v| v.as_array())
                .and_then(|a| a.iter().find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page")))
                .and_then(|t| t.get("targetId").and_then(|v| v.as_str()).map(String::from));
            if target.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let target = target.ok_or_else(|| anyhow::anyhow!("браузер не открыл ни одной вкладки"))?;
        // flatten — сессия поверх того же сокета, отдельное соединение не нужно.
        let r = self
            .call("Target.attachToTarget", serde_json::json!({ "targetId": &target, "flatten": true }))
            .await?;
        let session = r
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("CDP не выдал сессию вкладки"))?;
        Ok((target, session))
    }

    /// Следующее СОБЫТИЕ (не ответ на команду). `None` — соединение закрылось
    /// или сработал `stop`.
    async fn next_event(&mut self, stop: &Stop) -> Option<Value> {
        use futures::StreamExt;
        loop {
            let m = tokio::select! {
                _ = stop.wait() => return None,
                m = self.ws.next() => m?.ok()?,
            };
            let Ok(txt) = m.into_text() else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&txt) else { continue };
            if v.get("method").is_some() {
                return Some(v);
            }
        }
    }

    /// Жива ли сессия вкладки. Дешёвый вопрос, на который мёртвая сессия
    /// отвечает ошибкой, — нужен, чтобы не сыпать пятью одинаковыми жалобами
    /// подряд, когда вкладки уже нет.
    async fn session_alive(&mut self, session: &str) -> bool {
        self.call_in(session, "Page.getFrameTree", serde_json::json!({})).await.is_ok()
    }

    /// Все куки браузера (не только текущей вкладки).
    async fn cookies(&mut self) -> anyhow::Result<Vec<Value>> {
        let r = self.call("Storage.getCookies", serde_json::json!({})).await?;
        Ok(r.get("cookies").and_then(|c| c.as_array()).cloned().unwrap_or_default())
    }
}

/// Инжект отпечатка — см. `stealth.js`.
const STEALTH_JS: &str = include_str!("stealth.js");

/// Конфиг для инжекта: ровно те поля, которые читает `stealth.js`, и под теми же
/// именами. Персона у двух версий общая, поэтому имена трогать нельзя.
fn payload_config(p: &Persona) -> Value {
    serde_json::json!({
        "screen": p.screen,
        "hardwareConcurrency": p.hardware_concurrency,
        "deviceMemory": p.device_memory,
        "maxTouchPoints": p.max_touch_points,
        "languages": p.languages,
        "gpu": p.gpu,
        "connection": p.connection,
        "noise": p.noise,
    })
}

/// Исходник инжекта с зашитым конфигом — в той форме, которую ждёт
/// `Page.addScriptToEvaluateOnNewDocument`.
fn stealth_source(p: &Persona) -> String {
    format!("({STEALTH_JS})({});", payload_config(p))
}

/// Надеть на вкладку отпечаток персоны.
///
/// До этого браузеру доставались только UA, язык и размер окна, а таймзона,
/// экран, ядра, память, GPU и шум canvas вычислялись на каждый аккаунт и
/// выбрасывались. Тридцать входов с одной машины отдавали один и тот же canvas
/// и одно железо — то есть для антифрода это был один человек с тридцатью
/// аккаунтами, и прокси тут не помогали.
///
/// Порядок важен: инжект регистрируется ДО перехода на страницу, иначе первый
/// же документ успевает прочитать настоящие значения.
async fn wear_persona(
    cdp: &mut Cdp,
    session: &str,
    p: &Persona,
    log: &Log,
    announce: bool,
) -> anyhow::Result<()> {
    cdp.call_in(session, "Page.enable", serde_json::json!({})).await?;
    // Инжект регистрируется РОВНО один раз: он ставит шум canvas, и второй
    // проход положил бы шум поверх шума — отпечаток аккаунта перестал бы быть
    // постоянным и зависел бы от того, сколько раз мы его навесили.
    cdp.call_in(
        session,
        "Page.addScriptToEvaluateOnNewDocument",
        serde_json::json!({ "source": stealth_source(p) }),
    )
    .await?;
    // До перехода страница ещё about:blank — спрашивать её не о чем, важно
    // лишь, что команды приняты. Проверка ждёт настоящего документа.
    for f in apply_emulation(cdp, session, p).await {
        log(&format!("[!] Отпечаток: не применилось — {f}"));
    }
    if announce {
        log(&format!(
            "[>] Отпечаток аккаунта: экран {}x{}, ядер {}, {}, {}",
            p.screen.width,
            p.screen.height,
            p.hardware_concurrency,
            p.timezone_id,
            crate::util::clip(&p.gpu.renderer, 40)
        ));
    }
    Ok(())
}

/// Подмены уровня браузера. Вынесены отдельно, потому что их приходится
/// применять ПОВТОРНО: при переходе на другой сайт Chrome может сменить процесс
/// отрисовки, а вместе с ним теряются и оверрайды. Замерено живьём — до этой
/// правки страница видела настоящую таймзону и настоящее число ядер, хотя
/// команды уходили без ошибок. Инжект так не теряется: он регистрируется на
/// страницу и переживает переходы, поэтому и вызывается один раз.
async fn apply_emulation(cdp: &mut Cdp, session: &str, p: &Persona) -> Vec<String> {
    let mut failed = Vec::new();
    // Метаданные UA — ключевая часть: из них Chrome сам собирает и
    // navigator.userAgentData, и заголовки Sec-CH-UA. Без них UA скажет одно, а
    // client hints другое, и противоречие ловится одним сравнением.
    let m = &p.ua_metadata;
    let ua = cdp
        .call_in(
            session,
            "Emulation.setUserAgentOverride",
            serde_json::json!({
                "userAgent": p.ua,
                "acceptLanguage": p.accept_language,
                "platform": p.platform,
                "userAgentMetadata": m,
            }),
        )
        .await;
    if let Err(e) = ua {
        failed.push(format!("метаданные UA ({e})"));
    }

    // Дальше — по одной необязательной подмене. Каждая может отсутствовать в
    // конкретной сборке браузера, и терять из-за этого всё остальное незачем.
    for (method, params) in [
        ("Emulation.setTimezoneOverride", serde_json::json!({ "timezoneId": p.timezone_id })),
        ("Emulation.setLocaleOverride", serde_json::json!({ "locale": p.locale })),
        (
            "Emulation.setHardwareConcurrencyOverride",
            serde_json::json!({ "hardwareConcurrency": p.hardware_concurrency }),
        ),
        (
            "Emulation.setEmulatedMedia",
            serde_json::json!({
                "features": [{ "name": "prefers-color-scheme", "value": p.color_scheme }]
            }),
        ),
    ] {
        if let Err(e) = cdp.call_in(session, method, params).await {
            // Локаль — оверрайд на ВЕСЬ браузер, а не на вкладку: на второй
            // вкладке та же команда отвечает «уже действует». Это и есть нужное
            // состояние, а не сбой.
            if e.to_string().contains("locale override is already in effect") {
                continue;
            }
            failed.push(format!("{method} ({e})"));
        }
    }
    failed
}

/// Что страница видит на самом деле — по тем полям, которые ставит только
/// `Emulation`. GPU и экран сюда не входят: их подменяет инжект, и он переживает
/// переходы сам.
async fn seen_by_page(cdp: &mut Cdp, session: &str) -> Option<(String, i64, bool)> {
    let js = "JSON.stringify([Intl.DateTimeFormat().resolvedOptions().timeZone,        navigator.hardwareConcurrency,        !!(navigator.userAgentData&&navigator.userAgentData.brands||[]).some(b=>/Chrome/.test(b.brand))])";
    let r = cdp
        .call_in(session, "Runtime.evaluate", serde_json::json!({ "expression": js, "returnByValue": true }))
        .await
        .ok()?;
    parse_seen(&r)
}

/// Разбор ответа `Runtime.evaluate`.
///
/// Вынесено отдельно не для красоты: `call_in` отдаёт уже РАЗВЁРНУТЫЙ `result`
/// ответа CDP, и лишний `result` в пути молча превращал проверку в «страница
/// показывает своё» — на живых окнах отпечаток стоял, а в лог шла тревога.
fn parse_seen(result: &Value) -> Option<(String, i64, bool)> {
    let v: Value = serde_json::from_str(result.get("result")?.get("value")?.as_str()?).ok()?;
    let a = v.as_array()?;
    Some((a.first()?.as_str()?.to_string(), a.get(1)?.as_i64()?, a.get(2)?.as_bool()?))
}

/// Надеть подмены и УБЕДИТЬСЯ, что страница их видит.
///
/// `Page.navigate` возвращается, когда переход только начался. Документ
/// коммитится позже и, как правило, в новом процессе отрисовки — вместе со
/// старым пропадают все `Emulation.*`. Раскладка сразу после `navigate`
/// попадает в процесс, который вот-вот выбросят: команды уходят в живую сессию,
/// ошибок нет ни одной, а страница видит настоящие таймзону и число ядер.
/// Замерено живьём: два аккаунта с разными персонами показывали 16 ядер и
/// Asia/Yekaterinburg — железо и часовой пояс этого компьютера.
///
/// Поэтому не «применили и пошли дальше», а «применили и переспросили
/// страницу», пока она не ответит нужным.
async fn settle_persona(cdp: &mut Cdp, session: &str, p: &Persona, log: &Log) -> Settled {
    let mut session = session.to_string();
    let mut last_fail = Vec::new();
    for attempt in 0..12 {
        if attempt > 0 && !cdp.session_alive(&session).await {
            match cdp.attach_page().await {
                Ok((_, fresh)) => session = fresh,
                // Вкладки нет и новую не дают — значит окна уже нет. Человек
                // закрыл его сам, пока страница грузилась: жаловаться не на что.
                Err(_) => return Settled::WindowGone,
            }
        }
        last_fail = apply_emulation(cdp, &session, p).await;
        if let Some((tz, cores, chrome_brand)) = seen_by_page(cdp, &session).await {
            if tz == p.timezone_id && cores == p.hardware_concurrency && chrome_brand {
                return Settled::Ok;
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    let why = if last_fail.is_empty() {
        "страница показывает своё".to_string()
    } else {
        last_fail.join(", ")
    };
    log(&format!("[!] Отпечаток НЕ встал целиком ({why}) — окно светит настоящей машиной."));
    Settled::NotApplied
}

/// Чем кончилась попытка надеть отпечаток на открытую страницу.
#[derive(Debug, PartialEq, Eq)]
enum Settled {
    /// Страница показывает персону.
    Ok,
    /// Окно живо, но подмены не удержались — об этом уже сказано в логе.
    NotApplied,
    /// Окна больше нет: человек закрыл его, не дожидаясь загрузки.
    WindowGone,
}

/// Оставить в окне ровно одну вкладку — ту, которую сейчас откроем.
///
/// Профиль у аккаунта постоянный, и браузер норовит вернуть в него всё, что
/// было открыто в прошлый раз. Плюс если окно этого аккаунта уже открыто,
/// Chrome не поднимает второе, а доклеивает вкладку в старое. И в том, и в
/// другом случае человек получает десяток чужих вкладок, среди которых теряется
/// та, ради которой окно и открывали.
async fn close_other_tabs(cdp: &mut Cdp, keep: &str) {
    let Ok(list) = cdp.call("Target.getTargets", serde_json::json!({})).await else { return };
    let others: Vec<String> = list
        .get("targetInfos")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
                .filter_map(|t| t.get("targetId").and_then(|v| v.as_str()))
                .filter(|id| *id != keep)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    for id in &others {
        let _ = cdp.call("Target.closeTarget", serde_json::json!({ "targetId": id })).await;
    }
}

/// Открыть НОВУЮ вкладку под той же персоной и увести её на адрес.
///
/// Отпечаток надевается ДО первого документа: инжект работает только на новых
/// документах, и вкладка, созданная сразу с адресом, его бы не увидела.
/// Возвращает id вкладки — чтобы [`hold_persona`] не одел её второй раз.
async fn open_dressed_tab(cdp: &mut Cdp, p: &Persona, url: &str, log: &Log) -> anyhow::Result<String> {
    let created = cdp.call("Target.createTarget", serde_json::json!({ "url": "about:blank" })).await?;
    let id = created
        .get("targetId")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("CDP не создал вкладку"))?;
    let att =
        cdp.call("Target.attachToTarget", serde_json::json!({ "targetId": &id, "flatten": true })).await?;
    let session = att
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("CDP не выдал сессию вкладки"))?;
    wear_persona(cdp, &session, p, log, false).await?;
    if let Ok(Some(err)) = cdp.navigate(&session, url).await {
        log(&format!("[!] Страница не открылась: {err} — похоже на прокси."));
    }
    settle_persona(cdp, &session, p, log).await;
    Ok(id)
}

/// Держать соединение открытым, пока живёт окно, и одевать новые вкладки.
///
/// `Emulation.*` — это оверрайды ОТЛАДОЧНОЙ СЕССИИ, а не настройки браузера:
/// как только клиент CDP отсоединяется, Chrome возвращает настоящие значения.
/// Замерено живьём: с подключённым отладчиком страница показывала
/// `Europe/Moscow / 4`, а через мгновение после закрытия сокета — реальные
/// `Asia/Yekaterinburg / 16`.
///
/// Раньше `open_as` закрывал соединение сразу после навигации, и окно,
/// открытое «под аккаунтом», почти сразу начинало светить настоящими таймзоной,
/// числом ядер и метаданными UA. Инжект при этом оставался — то есть GPU и
/// память были поддельными, а часовой пояс настоящим. Такое противоречие
/// заметнее, чем честное отсутствие подмен.
async fn hold_persona(mut cdp: Cdp, dressed: Vec<String>, p: Persona, log: Log, stop: Stop) {
    // Новая вкладка открывается без сессии, а значит и без подмен. Ловим её
    // появление и одеваем так же, как первую.
    if cdp.call("Target.setDiscoverTargets", serde_json::json!({ "discover": true })).await.is_err() {
        // Не смогли подписаться — всё равно держим сокет: подмены на уже
        // одетой вкладке живут ровно до его закрытия.
        stop.wait().await;
        return;
    }
    while let Some(ev) = cdp.next_event(&stop).await {
        if ev.get("method").and_then(|m| m.as_str()) != Some("Target.targetCreated") {
            continue;
        }
        let info = ev.get("params").and_then(|x| x.get("targetInfo"));
        if info.and_then(|i| i.get("type")).and_then(|t| t.as_str()) != Some("page") {
            continue;
        }
        let Some(id) = info.and_then(|i| i.get("targetId")).and_then(|t| t.as_str()).map(String::from) else {
            continue;
        };
        // Уже одетые вкладки пропускаем: подписка сообщает и про существующие,
        // а второй инжект положил бы шум canvas поверх шума — отпечаток
        // перестал бы быть постоянным.
        if dressed.contains(&id) {
            continue;
        }
        let attach =
            cdp.call("Target.attachToTarget", serde_json::json!({ "targetId": id, "flatten": true })).await;
        let Some(sid) = attach.ok().and_then(|r| r.get("sessionId")?.as_str().map(String::from)) else {
            continue;
        };
        if wear_persona(&mut cdp, &sid, &p, &log, false).await.is_err() {
            log("[!] Отпечаток: новая вкладка осталась без подмен.");
        }
    }
}

/// Куки mail.ru из ответа CDP → строка заголовка `a=1; b=2`.
fn cookies_for_mailru(list: &[Value]) -> String {
    let mut jar: Vec<(String, String)> = Vec::new();
    for c in list {
        let domain = c.get("domain").and_then(|v| v.as_str()).unwrap_or("");
        if !domain.contains("mail.ru") {
            continue;
        }
        let (Some(name), Some(value)) =
            (c.get("name").and_then(|v| v.as_str()), c.get("value").and_then(|v| v.as_str()))
        else {
            continue;
        };
        if let Some(slot) = jar.iter_mut().find(|(n, _)| n == name) {
            slot.1 = value.to_string();
        } else {
            jar.push((name.to_string(), value.to_string()));
        }
    }
    jar_to_string(&jar)
}

/// Открыть браузер ПОД аккаунтом: те же куки, тот же отпечаток, тот же прокси.
///
/// Нужен для ручной работы — посмотреть, что видит аккаунт, разобрать капчу,
/// поправить профиль. Куки ставим ДО открытия страницы (`Storage.setCookies`
/// на уровне браузера, без возни с сессиями вкладок), а саму страницу открываем
/// уже после — иначе она успеет загрузиться гостем.
///
/// Окно живёт своей жизнью: закроет его человек, а не мы. Мост до SOCKS-прокси
/// держим ровно столько же.
pub async fn open_as(
    root: &Path,
    persona: &Persona,
    proxy: Option<&str>,
    profile_dir: &Path,
    cookies: &str,
    url: &str,
    log: &Log,
) -> anyhow::Result<()> {
    let chrome = chrome_path(root).ok_or_else(|| {
        anyhow::anyhow!(
            "не найден браузер. Он должен лежать в папке browsers рядом с программой — скачай архив с релиза целиком, там она уже внутри"
        )
    })?;
    std::fs::create_dir_all(profile_dir).ok();
    // Файл от прошлого запуска: браузер удаляет его, когда закрывается сам, но
    // не когда его убили. Оставить — значит подключиться к мёртвому порту.
    let _ = std::fs::remove_file(devtools_port_file(profile_dir));

    // Своя «жизнь» моста и наблюдателя за окном.
    let alive = Stop::new();

    let mut cmd = tokio::process::Command::new(&chrome);
    cmd.arg(format!("--user-data-dir={}", profile_dir.display()))
        // 0 = порт выбирает браузер и пишет его в свой профиль, см. wait_devtools.
        .arg("--remote-debugging-port=0")
        .arg("--remote-allow-origins=*")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        // Прошлый раз окно могли закрыть жёстко — тогда браузер предлагает
        // «восстановить страницы». Нам восстанавливать нечего: вкладку мы
        // открываем свою.
        .arg("--hide-crash-restore-bubble")
        .arg("--disable-blink-features=AutomationControlled")
        // WebRTC умеет ходить по UDP мимо HTTP-прокси и через STUN отдать
        // настоящий IP. Для аккаунта на прокси это мгновенный деанон.
        .arg("--force-webrtc-ip-handling-policy=disable_non_proxied_udp")
        // UA ставим ещё и флагом: первый сетевой запрос успевает уйти раньше,
        // чем применится подмена по CDP, и на нём светился бы настоящий.
        .arg(format!("--user-agent={}", persona.ua))
        .arg(format!("--lang={}", persona.locale))
        .arg(format!("--accept-lang={}", persona.accept_language))
        .arg(format!("--window-size={},{}", persona.window.width, persona.window.height))
        .arg("about:blank");
    if let Some(p) = browser_proxy_arg(proxy, &alive, log).await {
        log(&format!("[>] Браузер через прокси: {}", crate::proxy::mask_proxy(&p)));
        cmd.arg(format!("--proxy-server={p}"));
    }
    // DPR флагом = настоящий DPR. Подменять его из JS нельзя: разойдутся
    // window.devicePixelRatio и matchMedia('(resolution: Ndppx)') — дешёвая проверка.
    if (persona.dpr - 1.0).abs() > f64::EPSILON {
        cmd.arg(format!("--force-device-scale-factor={}", persona.dpr));
    }
    cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("не запустить браузер: {e}"))?;

    let mut cdp = match wait_devtools(profile_dir, &mut child).await {
        Ok(ws) => match Cdp::connect(&ws).await {
            Ok(c) => c,
            Err(e) => {
                let _ = child.kill().await;
                alive.stop();
                return Err(e);
            }
        },
        Err(e) => {
            let _ = child.kill().await;
            alive.stop();
            return Err(e);
        }
    };

    let jar = crate::accounts::parse_cookie_jar(cookies);
    let list: Vec<Value> = jar
        .iter()
        .map(|(n, v)| {
            serde_json::json!({ "name": n, "value": v, "domain": ".mail.ru", "path": "/", "secure": true })
        })
        .collect();
    let n = list.len();
    if let Err(e) = cdp.call("Storage.setCookies", serde_json::json!({ "cookies": list })).await {
        let _ = child.kill().await;
        alive.stop();
        return Err(anyhow::anyhow!("не удалось поставить куки: {e}"));
    }
    // Дальше работаем в ТОЙ ЖЕ пустой вкладке, а не открываем новую: отпечаток
    // надо надеть до первого документа, а `Target.createTarget` с адресом
    // навигирует сразу — инжект уже не успел бы.
    let (target, session) = match cdp.attach_page().await {
        Ok(pair) => pair,
        Err(e) => {
            let _ = child.kill().await;
            alive.stop();
            return Err(e);
        }
    };
    // Чистим ДО отпечатка и перехода: окно должно открыться с одной вкладкой,
    // а не с прошлогодним хвостом.
    close_other_tabs(&mut cdp, &target).await;
    if let Err(e) = wear_persona(&mut cdp, &session, persona, log, true).await {
        log(&format!("[!] Отпечаток не встал целиком ({e}) — открываю как есть."));
    }
    match cdp.navigate(&session, url).await {
        Err(e) => {
            let _ = child.kill().await;
            alive.stop();
            return Err(anyhow::anyhow!("не удалось открыть страницу: {e}"));
        }
        Ok(Some(err)) => log(&format!("[!] Страница не открылась: {err} — похоже на прокси.")),
        Ok(None) => {}
    }
    // Переход мог сменить процесс отрисовки, а то и саму вкладку.
    if settle_persona(&mut cdp, &session, persona, log).await == Settled::WindowGone {
        // Врать «браузер открыт» про закрытое окно незачем.
        log("[x] Окно закрыли, не дождавшись страницы.");
        alive.stop();
        return Ok(());
    }
    log(&format!("[+] Браузер открыт, кук перенесено: {n}"));

    // Соединение НЕ закрываем: вместе с ним исчезли бы все `Emulation.*`.
    // Оно живёт ровно столько же, сколько окно.
    let persona = persona.clone();
    let log_held = log.clone();
    let held = alive.clone();
    tokio::spawn(async move {
        hold_persona(cdp, vec![target], persona, log_held, held).await;
    });
    // Ждём, пока человек закроет окно, — чтобы прибрать мост, отпустить
    // соединение и не оставлять зомби-процесс.
    tokio::spawn(async move {
        let _ = child.wait().await;
        alive.stop();
    });
    Ok(())
}

/// Открыть браузер и дождаться входа. Возвращает снятые куки.
///
/// `stop` = кнопка «Отмена» в интерфейсе: она закрывает и браузер, и мост.
pub async fn login_and_harvest(
    root: &Path,
    persona: &Persona,
    proxy: Option<&str>,
    profile_dir: &Path,
    log: &Log,
    stop: &Stop,
) -> anyhow::Result<Harvest> {
    let chrome = chrome_path(root).ok_or_else(|| {
        anyhow::anyhow!(
            "не найден браузер. Он должен лежать в папке browsers рядом с программой — скачай архив с релиза целиком, там она уже внутри"
        )
    })?;
    // Профиль входа — одноразовый, и начинать надо с чистого. Остаться он
    // может, если программу закрыли с открытым окном входа; в нём лежат куки
    // прошлой сессии, и вход бы «удался» сам собой, не спросив человека, — а
    // снялись бы старые куки вместо новых.
    let _ = std::fs::remove_dir_all(profile_dir);
    std::fs::create_dir_all(profile_dir).ok();
    let _ = std::fs::remove_file(devtools_port_file(profile_dir));

    let mut cmd = tokio::process::Command::new(&chrome);
    cmd.arg(format!("--user-data-dir={}", profile_dir.display()))
        // 0 = порт выбирает браузер и пишет его в свой профиль, см. wait_devtools.
        .arg("--remote-debugging-port=0")
        .arg("--remote-allow-origins=*")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        // Прошлый раз окно могли закрыть жёстко — тогда браузер предлагает
        // «восстановить страницы». Нам восстанавливать нечего: вкладку мы
        // открываем свою.
        .arg("--hide-crash-restore-bubble")
        .arg("--disable-blink-features=AutomationControlled")
        // WebRTC умеет ходить по UDP мимо HTTP-прокси и через STUN отдать
        // настоящий IP. Для аккаунта на прокси это мгновенный деанон.
        .arg("--force-webrtc-ip-handling-policy=disable_non_proxied_udp")
        // UA ставим ещё и флагом: первый сетевой запрос успевает уйти раньше,
        // чем применится подмена по CDP, и на нём светился бы настоящий.
        .arg(format!("--user-agent={}", persona.ua))
        .arg(format!("--lang={}", persona.locale))
        .arg(format!("--accept-lang={}", persona.accept_language))
        .arg(format!("--window-size={},{}", persona.window.width, persona.window.height))
        .arg(format!("--window-position={},{}", persona.window.left, persona.window.top))
        // Стартуем с пустой вкладки: отпечаток надо надеть ДО первого документа,
        // а страница входа, открытая флагом, начала бы грузиться раньше.
        .arg("about:blank");
    // Мост живёт столько же, сколько окно: `stop` — это кнопка «Отмена», и её
    // снимают сразу после входа, а окно после этого остаётся открытым.
    let alive = Stop::new();
    if let Some(p) = browser_proxy_arg(proxy, &alive, log).await {
        log(&format!("[>] Браузер через прокси: {}", crate::proxy::mask_proxy(&p)));
        cmd.arg(format!("--proxy-server={p}"));
    }

    // Chromium щедро сыплет в stderr предупреждениями про песочницу и GCM.
    // Пользователю это не нужно, а в GUI консоли и нет — глушим.
    // DPR флагом = настоящий DPR. Подменять его из JS нельзя: разойдутся
    // window.devicePixelRatio и matchMedia('(resolution: Ndppx)') — дешёвая проверка.
    if (persona.dpr - 1.0).abs() > f64::EPSILON {
        cmd.arg(format!("--force-device-scale-factor={}", persona.dpr));
    }
    cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());

    log("[>] Открываю браузер — войди в аккаунт вручную. Куки снимутся, когда закроешь окно.");
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("не запустить браузер: {e}"))?;

    let mut cdp = match wait_devtools(profile_dir, &mut child).await {
        Ok(ws) => match Cdp::connect(&ws).await {
            Ok(c) => c,
            Err(e) => {
                let _ = child.kill().await;
                alive.stop();
                return Err(e);
            }
        },
        Err(e) => {
            let _ = child.kill().await;
            alive.stop();
            return Err(e);
        }
    };

    // Персона на вкладку — и только потом переход на страницу входа. Именно
    // здесь мы и показываемся сайту живым браузером: тридцать входов с одной
    // машины должны выглядеть как тридцать разных машин.
    // Первой вкладкой — Ответы, второй — страница входа. Порядок именно такой:
    // вход браузер сам делает активным, а когда человек его закончит, слева уже
    // лежит то, ради чего аккаунт и заводили.
    let (site_tab, site_session) = match cdp.attach_page().await {
        Ok((target, session)) => {
            close_other_tabs(&mut cdp, &target).await;
            if let Err(e) = wear_persona(&mut cdp, &session, persona, log, true).await {
                log(&format!("[!] Отпечаток не встал целиком ({e}) — вход как есть."));
            }
            if let Ok(Some(err)) = cdp.navigate(&session, SITE_URL).await {
                log(&format!("[!] Ответы не открылись: {err} — похоже на прокси."));
            }
            settle_persona(&mut cdp, &session, persona, log).await;
            (target, session)
        }
        Err(e) => {
            let _ = child.kill().await;
            alive.stop();
            return Err(e);
        }
    };

    let login_tab = match open_dressed_tab(&mut cdp, persona, LOGIN_URL, log).await {
        Ok(id) => Some(id),
        Err(e) => {
            // Без второй вкладки входить негде — уводим туда первую.
            log(&format!("[!] Не открылась вкладка входа ({e}) — открываю вход в этой же."));
            if let Ok(Some(err)) = cdp.navigate(&site_session, LOGIN_URL).await {
                log(&format!("[!] Страница входа не открылась: {err} — похоже на прокси."));
            }
            settle_persona(&mut cdp, &site_session, persona, log).await;
            None
        }
    };

    // Куки снимаются НЕ в момент входа, а те, что есть на момент закрытия окна.
    //
    // После входа человек обычно ещё что-то доделывает руками, и снимок «в
    // первую же секунду» этого не застаёт. Но и прочитать куки из закрытого
    // браузера нельзя — вместе с окном умирает отладчик. Поэтому держим свежий
    // снимок: каждый круг перечитываем, а в дело идёт последний.
    let mut harvested = String::new();
    let mut announced = false;
    let mut keep_open = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(60 * 60);
    loop {
        if stop.is_stopped() {
            log("[x] Вход отменён.");
            break;
        }
        if std::time::Instant::now() > deadline {
            log("[!] Час прошёл — перестаю ждать. Окно оставляю открытым.");
            keep_open = true;
            break;
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            log("[>] Окно закрыто — снимаю куки.");
            break;
        }
        match cdp.cookies().await {
            Ok(list) => {
                let jar = cookies_for_mailru(&list);
                if looks_logged_in(&jar) {
                    harvested = jar;
                    if !announced {
                        announced = true;
                        log("[+] Сессия есть. Доделывай что нужно — куки снимутся, когда закроешь окно.");
                        // Ответы открывались гостем, до входа. Перечитываем их
                        // под свежей сессией, иначе человек смотрит на страницу,
                        // где он не залогинен.
                        if login_tab.is_some() {
                            if let Ok(Some(err)) = cdp.navigate(&site_session, SITE_URL).await {
                                log(&format!("[!] Ответы не перечитались: {err}"));
                            }
                            settle_persona(&mut cdp, &site_session, persona, log).await;
                        }
                    }
                }
            }
            Err(_) => {
                // Браузер мог закрыться между проверками — переспросим на следующем круге.
            }
        }
        if stop.sleep_ms(1500).await {
            break;
        }
    }

    // Отмена и «не вошли» окно закрывают: держать его незачем. По закрытию
    // закрывать уже нечего, а после часа ожидания окно остаётся человеку.
    if !keep_open {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    if harvested.is_empty() {
        alive.stop();
        let _ = std::fs::remove_dir_all(profile_dir);
        anyhow::bail!("вход не завершён — куки сессии не появились");
    }

    if matches!(child.try_wait(), Ok(Some(_))) {
        // Окна уже нет — прибираем сразу.
        alive.stop();
        let _ = std::fs::remove_dir_all(profile_dir);
    } else {
        // Окно живо. Соединение НЕ закрываем: вместе с ним пропали бы все
        // `Emulation.*`, и окно начало бы светить настоящей машиной.
        let mut dressed = vec![site_tab];
        if let Some(id) = login_tab {
            dressed.push(id);
        }
        {
            let persona = persona.clone();
            let log = log.clone();
            let alive = alive.clone();
            tokio::spawn(async move {
                hold_persona(cdp, dressed, persona, log, alive).await;
            });
        }
        // Окно закроют — гасим мост и убираем временный профиль.
        let alive = alive.clone();
        let dir = profile_dir.to_path_buf();
        tokio::spawn(async move {
            let _ = child.wait().await;
            alive.stop();
            let _ = std::fs::remove_dir_all(dir);
        });
    }
    Ok(Harvest { cookies: harvested, ua: persona.ua.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ответ снят с живого Chromium. Путь до значения тут легко промахнуть на
    /// один уровень, и промах не виден: проверка отпечатка просто всегда
    /// говорит «не встал», хотя на странице всё стоит.
    #[test]
    fn the_page_answer_is_unwrapped_at_the_right_depth() {
        let ok = serde_json::json!({
            "result": { "type": "string", "value": "[\"Europe/Moscow\",8,true]" }
        });
        assert_eq!(parse_seen(&ok), Some(("Europe/Moscow".into(), 8, true)));

        // Лишний уровень — это ответ ЦЕЛИКОМ, а не развёрнутый `result`.
        let wrapped = serde_json::json!({ "result": ok.clone() });
        assert_eq!(parse_seen(&wrapped), None);

        // Страница ответила не тем — молчим, а не паникуем.
        assert_eq!(parse_seen(&serde_json::json!({ "result": { "value": "не json" } })), None);
        assert_eq!(parse_seen(&serde_json::json!({})), None);
        assert_eq!(parse_seen(&serde_json::json!({ "result": { "value": "[\"Europe/Moscow\"]" } })), None);
    }

    /// Формат файла снят с живого Chromium: порт, затем путь сокета.
    ///
    /// Читать его нужно целиком и с проверками: браузер создаёт файл пустым и
    /// дописывает уже потом, так что на полуготовом мы обязаны сказать «ещё
    /// нет», а не собрать битый адрес и подключиться неизвестно куда.
    #[test]
    fn debugger_address_is_read_from_the_browsers_own_file() {
        let dir = std::env::temp_dir().join(format!("otvetware-devtools-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = devtools_port_file(&dir);
        assert_eq!(file.file_name().unwrap(), "DevToolsActivePort");

        assert_eq!(read_devtools_url(&file), None, "файла ещё нет");

        std::fs::write(&file, "").unwrap();
        assert_eq!(read_devtools_url(&file), None, "пустой файл — браузер только начал");

        std::fs::write(
            &file, "56731
",
        )
        .unwrap();
        assert_eq!(read_devtools_url(&file), None, "порт есть, пути ещё нет");

        std::fs::write(
            &file,
            "56731
/devtools/browser/9f2c
",
        )
        .unwrap();
        assert_eq!(read_devtools_url(&file).as_deref(), Some("ws://127.0.0.1:56731/devtools/browser/9f2c"));

        std::fs::write(
            &file,
            "0
/devtools/browser/9f2c
",
        )
        .unwrap();
        assert_eq!(read_devtools_url(&file), None, "нулевой порт — браузер не поднял отладчик");

        std::fs::write(
            &file,
            "мусор
тоже мусор
",
        )
        .unwrap();
        assert_eq!(read_devtools_url(&file), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// В инжект должны уезжать значения ИМЕННО этой персоны. Если конфиг
    /// разъедется с тем, что читает `stealth.js`, подмена молча не сработает:
    /// ошибки не будет, просто у всех аккаунтов снова одно железо.
    #[test]
    fn the_injected_fingerprint_belongs_to_the_persona() {
        let root = std::path::Path::new(".");
        let a = crate::persona::build_persona("Аккаунт 1", &crate::persona::PersonaOpts::default(), root);
        let b = crate::persona::build_persona("Аккаунт 2", &crate::persona::PersonaOpts::default(), root);

        let cfg = payload_config(&a);
        // Ровно те восемь полей, которые читает stealth.js, и под теми же именами.
        let mut keys: Vec<&str> = cfg.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "connection",
                "deviceMemory",
                "gpu",
                "hardwareConcurrency",
                "languages",
                "maxTouchPoints",
                "noise",
                "screen"
            ],
            "состав конфига разошёлся с тем, что ждёт stealth.js"
        );
        assert_eq!(cfg["hardwareConcurrency"], a.hardware_concurrency);
        assert_eq!(cfg["deviceMemory"], a.device_memory);
        assert_eq!(cfg["screen"]["width"], a.screen.width);
        assert_eq!(cfg["gpu"]["renderer"], a.gpu.renderer);

        // Исходник — самовызывающаяся функция с зашитым конфигом, и в ней видны
        // значения аккаунта, а не заглушки.
        let src = stealth_source(&a);
        assert!(
            src.starts_with("(//")
                || src.starts_with("(function")
                || src.starts_with(
                    "(
"
                ),
            "{}",
            &src[..40]
        );
        assert!(src.trim_end().ends_with(");"), "инжект не самовызывающийся");
        assert!(src.contains(&a.gpu.renderer), "GPU персоны не попал в инжект");
        assert!(src.contains(&a.screen.width.to_string()), "экран персоны не попал в инжект");

        // И у другого аккаунта отпечаток другой — иначе весь слой бессмыслен.
        assert_ne!(payload_config(&a), payload_config(&b), "две персоны дали один отпечаток");
    }

    #[test]
    fn picks_only_mailru_cookies() {
        let list = vec![
            serde_json::json!({ "domain": ".mail.ru", "name": "Mpop", "value": "1" }),
            serde_json::json!({ "domain": ".mail.ru", "name": "Auth-Token", "value": "9" }),
            serde_json::json!({ "domain": ".google.com", "name": "NID", "value": "2" }),
            serde_json::json!({ "domain": "otvet.mail.ru", "name": "oid", "value": "3" }),
        ];
        let jar = cookies_for_mailru(&list);
        assert!(jar.contains("Mpop=1"));
        assert!(jar.contains("oid=3"));
        assert!(!jar.contains("NID"));
        assert!(looks_logged_in(&jar));
    }

    #[test]
    fn base64_matches_the_textbook() {
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b""), "");
    }

    /// Главное про мост: браузер ходит без пароля, а логин подставляем мы.
    /// Раньше для HTTP-прокси моста не было вовсе, и Chromium показывал окно
    /// «укажите имя пользователя и пароль» на каждый запуск.
    #[tokio::test]
    async fn http_proxy_bridge_adds_credentials() {
        use parking_lot::Mutex as PMutex;
        use std::sync::Arc;

        // Фальшивый вышестоящий HTTP-прокси: запоминает запрос и эхоит данные.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = listener.local_addr().unwrap();
        let seen: Arc<PMutex<String>> = Arc::new(PMutex::new(String::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 2048];
            let n = s.read(&mut buf).await.unwrap();
            *seen2.lock() = String::from_utf8_lossy(&buf[..n]).to_string();
            s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.unwrap();
            let mut b = vec![0u8; 64];
            let n = s.read(&mut b).await.unwrap();
            s.write_all(&b[..n]).await.unwrap();
        });

        let cfg = parse_proxy(&format!("http://user:pass@{upstream}")).unwrap();
        let stop = Stop::new();
        let bridge = auth_bridge(cfg, stop.clone()).await.unwrap();

        let mut c = TcpStream::connect(&bridge).await.unwrap();
        c.write_all(b"CONNECT otvet.mail.ru:443 HTTP/1.1\r\nHost: otvet.mail.ru:443\r\n\r\n").await.unwrap();
        let mut resp = vec![0u8; 128];
        let n = c.read(&mut resp).await.unwrap();
        assert!(
            String::from_utf8_lossy(&resp[..n]).starts_with("HTTP/1.1 200"),
            "мост не подтвердил туннель: {}",
            String::from_utf8_lossy(&resp[..n])
        );

        c.write_all(b"ping").await.unwrap();
        let mut back = vec![0u8; 16];
        let n = c.read(&mut back).await.unwrap();
        assert_eq!(&back[..n], b"ping", "данные через туннель не прошли");

        let head = seen.lock().clone();
        assert!(head.starts_with("CONNECT otvet.mail.ru:443"), "не тот запрос к прокси: {head}");
        assert!(head.contains("Proxy-Authorization: Basic dXNlcjpwYXNz"), "мост не подставил логин: {head}");
        stop.stop();
    }

    #[test]
    fn parses_http_head_end() {
        let head = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(find_headers_end(head), Some(head.len()));
        assert!(find_headers_end(b"GET / HTTP/1.1\r\n").is_none());
    }
}
