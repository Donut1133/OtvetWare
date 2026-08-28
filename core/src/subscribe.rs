//! subscribe.rs — подписка/отписка на пользователей. Порт bot.js.
//!
//! Контракт (снят с JS-бандла + живой проверкой):
//!   POST   /api/topic/subscription  {user:{id}}          → {result:"Ok"}
//!   DELETE /api/topic/subscription  {user:{id}}          → отписка
//!   GET    /api/topic/subscriptions/{id}/subscribed      → {result:bool}

use crate::accounts::Account;
use crate::api;
use crate::http::{HttpError, ReqOpts};
use crate::util::{Log, Stop};
use crate::votes::{resolve_profile, Victim};
use crate::{Core, RunOutcome};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAction {
    Subscribe,
    Unsubscribe,
}

impl SubAction {
    pub fn ru(self) -> &'static str {
        match self {
            SubAction::Subscribe => "подписка",
            SubAction::Unsubscribe => "отписка",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubRes {
    Ok,
    Blocked,
    Fail,
}

pub async fn subscribe_one(
    core: &Core,
    acc: &Account,
    target_id: i64,
    action: SubAction,
    referer: &str,
    stop: &Stop,
) -> Result<SubRes, HttpError> {
    let body = json!({ "user": { "id": target_id } });
    let opts = match action {
        SubAction::Subscribe => ReqOpts::post(body),
        SubAction::Unsubscribe => ReqOpts::delete(body),
    }
    .referer(referer);
    let r = core.http.request(acc, "/api/topic/subscription", opts, stop).await?;
    if r.blocked {
        return Ok(SubRes::Blocked);
    }
    let ok = match r.result() {
        Some(v) => v.as_str().map(|s| s.eq_ignore_ascii_case("ok")).unwrap_or(v.as_bool().unwrap_or(false)),
        None => false,
    };
    Ok(if r.ok && ok { SubRes::Ok } else { SubRes::Fail })
}

/// Подписан ли аккаунт на юзера по ИЗВЕСТНОМУ id (без резолва — дёшево).
pub async fn is_subscribed(core: &Core, acc: &Account, target_id: i64, referer: &str, stop: &Stop) -> bool {
    match core
        .http
        .request(
            acc,
            &format!("/api/topic/subscriptions/{target_id}/subscribed"),
            ReqOpts::get().referer(referer),
            stop,
        )
        .await
    {
        Ok(r) => r.result().and_then(|v| v.as_bool()).unwrap_or(false),
        Err(_) => false,
    }
}

/// Проверка «подписан ли» по ссылке на профиль (резолвит id).
pub async fn subscription_status(
    core: &Core,
    acc: &Account,
    profile_url: &str,
    stop: &Stop,
) -> Option<(Victim, bool)> {
    let who = resolve_profile(core, acc, profile_url, stop).await?;
    let referer = format!("https://otvet.mail.ru/profile/{}", who.name);
    let sub = is_subscribed(core, acc, who.id, &referer, stop).await;
    Some((who, sub))
}

#[derive(Debug, Clone)]
pub struct SubParams {
    pub profiles: Vec<String>,
    pub action: SubAction,
    pub delay: f64,
    pub check_auth: bool,
}

impl Default for SubParams {
    fn default() -> Self {
        Self { profiles: vec![], action: SubAction::Subscribe, delay: 2.0, check_auth: true }
    }
}

pub async fn run_subscriber(core: &Core, acc: &Account, p: &SubParams, log: &Log, stop: &Stop) -> RunOutcome {
    let mut out = RunOutcome::default();
    let targets = crate::util::unique_targets(&p.profiles);
    if targets.is_empty() {
        log("❌ Не заданы профили.");
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

    let verb = p.action.ru();
    log(&format!(
        "👤 {} — профилей: {}",
        if p.action == SubAction::Subscribe { "➕ Подписка" } else { "➖ Отписка" },
        targets.len()
    ));

    for (i, t) in targets.iter().enumerate() {
        if stop.is_stopped() {
            log("\n⛔ Остановлено пользователем");
            break;
        }
        if out.blocked {
            log("\n🛑 Блокировка mail.ru (418/429) — стоп аккаунта.");
            break;
        }
        let Some(who) = resolve_profile(core, acc, t, stop).await else {
            log(&format!("⚠️  Не понял профиль / не нашёл id: {t}"));
            continue;
        };
        let referer = format!("https://otvet.mail.ru/profile/{}", who.name);
        match subscribe_one(core, acc, who.id, p.action, &referer, stop).await {
            Ok(SubRes::Ok) => {
                out.done += 1;
                log(&format!("✅ {verb}: {} (id {}) [{}]", who.name, who.id, out.done));
            }
            Ok(SubRes::Blocked) => {
                out.blocked = true;
                log(&format!("🛑 Антибот на {verb} {} — стоп аккаунта.", who.name));
                break;
            }
            Ok(SubRes::Fail) => log(&format!("⚠️  Не удалось ({verb}): {}", who.name)),
            Err(HttpError::Aborted) => break,
            Err(e) => log(&format!("⚠️  Сеть ({verb} {}): {e}", who.name)),
        }
        if i + 1 < targets.len() && !out.blocked && stop.sleep_human(p.delay).await {
            break;
        }
    }

    log(&format!("\n{}\n✅ Готово!  {verb}: {}\n{}", "=".repeat(50), out.done, "=".repeat(50)));
    out
}
