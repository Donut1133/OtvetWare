//! util.rs — общие мелочи: сигнал «Стоп», прерываемые паузы, логгер, рандом.

use rand::Rng;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Логгер прогона. GUI подсовывает сюда отправку строки в канал.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

pub fn no_log() -> Log {
    Arc::new(|_: &str| {})
}

// ─── Метки строк лога ───────────────────────────────────────────────────────
//
// Смысл строки задаётся коротким ASCII-маркером в начале, а не эмодзи: окно
// рисует по нему свою векторную иконку (эмодзи в чёрно-белом интерфейсе
// выглядят инородно и зависят от шрифта системы), а в консоли примеров маркер
// читается как есть.

/// Получилось.
pub const OK: &str = "[+]";
/// Не получилось: ошибка, отказ сервера.
pub const BAD: &str = "[-]";
/// Внимание: пропуск, повтор, сомнительное место.
pub const WARN: &str = "[!]";
/// Стоп: антибот, разлогин, остановка пользователем.
pub const STOP: &str = "[x]";
/// Шаг работы.
pub const STEP: &str = "[>]";
/// Заголовок или итог.
pub const HEAD: &str = "[=]";

/// Разобрать строку лога на маркер и текст. Маркер ищем после отступа, поэтому
/// вложенные шаги («   [+] отправлено») тоже разбираются.
pub fn split_mark(line: &str) -> (Option<char>, &str) {
    let body = line.trim_start_matches(' ');
    let b = body.as_bytes();
    if b.len() >= 3 && b[0] == b'[' && b[2] == b']' && matches!(b[1], b'+' | b'-' | b'!' | b'x' | b'>' | b'=')
    {
        return (Some(b[1] as char), body[3..].trim_start_matches(' '));
    }
    (None, body)
}

/// Живой счётчик сделанного. Один на прогон: режимы дёргают его на каждом
/// успешном действии, окно читает без блокировок. Раньше счётчик обновлялся
/// только когда аккаунт заканчивал работу целиком — на длинном прогоне он
/// часами показывал ноль, хотя ответы уходили.
#[derive(Clone, Default)]
pub struct Progress(Arc<AtomicI64>);

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }
    /// Обернуть уже существующий счётчик (его же читает интерфейс).
    pub fn from_arc(counter: Arc<AtomicI64>) -> Self {
        Self(counter)
    }
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
    pub fn reset(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Progress({})", self.get())
    }
}

/// Сигнал остановки: и опрашивается
/// (`is_stopped`), и ждётся (`wait`) — поэтому «Стоп» рвёт и паузу между шагами,
/// и висящий сетевой запрос, а не ждёт его таймаута.
#[derive(Clone, Default)]
pub struct Stop(Arc<StopInner>);

#[derive(Default)]
struct StopInner {
    flag: AtomicBool,
    /// Пауза: работа не прекращается, а замирает до «Продолжить».
    paused: AtomicBool,
    notify: Notify,
    resume: Notify,
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

    /// Встать на паузу. Прогон не заканчивается: аккаунты, лимиты и журналы
    /// остаются как есть, работа замирает до `resume`.
    pub fn pause(&self) {
        self.0.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.0.paused.store(false, Ordering::SeqCst);
        self.0.resume.notify_waiters();
    }

    /// На паузе ли прогон. Остановленный — уже не на паузе, он просто кончился.
    pub fn is_paused(&self) -> bool {
        self.0.paused.load(Ordering::SeqCst) && !self.is_stopped()
    }

    /// Постоять, пока держат паузу. `true` — во время ожидания нажали «Стоп».
    ///
    /// Вызывается перед каждым ожиданием и в начале каждого круга по аккаунтам,
    /// поэтому пауза срабатывает на ближайшем шаге, а не когда-нибудь потом.
    pub async fn hold(&self) -> bool {
        loop {
            if self.is_stopped() {
                return true;
            }
            if !self.0.paused.load(Ordering::SeqCst) {
                return false;
            }
            // Та же гонка, что и в `wait`: в очередь ожидающих встаём ДО
            // перепроверки флага, иначе «Продолжить» между проверкой и `await`
            // разбудит пустую очередь и ожидание не проснётся никогда.
            let n = self.0.resume.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.is_stopped() {
                return true;
            }
            if !self.0.paused.load(Ordering::SeqCst) {
                return false;
            }
            tokio::select! {
                _ = n => {}
                _ = self.wait() => return true,
            }
        }
    }

    /// Ждёт остановки. Если она уже случилась — возвращается сразу.
    pub async fn wait(&self) {
        loop {
            if self.is_stopped() {
                return;
            }
            // ВАЖНО: сам по себе `notified()` в очередь ожидающих НЕ встаёт —
            // это происходит при первом опросе будущего. Без `enable()` между
            // проверкой флага и `await` остаётся окно, в которое `stop()`
            // успевает позвать `notify_waiters()` при пустой очереди: сигнал
            // уходит в никуда, и это ожидание не проснётся уже никогда — то
            // есть «Стоп» не оборвёт ни запрос, ни паузу.
            let n = self.0.notify.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.is_stopped() {
                return;
            }
            n.await;
        }
    }

    /// Пауза, прерываемая «Стопом». `true` — паузу прервали.
    pub async fn sleep(&self, d: Duration) -> bool {
        // Пока держат паузу — стоим здесь. Через эту дверь проходят все ожидания
        // бота, поэтому одной проверки хватает на все режимы.
        if self.hold().await {
            return true;
        }
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
    /// с добавкой `rand(80,300)` мс.
    pub async fn sleep_human(&self, secs: f64) -> bool {
        if secs <= 0.0 {
            // Нулевая пауза — не повод проскочить мимо паузы человека.
            return self.hold().await || self.is_stopped();
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

/// Папка с картинками: относительный путь считаем от папки данных, а не от
/// текущей. Текущая при запуске двойным кликом — папка с .exe, и настройка
/// «images» тогда указывала бы мимо данных.
pub fn images_dir(root: &std::path::Path, dir: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(dir);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
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
    /// Пауза обязана держать ожидание, а не пропускать его.
    #[tokio::test]
    async fn pause_holds_until_resume() {
        let s = Stop::new();
        s.pause();
        assert!(s.is_paused());

        let worker = s.clone();
        let t = tokio::spawn(async move { worker.sleep_ms(0).await });
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!t.is_finished(), "на паузе бот пошёл дальше");

        s.resume();
        assert!(!s.is_paused());
        let interrupted = tokio::time::timeout(Duration::from_secs(2), t).await.expect("не проснулся");
        assert!(!interrupted.unwrap(), "продолжение не должно выглядеть как «Стоп»");
    }

    /// «Стоп» на паузе обязан будить: иначе кнопка перестала бы работать.
    #[tokio::test]
    async fn stop_wakes_a_paused_run() {
        let s = Stop::new();
        s.pause();
        let worker = s.clone();
        let t = tokio::spawn(async move { worker.hold().await });
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(!t.is_finished());

        s.stop();
        let stopped = tokio::time::timeout(Duration::from_secs(2), t).await.expect("не проснулся");
        assert!(stopped.unwrap(), "ожидание должно вернуть «остановлено»");
        assert!(!s.is_paused(), "остановленный прогон уже не на паузе");
    }

    /// Пауза, поставленная во время ожидания, продлевает его.
    #[tokio::test]
    async fn pause_set_during_a_wait_still_holds() {
        let s = Stop::new();
        let worker = s.clone();
        let t = tokio::spawn(async move {
            worker.sleep_ms(30).await;
            // Второе ожидание уже упрётся в паузу.
            worker.sleep_ms(0).await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        s.pause();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!t.is_finished(), "пауза не сработала на следующем шаге");
        s.resume();
        let _ = tokio::time::timeout(Duration::from_secs(2), t).await.expect("не проснулся");
    }

    use super::*;

    #[test]
    fn safe_name_matches_js_rules() {
        assert_eq!(safe_name("Аккаунт 1"), "Аккаунт_1");
        assert_eq!(safe_name("аккич10 (непрогрет)"), "аккич10_непрогрет_");
        assert_eq!(safe_name(""), "acc");
    }

    /// «Стоп» из соседнего потока обязан будить ожидание, а не оставлять его
    /// висеть. Честно: саму гонку регистрации тест не воспроизводит — окно там
    /// в несколько инструкций, и поймать его извне нельзя; он ловит грубые
    /// поломки `wait()` и держит инвариант на виду.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stop_wakes_a_waiter_from_another_thread() {
        for _ in 0..500 {
            let s = Stop::new();
            let s2 = s.clone();
            let h = tokio::spawn(async move { s2.stop() });
            let waited = tokio::time::timeout(Duration::from_secs(5), s.wait()).await;
            assert!(waited.is_ok(), "ожидание «Стопа» не проснулось");
            let _ = h.await;
        }
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
