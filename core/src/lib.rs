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
pub mod util;
pub mod votes;

use accounts::{AccountsStore, Karma};
use ai::AiClient;
use http::Http;
use journals::ConvoStore;
use persona::PersonaStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
}

impl Core {
    pub fn open(root: impl AsRef<Path>) -> Arc<Self> {
        let root = root.as_ref().to_path_buf();
        let accounts = Arc::new(AccountsStore::open(&root));
        let personas = Arc::new(PersonaStore::new(&root));
        let http = Arc::new(Http::new(personas.clone(), accounts.clone()));
        let convo = ConvoStore::open(&root);
        Arc::new(Self { root, accounts, personas, http, ai: AiClient::new(), convo })
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
}

impl RunOutcome {
    pub fn skipped() -> Self {
        Self { skipped: true, ..Default::default() }
    }
    pub fn blocked() -> Self {
        Self { blocked: true, ..Default::default() }
    }
}
