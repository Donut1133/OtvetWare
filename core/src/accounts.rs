//! accounts.rs — модель аккаунта и хранилище accounts.json.
//!
//! Файл читают и другие инструменты, поэтому:
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
/// ссылке. В JSON не попадает.
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
    /// Заблокирован сайтом (`user_status < 0`). Не то же самое, что разлогин:
    /// сессия жива, а действия молча не проходят.
    #[serde(rename = "banned", default, skip_serializing_if = "Option::is_none")]
    pub banned: Option<bool>,
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

    /// Персона, сохранённая в самом аккаунте (поле `_persona`).
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

    /// Текущий список прокси. Перечитываем, только если поле изменилось; индекс,
    /// вышедший за диапазон, роняем в 0.
    ///
    /// Ключ кэша — исходная строка поля, а не разобранный список: на каждый
    /// HTTP-запрос иначе уходил разбор строки, склейка ключа и копия вектора,
    /// а запросов за прогон десятки тысяч.
    pub fn proxy_list(&self) -> Vec<String> {
        let src = match &self.proxies {
            Some(v) if !v.is_empty() => v.join("|"),
            _ => self.proxy.clone().unwrap_or_default(),
        };
        let mut cached_key = self.rt.proxy_list_key.lock();
        if *cached_key == src {
            return self.rt.proxy_list.lock().clone();
        }
        let list = crate::proxy::split_proxies(&src);
        *cached_key = src;
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
            // Сбрасываем и ключ, и сам список: пустая строка — это ВАЛИДНЫЙ
            // ключ (аккаунт без прокси), и по одному только ключу кэш бы не
            // обновился.
            *self.rt.proxy_list_key.lock() = String::new();
            self.rt.proxy_list.lock().clear();
            self.proxy_list()
        };
        *self.rt.active_proxy.lock() = list.first().cloned();
    }
}

/// Живая ли это сессия.
///
/// Решает ОДНА кука — `Auth-SessionToken`. Проверено перебором на живом
/// аккаунте: с ней одной `/api/auth/user` отвечает 200, без неё — 403, сколько
/// бы ни было остальных. `Mpop`, `Auth-Token` и `Auth-RefreshToken` на ответ не
/// влияют вовсе.
///
/// Раньше тут стоял `Auth-Token`, а до него — `Mpop`, и каждый раз это ломало
/// вход одинаково: бот хватал куки на первой попавшейся, пока человек ещё
/// дописывал пароль, и в базу ложилась половина сессии. Осенью 2026 mail.ru
/// перешёл на новую пару токенов, и все аккаунты, заведённые раньше, разом
/// стали «не авторизован» — при том что в браузере они живые.
pub fn looks_logged_in(cookie_header: &str) -> bool {
    cookie_header.split(';').any(|part| {
        let name = part.trim().split('=').next().unwrap_or("").trim();
        name.eq_ignore_ascii_case("Auth-SessionToken")
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

/// Как часто не жалко переписывать файл из-за ротации кук, секунд.
const COOKIE_FLUSH_SEC: u64 = 3;

/// accounts.json. Все изменения идут через `mutate`, запись атомарная.
pub struct AccountsStore {
    writer: crate::store_io::FileWriter,
    list: RwLock<Vec<Account>>,
    /// Файл существует, но не разобрался. Держим текст ошибки, чтобы сказать о
    /// ней вслух, а не делать вид, что аккаунтов просто нет.
    load_error: RwLock<Option<String>>,
    /// В памяти новее, чем на диске: отложенная запись ротации кук.
    dirty: std::sync::atomic::AtomicBool,
    last_save: Mutex<std::time::Instant>,
}

impl AccountsStore {
    pub fn open(root: &Path) -> Self {
        let path = root.join("accounts.json");
        let (list, load_error) = Self::read_file(&path);
        Self {
            writer: crate::store_io::FileWriter::new(path),
            list: RwLock::new(list),
            load_error: RwLock::new(load_error),
            dirty: std::sync::atomic::AtomicBool::new(false),
            last_save: Mutex::new(std::time::Instant::now()),
        }
    }

    /// Прочитать файл. Если он есть, но испорчен — СНАЧАЛА кладём копию рядом.
    ///
    /// Иначе выходило так: битый JSON читается как «аккаунтов нет», первая же
    /// правка перезаписывает файл пустым списком — и куки всех аккаунтов
    /// потеряны безвозвратно. Копия стоит миллисекунды и спасает от этого.
    fn read_file(path: &Path) -> (Vec<Account>, Option<String>) {
        let Ok(txt) = std::fs::read_to_string(path) else {
            return (Vec::new(), None); // файла нет — обычный первый запуск
        };
        match serde_json::from_str::<Vec<Account>>(&txt) {
            Ok(list) => (list, None),
            Err(e) => {
                let backup = path.with_extension(format!("broken-{}.json", crate::journals::now_ms()));
                let saved = std::fs::write(&backup, &txt).is_ok();
                let msg = format!(
                    "accounts.json не разобрался ({e}). {}",
                    if saved {
                        format!("Копия сохранена: {}", backup.display())
                    } else {
                        "Скопировать не удалось — исправь файл вручную.".to_string()
                    }
                );
                (Vec::new(), Some(msg))
            }
        }
    }

    /// Что пошло не так при чтении файла (если пошло).
    pub fn load_error(&self) -> Option<String> {
        self.load_error.read().clone()
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
        let changed = self.mutate_mem(name, f);
        if changed {
            let _ = self.save();
        }
        changed
    }

    /// Правка только в памяти, без записи на диск.
    fn mutate_mem<F: FnOnce(&mut Account)>(&self, name: &str, f: F) -> bool {
        let mut list = self.list.write();
        match list.iter_mut().find(|a| a.name == name) {
            Some(a) => {
                f(a);
                true
            }
            None => false,
        }
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

    pub fn set_banned(&self, name: &str, banned: bool) {
        self.mutate(name, |a| a.banned = Some(banned));
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
    ///
    /// В память — сразу, на диск — не чаще раза в `COOKIE_FLUSH_SEC`. mail.ru
    /// обновляет сессионную куку почти на каждом ответе, и запись всего файла
    /// (со всеми аккаунтами и куками, это сотни килобайт) на каждый ответ
    /// упирала прогон в диск: блокирующий вызов на воркере tokio, которых
    /// всего четыре, да ещё под общим мьютексом записи. Остаток дописывается
    /// в `flush()` — по концу прогона и при выходе.
    pub fn set_cookies(&self, name: &str, cookies: &str) {
        if !self.mutate_mem(name, |a| a.cookies = Some(cookies.to_string())) {
            return;
        }
        let due = {
            let last = self.last_save.lock();
            last.elapsed() >= std::time::Duration::from_secs(COOKIE_FLUSH_SEC)
        };
        if due {
            let _ = self.save();
        } else {
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Дописать отложенные изменения на диск (конец прогона, выход из программы).
    pub fn flush(&self) {
        if self.dirty.load(Ordering::Relaxed) {
            let _ = self.save();
        }
    }

    pub fn set_proxy(&self, name: &str, proxy: &str) {
        self.mutate(name, |a| {
            a.proxy = if proxy.trim().is_empty() { None } else { Some(proxy.trim().to_string()) };
            // список прокси перечитается на следующем обращении
            *a.rt.proxy_list_key.lock() = String::new();
            a.rt.proxy_list.lock().clear();
            *a.rt.active_proxy.lock() = None;
        });
    }

    pub fn save(&self) -> std::io::Result<()> {
        // Флаг снимаем ДО снимка списка: правка, случившаяся во время записи,
        // должна снова пометить файл грязным, а не потеряться.
        self.dirty.store(false, Ordering::Relaxed);
        *self.last_save.lock() = std::time::Instant::now();
        let txt = {
            let list = self.list.read();
            serde_json::to_string_pretty(&*list).unwrap_or_else(|_| "[]".into())
        };
        self.writer.write(&txt)
    }

    /// Перечитать файл с диска (его могли поправить снаружи или руками).
    pub fn reload(&self) {
        let (list, err) = Self::read_file(self.writer.path());
        // Битый файл не должен молча превращать список в пустой: оставляем то,
        // что уже загружено, и показываем ошибку.
        if err.is_none() {
            *self.list.write() = list;
            // Память теперь равна диску — отложенных правок больше нет.
            self.dirty.store(false, Ordering::Relaxed);
        }
        *self.load_error.write() = err;
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
        assert!(looks_logged_in("foo=1; Auth-SessionToken=abc"));
        assert!(looks_logged_in("auth-sessiontoken=abc"), "регистр имени не важен");
        // Старая пара без новой — ровно то, что сайт отдаёт 403.
        assert!(!looks_logged_in("Mpop=x; Auth-Token=y; Auth-RefreshToken=z"));
        assert!(!looks_logged_in("foo=1; _ga=2"));
        let from_json = normalize_cookies_input(r#"[{"name":"a","value":"1"},{"name":"b","value":"2"}]"#);
        assert_eq!(from_json, "a=1; b=2");
    }

    /// Ротация кук пишется на диск отложенно — но НЕ теряется: в памяти она
    /// видна сразу, а `flush()` дописывает хвост.
    #[test]
    fn deferred_cookie_write_is_not_lost() {
        let dir = std::env::temp_dir().join(format!("otvetware-cookies-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = AccountsStore::open(&dir);
        store.add(Account::new("acc")).unwrap();

        store.set_cookies("acc", "Mpop=first");
        store.set_cookies("acc", "Mpop=second");
        // В памяти — последнее значение, кто бы ни спросил.
        assert_eq!(store.get("acc").and_then(|a| a.cookies).as_deref(), Some("Mpop=second"));

        store.flush();
        let txt = std::fs::read_to_string(dir.join("accounts.json")).unwrap();
        assert!(txt.contains("Mpop=second"), "куки не дописаны на диск: {txt}");

        // Повторный flush без изменений ничего не портит.
        store.flush();
        assert!(std::fs::read_to_string(dir.join("accounts.json")).unwrap().contains("Mpop=second"));
        let _ = std::fs::remove_dir_all(&dir);
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
