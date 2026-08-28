//! Чтение ленты под аккаунтом: cargo run -p otvet-core --example feed -- "Аккаунт 2"
use otvet_core::{answerer, util::Stop, Core};
use std::collections::HashSet;

#[tokio::main]
async fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "Аккаунт 2".into());
    let root = std::env::var("OTVET_ROOT").unwrap_or_else(|_| ".".into());
    let core = Core::open(&root);
    let Some(acc) = core.accounts.get(&name) else {
        eprintln!("нет аккаунта");
        return;
    };
    let answered = otvet_core::journals::load_answered(&core.root, &acc.name);
    println!("в журнале уже отвечено: {}", answered.len());
    let stop = Stop::new();
    match answerer::collect_questions(&core, &acc, &answered, &HashSet::new(), 5, &stop).await {
        Err(e) => println!("ошибка ленты: {e}"),
        Ok(qs) => {
            println!("свежих отвечаемых вопросов: {}", qs.len());
            for q in qs.iter().take(3) {
                println!(
                    "  #{} {} | автор @{} | {}",
                    q.id,
                    otvet_core::util::clip(&q.title, 60),
                    q.author_user,
                    q.date
                );
            }
        }
    }
}
