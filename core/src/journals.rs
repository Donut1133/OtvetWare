//! journals.rs — журналы и справочники на диске. Формат общий с JS-версией.
//!
//!  · `answered_<акк>.ndjson` — на что уже отвечали (одна строка = один JSON,
//!    append-only: переписывать файл целиком после каждого ответа — это O(n²) IO);
//!  · `asked_<акк>.ndjson`    — заданные вопросы (нужны, чтобы не повторяться);
//!  · `replied_<акк>.ndjson`  — на какие реплики уже отвечали;
//!  · `styles.json`           — стили ответов (промпты + температуры);
//!  · `gif-pool.json`         — пул картинок;
//!  · `convo.json`            — единый чат диалогового режима (общий на аккаунты).

use crate::util::safe_name;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

fn journal_path(root: &Path, prefix: &str, account: &str) -> PathBuf {
    if account.is_empty() {
        root.join(format!("{prefix}.ndjson"))
    } else {
        root.join(format!("{prefix}_{}.ndjson", safe_name(account)))
    }
}

/// Прочитать журнал: сначала ndjson, затем legacy-массив из `<prefix>_<акк>.json`.
fn read_journal_urls(root: &Path, prefix: &str, account: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let nd = journal_path(root, prefix, account);
    if let Ok(txt) = std::fs::read_to_string(&nd) {
        for line in txt.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(u) = v.get("url").and_then(|u| u.as_str()) {
                    out.insert(u.to_string());
                }
            }
        }
    }
    let legacy = if account.is_empty() {
        root.join(format!("{prefix}.json"))
    } else {
        root.join(format!("{prefix}_{}.json", safe_name(account)))
    };
    if let Ok(txt) = std::fs::read_to_string(&legacy) {
        if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&txt) {
            for x in arr {
                if let Some(u) = x.as_str() {
                    out.insert(u.to_string());
                }
            }
        }
    }
    out
}

fn append_line(path: &Path, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Перенести журналы аккаунта на новое имя.
///
/// Без этого переименование стоит дорого: журналы привязаны к имени, и аккаунт
/// разом теряет память о том, на что уже отвечал. Бот идёт по второму кругу и
/// пишет повторные ответы под теми же вопросами — модерация это видит сразу.
pub fn rename_account(root: &Path, old: &str, new: &str) {
    let (o, n) = (safe_name(old), safe_name(new));
    if o == n {
        return;
    }
    for prefix in ["answered", "asked", "replied"] {
        for ext in ["ndjson", "json"] {
            let from = root.join(format!("{prefix}_{o}.{ext}"));
            let to = root.join(format!("{prefix}_{n}.{ext}"));
            // Чужой журнал не затираем: если файл под новым именем уже есть,
            // оставляем оба — потерять данные хуже, чем оставить лишний файл.
            if from.exists() && !to.exists() {
                let _ = std::fs::rename(&from, &to);
            }
        }
    }
    // Папка профиля браузера — по тому же имени.
    let from = root.join("profiles").join(&o);
    let to = root.join("profiles").join(&n);
    if from.exists() && !to.exists() {
        let _ = std::fs::rename(&from, &to);
    }
}

// ─── Отвеченные вопросы ─────────────────────────────────────────────────────

pub fn load_answered(root: &Path, account: &str) -> HashSet<String> {
    read_journal_urls(root, "answered", account)
}

/// Все отвеченные всеми аккаунтами (режим «не отвечать за другими»).
pub fn load_all_answered(root: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(dir) = std::fs::read_dir(root) else { return out };
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("answered") {
            continue;
        }
        if name.ends_with(".ndjson") {
            if let Ok(txt) = std::fs::read_to_string(e.path()) {
                for line in txt.lines() {
                    if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                        if let Some(u) = v.get("url").and_then(|u| u.as_str()) {
                            out.insert(u.to_string());
                        }
                    }
                }
            }
        } else if name.ends_with(".json") {
            if let Ok(txt) = std::fs::read_to_string(e.path()) {
                if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&txt) {
                    for x in arr {
                        if let Some(u) = x.as_str() {
                            out.insert(u.to_string());
                        }
                    }
                }
            }
        }
    }
    out
}

pub fn append_answered(root: &Path, account: &str, url: &str) {
    let line = serde_json::json!({ "url": url, "ts": now_ms() }).to_string();
    append_line(&journal_path(root, "answered", account), &line);
}

// ─── Заданные вопросы ───────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AskedEntry {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub ts: i64,
}

/// Заголовки прошлых вопросов — чтобы просить у нейросети что-то новое.
pub fn load_asked_titles(root: &Path, account: &str) -> Vec<String> {
    let path = journal_path(root, "asked", account);
    let Ok(txt) = std::fs::read_to_string(&path) else { return vec![] };
    txt.lines()
        .filter_map(|l| serde_json::from_str::<AskedEntry>(l.trim()).ok())
        .map(|e| e.title)
        .filter(|t| !t.is_empty())
        .collect()
}

pub fn append_asked(root: &Path, account: &str, entry: &AskedEntry) {
    if let Ok(line) = serde_json::to_string(entry) {
        append_line(&journal_path(root, "asked", account), &line);
    }
}

// ─── Отвеченные реплики (режим «Комменты») ──────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RepliedEntry {
    /// id реплики собеседника, на которую ответили.
    pub entity: String,
    /// id нашего объекта (пост/ответ), под которым идёт ветка.
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub topic: String,
    /// id нашей отправленной реплики.
    #[serde(default)]
    pub reply: Value,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub ts: i64,
}

/// Множество id реплик, на которые этот аккаунт уже отвечал.
pub fn load_replied(root: &Path, account: &str) -> HashSet<String> {
    let path = journal_path(root, "replied", account);
    let Ok(txt) = std::fs::read_to_string(&path) else { return HashSet::new() };
    txt.lines()
        .filter_map(|l| serde_json::from_str::<RepliedEntry>(l.trim()).ok())
        .map(|e| e.entity)
        .collect()
}

pub fn append_replied(root: &Path, account: &str, entry: &RepliedEntry) {
    if let Ok(line) = serde_json::to_string(entry) {
        append_line(&journal_path(root, "replied", account), &line);
    }
}

// ─── Стили ответов ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Styles {
    /// имя стиля → промпты по режимам (`answer`, `question`, `reply`).
    pub prompts: BTreeMap<String, BTreeMap<String, String>>,
    pub temperatures: BTreeMap<String, f64>,
    /// Темы для генерации вопросов.
    pub topics: Vec<String>,
}

impl Styles {
    pub fn load(root: &Path) -> Self {
        let Ok(txt) = std::fs::read_to_string(root.join("styles.json")) else {
            return Self::fallback();
        };
        let Ok(v) = serde_json::from_str::<Value>(&txt) else {
            return Self::fallback();
        };
        let mut out = Self::default();
        if let Some(obj) = v.get("prompts").and_then(|p| p.as_object()) {
            for (name, val) in obj {
                let mut modes = BTreeMap::new();
                match val {
                    // Старый формат: строка = промпт ответа.
                    Value::String(s) => {
                        modes.insert("answer".to_string(), s.clone());
                    }
                    Value::Object(o) => {
                        for (k, vv) in o {
                            if let Some(s) = vv.as_str() {
                                modes.insert(k.clone(), s.to_string());
                            }
                        }
                    }
                    _ => {}
                }
                out.prompts.insert(name.clone(), modes);
            }
        }
        if let Some(obj) = v.get("temperatures").and_then(|p| p.as_object()) {
            for (name, val) in obj {
                if let Some(f) = val.as_f64() {
                    out.temperatures.insert(name.clone(), f);
                }
            }
        }
        if let Some(arr) = v.get("topics").and_then(|p| p.as_array()) {
            out.topics = arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
        }
        if out.prompts.is_empty() {
            return Self::fallback();
        }
        out
    }

    fn fallback() -> Self {
        let mut prompts = BTreeMap::new();
        let mut m = BTreeMap::new();
        m.insert("answer".to_string(), "Отвечай как обычный человек, коротко и по-простому.".to_string());
        prompts.insert("Обычный чел".to_string(), m);
        Self { prompts, temperatures: BTreeMap::new(), topics: vec![] }
    }

    pub fn names(&self) -> Vec<String> {
        self.prompts.keys().cloned().collect()
    }

    /// Промпт стиля для режима: `answer` | `question` | `reply`.
    /// Если для режима своего промпта нет — берём `answer` (так же вела себя
    /// JS-версия: у большинства стилей описан только он).
    pub fn prompt(&self, style: &str, mode: &str) -> String {
        let by_style = self
            .prompts
            .get(style)
            .or_else(|| self.prompts.get("Обычный чел"))
            .or_else(|| self.prompts.values().next());
        match by_style {
            Some(m) => m.get(mode).or_else(|| m.get("answer")).cloned().unwrap_or_default(),
            None => String::new(),
        }
    }

    pub fn temperature(&self, style: &str) -> Option<f64> {
        self.temperatures.get(style).copied()
    }
}

// ─── Пул картинок ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PoolImage {
    pub hash: String,
    #[serde(default)]
    pub width: i64,
    #[serde(default)]
    pub height: i64,
    #[serde(default)]
    pub tag: String,
}

pub fn load_gif_pool(root: &Path) -> Vec<PoolImage> {
    let Ok(txt) = std::fs::read_to_string(root.join("gif-pool.json")) else { return vec![] };
    match serde_json::from_str::<Value>(&txt) {
        Ok(Value::Array(arr)) => arr
            .into_iter()
            .filter_map(|v| serde_json::from_value::<PoolImage>(v).ok())
            .filter(|g| !g.hash.is_empty())
            .collect(),
        Ok(Value::Object(o)) => o
            .get("items")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().filter_map(|v| serde_json::from_value::<PoolImage>(v.clone()).ok()).collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    }
}

// ─── Единый чат диалогового режима ──────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Convo {
    #[serde(default)]
    pub turns: Vec<crate::ai::Msg>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub ts: i64,
}

/// Общий чат на все аккаунты. Держится под мьютексом: аккаунты крутятся
/// параллельно, и без него сжатие контекста одного затёрло бы ходы другого.
pub struct ConvoStore {
    writer: crate::store_io::FileWriter,
    inner: Mutex<Convo>,
    /// Имя аккаунта, который сейчас владеет диалоговым режимом.
    owner: Mutex<Option<String>>,
}

impl ConvoStore {
    pub fn open(root: &Path) -> Self {
        let path = root.join("convo.json");
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Convo>(&t).ok())
            .unwrap_or_default();
        Self {
            writer: crate::store_io::FileWriter::new(path),
            inner: Mutex::new(inner),
            owner: Mutex::new(None),
        }
    }

    /// Занять диалоговый режим. Общая история = работает ОДИН аккаунт за раз.
    pub fn try_acquire(&self, account: &str) -> Result<(), String> {
        let mut o = self.owner.lock();
        match o.as_deref() {
            Some(cur) if cur != account => Err(cur.to_string()),
            _ => {
                *o = Some(account.to_string());
                Ok(())
            }
        }
    }

    pub fn release(&self, account: &str) {
        let mut o = self.owner.lock();
        if o.as_deref() == Some(account) {
            *o = None;
        }
    }

    pub fn snapshot(&self) -> Convo {
        self.inner.lock().clone()
    }

    pub fn push_turn(&self, user: crate::ai::Msg, assistant: crate::ai::Msg) {
        let mut c = self.inner.lock();
        c.turns.push(user);
        c.turns.push(assistant);
        c.ts = now_ms();
        let snapshot = c.clone();
        drop(c);
        self.save(&snapshot);
    }

    pub fn set_compressed(&self, summary: String, keep: Vec<crate::ai::Msg>) {
        let mut c = self.inner.lock();
        c.summary = summary;
        c.turns = keep;
        c.ts = now_ms();
        let snapshot = c.clone();
        drop(c);
        self.save(&snapshot);
    }

    fn save(&self, c: &Convo) {
        if let Ok(txt) = serde_json::to_string(c) {
            let _ = self.writer.write(&txt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_load_or_fallback() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let s = Styles::load(&root);
        assert!(!s.prompts.is_empty());
        // Промпт всегда находится: неизвестный стиль откатывается на дефолтный.
        assert!(!s.prompt("нет такого стиля", "answer").is_empty());
    }

    #[test]
    fn journal_paths_match_js() {
        let root = Path::new("X:/data");
        assert!(journal_path(root, "answered", "Аккаунт 1").ends_with("answered_Аккаунт_1.ndjson"));
        assert!(journal_path(root, "replied", "аккич10 (непрогрет)")
            .ends_with("replied_аккич10_непрогрет_.ndjson"));
    }
}
