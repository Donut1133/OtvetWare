//! Живой прогон боевых путей по НАСТОЯЩЕМУ сайту.
//!
//! ВНИМАНИЕ: этот пример реально постит: задаёт вопросы, пишет ответы, ставит голоса,
//! подписывается и отправляет жалобы. Запускать только на своих аккаунтах и
//! только осознанно — поэтому нужен флаг подтверждения:
//!
//!   set OTVET_LIVE=yes
//!   cargo run -p otvet-core --example live -- whoami "Аккаунт 2"
//!   cargo run -p otvet-core --example live -- ask "Аккаунт 4"
//!   cargo run -p otvet-core --example live -- answer "Аккаунт 2" <url вопроса>
//!   cargo run -p otvet-core --example live -- vote "Аккаунт 2" <url> plus
//!   cargo run -p otvet-core --example live -- sub|unsub "Аккаунт 2" <профиль>
//!   cargo run -p otvet-core --example live -- complain "Аккаунт 2" <профиль>
//!   cargo run -p otvet-core --example live -- raw-reply "Аккаунт 4" <topic> <entity>
//!   cargo run -p otvet-core --example live -- comments "Аккаунт 2"
//!
//! Шаги специально мелкие: так видно, на каком именно ломается.

use otvet_core::accounts::Account;
use otvet_core::util::{Log, Stop};
use otvet_core::{answerer, api, asker, complain, replier, subscribe, votes, Core};
use std::sync::Arc;

fn logger() -> Log {
    Arc::new(|line: &str| {
        for l in line.split('\n') {
            println!("  {l}");
        }
    })
}

fn need(args: &[String], i: usize, what: &str) -> String {
    args.get(i).cloned().unwrap_or_else(|| {
        eprintln!("не хватает аргумента: {what}");
        std::process::exit(2);
    })
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_default();
    let root = std::env::var("OTVET_ROOT").unwrap_or_else(|_| ".".into());
    let core = Core::open(&root);
    let stop = Stop::new();
    let log = logger();

    // Читающие команды разрешены всегда, пишущие — только с подтверждением.
    let writes = !matches!(cmd.as_str(), "whoami" | "feed" | "voters" | "subs-check" | "");
    if writes && std::env::var("OTVET_LIVE").unwrap_or_default() != "yes" {
        eprintln!("это пишущая команда: поставь OTVET_LIVE=yes, если правда хочешь постить");
        std::process::exit(2);
    }

    let account = |name: &str| -> Account {
        core.accounts.get(name).unwrap_or_else(|| {
            eprintln!("нет аккаунта «{name}». Есть: {:?}", core.accounts.names());
            std::process::exit(2);
        })
    };

    match cmd.as_str() {
        "whoami" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let v = api::validate_account(&core, &acc, &stop).await;
            println!(
                "{}: alive={} authBad={} banned={} blocked={} id={:?} ник={:?} карма={:?}",
                acc.name, v.alive, v.auth_bad, v.banned, v.blocked, v.user_id, v.username, v.karma
            );
            if let Some(u) = &v.username {
                println!("профиль: https://otvet.mail.ru/profile/{u}");
            }
        }

        "feed" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            // Сколько последних смотреть — то же, что «Смотреть последних» в
            // интерфейсе. Полезно проверить, сколько сайт отдаёт на самом деле.
            let n: i64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
            let qs = answerer::collect_questions(
                &core,
                &acc,
                &Default::default(),
                &Default::default(),
                &Default::default(),
                n,
                &stop,
            )
            .await
            .unwrap_or_default();
            println!("просили {n}, отвечаемых нашлось {}", qs.len());
            for q in &qs {
                println!("#{} {}", q.id, q.title);
            }
        }

        "ask" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let p = asker::AskParams {
                mode: asker::AskMode::NoAi,
                limit: 1,
                delay_min: 0.0,
                delay_max: 0.0,
                check_auth: true,
                ..Default::default()
            };
            let out = asker::run_asker(&core, &acc, &p, &log, &stop).await;
            println!("итог: задано {} blocked={}", out.done, out.blocked);
        }

        "answer" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let url = need(&args, 2, "ссылка на вопрос");
            // Третьим аргументом — «ответов на один вопрос»: тем же аккаунтом
            // подряд. Проверка того, что сайт вообще такое разрешает.
            let rep: i64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1);
            let p = answerer::AnswerParams {
                mode: answerer::AnswerMode::NoAi,
                target: answerer::TargetMode::Links,
                links: vec![url],
                limit: rep,
                repeat_per_question: rep,
                delay_min: 0.0,
                delay_max: 0.0,
                check_auth: true,
                ..Default::default()
            };
            let out = answerer::run_answerer(&core, &acc, &p, &log, &stop).await;
            println!("итог: ответов {} blocked={}", out.done, out.blocked);
        }

        "vote" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let url = need(&args, 2, "ссылка на пост/ответ");
            let dir = args.get(3).cloned().unwrap_or_else(|| "plus".into());
            let p = votes::VoteParams {
                targets: vec![url],
                vote: if dir == "minus" { votes::Vote::Minus } else { votes::Vote::Plus },
                delay: 0.0,
                limit: 0,
                check_auth: true,
                progress: Default::default(),
            };
            let out = votes::run_votes(&core, &acc, &p, &log, &stop).await;
            println!("итог: голосов {} blocked={}", out.done, out.blocked);
        }

        "voters" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let url = need(&args, 2, "ссылка на пост/ответ");
            let Some(t) = votes::parse_target_id(&url) else {
                eprintln!("не понял ссылку");
                return;
            };
            match votes::list_voters(&core, &acc, &t, &stop).await {
                Err(e) => println!("ошибка: {e}"),
                Ok(v) => {
                    println!("голосов: {}", v.len());
                    for x in &v {
                        println!(
                            "  {} id{} mine={}",
                            if x.reaction == 1 { "+" } else { "−" },
                            x.author_id,
                            x.mine
                        );
                    }
                }
            }
        }

        "sub" | "unsub" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let profile = need(&args, 2, "ссылка на профиль");
            let p = subscribe::SubParams {
                profiles: vec![profile],
                action: if cmd == "unsub" {
                    subscribe::SubAction::Unsubscribe
                } else {
                    subscribe::SubAction::Subscribe
                },
                delay: 0.0,
                limit: 0,
                check_auth: true,
                progress: Default::default(),
            };
            let out = subscribe::run_subscriber(&core, &acc, &p, &log, &stop).await;
            println!("итог: {} blocked={}", out.done, out.blocked);
        }

        "subs-check" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let profile = need(&args, 2, "ссылка на профиль");
            match subscribe::subscription_status(&core, &acc, &profile, &stop).await {
                Some((who, sub)) => println!("{} (id {}): подписан={sub}", who.name, who.id),
                None => println!("не удалось резолвить профиль"),
            }
        }

        "complain" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let target = need(&args, 2, "ссылка на профиль или пост");
            let kind = args.get(3).cloned().unwrap_or_else(|| "user".into());
            let p = complain::ComplainParams {
                targets: vec![target],
                target: match kind.as_str() {
                    "single" => complain::ComplainTarget::Single,
                    "topics" => complain::ComplainTarget::Topics,
                    "replies" => complain::ComplainTarget::Replies,
                    _ => complain::ComplainTarget::User,
                },
                reason: "spam".into(),
                delay: 0.0,
                limit: 1,
                check_auth: true,
                progress: Default::default(),
            };
            let out = complain::run_complainer(&core, &acc, &p, &log, &stop).await;
            println!("итог: жалоб {} blocked={}", out.done, out.blocked);
        }

        // Написать реплику под конкретный ответ — нужно, чтобы создать событие
        // для режима «Комменты».
        "raw-reply" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let topic = need(&args, 2, "id вопроса");
            let entity = need(&args, 3, "id ответа, под которым пишем");
            let text = args.get(4).cloned().unwrap_or_else(|| "а почему так?".into());
            match replier::post_reply(&core, &acc, &topic, &entity, &text, &log, &stop).await {
                Ok(replier::PostRes::Ok(id)) => println!("реплика #{id} отправлена"),
                Ok(other) => println!("не отправлено: {other:?}"),
                Err(e) => println!("ошибка: {e}"),
            }
        }

        "comments" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let p = replier::ReplyParams {
                mode: replier::ReplyMode::NoAi,
                limit: 1,
                delay_min: 0.0,
                delay_max: 0.0,
                max_age_hours: 0.0,
                skip_own: false,
                check_auth: true,
                ..Default::default()
            };
            let out = replier::run_replier(&core, &acc, &p, &log, &stop).await;
            println!("итог: ответов {} blocked={}", out.done, out.blocked);
        }

        // Список ответов под вопросом — нужен, чтобы узнать id ответа для реплики.
        "answers" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let topic = need(&args, 2, "id вопроса");
            let r = core
                .http
                .request(
                    &acc,
                    &format!("/api/topic/answers/{topic}"),
                    otvet_core::http::ReqOpts::get(),
                    &stop,
                )
                .await
                .expect("запрос ответов");
            let empty = vec![];
            let replies =
                r.result().and_then(|v| v.get("replies")).and_then(|v| v.as_array()).unwrap_or(&empty);
            println!("ответов: {}", replies.len());
            for x in replies {
                println!(
                    "  #{} от @{} — {}",
                    x.get("id").map(|v| v.to_string()).unwrap_or_default(),
                    x.pointer("/author/username").and_then(|v| v.as_str()).unwrap_or("?"),
                    otvet_core::util::clip(
                        &otvet_core::content::doc_to_text(
                            x.get("content").unwrap_or(&serde_json::Value::Null)
                        ),
                        60
                    )
                );
            }
        }

        // Сырой GET по произвольному пути — для разбора контрактов API.
        "get" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let path = need(&args, 2, "путь, например /api/auth/users/vasya");
            let r = core
                .http
                .request(&acc, &path, otvet_core::http::ReqOpts::get(), &stop)
                .await
                .expect("запрос");
            println!("HTTP {} (blocked={}) url={}", r.status, r.blocked, r.url);
            println!("{}", otvet_core::util::clip(&r.text, 600));
        }

        // Диагностика прокси: полный текст ошибки, а не короткая сводка.
        "diag" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let prx = acc.active_proxy();
            println!("прокси: {:?}", prx.as_deref().map(otvet_core::proxy::mask_proxy));
            let cfg = prx.as_deref().and_then(otvet_core::proxy::parse_proxy);
            println!("разобран: {:?}", cfg.as_ref().map(|c| c.server.clone()));
            let mut b = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(20));
            if let Some(c) = &cfg {
                b = b.proxy(reqwest::Proxy::all(c.full_url()).expect("прокси"));
            }
            let client = b.build().unwrap();
            match client.get("https://otvet.mail.ru/robots.txt").send().await {
                Ok(r) => println!("через прокси: HTTP {}", r.status()),
                Err(e) => {
                    println!("через прокси ОШИБКА: {e}");
                    let mut src = std::error::Error::source(&e);
                    while let Some(s) = src {
                        println!("  причина: {s}");
                        src = std::error::Error::source(s);
                    }
                }
            }
            match reqwest::Client::new().get("https://otvet.mail.ru/robots.txt").send().await {
                Ok(r) => println!("напрямую: HTTP {}", r.status()),
                Err(e) => println!("напрямую ОШИБКА: {e}"),
            }
            // Тот же прокси, но через наш движок — в том же процессе.
            match core
                .http
                .request(&acc, "/robots.txt", otvet_core::http::ReqOpts::get().no_retry(), &stop)
                .await
            {
                Ok(r) => println!("через движок: HTTP {}", r.status),
                Err(e) => println!("через движок ОШИБКА: {e}"),
            }
            // И ещё раз сырым клиентом, но с теми же настройками, что у движка.
            if let Some(c) = &cfg {
                let cl = reqwest::Client::builder()
                    .connect_timeout(std::time::Duration::from_secs(20))
                    .pool_idle_timeout(std::time::Duration::from_secs(90))
                    .redirect(reqwest::redirect::Policy::limited(10))
                    .proxy(reqwest::Proxy::all(c.full_url()).unwrap())
                    .build()
                    .unwrap();
                match cl.get("https://otvet.mail.ru/robots.txt").send().await {
                    Ok(r) => println!("сырой клиент с настройками движка: HTTP {}", r.status()),
                    Err(e) => println!("сырой клиент с настройками движка ОШИБКА: {e}"),
                }
            }
        }

        // Сколько из N попыток через прокси реально проходят.
        "proxyloop" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
            let cfg = acc.active_proxy().as_deref().and_then(otvet_core::proxy::parse_proxy);
            println!("прокси: {:?}", cfg.as_ref().map(|c| c.server.clone()));

            let mut raw_ok = 0;
            for _ in 0..n {
                let mut b = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(15));
                if let Some(c) = &cfg {
                    b = b.proxy(reqwest::Proxy::all(c.full_url()).unwrap());
                }
                if b.build().unwrap().get("https://otvet.mail.ru/robots.txt").send().await.is_ok() {
                    raw_ok += 1;
                }
            }
            println!("сырой клиент (новое соединение каждый раз): {raw_ok}/{n}");

            let mut eng_ok = 0;
            for _ in 0..n {
                if core
                    .http
                    .request(&acc, "/robots.txt", otvet_core::http::ReqOpts::get().no_retry(), &stop)
                    .await
                    .is_ok()
                {
                    eng_ok += 1;
                }
            }
            println!("движок (общий пул соединений): {eng_ok}/{n}");
        }

        // Ответ на вопрос ИЗ ЛЕНТЫ — основной путь режима «Ответы».
        "answer-feed" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let p = answerer::AnswerParams {
                mode: answerer::AnswerMode::NoAi,
                target: answerer::TargetMode::Feed,
                limit: 1,
                delay_min: 0.0,
                delay_max: 0.0,
                feed_min: 3.0,
                feed_max: 5.0,
                recent_scan: 10,
                check_auth: true,
                ..Default::default()
            };
            let out = answerer::run_answerer(&core, &acc, &p, &log, &stop).await;
            println!("итог: ответов {} blocked={}", out.done, out.blocked);
        }

        // Голоса по ВСЕЙ ленте профиля — самый сложный цикл (пагинация + дедуп).
        "vote-profile" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let profile = need(&args, 2, "ссылка на профиль");
            let limit: i64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2);
            let p = votes::VoteParams {
                targets: vec![profile],
                vote: votes::Vote::Plus,
                delay: 0.5,
                limit,
                check_auth: true,
                progress: Default::default(),
            };
            let out = votes::run_votes(&core, &acc, &p, &log, &stop).await;
            println!("итог: голосов {} blocked={}", out.done, out.blocked);
        }

        // Заливка картинки в пул: проверяет весь путь multipart-запроса.
        "upload" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let file = need(&args, 2, "путь к картинке");
            match core.http.upload_picture(&acc, std::path::Path::new(&file), &stop).await {
                Ok(up) => println!(
                    "залито: url={} {}x{} размер={}
хэш: {:?}",
                    up.url,
                    up.width,
                    up.height,
                    up.size,
                    api::extract_cdn_hash(&up.url)
                ),
                Err(e) => println!("не залилось: {e}"),
            }
        }

        // Браузер под аккаунтом: те же куки, отпечаток и прокси. Проверяет и
        // мост до прокси с логином — окно с паролем появляться не должно.
        "browser" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let persona = core.http.persona_for(&acc);
            let dir = core.root.join("profiles").join(otvet_core::util::safe_name(&acc.name));
            let cookies = acc.cookie_header().unwrap_or_default();
            println!("прокси: {:?}", acc.active_proxy().map(|p| otvet_core::proxy::mask_proxy(&p)));
            match otvet_core::cdp::open_as(
                &core.root,
                &persona,
                acc.active_proxy().as_deref(),
                &dir,
                &cookies,
                "https://otvet.mail.ru/",
                &log,
            )
            .await
            {
                Ok(()) => println!("окно открыто"),
                Err(e) => println!("не открылось: {e}"),
            }
            // Даём посмотреть на окно, потом выходим — браузер останется жить.
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        }

        "notifs" => {
            let acc = account(&need(&args, 1, "имя аккаунта"));
            let page = replier::fetch_notifications(&core, &acc, None, &stop).await;
            println!(
                "ok={} blocked={} throttled={} записей={}",
                page.ok,
                page.blocked,
                page.throttled,
                page.items.len()
            );
            // Сводка по типам: видно, чего в колокольчике на самом деле много.
            let mut by_type: std::collections::BTreeMap<String, i32> = Default::default();
            for it in &page.items {
                *by_type
                    .entry(it.get("type").and_then(|v| v.as_str()).unwrap_or("?").to_string())
                    .or_insert(0) += 1;
            }
            for (t, n) in &by_type {
                println!("  {t}: {n}");
            }
            for it in page.items.iter().take(8) {
                let t = replier::to_target(it);
                println!(
                    "  type={} → {}",
                    it.get("type").and_then(|v| v.as_str()).unwrap_or("?"),
                    t.map(|t| format!("topic {} entity {} от {}", t.topic_id, t.entity_id, t.author_name))
                        .unwrap_or_else(|| "не наша цель".into())
                );
            }
        }

        other => {
            eprintln!("неизвестная команда: {other}");
            eprintln!("см. комментарий в начале файла");
            std::process::exit(2);
        }
    }
}
