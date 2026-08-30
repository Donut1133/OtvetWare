//! otvet-core — вся логика бота otvet.mail.ru без единого пикселя интерфейса.
//!
//! Порт JS-версии (httpclient/bot/answerer/asker/replier/server) на Rust.
//! Данные общие со старым приложением: `accounts.json`, `personas.json`,
//! `styles.json`, `gif-pool.json`, журналы `answered_*.ndjson` и т.д. — можно
//! запускать то одну версию, то другую на одной папке.

pub mod accounts;
pub mod ai;
pub mod answerer;
pub mod api;
pub mod asker;
pub mod cdp;
pub mod complain;
pub mod content;
pub mod http;
pub mod journals;
pub mod persona;
pub mod proxy;
pub mod replier;
pub mod runner;
pub mod store_io;
pub mod subscribe;
pub mod uniq;
pub mod util;
pub mod votes;

use accounts::{AccountsStore, Karma};
use ai::AiClient;
use http::Http;
use journals::ConvoStore;
use parking_lot::Mutex;
use persona::PersonaStore;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Общий контекст приложения: где лежат данные и чем ходим в сеть.
pub struct Core {
    pub root: PathBuf,
    pub accounts: Arc<AccountsStore>,
    pub personas: Arc<PersonaStore>,
    pub http: Arc<Http>,
    /// Клиент нейросети — отдельный от бота: другой сервис, другие заголовки.
    pub ai: AiClient,
    /// Единый чат диалогового режима, общий на все аккаунты.
    pub convo: ConvoStore,
    /// Прогретые проверки аккаунтов: раннер проверяет следующий аккаунт, пока
    /// работает текущий, и тот стартует уже без трёх запросов на разогрев.
    warm: Mutex<HashMap<String, (Instant, api::Validation)>>,
}

/// Выбор папки данных по трём подсказкам. Вынесено из [`Core::find_root`]
/// отдельно: переменные окружения и текущая папка — состояние всего процесса,
/// а так порядок поиска можно проверить тестом.
fn root_from(
    env: Option<String>,
    exe_dir: Option<std::path::PathBuf>,
    cwd: std::path::PathBuf,
) -> std::path::PathBuf {
    if let Some(r) = env.filter(|r| !r.trim().is_empty()) {
        return std::path::PathBuf::from(r);
    }
    if let Some(portable) = exe_dir.map(|d| d.join("accounts")) {
        if portable.join("accounts.json").exists() {
            return portable;
        }
    }
    let mut at = cwd.as_path();
    loop {
        if at.join("accounts.json").exists() {
            return at.to_path_buf();
        }
        match at.parent() {
            Some(p) => at = p,
            None => return cwd.clone(),
        }
    }
}

impl Core {
    /// Где лежат данные, если никто не сказал явно.
    ///
    /// Порядок: `OTVET_ROOT` → папка `accounts` рядом с программой → первая
    /// папка вверх от текущей, где лежит `accounts.json` → сама текущая. Нужен
    /// примерам и скриптам: интерфейс ищет богаче (он умеет ещё и собирать
    /// портативную папку), но «запустил из клона и получил пустой список
    /// аккаунтов» не должно случаться нигде.
    pub fn find_root() -> std::path::PathBuf {
        root_from(
            std::env::var("OTVET_ROOT").ok(),
            std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)),
            std::env::current_dir().unwrap_or_else(|_| ".".into()),
        )
    }

    pub fn open(root: impl AsRef<Path>) -> Arc<Self> {
        let root = root.as_ref().to_path_buf();
        let accounts = Arc::new(AccountsStore::open(&root));
        let personas = Arc::new(PersonaStore::new(&root));
        let http = Arc::new(Http::new(personas.clone(), accounts.clone()));
        let convo = ConvoStore::open(&root);
        Arc::new(Self {
            root,
            accounts,
            personas,
            http,
            ai: AiClient::new(),
            convo,
            warm: Mutex::new(HashMap::new()),
        })
    }

    /// Положить прогретую проверку.
    pub fn warm_put(&self, name: &str, v: api::Validation) {
        let mut m = self.warm.lock();
        // Хранилище не должно расти бесконечно на прогоне из сотни аккаунтов.
        if m.len() > 64 {
            m.clear();
        }
        m.insert(name.to_string(), (Instant::now(), v));
    }

    /// Забрать прогретую проверку, если она ещё свежая. Забрать — именно
    /// забрать: второй раз тот же результат выдавать нельзя, аккаунт мог
    /// разлогиниться прямо в прогоне.
    pub fn warm_take(&self, name: &str, max_age: Duration) -> Option<api::Validation> {
        let mut m = self.warm.lock();
        match m.remove(name) {
            Some((at, v)) if at.elapsed() <= max_age => Some(v),
            _ => None,
        }
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Переименовать аккаунт целиком: запись, персона и журналы.
    ///
    /// Три вещи, и все три обязательны. Персона — иначе под теми же куками
    /// сменится «железо» (ровно тот скачок отпечатка, на который смотрит
    /// антифрод). Журналы — иначе теряется память о сделанном. Сам аккаунт —
    /// понятно.
    pub fn rename_account(&self, old: &str, new: &str) -> Result<(), String> {
        let new = new.trim();
        if new.is_empty() {
            return Err("пустое имя".into());
        }
        if new == old {
            return Ok(());
        }
        self.accounts.rename(old, new)?;
        self.personas.rename(old, new);
        journals::rename_account(&self.root, old, new);
        Ok(())
    }
}

/// Итог работы одного аккаунта за проход. Все режимы возвращают его же —
/// раннеру важно лишь «сколько сделано», «не заблокировали ли» и свежая карма.
#[derive(Debug, Clone, Default)]
pub struct RunOutcome {
    /// Антибот mail.ru (418/429) — аккаунт на этом круге дальше не идёт.
    pub blocked: bool,
    pub karma: Option<Karma>,
    /// Сколько полезных действий сделано (голосов, ответов, подписок…).
    pub done: i64,
    /// Аккаунт пропущен (нет кук, исчерпан лимит, не залогинен).
    pub skipped: bool,
    /// Работы больше нет и не появится: например, диапазон номеров пройден
    /// целиком. Раннер по этому флагу заканчивает круги — в отличие от ленты,
    /// где новые вопросы появляются сами.
    pub exhausted: bool,
}

impl RunOutcome {
    pub fn skipped() -> Self {
        Self { skipped: true, ..Default::default() }
    }
    pub fn blocked() -> Self {
        Self { blocked: true, ..Default::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::root_from;

    /// Порядок поиска папки данных: переменная, портативная папка рядом с
    /// программой, потом первая папка вверх от текущей с `accounts.json`.
    /// Ошибка тут выглядит как «запустил из клона — аккаунтов нет».
    #[test]
    fn data_folder_is_found_in_the_right_order() {
        let dir = std::env::temp_dir().join(format!("otvetware-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let deep = dir.join("проект").join("rust");
        std::fs::create_dir_all(&deep).unwrap();
        let exe = dir.join("рядом");
        std::fs::create_dir_all(exe.join("accounts")).unwrap();

        // Переменная важнее всего остального, даже если по ней ничего нет.
        assert_eq!(
            root_from(Some("C:/явно".into()), Some(exe.clone()), deep.clone()),
            std::path::PathBuf::from("C:/явно")
        );
        // Пустая переменная — это «не задано», а не «искать в пустоте».
        assert_eq!(root_from(Some("  ".into()), None, dir.clone()), dir);

        // Портативная папка считается только когда в ней есть accounts.json.
        assert_eq!(root_from(None, Some(exe.clone()), dir.clone()), dir);
        std::fs::write(exe.join("accounts").join("accounts.json"), "[]").unwrap();
        assert_eq!(root_from(None, Some(exe.clone()), dir.clone()), exe.join("accounts"));

        // Иначе — вверх от текущей папки до первой с файлом аккаунтов.
        std::fs::write(dir.join("проект").join("accounts.json"), "[]").unwrap();
        assert_eq!(root_from(None, None, deep.clone()), dir.join("проект"));
        // Не нашли вообще ничего — работаем там, где стоим.
        let lonely = dir.join("пусто");
        std::fs::create_dir_all(&lonely).unwrap();
        assert_eq!(root_from(None, None, lonely.clone()), lonely);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
