//! Дымовой тест входа через браузер: открыть Chromium, подключиться по CDP,
//! прочитать куки и закрыться. Логиниться не нужно — проверяем сам путь.
//!
//!   cargo run -p otvet-core --example login_smoke
use otvet_core::{cdp, persona::PersonaOpts, util::Stop, Core};

#[tokio::main]
async fn main() {
    let root = std::env::var("OTVET_ROOT").unwrap_or_else(|_| ".".into());
    let core = Core::open(&root);
    println!("папка данных: {}", core.root.display());
    match cdp::chrome_path(&core.root) {
        Some(p) => println!("браузер: {}", p.display()),
        None => {
            println!("браузер не найден — положи сборку в browsers/ или установи Chrome");
            return;
        }
    }
    let persona = core.personas.get("__smoke__", &PersonaOpts::default());
    println!("персона: {}", persona.ua);

    let stop = Stop::new();
    let s2 = stop.clone();
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        println!("(закрываю окно)");
        s2.stop();
    });

    let log: otvet_core::util::Log = std::sync::Arc::new(|line: &str| println!("{line}"));
    let dir = std::env::temp_dir().join("otvetware-login-smoke");
    let t = std::time::Instant::now();
    match cdp::login_and_harvest(&core.root, &persona, None, &dir, &log, &stop).await {
        Ok(h) => println!("сняты куки: {} символов", h.cookies.len()),
        Err(e) => println!("итог: {e} (за {} мс)", t.elapsed().as_millis()),
    }
    let _ = std::fs::remove_dir_all(&dir);
    core.personas.drop_persona("__smoke__");
}
