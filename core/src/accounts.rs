//! accounts.rs — модель аккаунта и хранилище accounts.json.
//!
//! Файл общий с JS-версией, поэтому:
//!  · имена полей — camelCase как там (`authBad`, `userId`, `karmaAt`, …);
//!  · всё незнакомое (`_persona`, `profileDir`, будущие поля) складывается в
//!    `extra` и пишется обратно как есть — иначе запуск Rust-версии молча съел бы
//!    отпечатки аккаунтов;
//!  · запись атомарная (tmp + rename): оборванная запись не оставит битый файл
//!    с 25 аккаунтами и куками.

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Karma {
    #[serde(default)]
    pub total: i64,
    #[serde(default)]
    pub history: i64,
    #[serde(default)]
    pub knowledge: i64,
    #[serde(default)]
    pub discussion: i64,
}

/// Рантайм-состояние аккаунта: ротация прокси и кэш персоны. Живёт в `Arc`,
/// поэтому клон аккаунта, ушедший в воркер, видит те же счётчики — как объект по
/// ссылке в JS. В JSON не попадает.
#[derive(Default)]
pub struct AccountRt {
    pub proxy_idx: AtomicUsize,
    pub fail_cnt: AtomicU32,
    /// Порог сетевых сбоев для ротации (0 → дефолт 2).
    pub rotate_fails: AtomicU32,
    /// Активный прокси (меняется ротацией по ходу прогона).
    pub active_proxy: Mutex<Option<String>>,
    pub proxy_list: Mutex<Vec<String>>,
    pub proxy_list_key: Mutex<String>,
    /// Живая строка кук: мержится из Set-Cookie по ходу прогона. Все клоны
    /// аккаунта делят один `Arc`, поэтому обновление видят все воркеры сразу.
    pub cookies: Mutex<Option<String>>,
    pub persona: Mutex<Option<crate::persona::Persona>>,
    /// Куда писать сообщения о ротации прокси (лог конкретного аккаунта).
    pub rotate_log: Mutex<Option<crate::util::Log>>,
}

impl std::fmt::Debug for AccountRt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountRt")
            .field("proxy_idx", &self.proxy_idx)
            .field("fail_cnt", &self.fail_cnt)
            .field("active_proxy", &*self.active_proxy.lock())
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Account {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxies: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookies: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ua: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub karma: Option<Karma>,
    #[serde(rename = "karmaAt", default, skip_serializing_if = "Option::is_none")]
    pub karma_at: Option<i64>,
    #[serde(rename = "authBad", default, skip_serializing_if = "Option::is_none")]
    pub auth_bad: Option<bool>,
    #[serde(rename = "userId", default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Всё остальное из файла (`_persona`, `profileDir`, `transport`, …).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,

    #[serde(skip)]
    pub rt: Arc<AccountRt>,
}

impl Account {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), ..Default::default() }
    }

    pub fn has_cookies(&self) -> bool {
        self.cookies.as_deref().map(|c| !c.trim().is_empty()).unwrap_or(false)
    }

    /// Похоже ли, что в куках ЖИВАЯ сессия mail.ru, а не гостевые куки.
    /// Судить по длине нельзя — гостевой визит ставит десяток кук; смотрим на
    /// авторизационные куки Паспорта.
    pub fn looks_logged_in(&self) -> bool {
        looks_logged_in(self.cookies.as_deref().unwrap_or(""))
    }

    pub fn karma_total(&self) -> Option<i64> {
        self.karma.as_ref().map(|k| k.total)
    }

    /// Персона, сохранённая в самом аккаунте (`_persona` из JS-версии).
    pub fn stored_persona(&self) -> Option<crate::persona::Persona> {
        self.extra.get("_persona").and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    pub fn cached_persona(&self) -> Option<crate::persona::Persona> {
        self.rt.persona.lock().clone()
    }

    pub fn cache_persona(&self, p: crate::persona::Persona) {
        *self.rt.persona.lock() = Some(p);
    }

    /// Актуальная строка кук: сначала живая (обновлённая Set-Cookie), иначе из файла.
    pub fn cookie_header(&self) -> Option<String> {
        if let Some(c) = self.rt.cookies.lock().clone() {
            return Some(c);
        }
        self.cookies.clone()
    }

    pub fn set_cookie_header(&self, cookies: &str) {
        *self.rt.cookies.lock() = Some(cookies.to_string());
    }

    /// Лог, куда уходят сообщения о смене прокси во время прогона.
    pub fn set_rotate_log(&self, log: crate::util::Log) {
        *self.rt.rotate_log.lock() = Some(log);
    }

    // ─── прокси и ротация ───────────────────────────────────────────────────

    /// Текущий список прокси. Перечитываем, если поле изменилось; индекс,
    /// вышедший за диапазон, роняем в 0.
    pub fn proxy_list(&self) -> Vec<String> {
        let src = match &self.proxies {
            Some(v) if !v.is_empty() => v.join("|"),
            _ => self.proxy.clone().unwrap_or_default(),
        };
        let list = crate::proxy::split_proxies(&src);
        let key = list.join("||");
        let mut cached_key = self.rt.proxy_list_key.lock();
        if *cached_key != key {
            *cached_key = key;
            *self.rt.proxy_list.lock() = list.clone();
            let idx = self.rt.proxy_idx.load(Ordering::Relaxed);
            if idx >= list.len() {
                self.rt.proxy_idx.store(0, Ordering::Relaxed);
            }
            // стартовая синхронизация: активный прокси = выбранный элемент списка
            let mut act = self.rt.active_proxy.lock();
            if act.is_none() && !list.is_empty() {
                *act = Some(list[self.rt.proxy_idx.load(Ordering::Relaxed).min(list.len() - 1)].clone());
            }
        }
        list
    }

    /// Прокси, которым идём прямо сейчас.
    pub fn active_proxy(&self) -> Option<String> {
        let list = self.proxy_list();
        if let Some(p) = self.rt.active_proxy.lock().clone() {
            return Some(p);
        }
        list.first().cloned()
    }

    pub fn rotate_threshold(&self) -> u32 {
        match self.rt.rotate_fails.load(Ordering::Relaxed) {
            0 => 2,
            n => n,
        }
    }

    /// Засчитать сетевой сбой текущему прокси. При достижении порога переключаемся
    /// на следующий по кругу. `Some((old, new))`, если прокси сменился.
    pub fn note_proxy_fail(&self) -> Option<(String, String)> {
        let list = self.proxy_list();
        if list.len() <= 1 {
            return None; // ротировать некуда
        }
        let cnt = self.rt.fail_cnt.fetch_add(1, Ordering::Relaxed) + 1;
        if cnt < self.rotate_threshold() {
            return None;
        }
        let old_idx = self.rt.proxy_idx.load(Ordering::Relaxed).min(list.len() - 1);
        let new_idx = (old_idx + 1) % list.len();
        self.rt.proxy_idx.store(new_idx, Ordering::Relaxed);
        self.rt.fail_cnt.store(0, Ordering::Relaxed);
        let old = list[old_idx].clone();
        let new = list[new_idx].clone();
        *self.rt.active_proxy.lock() = Some(new.clone());
        Some((old, new))
    }

    /// Сброс состояния ротации перед новым прогоном.
    pub fn reset_proxy_state(&self, rotate_fails: u32) {
        self.rt.rotate_fails.store(rotate_fails, Ordering::Relaxed);
        self.rt.fail_cnt.store(0, Ordering::Relaxed);
        self.rt.proxy_idx.store(0, Ordering::Relaxed);
        let list = {
            *self.rt.proxy_list_key.lock() = String::new();
            self.proxy_list()
        };
        *self.rt.active_proxy.lock() = list.first().cloned();
    }
}

pub fn looks_logged_in(cookie_header: &str) -> bool {
    cookie_header.split(';').any(|part| {
        let name = part.trim().split('=').next().unwrap_or("").trim();
        name.eq_ignore_ascii_case("Mpop") || name.eq_ignore_ascii_case("Auth-Token")
    })
}

// ─── Разбор строки кук ──────────────────────────────────────────────────────

/// `a=1; b=2` → упорядоченный список пар. Порядок важен: mail.ru его не требует,
/// но стабильный порядок кук — часть постоянства отпечатка.
pub fn parse_cookie_jar(s: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for part in s.split(';') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let (n, v) = match p.split_once('=') {
            Some((n, v)) => (n.trim().to_string(), v.trim().to_string()),
            None => (p.to_string(), String::new()),
        };
        if n.is_empty() {
            continue;
        }
        if let Some(slot) = out.iter_mut().find(|(k, _)| *k == n) {
            slot.1 = v;
        } else {
            out.push((n, v));
        }
    }
    out
}

pub fn jar_to_string(jar: &[(String, String)]) -> String {
    jar.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("; ")
}

/// Нормализация ввода кук из GUI: принимаем и заголовок `a=1; b=2`, и JSON-массив
/// объектов из расширений вида EditThisCookie (`[{"name":…,"value":…}]`).
pub fn normalize_cookies_input(input: &str) -> String {
    let s = input.trim();
    if s.is_empty() {
        return String::new();
    }
    if s.starts_with('[') || s.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            let arr = match &v {
                Value::Array(a) => a.clone(),
                Value::Object(o) => o.get("cookies").and_then(|c| c.as_array()).cloned().unwrap_or_default(),
                _ => vec![],
            };
            if !arr.is_empty() {
                let pairs: Vec<String> = arr
                    .iter()
                    .filter_map(|c| {
                        let n = c.get("name")?.as_str()?;
                        let val = c.get("value").and_then(|x| x.as_str()).unwrap_or("");
                        Some(format!("{n}={val}"))
                    })
                    .collect();
                return pairs.join("; ");
            }
        }
    }
    // Обычный заголовок: чистим переводы строк, схлопываем пробелы.
    jar_to_string(&parse_cookie_jar(&s.replace(['\r', '\n'], " ")))
}

// ─── Хранилище ──────────────────────────────────────────────────────────────

/// accounts.json. Все изменения идут через `mutate`, запись атомарная.
pub struct AccountsStore {
    writer: crate::store_io::FileWriter,
    list: RwLock<Vec<Account>>,
}

impl AccountsStore {
    pub fn open(root: &Path) -> Self {
        let path = root.join("accounts.json");
        let list = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<Account>>(&t).ok())
            .unwrap_or_default();
        Self { writer: crate::store_io::FileWriter::new(path), list: RwLock::new(list) }
    }

    pub fn path(&self) -> &Path {
        self.writer.path()
    }

    pub fn all(&self) -> Vec<Account> {
        self.list.read().clone()
    }

    pub fn get(&self, name: &str) -> Option<Account> {
        self.list.read().iter().find(|a| a.name == name).cloned()
    }

    pub fn len(&self) -> usize {
        self.list.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.read().is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        self.list.read().iter().map(|a| a.name.clone()).collect()
    }

    /// Правка одного аккаунта по имени + немедленная запись файла.
    pub fn mutate<F: FnOnce(&mut Account)>(&self, name: &str, f: F) -> bool {
        let changed = {
            let mut list = self.list.write();
            match list.iter_mut().find(|a| a.name == name) {
                Some(a) => {
                    f(a);
                    true
                }
                None => false,
            }
        };
        if changed {
            let _ = self.save();
        }
        changed
    }

    /// Правка всего списка (добавление/удаление/массовые операции).
    pub fn mutate_all<R, F: FnOnce(&mut Vec<Account>) -> R>(&self, f: F) -> R {
        let r = {
            let mut list = self.list.write();
            f(&mut list)
        };
        let _ = self.save();
        r
    }

    pub fn add(&self, acc: Account) -> Result<(), String> {
        if acc.name.trim().is_empty() {
            return Err("пустое имя аккаунта".into());
        }
        self.mutate_all(|list| {
            if list.iter().any(|a| a.name == acc.name) {
                return Err(format!("аккаунт «{}» уже есть", acc.name));
            }
            list.push(acc);
            Ok(())
        })
    }

    pub fn remove(&self, name: &str) -> bool {
        self.mutate_all(|list| {
            let before = list.len();
            list.retain(|a| a.name != name);
            before != list.len()
        })
    }

    pub fn rename(&self, old: &str, new: &str) -> Result<(), String> {
        if new.trim().is_empty() {
            return Err("пустое имя".into());
        }
        self.mutate_all(|list| {
            if list.iter().any(|a| a.name == new) {
                return Err(format!("имя «{new}» занято"));
            }
            match list.iter_mut().find(|a| a.name == old) {
                Some(a) => {
                    a.name = new.to_string();
                    Ok(())
                }
                None => Err(format!("аккаунт «{old}» не найден")),
            }
        })
    }

    // Точечные правки, которыми пользуются прогоны.

    pub fn set_auth(&self, name: &str, ok: bool) {
        self.mutate(name, |a| a.auth_bad = Some(!ok));
    }

    pub fn set_karma(&self, name: &str, karma: Karma) {
        self.mutate(name, |a| {
            a.karma = Some(karma);
            a.karma_at = Some(chrono::Utc::now().timestamp_millis());
        });
    }

    pub fn set_ident(&self, name: &str, user_id: Option<i64>, username: Option<String>) {
        self.mutate(name, |a| {
            if let Some(id) = user_id {
                a.user_id = Some(id);
            }
            if let Some(u) = username {
                if !u.is_empty() {
                    a.username = Some(u);
                }
            }
        });
    }

    /// Ротация сессионных кук: сервер прислал Set-Cookie — сохраняем.
    pub fn set_cookies(&self, name: &str, cookies: &str) {
        self.mutate(name, |a| a.cookies = Some(cookies.to_string()));
    }

    pub fn set_proxy(&self, name: &str, proxy: &str) {
        self.mutate(name, |a| {
            a.proxy = if proxy.trim().is_empty() { None } else { Some(proxy.trim().to_string()) };
            // список прокси перечитается на следующем обращении
            *a.rt.proxy_list_key.lock() = String::new();
            *a.rt.active_proxy.lock() = None;
        });
    }

    pub fn save(&self) -> std::io::Result<()> {
        let txt = {
            let list = self.list.read();
            serde_json::to_string_pretty(&*list).unwrap_or_else(|_| "[]".into())
        };
        self.writer.write(&txt)
    }

    /// Перечитать файл с диска (его могла поправить JS-версия или руки).
    pub fn reload(&self) {
        if let Some(list) = std::fs::read_to_string(self.writer.path())
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<Account>>(&t).ok())
        {
            *self.list.write() = list;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_unknown_fields() {
        let src = r#"[{"name":"a","cookies":"x=1","_persona":{"v":2},"profileDir":"C:\\p"}]"#;
        let list: Vec<Account> = serde_json::from_str(src).unwrap();
        assert_eq!(list[0].name, "a");
        assert!(list[0].extra.contains_key("_persona"));
        let back = serde_json::to_string(&list).unwrap();
        assert!(back.contains("_persona"));
        assert!(back.contains("profileDir"));
    }

    #[test]
    fn cookie_helpers() {
        let jar = parse_cookie_jar(" a=1; b = 2 ;;c=3 ");
        assert_eq!(jar.len(), 3);
        assert_eq!(jar_to_string(&jar), "a=1; b=2; c=3");
        assert!(looks_logged_in("foo=1; Mpop=abc"));
        assert!(!looks_logged_in("foo=1; _ga=2"));
        let from_json = normalize_cookies_input(r#"[{"name":"a","value":"1"},{"name":"b","value":"2"}]"#);
        assert_eq!(from_json, "a=1; b=2");
    }

    #[test]
    fn proxy_rotation_cycles() {
        let mut a = Account::new("t");
        a.proxy = Some("http://1.1.1.1:1|http://2.2.2.2:2".into());
        a.reset_proxy_state(2);
        assert_eq!(a.active_proxy().as_deref(), Some("http://1.1.1.1:1"));
        assert!(a.note_proxy_fail().is_none()); // первый сбой — порог не достигнут
        let (old, new) = a.note_proxy_fail().unwrap();
        assert_eq!(old, "http://1.1.1.1:1");
        assert_eq!(new, "http://2.2.2.2:2");
        assert_eq!(a.active_proxy().as_deref(), Some("http://2.2.2.2:2"));
    }
}
