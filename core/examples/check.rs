//! Живая проверка одного аккаунта: cargo run -p otvet-core --example check -- "Аккаунт 2"
use otvet_core::{api, util::Stop, Core};

#[tokio::main]
async fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "Аккаунт 2".into());
    let root = std::env::var("OTVET_ROOT").unwrap_or_else(|_| "../..".into());
    let core = Core::open(&root);
    let Some(acc) = core.accounts.get(&name) else {
        eprintln!("нет аккаунта «{name}». Есть: {:?}", core.accounts.names());
        return;
    };
    println!("аккаунт: {} | прокси: {:?}", acc.name, acc.active_proxy());
    let t = std::time::Instant::now();
    let v = api::validate_account(&core, &acc, &Stop::new()).await;
    println!(
        "alive={} authBad={} blocked={} err={:?} userId={:?} username={:?} karma={:?} ({} мс)",
        v.alive,
        v.auth_bad,
        v.blocked,
        v.error,
        v.user_id,
        v.username,
        v.karma,
        t.elapsed().as_millis()
    );
}
