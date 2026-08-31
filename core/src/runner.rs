//! runner.rs — оркестратор прогона: аккаунты, параллельность, круги, лимиты.
//!
//! Один прогон
//! на режим: старт нового останавливает предыдущий (иначе два бота крутились бы
//! на одних аккаунтах).
//!
//! Лимиты два и они независимы:
//!   · общий на аккаунт за весь прогон (`total_limit`, 0 = без лимита);
//!   · на один круг (`round_limit`, 0 = без лимита).
//! На проход уходит строжайший из «остатка общего» и «лимита круга».

use crate::accounts::Account;
use crate::util::{Log, Stop};
use crate::{Core, RunOutcome};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Что делает один аккаунт за проход. `pass_limit` = 0 означает «без лимита».
pub type RunOne =
    Arc<dyn Fn(Account, i64, Log, Stop) -> Pin<Box<dyn Future<Output = RunOutcome> + Send>> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct RunnerCfg {
    /// Сколько аккаунтов работают одновременно.
    pub concurrency: usize,
    /// Общий лимит действий на аккаунт за прогон (0 = без лимита).
    pub total_limit: i64,
    /// Лимит на один круг (0 = без лимита).
    pub round_limit: i64,
    /// Гонять круги до «Стоп».
    pub repeat_rounds: bool,
    /// Пауза между кругами, минуты.
    pub round_pause_min: f64,
    /// Порог сетевых сбоев для ротации прокси.
    pub proxy_rotate_fails: u32,
    /// Прогревать следующий аккаунт, пока работает текущий.
    pub prefetch_next: bool,
    /// Убирать из списка аккаунты, про которых стало точно известно, что они
    /// разлогинены или забанены.
    pub drop_dead: bool,
}

impl Default for RunnerCfg {
    fn default() -> Self {
        Self {
            concurrency: 1,
            total_limit: 0,
            round_limit: 0,
            repeat_rounds: false,
            round_pause_min: 0.0,
            proxy_rotate_fails: 2,
            prefetch_next: true,
            drop_dead: true,
        }
    }
}

/// Аккаунт, которому в следующем круге делать нечего.
///
/// Бан отличается от разлогина тем, что сессия у него живая и сайт отвечает как
/// обычно — но ни одно действие не проходит, так что для круга он такой же
/// мёртвый груз.
fn is_dead(a: &Account) -> bool {
    a.auth_bad == Some(true) || a.banned == Some(true)
}

#[derive(Debug, Default, Clone)]
pub struct RunSummary {
    pub rounds: i64,
    pub done: i64,
    pub blocked_accounts: usize,
    pub stopped: bool,
}

/// Прогон по списку аккаунтов. Возвращается, когда всё закончилось или нажали «Стоп».
pub async fn run(
    core: Arc<Core>,
    accounts: Vec<Account>,
    cfg: RunnerCfg,
    run_one: RunOne,
    log: Log,
    stop: Stop,
) -> RunSummary {
    let mut accounts = accounts;
    let mut summary = RunSummary::default();
    if accounts.is_empty() {
        log("[-] Не выбран ни один аккаунт с куками. Добавь аккаунт (вход через браузер или вставка кук) и отметь галочкой.");
        return summary;
    }

    // Свежий старт ротации прокси на каждом прогоне.
    for a in &accounts {
        a.reset_proxy_state(cfg.proxy_rotate_fails);
    }

    let concurrency = cfg.concurrency.clamp(1, accounts.len());
    let prefixed = concurrency > 1;
    log(&format!(
        "[=] Аккаунтов: {} | одновременно: {}{}",
        accounts.len(),
        concurrency,
        if prefixed { " (параллельно)" } else { " (по очереди)" }
    ));

    if cfg.repeat_rounds {
        log(&format!(
            "[=] Повтор кругов включён — новый круг через {} мин (до «Стоп»). Лимит: всего {}, за круг {}.",
            cfg.round_pause_min,
            if cfg.total_limit > 0 { cfg.total_limit.to_string() } else { "∞".into() },
            if cfg.round_limit > 0 { cfg.round_limit.to_string() } else { "∞".into() },
        ));
    }

    let done_by_acc: Arc<parking_lot::Mutex<HashMap<String, i64>>> =
        Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let mut round = 0i64;

    loop {
        if stop.is_stopped() {
            break;
        }
        round += 1;
        if cfg.repeat_rounds {
            log(&format!("\n{}", "═".repeat(48)));
            log(&format!("[=] Круг {round}"));
            log(&"═".repeat(48));
        }

        let pass = run_one_pass(
            core.clone(),
            &accounts,
            concurrency,
            prefixed,
            &cfg,
            &run_one,
            &log,
            &stop,
            done_by_acc.clone(),
        )
        .await;
        summary.done += pass.done;
        summary.blocked_accounts = pass.blocked;

        if stop.is_stopped() || !cfg.repeat_rounds {
            break;
        }
        if pass.processed == 0 {
            // Причина бывает любой: лимит, разлогин, бан, нет кук. Общее одно —
            // за целый круг никто не отработал, и следующий будет таким же.
            log("\n[=] За круг не отработал ни один аккаунт (лимит, разлогин или бан). Останавливаюсь.");
            break;
        }
        // Работа кончилась по существу: диапазон номеров пройден целиком. В
        // ленте так не бывает — там новые вопросы появляются сами.
        if pass.exhausted >= pass.processed {
            log("\n[=] Работа кончилась: отвечать больше не на что. Останавливаюсь.");
            break;
        }
        log(&format!(
            "\n[=] Круг {round} завершён. Отработали: {} из {}, поймали антибот: {}.",
            accounts.len() - pass.blocked,
            accounts.len(),
            pass.blocked
        ));

        // Разлогиненный и забаненный из круга в круг ничего не сделают: у
        // первого мертва сессия, у второго сайт молча выбрасывает любое
        // действие. Держать их в списке — это по лишнему запросу на проверку
        // каждый круг и мусор в логе.
        if cfg.drop_dead {
            let before = accounts.len();
            accounts.retain(|a| core.accounts.get(&a.name).is_none_or(|s| !is_dead(&s)));
            let dropped = before - accounts.len();
            if dropped > 0 {
                log(&format!(
                    "[=] Выкинул из круга: {dropped} (разлогин или бан). Осталось: {}.",
                    accounts.len()
                ));
            }
            if accounts.is_empty() {
                log("
[=] Рабочих аккаунтов не осталось. Останавливаюсь.");
                break;
            }
        }

        let wait_ms = (cfg.round_pause_min * 60_000.0) as u64;
        if wait_ms > 0 {
            log(&format!(
                "[>] Пауза перед кругом {}: {} (можно нажать «Стоп»)...",
                round + 1,
                fmt_left(wait_ms)
            ));
            if countdown(wait_ms, round + 1, &log, &stop).await {
                break;
            }
        }
    }

    // Ротация кук пишется на диск с задержкой — дописываем хвост, пока прогон
    // ещё в руках у нас, а не в момент, когда программу закрыли.
    core.accounts.flush();

    summary.rounds = round;
    summary.stopped = stop.is_stopped();
    if summary.stopped {
        log("\n[x] Остановлено пользователем");
    }
    log("\n[+] Готово по всем аккаунтам.");
    summary
}

struct PassResult {
    blocked: usize,
    processed: usize,
    /// Сколько аккаунтов сказали «работы больше нет» — например, диапазон
    /// номеров пройден целиком.
    exhausted: usize,
    done: i64,
}

#[allow(clippy::too_many_arguments)]
async fn run_one_pass(
    core: Arc<Core>,
    accounts: &[Account],
    concurrency: usize,
    prefixed: bool,
    cfg: &RunnerCfg,
    run_one: &RunOne,
    log: &Log,
    stop: &Stop,
    done_by_acc: Arc<parking_lot::Mutex<HashMap<String, i64>>>,
) -> PassResult {
    let idx = Arc::new(AtomicUsize::new(0));
    let blocked = Arc::new(AtomicUsize::new(0));
    let processed = Arc::new(AtomicUsize::new(0));
    let exhausted = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(parking_lot::Mutex::new(0i64));
    let total = accounts.len();

    let mut workers = Vec::new();
    for _ in 0..concurrency.min(total) {
        let idx = idx.clone();
        let blocked = blocked.clone();
        let processed = processed.clone();
        let exhausted = exhausted.clone();
        let done = done.clone();
        let accounts: Vec<Account> = accounts.to_vec();
        let run_one = run_one.clone();
        let log = log.clone();
        let stop = stop.clone();
        let core = core.clone();
        let cfg = cfg.clone();
        let done_by_acc = done_by_acc.clone();

        workers.push(tokio::spawn(async move {
            loop {
                if stop.is_stopped() {
                    return;
                }
                // На паузе новый аккаунт не берём: пусть очередь стоит целиком,
                // а не «текущий доработал, следующий уже пошёл».
                if stop.hold().await {
                    return;
                }
                let i = idx.fetch_add(1, Ordering::SeqCst);
                if i >= accounts.len() {
                    return;
                }
                let acc = accounts[i].clone();

                let already = *done_by_acc.lock().get(&acc.name).unwrap_or(&0);
                if cfg.total_limit > 0 && already >= cfg.total_limit {
                    log(&format!(
                        "[!] {}: общий лимит {} исчерпан — пропускаю круг",
                        acc.name, cfg.total_limit
                    ));
                    continue;
                }

                // Лог этого аккаунта: при параллельном прогоне строки помечаем именем,
                // иначе логи 10 аккаунтов превращаются в кашу.
                let acc_log: Log = if prefixed {
                    let name = acc.name.clone();
                    let l = log.clone();
                    Arc::new(move |line: &str| l(&format!("[{name}] {line}")))
                } else {
                    log.clone()
                };
                acc.set_rotate_log(acc_log.clone());

                if prefixed {
                    log(&format!("[>] Старт: {} ({}/{})", acc.name, i + 1, total));
                } else {
                    log(&format!("\n{}", "═".repeat(48)));
                    log(&format!("[=] Аккаунт {}/{}: {}", i + 1, total, acc.name));
                    log(&"═".repeat(48));
                }

                let prxs = acc.proxy_list();
                if prxs.len() > 1 {
                    acc_log(&format!(
                        "[>] Прокси: {} (×{}, ротация при {} сбоях)",
                        prxs.iter().map(|p| crate::proxy::mask_proxy(p)).collect::<Vec<_>>().join(" | "),
                        prxs.len(),
                        cfg.proxy_rotate_fails
                    ));
                } else if let Some(p) = acc.active_proxy() {
                    acc_log(&format!("[>] Прокси: {}", crate::proxy::mask_proxy(&p)));
                }

                // Лимит на этот проход = строжайший из остатка общего и лимита круга.
                let remain_total = if cfg.total_limit > 0 { cfg.total_limit - already } else { i64::MAX };
                let round_limit = if cfg.round_limit > 0 { cfg.round_limit } else { i64::MAX };
                let effective = remain_total.min(round_limit);
                let pass_limit = if effective == i64::MAX { 0 } else { effective.max(0) };

                // Пока этот аккаунт работает, проверяем следующий по очереди:
                // авторизация и карма — три запроса, и делать их на старте, пока
                // человек смотрит в пустой лог, незачем. Только для работы по
                // очереди: при параллельном прогоне аккаунты и так перекрываются.
                if cfg.prefetch_next && concurrency == 1 {
                    if let Some(next) = accounts.get(i + 1).cloned() {
                        let core = core.clone();
                        let stop = stop.clone();
                        tokio::spawn(async move {
                            crate::api::warm_account(&core, &next, &stop).await;
                        });
                    }
                }

                let res = run_one(acc.clone(), pass_limit, acc_log.clone(), stop.clone()).await;

                *done_by_acc.lock().entry(acc.name.clone()).or_insert(0) += res.done;
                *done.lock() += res.done;
                if let Some(k) = &res.karma {
                    core.accounts.set_karma(&acc.name, k.clone());
                }
                if res.blocked {
                    blocked.fetch_add(1, Ordering::SeqCst);
                }
                if !res.skipped {
                    processed.fetch_add(1, Ordering::SeqCst);
                }
                if res.exhausted {
                    exhausted.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }

    for w in workers {
        let _ = w.await;
    }

    let done_total = *done.lock();
    PassResult {
        blocked: blocked.load(Ordering::SeqCst),
        processed: processed.load(Ordering::SeqCst),
        exhausted: exhausted.load(Ordering::SeqCst),
        done: done_total,
    }
}

fn fmt_left(ms: u64) -> String {
    let s = (ms as f64 / 1000.0).ceil() as u64;
    let (m, ss) = (s / 60, s % 60);
    if m > 0 {
        format!("{m} мин {ss:02} сек")
    } else {
        format!("{ss} сек")
    }
}

/// Обратный отсчёт до следующего круга. `true` — прервали «Стопом».
async fn countdown(wait_ms: u64, next_round: i64, log: &Log, stop: &Stop) -> bool {
    let step = if wait_ms >= 2 * 60_000 { 60_000 } else { 15_000 };
    let start = std::time::Instant::now();
    let mut last_bucket = wait_ms.div_ceil(step);
    loop {
        if stop.sleep(Duration::from_millis(1000)).await {
            return true;
        }
        let elapsed = start.elapsed().as_millis() as u64;
        if elapsed >= wait_ms {
            return false;
        }
        let left = wait_ms - elapsed;
        let bucket = left.div_ceil(step);
        if bucket < last_bucket {
            last_bucket = bucket;
            log(&format!("[>] До круга {next_round}: осталось {}", fmt_left(left)));
        }
    }
}
