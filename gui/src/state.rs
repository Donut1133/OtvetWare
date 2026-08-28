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
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Сколько строк лога держим. Больше — не нужно: старое всё равно не читают, а
/// память и отрисовка не бесплатны.
const LOG_CAP: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// Кольцевой буфер лога с дешёвым доступом по индексу — консоль рисуется
/// виртуализованно, поэтому важно уметь брать произвольный диапазон строк.
pub struct LogBuf {
    lines: Mutex<VecDeque<String>>,
}

impl Default for LogBuf {
    fn default() -> Self {
        Self { lines: Mutex::new(VecDeque::with_capacity(1024)) }
    }
}

impl LogBuf {
    pub fn push(&self, text: &str) {
        let mut lines = self.lines.lock();
        // Бот шлёт многострочные сообщения одним куском — разворачиваем, иначе
        // виртуализация консоли поедет (одна «строка» высотой в пять).
        for raw in text.split('\n') {
            lines.push_back(raw.trim_end_matches('\r').to_string());
            if lines.len() > LOG_CAP {
                lines.pop_front();
            }
        }
    }

    pub fn len(&self) -> usize {
        self.lines.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn with_range<R>(
        &self,
        range: std::ops::Range<usize>,
        f: impl FnOnce(&mut dyn Iterator<Item = &str>) -> R,
    ) -> R {
        let lines = self.lines.lock();
        let mut it =
            lines.iter().skip(range.start).take(range.end.saturating_sub(range.start)).map(|s| s.as_str());
        f(&mut it)
    }

    pub fn all_text(&self) -> String {
        self.lines.lock().iter().cloned().collect::<Vec<_>>().join("\n")
    }

    pub fn clear(&self) {
        self.lines.lock().clear();
    }

    /// Логгер для ядра.
    pub fn logger(self: &Arc<Self>) -> Log {
        let me = self.clone();
        Arc::new(move |line: &str| me.push(line))
    }
}

/// Состояние одного режима: свой лог, своя кнопка «Стоп», свой признак работы.
pub struct ModeState {
    pub log: Arc<LogBuf>,
    pub stop: Mutex<Stop>,
    pub running: Arc<AtomicBool>,
    /// Автопрокрутка консоли вниз.
    pub follow: AtomicBool,
    /// Сделано за текущий прогон. Считаем по факту (что вернул бот), а не по
    /// галочкам в логе: там ✅ ставится и на «Авторизован», и счётчик врал.
    pub done: Arc<AtomicI64>,
}

impl Default for ModeState {
    fn default() -> Self {
        Self {
            log: Arc::new(LogBuf::default()),
            stop: Mutex::new(Stop::new()),
            running: Arc::new(AtomicBool::new(false)),
            follow: AtomicBool::new(true),
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
    /// Снимок аккаунтов для отрисовки + время его получения.
    snapshot: Mutex<(Vec<Account>, Instant)>,
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
        let snap = core.accounts.all();
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

    /// Снимок аккаунтов для UI. Обновляется не чаще, чем раз в 400 мс: список
    /// с куками весит сотни килобайт, копировать его каждый кадр незачем.
    pub fn accounts(&self) -> Vec<Account> {
        {
            let s = self.snapshot.lock();
            if s.1.elapsed() < Duration::from_millis(400) {
                return s.0.clone();
            }
        }
        self.refresh_accounts();
        self.snapshot.lock().0.clone()
    }

    pub fn refresh_accounts(&self) {
        let fresh = self.core.accounts.all();
        *self.snapshot.lock() = (fresh, Instant::now());
    }

    /// Перечитать accounts.json с диска (его мог поправить кто-то ещё).
    pub fn reload_accounts(&self) {
        self.core.accounts.reload();
        self.refresh_accounts();
    }
}
