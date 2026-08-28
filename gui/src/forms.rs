//! forms.rs — параметры режимов: то, что пользователь крутит слева.
//!
//! Настроек много, и это осознанно: ни одна не выброшена. Чтобы панель не
//! выглядела свалкой, всё разложено по блокам — сверху то, ради чего запускают
//! прогон, ниже темп и лимиты, редкое убрано под сворачивающиеся заголовки.
//! Формы сериализуются: настройки переживают перезапуск.

use egui::Ui;
use otvet_core::complain::{ComplainParams, ComplainTarget, REASONS};
use otvet_core::subscribe::{SubAction, SubParams};
use otvet_core::votes::{Vote, VoteParams};
use serde::{Deserialize, Serialize};

use crate::theme;

/// Заголовок блока настроек. Один вид на всю панель — глазу проще.
pub fn block(ui: &mut Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(egui::RichText::new(title).color(theme::FG_STRONG).strong());
    ui.add_space(2.0);
}

/// Сворачивающийся блок для редких настроек: они на месте, но не мозолят глаза.
pub fn extra<R>(ui: &mut Ui, title: &str, id: &str, add: impl FnOnce(&mut Ui) -> R) {
    ui.add_space(6.0);
    egui::CollapsingHeader::new(egui::RichText::new(title).color(theme::FG_DIM))
        .id_salt(id)
        .default_open(false)
        .show(ui, add);
}

/// Подпись под полем: зачем оно нужно.
pub fn hint(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(11.0).color(theme::FG_FAINT));
}

/// Общие для всех режимов настройки прогона.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CommonForm {
    pub concurrency: usize,
    pub all_at_once: bool,
    /// Общий лимит действий на аккаунт за прогон (0 = без лимита).
    pub total_limit: i64,
    /// Лимит на один круг (0 = без лимита).
    pub round_limit: i64,
    pub repeat_rounds: bool,
    pub round_pause_min: f64,
    pub proxy_rotate_fails: u32,
    pub check_auth: bool,
}

impl Default for CommonForm {
    fn default() -> Self {
        Self {
            concurrency: 1,
            all_at_once: false,
            total_limit: 0,
            round_limit: 0,
            repeat_rounds: false,
            round_pause_min: 10.0,
            proxy_rotate_fails: 2,
            check_auth: true,
        }
    }
}

impl CommonForm {
    pub fn ui(&mut self, ui: &mut Ui, selected: usize, rounds_supported: bool) {
        block(ui, "Аккаунты");
        ui.horizontal(|ui| {
            ui.label("Работают одновременно");
            ui.add_enabled(
                !self.all_at_once,
                egui::DragValue::new(&mut self.concurrency).range(1..=64).speed(0.2),
            );
            ui.checkbox(&mut self.all_at_once, "все сразу");
        });
        hint(ui, "По одному — медленно, но незаметно. Все сразу — быстро, но нагрузка видна антиботу.");
        if self.all_at_once && selected > 8 {
            ui.colored_label(theme::WARN, format!("{selected} аккаунтов разом — это заметно"));
        }

        extra(ui, "Круги, лимиты, прокси", "common_extra", |ui| {
            ui.horizontal(|ui| {
                ui.label("Общий лимит на аккаунт");
                ui.add(egui::DragValue::new(&mut self.total_limit).range(0..=100_000));
                if rounds_supported {
                    ui.label("за круг");
                    ui.add(egui::DragValue::new(&mut self.round_limit).range(0..=100_000));
                }
            });
            hint(ui, "0 = не ограничивать. Действует поверх лимита самого режима — берётся меньшее.");

            if rounds_supported {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.repeat_rounds, "Повторять круги");
                    ui.add_enabled(
                        self.repeat_rounds,
                        egui::DragValue::new(&mut self.round_pause_min).range(0.0..=1440.0).suffix(" мин"),
                    );
                });
                hint(ui, "Пройти по всем аккаунтам, подождать и начать заново — до нажатия «Стоп».");
            }

            ui.checkbox(&mut self.check_auth, "Проверять авторизацию перед работой");
            hint(
                ui,
                "Заодно обновляет карму. Разлогиненные аккаунты пропускаются, а не тратят прогон впустую.",
            );

            ui.horizontal(|ui| {
                ui.label("Менять прокси после");
                ui.add(egui::DragValue::new(&mut self.proxy_rotate_fails).range(1..=20));
                ui.label("сбоев подряд");
            });
            hint(ui, "Работает, если у аккаунта задано несколько прокси через | или с новой строки.");
        });
    }
}

// ─── Голоса ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoteTargetKind {
    Profile,
    Single,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VotesForm {
    pub target_kind: VoteTargetKind,
    pub links: String,
    pub plus: bool,
    pub delay: f64,
    /// Лимит постов на профиль (0 = все).
    pub limit: i64,
}

impl Default for VotesForm {
    fn default() -> Self {
        Self { target_kind: VoteTargetKind::Profile, links: String::new(), plus: true, delay: 2.0, limit: 0 }
    }
}

impl VotesForm {
    pub fn ui(&mut self, ui: &mut Ui) {
        block(ui, "Кому крутим");
        ui.horizontal(|ui| {
            seg(ui, &mut self.target_kind, VoteTargetKind::Profile, "Весь профиль");
            seg(ui, &mut self.target_kind, VoteTargetKind::Single, "Один пост / ответ");
        });
        links_edit(
            ui,
            &mut self.links,
            match self.target_kind {
                VoteTargetKind::Profile => "https://otvet.mail.ru/profile/id000000000/",
                VoteTargetKind::Single => "https://otvet.mail.ru/question/123456789",
            },
        );
        hint(
            ui,
            match self.target_kind {
                VoteTargetKind::Profile => "По одной ссылке в строке. Каждый аккаунт пройдёт по всем профилям. Ссылка на /answers — голоса за ответы, а не за вопросы.",
                VoteTargetKind::Single => "По одной ссылке в строке. /question/12345 — сам вопрос, ?reply=67890 — конкретный ответ.",
            },
        );

        block(ui, "Что ставим");
        ui.horizontal(|ui| {
            seg(ui, &mut self.plus, true, "Плюсы  +");
            seg(ui, &mut self.plus, false, "Минусы  −");
        });

        block(ui, "Темп");
        ui.horizontal(|ui| {
            ui.label("Пауза между голосами");
            ui.add(egui::DragValue::new(&mut self.delay).range(0.0..=600.0).speed(0.1).suffix(" сек"));
        });
        if self.target_kind == VoteTargetKind::Profile {
            ui.horizontal(|ui| {
                ui.label("Постов с профиля");
                ui.add(egui::DragValue::new(&mut self.limit).range(0..=100_000));
                ui.label("(0 — все)");
            });
        }
        hint(ui, "Голос уже поставленный повторно снимается, поэтому один и тот же пост бот второй раз не трогает.");
    }

    pub fn to_params(&self, check_auth: bool) -> VoteParams {
        VoteParams {
            targets: split_lines(&self.links),
            vote: if self.plus { Vote::Plus } else { Vote::Minus },
            delay: self.delay,
            limit: self.limit,
            check_auth,
        }
    }

    /// Короткое «что сейчас произойдёт» — строка над кнопкой запуска.
    pub fn summary(&self) -> String {
        let n = split_lines(&self.links).len();
        let what = if self.plus { "плюсы" } else { "минусы" };
        match self.target_kind {
            VoteTargetKind::Profile => format!(
                "{what} на {n} профил(я/ей){}, пауза {:.1} с",
                if self.limit > 0 {
                    format!(", до {} постов", self.limit)
                } else {
                    ", все посты".into()
                },
                self.delay
            ),
            VoteTargetKind::Single => format!("{what} на {n} пост(ов), пауза {:.1} с", self.delay),
        }
    }
}

// ─── Подписки ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SubsForm {
    pub subscribe: bool,
    pub links: String,
    pub delay: f64,
}

impl Default for SubsForm {
    fn default() -> Self {
        Self { subscribe: true, links: String::new(), delay: 2.0 }
    }
}

impl SubsForm {
    pub fn ui(&mut self, ui: &mut Ui) {
        block(ui, "Что делаем");
        ui.horizontal(|ui| {
            seg(ui, &mut self.subscribe, true, "Подписаться");
            seg(ui, &mut self.subscribe, false, "Отписаться");
        });

        block(ui, "На кого");
        links_edit(ui, &mut self.links, "https://otvet.mail.ru/profile/id000000000/");
        hint(ui, "По одной ссылке на профиль в строке.");

        block(ui, "Темп");
        ui.horizontal(|ui| {
            ui.label("Пауза");
            ui.add(egui::DragValue::new(&mut self.delay).range(0.0..=600.0).speed(0.1).suffix(" сек"));
        });
        hint(ui, "Круги здесь не нужны: подписаться дважды нельзя.");
    }

    pub fn to_params(&self, check_auth: bool) -> SubParams {
        SubParams {
            profiles: split_lines(&self.links),
            action: if self.subscribe { SubAction::Subscribe } else { SubAction::Unsubscribe },
            delay: self.delay,
            check_auth,
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} на {} профил(я/ей), пауза {:.1} с",
            if self.subscribe { "подписка" } else { "отписка" },
            split_lines(&self.links).len(),
            self.delay
        )
    }
}

// ─── Жалобы ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComplainKind {
    User,
    Topics,
    Replies,
    Single,
}

impl ComplainKind {
    fn to_core(self) -> ComplainTarget {
        match self {
            ComplainKind::User => ComplainTarget::User,
            ComplainKind::Topics => ComplainTarget::Topics,
            ComplainKind::Replies => ComplainTarget::Replies,
            ComplainKind::Single => ComplainTarget::Single,
        }
    }
    fn ru(self) -> &'static str {
        match self {
            ComplainKind::User => "на профиль",
            ComplainKind::Topics => "на посты",
            ComplainKind::Replies => "на ответы",
            ComplainKind::Single => "на один пост",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ComplainForm {
    pub kind: ComplainKind,
    pub reason: String,
    pub links: String,
    pub delay: f64,
    pub limit: i64,
}

impl Default for ComplainForm {
    fn default() -> Self {
        Self { kind: ComplainKind::User, reason: "spam".into(), links: String::new(), delay: 2.0, limit: 0 }
    }
}

impl ComplainForm {
    pub fn ui(&mut self, ui: &mut Ui) {
        block(ui, "На что жалуемся");
        ui.horizontal_wrapped(|ui| {
            seg(ui, &mut self.kind, ComplainKind::User, "Профиль");
            seg(ui, &mut self.kind, ComplainKind::Topics, "Все посты");
            seg(ui, &mut self.kind, ComplainKind::Replies, "Все ответы");
            seg(ui, &mut self.kind, ComplainKind::Single, "Один пост");
        });
        ui.horizontal(|ui| {
            ui.label("Причина");
            let cur =
                REASONS.iter().find(|(code, _)| *code == self.reason).map(|(_, ru)| *ru).unwrap_or("Спам");
            egui::ComboBox::from_id_salt("complain_reason").selected_text(cur).width(200.0).show_ui(
                ui,
                |ui| {
                    for (code, ru) in REASONS {
                        if ui.selectable_label(self.reason == *code, *ru).clicked() {
                            self.reason = (*code).to_string();
                        }
                    }
                },
            );
        });

        block(ui, "Ссылки");
        links_edit(
            ui,
            &mut self.links,
            match self.kind {
                ComplainKind::Single => "https://otvet.mail.ru/question/123456789?reply=987654",
                _ => "https://otvet.mail.ru/profile/id000000000/",
            },
        );
        hint(
            ui,
            match self.kind {
                ComplainKind::Single => "По одной ссылке на пост или ответ в строке.",
                _ => "По одной ссылке на профиль в строке.",
            },
        );

        block(ui, "Темп");
        ui.horizontal(|ui| {
            ui.label("Пауза");
            ui.add(egui::DragValue::new(&mut self.delay).range(0.0..=600.0).speed(0.1).suffix(" сек"));
            if matches!(self.kind, ComplainKind::Topics | ComplainKind::Replies) {
                ui.label("не больше");
                ui.add(egui::DragValue::new(&mut self.limit).range(0..=100_000));
                ui.label("жалоб (0 — вся лента)");
            }
        });
    }

    pub fn to_params(&self, check_auth: bool) -> ComplainParams {
        ComplainParams {
            targets: split_lines(&self.links),
            target: self.kind.to_core(),
            reason: self.reason.clone(),
            delay: self.delay,
            limit: self.limit,
            check_auth,
        }
    }

    pub fn summary(&self) -> String {
        let reason =
            REASONS.iter().find(|(code, _)| *code == self.reason).map(|(_, ru)| *ru).unwrap_or("Спам");
        format!(
            "жалобы {} ({}), целей: {}, пауза {:.1} с",
            self.kind.ru(),
            reason.to_lowercase(),
            split_lines(&self.links).len(),
            self.delay
        )
    }
}

// ─── Мелкие помощники ───────────────────────────────────────────────────────

/// Кнопка-сегмент: выбранная подсвечена.
pub fn seg<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, value: T, label: &str) {
    let on = *current == value;
    let btn = egui::Button::new(egui::RichText::new(label).color(if on {
        theme::FG_STRONG
    } else {
        theme::FG_DIM
    }))
    .fill(if on { theme::BG_ACTIVE } else { theme::BG_INPUT })
    .min_size(egui::vec2(0.0, 25.0));
    if ui.add(btn).clicked() {
        *current = value;
    }
}

pub fn links_edit(ui: &mut Ui, text: &mut String, hint_text: &str) {
    ui.add(
        egui::TextEdit::multiline(text)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text(hint_text)
            .font(egui::TextStyle::Monospace),
    );
}

pub fn split_lines(s: &str) -> Vec<String> {
    s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}
