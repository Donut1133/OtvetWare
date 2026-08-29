//! complain.rs — жалобы (abuse) на пользователей, посты и ответы. Порт bot.js.
//!
//! Контракт снят с JS-бандла otvet.mail.ru (antispam-клиент):
//!   POST /api/antispam/report_user   {id:<userId>,  report_type}
//!   POST /api/antispam/report_topic  {id:<topicId>, report_type}
//!   POST /api/antispam/report_reply  {id:<replyId>, report_type}

use crate::accounts::Account;
use crate::api;
use crate::http::{HttpError, ReqOpts};
use crate::util::{Log, Stop};
use crate::votes::{parse_target_id, Kind, Target};
use crate::{Core, RunOutcome};
use serde_json::json;
use std::collections::HashSet;

/// Причины жалобы — ровно те коды, что принимает API.
pub const REASONS: &[(&str, &str)] = &[
    ("spam", "Спам"),
    ("insult", "Оскорбление"),
    ("porn", "Порнография"),
    ("violence", "Насилие"),
    ("fraud", "Мошенничество"),
    ("sales", "Продажи"),
    ("dislike", "Не нравится"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComplainTarget {
    /// Жалоба на сам профиль.
    User,
    /// Перебор постов профиля.
    Topics,
    /// Перебор ответов профиля.
    Replies,
    /// Один конкретный пост/ответ по прямой ссылке.
    Single,
}

impl ComplainTarget {
    pub fn ru(self) -> &'static str {
        match self {
            ComplainTarget::User => "профиль",
            ComplainTarget::Topics => "посты",
            ComplainTarget::Replies => "ответы",
            ComplainTarget::Single => "один пост/ответ",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComplainRes {
    Ok,
    Blocked,
    Fail,
}

pub async fn complain_user(
    core: &Core,
    acc: &Account,
    target_id: i64,
    reason: &str,
    referer: &str,
    stop: &Stop,
) -> Result<ComplainRes, HttpError> {
    let body = json!({ "id": target_id, "report_type": reason });
    let r = core
        .http
        .request(acc, "/api/antispam/report_user", ReqOpts::post(body).referer(referer), stop)
        .await?;
    Ok(if r.blocked {
        ComplainRes::Blocked
    } else if r.ok {
        ComplainRes::Ok
    } else {
        ComplainRes::Fail
    })
}

pub async fn complain_content(
    core: &Core,
    acc: &Account,
    target: &Target,
    reason: &str,
    referer: &str,
    stop: &Stop,
) -> Result<ComplainRes, HttpError> {
    let path = match target.kind {
        Kind::Reply => "/api/antispam/report_reply",
        Kind::Topic => "/api/antispam/report_topic",
    };
    let body = json!({ "id": target.id.parse::<i64>().unwrap_or(0), "report_type": reason });
    let r = core.http.request(acc, path, ReqOpts::post(body).referer(referer), stop).await?;
    Ok(if r.blocked {
        ComplainRes::Blocked
    } else if r.ok {
        ComplainRes::Ok
    } else {
        ComplainRes::Fail
    })
}

/// Жалобы на все посты/ответы профиля (до `limit`). Пагинация с seen-Set —
/// курсор инклюзивный, без дедупа жалоба ушла бы на тот же id дважды.
#[allow(clippy::too_many_arguments)]
pub async fn complain_on_profile(
    core: &Core,
    acc: &Account,
    profile_url: &str,
    want_replies: bool,
    reason: &str,
    limit: i64,
    delay: f64,
    log: &Log,
    stop: &Stop,
    blocked: &mut bool,
) -> i64 {
    let who = match crate::votes::resolve_profile_result(core, acc, profile_url, stop).await {
        Ok(w) => w,
        Err(e) => {
            log(&format!("❌ {e}"));
            return 0;
        }
    };
    let kind = if want_replies { Kind::Reply } else { Kind::Topic };
    log(&format!(
        "🎯 Профиль: {} (id {}) → жалобы на {} (причина: {reason})",
        who.name,
        who.id,
        if want_replies { "ответы" } else { "посты" }
    ));

    let (mut success, mut processed, mut pos) = (0i64, 0i64, 0i64);
    let mut seen: HashSet<i64> = HashSet::new();

    loop {
        if stop.is_stopped() {
            log("\n⛔ Остановлено пользователем");
            break;
        }
        if *blocked {
            log("\n🛑 Блокировка mail.ru (418/429) — останавливаю аккаунт.");
            break;
        }
        if limit > 0 && processed >= limit {
            log(&format!("\n🏁 Достигнут лимит {limit}."));
            break;
        }

        let sub = if want_replies { "replies" } else { "topics" };
        let storage = format!("profile-{}-{}", who.id, if want_replies { "replies" } else { "posts" });
        let feed = format!(
            "/api/topic/profile/{}/{}?userID={}&dir={}&pos={}&limit=20&storage={}",
            who.id,
            sub,
            who.id,
            if want_replies { 0 } else { 1 },
            pos,
            storage
        );
        let r = match core.http.request(acc, &feed, ReqOpts::get(), stop).await {
            Ok(r) => r,
            Err(HttpError::Aborted) => break,
            Err(e) => {
                log(&format!("⚠️  Не загрузить ленту жертвы: {e}"));
                break;
            }
        };
        if r.blocked {
            *blocked = true;
            log("🛑 Блокировка при загрузке ленты жертвы.");
            break;
        }
        if !r.ok {
            log(&format!("⚠️  Не загрузить ленту жертвы (HTTP {}).", r.status));
            break;
        }
        let items = r
            .result()
            .and_then(|res| res.get(if want_replies { "replies" } else { "feed" }))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            log("📄 Посты/ответы кончились.");
            break;
        }

        let mut fresh = 0;
        let mut last_id = pos;
        for it in &items {
            if stop.is_stopped() || *blocked {
                break;
            }
            if limit > 0 && processed >= limit {
                break;
            }
            let Some(id) = it.get("id").and_then(|v| v.as_i64()) else { continue };
            last_id = id;
            if !seen.insert(id) {
                continue;
            }
            fresh += 1;
            let referer = if want_replies {
                format!(
                    "https://otvet.mail.ru/question/{}?reply={id}",
                    it.get("topic_id").and_then(|v| v.as_i64()).unwrap_or(0)
                )
            } else {
                format!("https://otvet.mail.ru/question/{id}")
            };
            let target = Target { kind, id: id.to_string() };
            match complain_content(core, acc, &target, reason, &referer, stop).await {
                Ok(ComplainRes::Ok) => {
                    success += 1;
                    log(&format!(
                        "  ✅ {} #{id}",
                        if want_replies { "report_reply" } else { "report_topic" }
                    ));
                }
                Ok(ComplainRes::Blocked) => {
                    *blocked = true;
                    log(&format!("🛑 Антибот на {} #{id} — стоп аккаунта.", kind.ru()));
                    break;
                }
                Ok(ComplainRes::Fail) => log(&format!("  ⚠️  {} #{id} (сервер отклонил)", kind.ru())),
                Err(HttpError::Aborted) => break,
                Err(e) => log(&format!("  ⚠️  {} #{id}: {e}", kind.ru())),
            }
            processed += 1;
            if stop.sleep_human(delay).await {
                break;
            }
        }

        let limit_hit = limit > 0 && processed >= limit;
        if fresh == 0 && !stop.is_stopped() && !*blocked && !limit_hit {
            log("📄 Новых постов/ответов нет — стоп.");
            break;
        }
        if last_id == pos {
            break;
        }
        pos = last_id;
    }
    success
}

#[derive(Debug, Clone)]
pub struct ComplainParams {
    pub targets: Vec<String>,
    pub target: ComplainTarget,
    pub reason: String,
    pub delay: f64,
    pub limit: i64,
    pub check_auth: bool,
}

impl Default for ComplainParams {
    fn default() -> Self {
        Self {
            targets: vec![],
            target: ComplainTarget::User,
            reason: "spam".into(),
            delay: 2.0,
            limit: 0,
            check_auth: true,
        }
    }
}

pub async fn run_complainer(
    core: &Core,
    acc: &Account,
    p: &ComplainParams,
    log: &Log,
    stop: &Stop,
) -> RunOutcome {
    let mut out = RunOutcome::default();
    let targets = crate::util::unique_targets(&p.targets);
    if targets.is_empty() {
        log("❌ Не заданы ссылки.");
        return out;
    }

    if p.check_auth {
        let v = api::validate_account(core, acc, stop).await;
        api::persist_validation(core, &acc.name, &v);
        out.karma = v.karma.clone();
        if v.blocked {
            out.blocked = true;
            log("🛑 Антибот (418/429) при проверке — статус не меняю.");
        } else if v.alive {
            log("✅ Авторизован");
        } else if v.auth_bad {
            log("🔒 НЕ авторизован");
            log("Пропускаю аккаунт — не залогинен.");
            out.skipped = true;
            return out;
        } else {
            log("⚠️  Не удалось проверить авторизацию (ошибка/сеть) — продолжаю, статус не трогаю.");
        }
    }

    log(&format!("🚩 Жалобы — цель: {} · причина: {} · ссылок: {}", p.target.ru(), p.reason, targets.len()));

    let mut blocked = out.blocked;
    for (i, t) in targets.iter().enumerate() {
        if stop.is_stopped() {
            log("\n⛔ Остановлено пользователем");
            break;
        }
        if blocked {
            log("\n🛑 Блокировка mail.ru (418/429) — стоп аккаунта.");
            break;
        }

        match p.target {
            ComplainTarget::User => match crate::votes::resolve_profile_result(core, acc, t, stop).await {
                Err(e) => log(&format!("⚠️  {e}")),
                Ok(who) => {
                    let referer = format!("https://otvet.mail.ru/profile/{}", who.name);
                    match complain_user(core, acc, who.id, &p.reason, &referer, stop).await {
                        Ok(ComplainRes::Ok) => {
                            out.done += 1;
                            log(&format!("✅ Жалоба (профиль): {} (id {}) [{}]", who.name, who.id, out.done));
                        }
                        Ok(ComplainRes::Blocked) => {
                            blocked = true;
                            log(&format!("🛑 Антибот на жалобу о {} — стоп аккаунта.", who.name));
                            break;
                        }
                        Ok(ComplainRes::Fail) => log(&format!("⚠️  Не удалось (жалоба о {})", who.name)),
                        Err(HttpError::Aborted) => break,
                        Err(e) => log(&format!("⚠️  Сеть (жалоба о {}): {e}", who.name)),
                    }
                }
            },
            ComplainTarget::Single => match parse_target_id(t) {
                None => log(&format!("⚠️  Ссылка не похожа на пост/ответ: {t}")),
                Some(tgt) => {
                    let referer = t.split('#').next().unwrap_or(t).to_string();
                    match complain_content(core, acc, &tgt, &p.reason, &referer, stop).await {
                        Ok(ComplainRes::Ok) => {
                            out.done += 1;
                            log(&format!("✅ Жалоба ({} #{}) [{}]", tgt.kind.ru(), tgt.id, out.done));
                        }
                        Ok(ComplainRes::Blocked) => {
                            blocked = true;
                            log(&format!("🛑 Антибот на {} #{} — стоп аккаунта.", tgt.kind.ru(), tgt.id));
                            break;
                        }
                        Ok(ComplainRes::Fail) => {
                            log(&format!("⚠️  Не удалось (жалоба на {} #{})", tgt.kind.ru(), tgt.id))
                        }
                        Err(HttpError::Aborted) => break,
                        Err(e) => log(&format!("⚠️  Сеть (жалоба на {} #{}): {e}", tgt.kind.ru(), tgt.id)),
                    }
                }
            },
            ComplainTarget::Topics | ComplainTarget::Replies => {
                out.done += complain_on_profile(
                    core,
                    acc,
                    t,
                    p.target == ComplainTarget::Replies,
                    &p.reason,
                    p.limit,
                    p.delay,
                    log,
                    stop,
                    &mut blocked,
                )
                .await;
            }
        }

        if i + 1 < targets.len() && !blocked && stop.sleep_human(p.delay).await {
            break;
        }
    }

    out.blocked = blocked;
    log(&format!("\n{}\n✅ Готово!  Жалоб отправлено: {}\n{}", "=".repeat(50), out.done, "=".repeat(50)));
    out
}
