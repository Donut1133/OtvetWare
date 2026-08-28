//! store_io.rs — атомарная запись файлов состояния.
//!
//! Все хранилища (`accounts.json`, `personas.json`, `convo.json`) пишутся по
//! схеме «во временный файл, затем rename». Тонкость, из-за которой это стоит
//! отдельного модуля: писателей несколько (аккаунты крутятся параллельно и
//! каждый мержит свои Set-Cookie), и наивная реализация теряет данные —
//!
//!   поток A: пишет tmp …                    поток B: пишет тот же tmp …
//!   поток A: rename(tmp → accounts.json)  ← переименовал ПОЛУЗАПИСАННЫЙ файл B
//!
//! и в accounts.json оказывается обрезанный JSON, то есть все аккаунты с куками
//! разом превращаются в мусор. Поэтому: свой временный файл на каждую запись
//! плюс мьютекс, чтобы rename'ы не наезжали друг на друга.

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

pub struct FileWriter {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileWriter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), lock: Mutex::new(()) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&self, contents: &str) -> std::io::Result<()> {
        let _guard = self.lock.lock();
        let tmp = self.path.with_extension(format!(
            "tmp{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(e) = std::fs::write(&tmp, contents) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        // На Windows rename поверх существующего файла работает как замена.
        match std::fs::rename(&tmp, &self.path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Главная проверка: параллельные записи никогда не оставляют файл битым.
    #[test]
    fn concurrent_writes_never_corrupt() {
        let dir = std::env::temp_dir().join(format!("otvetware-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let w = Arc::new(FileWriter::new(&path));

        let mut hs = Vec::new();
        for t in 0..8 {
            let w = w.clone();
            hs.push(std::thread::spawn(move || {
                for i in 0..40 {
                    let payload = serde_json::json!({ "thread": t, "i": i, "pad": "x".repeat(4096) });
                    w.write(&payload.to_string()).unwrap();
                }
            }));
        }
        // Пока идёт запись, файл обязан оставаться читаемым целиком.
        let reader = {
            let path = path.clone();
            std::thread::spawn(move || {
                for _ in 0..200 {
                    if let Ok(txt) = std::fs::read_to_string(&path) {
                        assert!(
                            serde_json::from_str::<serde_json::Value>(&txt).is_ok(),
                            "прочитан обрезанный файл длиной {}",
                            txt.len()
                        );
                    }
                    std::thread::yield_now();
                }
            })
        };
        for h in hs {
            h.join().unwrap();
        }
        reader.join().unwrap();

        let txt = std::fs::read_to_string(&path).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&txt).is_ok());
        // Мусор после себя не оставляем.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "остались временные файлы: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
