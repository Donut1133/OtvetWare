//! cdp.rs — вход в аккаунт через настоящий браузер и съём кук по CDP.
//!
//! Работать боту браузер не нужен — только войти. Схема:
//!   1. поднимаем Chromium из `browsers/` со своим профилем и `--remote-debugging-port`;
//!   2. человек логинится руками (капчу и СМС никто за него не решает);
//!   3. как только в куках появляются `Mpop`/`Auth-Token`, снимаем их через
//!      `Storage.getCookies` и закрываем окно.
//!
//! Про прокси честно: Chromium НЕ умеет SOCKS с логином/паролем («Browser does
//! not support socks5 proxy authentication»). Поэтому для таких прокси мы сами
//! поднимаем локальный HTTP-CONNECT мост, который тоннелирует через SOCKS5 —
//! браузер ходит на `127.0.0.1:<порт>` без авторизации. Без этого вход шёл бы с
//! реального IP, а работа потом — с прокси: сессия, снятая с одного IP и
//! используемая с другого, для антифрода mail.ru выглядит именно так, как
//! выглядит угон аккаунта.

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

pub fn chrome_path(root: &Path) -> Option<PathBuf> {
    // Сначала — сборка, лежащая рядом (та же, чью версию обещает персона).
    let bundled = root.join("browsers");
    if let Ok(rd) = std::fs::read_dir(&bundled) {
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
    // Затем — системный Chrome.
    for p in [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    ] {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Свободный порт от ОС (её же и просим выбрать, чтобы не угадывать).
async fn free_port() -> std::io::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0").await?;
    let p = l.local_addr()?.port();
    drop(l);
    Ok(p)
}

/// Локальный HTTP-CONNECT мост до SOCKS5-прокси с авторизацией.
/// Возвращает адрес `127.0.0.1:port`; живёт, пока не сработает `stop`.
async fn socks_bridge(cfg: ProxyCfg, stop: Stop) -> std::io::Result<String> {
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

    let mut upstream = socks5_connect(&cfg, &host, port).await?;
    if is_connect {
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
    } else {
        // Прокидываем исходный запрос как есть.
        upstream.write_all(&buf).await?;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
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
    if cfg.is_socks() && cfg.username.is_some() {
        match socks_bridge(cfg.clone(), stop.clone()).await {
            Ok(addr) => {
                log(&format!("🌉 SOCKS с авторизацией — поднял локальный мост {addr}"));
                Some(format!("http://{addr}"))
            }
            Err(e) => {
                log(&format!("⚠️  Не поднять мост до SOCKS ({e}) — браузер пойдёт НАПРЯМУЮ"));
                None
            }
        }
    } else if cfg.username.is_some() {
        // HTTP-прокси с логином: Chromium сам спросит логин/пароль окном.
        log("ℹ️  HTTP-прокси с логином: браузер спросит логин и пароль отдельным окном.");
        Some(cfg.server.clone())
    } else {
        Some(cfg.server.clone())
    }
}

// ─── CDP ────────────────────────────────────────────────────────────────────

struct Cdp {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    next_id: i64,
}

impl Cdp {
    async fn connect(port: u16) -> anyhow::Result<Self> {
        // Адрес отладчика браузер публикует по HTTP; ждём, пока поднимется.
        let http = reqwest::Client::new();
        let mut ws_url = None;
        for _ in 0..60 {
            if let Ok(r) = http.get(format!("http://127.0.0.1:{port}/json/version")).send().await {
                if let Ok(j) = r.json::<Value>().await {
                    if let Some(u) = j.get("webSocketDebuggerUrl").and_then(|v| v.as_str()) {
                        ws_url = Some(u.to_string());
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let url = ws_url.ok_or_else(|| anyhow::anyhow!("браузер не поднял отладочный порт"))?;
        let (ws, _) = tokio_tungstenite::connect_async(url).await?;
        Ok(Self { ws, next_id: 1 })
    }

    async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        use futures::{SinkExt, StreamExt};
        let id = self.next_id;
        self.next_id += 1;
        let msg = serde_json::json!({ "id": id, "method": method, "params": params });
        self.ws.send(tokio_tungstenite::tungstenite::Message::Text(msg.to_string())).await?;
        // Ответы перемешаны с событиями — ждём свой id.
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
    }

    /// Все куки браузера (не только текущей вкладки).
    async fn cookies(&mut self) -> anyhow::Result<Vec<Value>> {
        let r = self.call("Storage.getCookies", serde_json::json!({})).await?;
        Ok(r.get("cookies").and_then(|c| c.as_array()).cloned().unwrap_or_default())
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
        anyhow::anyhow!("не найден Chromium: положи сборку в browsers/ или установи Google Chrome")
    })?;
    let port = free_port().await?;
    std::fs::create_dir_all(profile_dir).ok();

    let mut cmd = tokio::process::Command::new(&chrome);
    cmd.arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--remote-allow-origins=*")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-blink-features=AutomationControlled")
        .arg(format!("--user-agent={}", persona.ua))
        .arg(format!("--lang={}", persona.locale))
        .arg(format!("--window-size={},{}", persona.window.width, persona.window.height))
        .arg(format!("--window-position={},{}", persona.window.left, persona.window.top))
        .arg("https://account.mail.ru/login");
    if let Some(p) = browser_proxy_arg(proxy, stop, log).await {
        log(&format!("🌐 Браузер через прокси: {}", crate::proxy::mask_proxy(&p)));
        cmd.arg(format!("--proxy-server={p}"));
    }

    // Chromium щедро сыплет в stderr предупреждениями про песочницу и GCM.
    // Пользователю это не нужно, а в GUI консоли и нет — глушим.
    cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());

    log("🪟 Открываю браузер — войди в аккаунт вручную. Окно закроется само.");
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("не запустить браузер: {e}"))?;

    let mut cdp = match Cdp::connect(port).await {
        Ok(c) => c,
        Err(e) => {
            let _ = child.kill().await;
            return Err(e);
        }
    };

    // Ждём появления авторизационных кук. Гостевой визит ставит десяток кук,
    // поэтому смотрим именно на Mpop/Auth-Token, а не на их количество.
    let mut harvested = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(15 * 60);
    loop {
        if stop.is_stopped() {
            log("⛔ Вход отменён.");
            break;
        }
        if std::time::Instant::now() > deadline {
            log("⏳ 15 минут прошло — закрываю окно входа.");
            break;
        }
        // Пользователь закрыл окно сам — выходим с тем, что успели снять.
        if matches!(child.try_wait(), Ok(Some(_))) {
            log("🚪 Окно браузера закрыто.");
            break;
        }
        match cdp.cookies().await {
            Ok(list) => {
                let jar = cookies_for_mailru(&list);
                if looks_logged_in(&jar) {
                    harvested = jar;
                    log("✅ Сессия появилась — снимаю куки.");
                    break;
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

    let _ = child.kill().await;
    let _ = child.wait().await;

    if harvested.is_empty() {
        anyhow::bail!("вход не завершён — куки сессии не появились");
    }
    Ok(Harvest { cookies: harvested, ua: persona.ua.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_only_mailru_cookies() {
        let list = vec![
            serde_json::json!({ "domain": ".mail.ru", "name": "Mpop", "value": "1" }),
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
    fn parses_http_head_end() {
        let head = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(find_headers_end(head), Some(head.len()));
        assert!(find_headers_end(b"GET / HTTP/1.1\r\n").is_none());
    }
}
