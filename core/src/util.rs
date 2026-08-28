//! util.rs — общие мелочи: сигнал «Стоп», прерываемые паузы, логгер, рандом.

use rand::Rng;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Логгер прогона. GUI подсовывает сюда отправку строки в канал.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

pub fn no_log() -> Log {
    Arc::new(|_: &str| {})
}

/// Сигнал остановки. Аналог AbortController из JS-версии: и опрашивается
/// (`is_stopped`), и ждётся (`wait`) — поэтому «Стоп» рвёт и паузу между шагами,
/// и висящий сетевой запрос, а не ждёт его таймаута.
#[derive(Clone, Default)]
pub struct Stop(Arc<StopInner>);

#[derive(Default)]
struct StopInner {
    flag: AtomicBool,
    notify: Notify,
}

impl Stop {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stop(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_stopped(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }

    /// Ждёт остановки. Если она уже случилась — возвращается сразу.
    pub async fn wait(&self) {
        loop {
            if self.is_stopped() {
                return;
            }
            // notified() регистрируется до повторной проверки флага, поэтому
            // сигнал, пришедший между проверкой и ожиданием, не потеряется.
            let n = self.0.notify.notified();
            if self.is_stopped() {
                return;
            }
            n.await;
        }
    }

    /// Пауза, прерываемая «Стопом». `true` — паузу прервали.
    pub async fn sleep(&self, d: Duration) -> bool {
        if self.is_stopped() {
            return true;
        }
        tokio::select! {
            _ = tokio::time::sleep(d) => false,
            _ = self.wait() => true,
        }
    }

    pub async fn sleep_ms(&self, ms: u64) -> bool {
        self.sleep(Duration::from_millis(ms)).await
    }

    /// Пауза «человеческая»: заданные секунды + случайный хвост 80–300 мс,
    /// как в JS-версии (`delay*1000 + rand(80,300)`).
    pub async fn sleep_human(&self, secs: f64) -> bool {
        if secs <= 0.0 {
            return self.is_stopped();
        }
        let ms = (secs * 1000.0) as u64 + rand_range(80, 300) as u64;
        self.sleep_ms(ms).await
    }
}

pub fn rand_range(a: i64, b: i64) -> i64 {
    if b <= a {
        return a;
    }
    rand::thread_rng().gen_range(a..=b)
}

pub fn rand_f64() -> f64 {
    rand::thread_rng().gen::<f64>()
}

/// Взвешенный выбор из `[(вес, значение)]`.
pub fn pick_weighted<T>(rows: &[(f64, T)]) -> &T {
    let total: f64 = rows.iter().map(|r| r.0).sum();
    let mut n = rand_f64() * total;
    for r in rows {
        n -= r.0;
        if n <= 0.0 {
            return &r.1;
        }
    }
    &rows[rows.len() - 1].1
}

pub fn pick_one<T>(items: &[T]) -> Option<&T> {
    if items.is_empty() {
        return None;
    }
    Some(&items[rand::thread_rng().gen_range(0..items.len())])
}

pub fn shuffle<T>(v: &mut [T]) {
    use rand::seq::SliceRandom;
    v.shuffle(&mut rand::thread_rng());
}

/// Имя файла из имени аккаунта (порт `safe`-функций из answerer/asker/replier):
/// оставляем буквы/цифры/`_`/`-`, остальное — в `_`, максимум 60 символов.
pub fn safe_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_us = false;
    for ch in name.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    let trimmed: String = out.chars().take(60).collect();
    if trimmed.is_empty() {
        "acc".to_string()
    } else {
        trimmed
    }
}

/// Список целей: обрезать, выбросить пустые и ПОВТОРЫ, сохранив порядок.
///
/// Повторы важны не косметически: голос, поставленный второй раз на тот же
/// пост, сайт трактует как отмену — дважды указанный профиль в списке молча
/// снимал бы уже накрученную карму. То же с жалобами: второй раз — впустую.
pub fn unique_targets<I, S>(items: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for it in items {
        let t = it.as_ref().trim();
        if t.is_empty() {
            continue;
        }
        // Ключ сравнения без регистра и без хвостового слэша: одна и та же
        // ссылка, вставленная дважды в чуть разном виде, — всё ещё одна ссылка.
        let key = t.trim_end_matches('/').to_lowercase();
        if seen.insert(key) {
            out.push(t.to_string());
        }
    }
    out
}

/// Обрезка строки по СИМВОЛАМ (не байтам) — для логов с кириллицей.
pub fn clip(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_name_matches_js_rules() {
        assert_eq!(safe_name("Аккаунт 1"), "Аккаунт_1");
        assert_eq!(safe_name("аккич10 (непрогрет)"), "аккич10_непрогрет_");
        assert_eq!(safe_name(""), "acc");
    }

    #[tokio::test]
    async fn stop_interrupts_sleep() {
        let s = Stop::new();
        let s2 = s.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            s2.stop();
        });
        let t = std::time::Instant::now();
        let interrupted = s.sleep(Duration::from_secs(5)).await;
        assert!(interrupted);
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
