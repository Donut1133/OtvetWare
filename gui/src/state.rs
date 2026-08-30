//! state.rs — мост между окном и ядром: tokio-рантайм, буферы логов, запуск и
//! остановка прогонов, снимок списка аккаунтов для отрисовки.
//!
//! Правило одно: GUI-поток НИЧЕГО не ждёт. Любая сетевая работа уходит в
//! рантайм, обратно приходит только текст в лог и обновлённый снимок аккаунтов.

use otvet_core::accounts::Account;
use otvet_core::util::{Log, Stop};
use otvet_core::Core;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Сколько строк лога держим. Больше — не нужно: старое всё равно не читают, а
/// память и отрисовка не бесплатны.
const LOG_CAP: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Mode {
    Votes,
    Answers,
    Comments,
    Questions,
    Subscribe,
    Complain,
}

impl Mode {
    pub const ALL: [Mode; 6] =
        [Mode::Votes, Mode::Answers, Mode::Comments, Mode::Questions, Mode::Subscribe, Mode::Complain];
    pub fn title(self) -> &'static str {
        match self {
            Mode::Votes => "Голоса",
            Mode::Answers => "Ответы",
            Mode::Comments => "Комменты",
            Mode::Questions => "Вопросы",
            Mode::Subscribe => "Подписки",
            Mode::Complain => "Жалобы",
        }
    }
    /// Счётчик под консолью подписывается по-разному в каждом режиме.
    pub fn counter_label(self) -> &'static str {
        match self {
            Mode::Votes => "Голоса",
            Mode::Answers => "Ответы",
            Mode::Comments => "Комменты",
            Mode::Questions => "Вопросы",
            Mode::Subscribe => "Подписки",
            Mode::Complain => "Жалобы",
        }
    }
}

/// Строка лога: время и текст. Время храним секундами от полуночи — это 4
/// байта вместо строки на каждую из двадцати тысяч строк.
pub struct LogLine {
    pub secs: u32,
    pub text: String,
}

/// Кольцевой буфер лога с дешёвым доступом по индексу — консоль рисуется
/// виртуализованно, поэтому важно уметь брать произвольный диапазон строк.
pub struct LogBuf {
    lines: Mutex<VecDeque<LogLine>>,
    /// Растёт на каждое изменение: по нему консоль понимает, что список
    /// отфильтрованных строк пора пересобрать, а не делать это каждый кадр.
    version: AtomicU64,
}

impl Default for LogBuf {
    fn default() -> Self {
        Self { lines: Mutex::new(VecDeque::with_capacity(1024)), version: AtomicU64::new(0) }
    }
}

impl LogBuf {
    pub fn push(&self, text: &str) {
        let secs = seconds_of_day();
        let mut lines = self.lines.lock();
        // Бот шлёт многострочные сообщения одним куском — разворачиваем, иначе
        // виртуализация консоли поедет (одна «строка» высотой в пять).
        for raw in text.split('\n') {
            lines.push_back(LogLine { secs, text: raw.trim_end_matches('\r').to_string() });
            if lines.len() > LOG_CAP {
                lines.pop_front();
            }
        }
        drop(lines);
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    pub fn len(&self) -> usize {
        self.lines.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// Пройтись по строкам с указанными номерами (их выбирает фильтр консоли).
    pub fn with_indices(&self, idx: &[usize], mut f: impl FnMut(&LogLine)) {
        let lines = self.lines.lock();
        for i in idx {
            if let Some(l) = lines.get(*i) {
                f(l);
            }
        }
    }

    /// Подряд идущие строки диапазона — путь без фильтра.
    pub fn with_range(&self, range: std::ops::Range<usize>, mut f: impl FnMut(&LogLine)) {
        let lines = self.lines.lock();
        for i in range {
            if let Some(l) = lines.get(i) {
                f(l);
            }
        }
    }

    /// Номера строк, подходящих под фильтр.
    ///
    /// Пустой фильтр сюда не приходит: собирать список «все двадцать тысяч»
    /// заново на каждую новую строку лога — это мусор на ровном месте, консоль
    /// в этом случае просто идёт по порядку.
    pub fn matching(&self, needle: &str) -> Vec<usize> {
        let n: Vec<char> = needle.to_lowercase().chars().collect();
        let lines = self.lines.lock();
        lines.iter().enumerate().filter(|(_, l)| contains_ci(&l.text, &n)).map(|(i, _)| i).collect()
    }

    pub fn all_text(&self) -> String {
        self.lines
            .lock()
            .iter()
            .map(|l| format!("{} {}", hhmmss(l.secs), l.text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn clear(&self) {
        self.lines.lock().clear();
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// Логгер для ядра.
    pub fn logger(self: &Arc<Self>) -> Log {
        let me = self.clone();
        Arc::new(move |line: &str| me.push(line))
    }
}

/// Подстрока без учёта регистра и без единой лишней аллокации: фильтр гоняется
/// по всему логу, а лог — это десятки тысяч строк.
fn contains_ci(haystack: &str, needle_lower: &[char]) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    for (at, _) in haystack.char_indices() {
        let mut hay = haystack[at..].chars().flat_map(|c| c.to_lowercase());
        if needle_lower.iter().all(|n| hay.next() == Some(*n)) {
            return true;
        }
    }
    false
}

fn seconds_of_day() -> u32 {
    use chrono::Timelike;
    let now = chrono::Local::now();
    now.hour() * 3600 + now.minute() * 60 + now.second()
}

/// «14:03:27» из секунд от полуночи.
pub fn hhmmss(secs: u32) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// Как показана консоль: прокрутка, фильтр, время. Живёт рядом с логом, но это
/// про показ, а не про данные.
pub struct ConsoleView {
    /// Автопрокрутка вниз. Снимается сама, когда человек уезжает вверх.
    pub follow: bool,
    pub show_time: bool,
    pub filter: String,
    /// Кэш номеров строк под фильтр: версия лога + сам фильтр.
    pub cache: (u64, String, Vec<usize>),
    /// Прокрутка на прошлом кадре — по ней видно, что список увели ВВЕРХ.
    pub last_offset: f32,
    /// Куда едем: положение прокрутки, к которому подтягиваемся по кадрам.
    /// Мгновенный прыжок к последней строке читать невозможно — глаз теряет
    /// место, — поэтому доводим плавно.
    pub glide_to: f32,
    /// Нижний край прокрутки на прошлом кадре: цель для плавного доезда.
    pub max_offset: f32,
}

impl Default for ConsoleView {
    fn default() -> Self {
        Self {
            follow: true,
            show_time: true,
            filter: String::new(),
            cache: (u64::MAX, String::new(), vec![]),
            last_offset: 0.0,
            glide_to: 0.0,
            max_offset: 0.0,
        }
    }
}

/// Состояние одного режима: свой лог, своя кнопка «Стоп», свой признак работы.
pub struct ModeState {
    pub log: Arc<LogBuf>,
    pub stop: Mutex<Stop>,
    pub running: Arc<AtomicBool>,
    /// Как показана консоль этого режима.
    pub view: Mutex<ConsoleView>,
    /// Сделано за текущий прогон. Считаем по факту (режимы дёргают счётчик на
    /// каждом успешном действии), а не по галочкам в логе — там отметка стоит и
    /// на строке «Авторизован».
    pub done: Arc<AtomicI64>,
}

impl Default for ModeState {
    fn default() -> Self {
        Self {
            log: Arc::new(LogBuf::default()),
            stop: Mutex::new(Stop::new()),
            running: Arc::new(AtomicBool::new(false)),
            view: Mutex::new(ConsoleView::default()),
            done: Arc::new(AtomicI64::new(0)),
        }
    }
}

impl ModeState {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    pub fn request_stop(&self) {
        self.stop.lock().stop();
    }
    pub fn is_paused(&self) -> bool {
        self.is_running() && self.stop.lock().is_paused()
    }
    /// Переключить паузу. `true` — теперь на паузе.
    pub fn toggle_pause(&self) -> bool {
        let s = self.stop.lock();
        if s.is_paused() {
            s.resume();
            false
        } else {
            s.pause();
            true
        }
    }
    /// Новый прогон: свежий сигнал остановки (старый уже «взведён»).
    pub fn fresh_stop(&self) -> Stop {
        let s = Stop::new();
        *self.stop.lock() = s.clone();
        s
    }
}

/// Фоновая часть приложения.
pub struct Bg {
    pub core: Arc<Core>,
    rt: tokio::runtime::Runtime,
    ctx: Mutex<Option<egui::Context>>,
    /// Снимок аккаунтов для отрисовки + время его получения. Под `Arc`, потому
    /// что за кадр его просят несколько панелей, а список с куками весит
    /// сотни килобайт — копировать столько на каждую отрисовку незачем.
    snapshot: Mutex<(Arc<Vec<Account>>, Instant)>,
    /// Счётчик активных фоновых задач — по нему GUI понимает, что надо
    /// перерисовываться и что «идёт работа».
    busy: Arc<AtomicI64>,
}

impl Bg {
    pub fn new(core: Arc<Core>) -> Arc<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let snap = Arc::new(core.accounts.all());
        Arc::new(Self {
            core,
            rt,
            ctx: Mutex::new(None),
            snapshot: Mutex::new((snap, Instant::now())),
            busy: Arc::new(AtomicI64::new(0)),
        })
    }

    pub fn set_ctx(&self, ctx: egui::Context) {
        *self.ctx.lock() = Some(ctx);
    }

    pub fn repaint(&self) {
        if let Some(c) = self.ctx.lock().as_ref() {
            c.request_repaint();
        }
    }

    pub fn busy(&self) -> i64 {
        self.busy.load(Ordering::SeqCst)
    }

    /// Запустить фоновую работу. Всё, что ходит в сеть, идёт только сюда.
    pub fn spawn<F>(self: &Arc<Self>, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let me = self.clone();
        self.busy.fetch_add(1, Ordering::SeqCst);
        self.rt.spawn(async move {
            fut.await;
            me.busy.fetch_sub(1, Ordering::SeqCst);
            me.refresh_accounts();
            me.repaint();
        });
    }

    /// Снимок аккаунтов для UI. Перечитывается не чаще раза в 400 мс: за это
    /// время в списке всё равно ничего не меняется на глаз.
    pub fn accounts(&self) -> Arc<Vec<Account>> {
        {
            let s = self.snapshot.lock();
            if s.1.elapsed() < Duration::from_millis(400) {
                return s.0.clone();
            }
        }
        self.refresh_accounts();
        let s = self.snapshot.lock();
        s.0.clone()
    }

    pub fn refresh_accounts(&self) {
        let fresh = Arc::new(self.core.accounts.all());
        *self.snapshot.lock() = (fresh, Instant::now());
    }

    /// Перечитать accounts.json с диска (его мог поправить кто-то ещё).
    pub fn reload_accounts(&self) {
        self.core.accounts.reload();
        self.refresh_accounts();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Лог не должен расти бесконечно: прогон на ночь пишет десятки тысяч
    /// строк, и без кольцевого буфера окно к утру съело бы всю память.
    #[test]
    fn log_never_grows_past_the_cap() {
        let log = LogBuf::default();
        for i in 0..LOG_CAP * 3 {
            log.push(&format!("[+] строка номер {i} с каким-то текстом внутри"));
        }
        assert_eq!(log.len(), LOG_CAP, "буфер лога перестал ограничиваться");

        // Выбрасываются САМЫЕ СТАРЫЕ: последняя строка обязана остаться.
        let last = format!("строка номер {}", LOG_CAP * 3 - 1);
        assert!(log.all_text().contains(&last), "потеряли свежие строки вместо старых");
        assert!(!log.all_text().contains("строка номер 0 "), "старые строки не выбрасываются");

        log.clear();
        assert!(log.is_empty());
    }

    /// Многострочное сообщение разворачивается в отдельные строки — и они тоже
    /// считаются: иначе одна «строка» на пять экранов обошла бы ограничение.
    #[test]
    fn multiline_messages_count_as_many_lines() {
        let log = LogBuf::default();
        log.push("первая\nвторая\nтретья");
        assert_eq!(log.len(), 3);
    }
}
