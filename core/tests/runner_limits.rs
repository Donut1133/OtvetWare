//! Тесты оркестратора: лимиты, круги, параллельность, «Стоп».
//!
//! Сеть здесь не нужна — вместо режима подставляется заглушка, которая просто
//! считает вызовы. Проверяется именно арифметика прогона: сколько раз бот
//! возьмётся за аккаунт и с каким лимитом.

use otvet_core::accounts::Account;
use otvet_core::runner::{self, RunOne, RunnerCfg};
use otvet_core::util::{no_log, Stop};
use otvet_core::{Core, RunOutcome};
use parking_lot::Mutex;
use std::sync::Arc;

fn core_and_accounts(name: &str, n: usize) -> (Arc<Core>, Vec<Account>) {
    let dir = std::env::temp_dir().join(format!("otvetware-runner-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let core = Core::open(&dir);
    let accounts = (0..n)
        .map(|i| {
            let mut a = Account::new(format!("acc{i}"));
            a.cookies = Some("Mpop=x".into());
            a
        })
        .collect();
    (core, accounts)
}

/// Заглушка режима: пишет в журнал (аккаунт, выданный лимит) и «делает» ровно
/// `per_call` действий.
fn spy(log: Arc<Mutex<Vec<(String, i64)>>>, per_call: i64) -> RunOne {
    Arc::new(move |acc, pass_limit, _log, _stop| {
        let log = log.clone();
        Box::pin(async move {
            log.lock().push((acc.name.clone(), pass_limit));
            RunOutcome { done: per_call, ..Default::default() }
        })
    })
}

#[tokio::test]
async fn total_limit_is_spread_across_rounds() {
    let (core, accounts) = core_and_accounts("total", 1);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let cfg = RunnerCfg {
        prefetch_next: false,
        concurrency: 1,
        total_limit: 5,
        round_limit: 2,
        repeat_rounds: true,
        round_pause_min: 0.0,
        proxy_rotate_fails: 2,
    };
    let summary = runner::run(core, accounts, cfg, spy(calls.clone(), 2), no_log(), Stop::new()).await;

    let seen: Vec<i64> = calls.lock().iter().map(|(_, l)| *l).collect();
    // Круги: 2 + 2 + остаток 1, потом аккаунт исчерпан и прогон заканчивается.
    assert_eq!(seen, vec![2, 2, 1], "лимит на круг и общий остаток должны сходиться: {seen:?}");
    assert_eq!(summary.done, 6, "заглушка отдаёт по 2 за вызов");
}

#[tokio::test]
async fn zero_limits_mean_unlimited_for_one_pass() {
    let (core, accounts) = core_and_accounts("zero", 2);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let cfg = RunnerCfg { prefetch_next: false, concurrency: 1, ..Default::default() };
    runner::run(core, accounts, cfg, spy(calls.clone(), 1), no_log(), Stop::new()).await;

    let seen = calls.lock().clone();
    assert_eq!(seen.len(), 2, "без кругов — ровно один проход по каждому аккаунту");
    assert!(seen.iter().all(|(_, l)| *l == 0), "0 означает «без лимита»: {seen:?}");
}

#[tokio::test]
async fn every_selected_account_runs_in_parallel_mode() {
    let (core, accounts) = core_and_accounts("par", 6);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let cfg = RunnerCfg { prefetch_next: false, concurrency: 3, ..Default::default() };
    runner::run(core, accounts, cfg, spy(calls.clone(), 1), no_log(), Stop::new()).await;

    let mut names: Vec<String> = calls.lock().iter().map(|(n, _)| n.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["acc0", "acc1", "acc2", "acc3", "acc4", "acc5"]);
}

#[tokio::test]
async fn stop_ends_the_run_between_accounts() {
    let (core, accounts) = core_and_accounts("stop", 20);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let stop = Stop::new();
    let s2 = stop.clone();
    let log = calls.clone();
    let run_one: RunOne = Arc::new(move |acc, _lim, _l, _s| {
        let log = log.clone();
        let stop = s2.clone();
        Box::pin(async move {
            log.lock().push((acc.name.clone(), 0));
            // Останавливаем прогон после третьего аккаунта.
            if log.lock().len() >= 3 {
                stop.stop();
            }
            RunOutcome { done: 1, ..Default::default() }
        })
    });
    let cfg = RunnerCfg { prefetch_next: false, concurrency: 1, ..Default::default() };
    let summary = runner::run(core, accounts, cfg, run_one, no_log(), stop).await;

    assert!(summary.stopped, "прогон должен пометиться остановленным");
    assert!(calls.lock().len() <= 4, "после «Стоп» новых аккаунтов брать нельзя: {}", calls.lock().len());
}

#[tokio::test]
async fn rounds_stop_when_nothing_left_to_do() {
    let (core, accounts) = core_and_accounts("exhaust", 2);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let cfg = RunnerCfg {
        prefetch_next: false,
        concurrency: 1,
        total_limit: 1,
        repeat_rounds: true,
        round_pause_min: 0.0,
        ..Default::default()
    };
    let summary = runner::run(core, accounts, cfg, spy(calls.clone(), 1), no_log(), Stop::new()).await;

    assert_eq!(calls.lock().len(), 2, "по одному вызову на аккаунт — общий лимит исчерпан");
    assert!(summary.rounds >= 1);
    assert!(!summary.stopped, "остановились сами, а не по кнопке");
}

#[tokio::test]
async fn blocked_accounts_are_counted() {
    let (core, accounts) = core_and_accounts("blocked", 3);
    let run_one: RunOne = Arc::new(move |acc, _l, _lg, _s| {
        Box::pin(async move { RunOutcome { blocked: acc.name != "acc0", done: 0, ..Default::default() } })
    });
    let cfg = RunnerCfg { prefetch_next: false, concurrency: 1, ..Default::default() };
    let summary = runner::run(core, accounts, cfg, run_one, no_log(), Stop::new()).await;
    assert_eq!(summary.blocked_accounts, 2, "два аккаунта поймали антибот");
}

/// Переименование обязано тащить за собой журналы: без них аккаунт теряет
/// память о сделанном и начинает отвечать по второму разу.
#[test]
fn rename_moves_journals_and_persona() {
    let dir = std::env::temp_dir().join(format!("otvetware-rename-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let core = Core::open(&dir);

    let mut a = Account::new("Старое имя");
    a.cookies = Some("Mpop=x".into());
    core.accounts.add(a).unwrap();
    otvet_core::journals::append_answered(&dir, "Старое имя", "https://otvet.mail.ru/question/1");
    let persona_before = core.personas.get("Старое имя", &Default::default());

    core.rename_account("Старое имя", "Новое имя").unwrap();

    assert!(core.accounts.get("Новое имя").is_some(), "аккаунт не переименовался");
    assert!(core.accounts.get("Старое имя").is_none());
    let answered = otvet_core::journals::load_answered(&dir, "Новое имя");
    assert!(answered.contains("https://otvet.mail.ru/question/1"), "журнал не переехал");
    assert!(
        otvet_core::journals::load_answered(&dir, "Старое имя").is_empty(),
        "старый журнал должен исчезнуть"
    );
    let persona_after = core.personas.get("Новое имя", &Default::default());
    assert_eq!(persona_before.noise.canvas, persona_after.noise.canvas, "отпечаток обязан сохраниться");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Битый accounts.json не должен превращаться в «аккаунтов нет» с последующей
/// перезаписью: сначала копия, потом честная ошибка.
#[test]
fn broken_accounts_file_is_backed_up_not_swallowed() {
    let dir = std::env::temp_dir().join(format!("otvetware-broken-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("accounts.json");
    // Оборванный файл — ровно то, что остаётся после сбоя питания на записи.
    std::fs::write(&path, r#"[{"name":"Аккаунт 1","cookies":"Mpop=секрет"#).unwrap();

    let core = Core::open(&dir);
    let err = core.accounts.load_error().expect("ошибка чтения должна быть видна");
    assert!(err.contains("accounts.json"), "непонятное сообщение: {err}");

    let backups: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("broken-"))
        .collect();
    assert_eq!(backups.len(), 1, "копия битого файла не сделана");
    let saved = std::fs::read_to_string(backups[0].path()).unwrap();
    assert!(saved.contains("Mpop=секрет"), "в копии нет исходных данных");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Когда работа кончилась по существу — диапазон номеров пройден целиком —
/// круги должны закончиться. Иначе бот до утра гоняет пустые проходы по тем же
/// номерам: каждый круг перечитывает журналы, ничего не делает и уходит в паузу.
#[tokio::test]
async fn rounds_stop_when_there_is_nothing_left() {
    let (core, accounts) = core_and_accounts("exhausted", 2);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let cfg =
        RunnerCfg { prefetch_next: false, repeat_rounds: true, round_pause_min: 0.0, ..Default::default() };
    // Первый круг что-то делает, второй сообщает «работы больше нет».
    let round = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let one: RunOne = {
        let calls = calls.clone();
        let round = round.clone();
        Arc::new(move |acc, _limit, _log, _stop| {
            let calls = calls.clone();
            let round = round.clone();
            Box::pin(async move {
                let n = round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                calls.lock().push(acc.name.clone());
                // Два аккаунта: первые два вызова — первый круг.
                RunOutcome { done: 1, exhausted: n >= 2, ..Default::default() }
            })
        })
    };
    let summary = runner::run(core, accounts, cfg, one, no_log(), Stop::new()).await;

    assert_eq!(summary.rounds, 2, "кругов должно быть ровно два: {}", summary.rounds);
    assert_eq!(calls.lock().len(), 4, "лишние проходы по аккаунтам");
    assert!(!summary.stopped, "останавливались не «Стопом», а по концу работы");
}
