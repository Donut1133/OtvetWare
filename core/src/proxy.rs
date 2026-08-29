//! proxy.rs — разбор строки прокси и ротация списка прокси на аккаунт.
//!
//! Порт `parseProxy` из bot.js + логики ротации из httpclient.js. Формат строки
//! терпимый: `host:port:user:pass`, `user:pass@host:port`, со схемой и без.
//! Несколько прокси в одном поле разделяются `|` или переводом строки.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyCfg {
    /// `scheme://host:port` — без логина/пароля.
    pub server: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ProxyCfg {
    pub fn scheme(&self) -> &str {
        self.server.split("://").next().unwrap_or("http")
    }
    pub fn is_socks(&self) -> bool {
        matches!(self.scheme(), "socks5" | "socks5h" | "socks4")
    }
    /// Полный URL с логином/паролем — то, что понимает `reqwest::Proxy`.
    pub fn full_url(&self) -> String {
        match (&self.username, &self.password) {
            (Some(u), p) => {
                let (scheme, host) = self.server.split_once("://").unwrap_or(("http", &self.server));
                format!(
                    "{}://{}:{}@{}",
                    scheme,
                    urlencoding::encode(u),
                    urlencoding::encode(p.as_deref().unwrap_or("")),
                    host
                )
            }
            _ => self.server.clone(),
        }
    }
}

fn has_scheme(s: &str) -> bool {
    match s.find("://") {
        Some(i) if i > 0 => s[..i].chars().all(|c| c.is_ascii_alphanumeric()),
        _ => false,
    }
}

/// Разобрать строку прокси. `None` — не распарсилось либо схема не поддержана.
pub fn parse_proxy(proxy: &str) -> Option<ProxyCfg> {
    let mut s = proxy.trim().to_string();
    if s.is_empty() {
        return None;
    }

    // host:port:user:pass — четыре сегмента, без схемы и без @. Приводим к
    // user:pass@host:port, дальше разбирается как обычный URL.
    if !has_scheme(&s) && !s.contains('@') {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() == 4 && parts[1].chars().all(|c| c.is_ascii_digit()) && !parts[1].is_empty() {
            s = format!("{}:{}@{}:{}", parts[2], parts[3], parts[0], parts[1]);
        }
    }
    if !has_scheme(&s) {
        s = format!("http://{s}");
    }

    let (scheme, rest) = s.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "socks5" | "socks5h" | "socks4") {
        return None;
    }
    // Логин/пароль отделяем по ПОСЛЕДНЕЙ @: пароль вполне может содержать @.
    let (creds, hostport) = match rest.rfind('@') {
        Some(i) => (Some(&rest[..i]), &rest[i + 1..]),
        None => (None, rest),
    };
    let hostport = hostport.trim_end_matches('/');
    if hostport.is_empty() {
        return None;
    }
    let (username, password) = match creds {
        Some(c) => {
            let (u, p) = c.split_once(':').unwrap_or((c, ""));
            (
                Some(urlencoding::decode(u).map(|x| x.into_owned()).unwrap_or_else(|_| u.to_string())),
                Some(urlencoding::decode(p).map(|x| x.into_owned()).unwrap_or_else(|_| p.to_string())),
            )
        }
        None => (None, None),
    };
    Some(ProxyCfg { server: format!("{scheme}://{hostport}"), username, password })
}

/// Несколько прокси в одном поле: разделители — `|` и перевод строки.
pub fn split_proxies(raw: &str) -> Vec<String> {
    raw.split(['|', '\n', '\r']).map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| s.to_string()).collect()
}

/// Маска для лога: прячем пароль, оставляем хост:порт.
///
/// Формат `host:port:логин:пароль` тоже маскируем — раньше он проходил мимо
/// (в нём нет `@`), и пароль от прокси спокойно уезжал в лог и на экран.
pub fn mask_proxy(p: &str) -> String {
    let s = p.trim();
    if let Some(at) = s.rfind('@') {
        let head = &s[..at];
        let tail = &s[at..];
        return match head.rfind(':') {
            // не путаем с двоеточием схемы (`http://`)
            Some(i) if !head[i..].starts_with("://") => format!("{}:***{}", &head[..i], tail),
            _ => format!("***{tail}"),
        };
    }
    // host:port:user:pass — четыре сегмента без схемы.
    let (scheme, rest) = match s.split_once("://") {
        Some((sc, r)) => (format!("{sc}://"), r),
        None => (String::new(), s),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() == 4 && parts[1].chars().all(|c| c.is_ascii_digit()) && !parts[1].is_empty() {
        return format!("{scheme}{}:{}:{}:***", parts[0], parts[1], parts[2]);
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_shapes() {
        let a = parse_proxy("socks5://user:pass@1.2.3.4:1080").unwrap();
        assert_eq!(a.server, "socks5://1.2.3.4:1080");
        assert_eq!(a.username.as_deref(), Some("user"));
        assert!(a.is_socks());

        let b = parse_proxy("1.2.3.4:8080:log:pw").unwrap();
        assert_eq!(b.server, "http://1.2.3.4:8080");
        assert_eq!(b.password.as_deref(), Some("pw"));

        let c = parse_proxy("1.2.3.4:8080").unwrap();
        assert_eq!(c.server, "http://1.2.3.4:8080");
        assert!(c.username.is_none());

        assert!(parse_proxy("ftp://1.2.3.4:21").is_none());
        assert!(parse_proxy("   ").is_none());
    }

    #[test]
    fn password_with_at_sign() {
        let p = parse_proxy("http://u:pa@ss@1.2.3.4:80").unwrap();
        assert_eq!(p.server, "http://1.2.3.4:80");
        assert_eq!(p.password.as_deref(), Some("pa@ss"));
    }

    #[test]
    fn masks_credentials() {
        assert_eq!(mask_proxy("socks5://user:secret@1.2.3.4:1080"), "socks5://user:***@1.2.3.4:1080");
        // Формат провайдеров host:port:логин:пароль — пароль тоже под маской.
        assert_eq!(mask_proxy("1.2.3.4:9256:login:secret"), "1.2.3.4:9256:login:***");
        assert_eq!(mask_proxy("http://1.2.3.4:9256:login:secret"), "http://1.2.3.4:9256:login:***");
        // Без пароля ничего не портим.
        assert_eq!(mask_proxy("http://1.2.3.4:8080"), "http://1.2.3.4:8080");
    }
}
