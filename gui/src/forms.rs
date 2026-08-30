//! forms.rs — параметры режимов: то, что пользователь крутит слева.
//!
//! Настроек много, и это осознанно: ни одна не выброшена. Чтобы панель не
//! выглядела свалкой, всё разложено по блокам — сверху то, ради чего запускают
//! прогон, ниже темп и лимиты, редкое убрано под сворачивающиеся заголовки.
//! Формы сериализуются: настройки переживают перезапуск.

use egui::Ui;
use otvet_core::complain::{ComplainParams, ComplainTarget, REASONS};
use otvet_core::subscribe::{SubAction, SubParams};
use otvet_core::uniq::UniqMode;
use otvet_core::util::Progress;
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
    /// `feed_mode` — бот сам находит себе работу (ответы, вопросы, комменты).
    /// В режимах по ссылкам списком ссылок работа и ограничена: лимиты и круги
    /// там показывать нечего.
    pub fn ui(&mut self, ui: &mut Ui, selected: usize, feed_mode: bool) {
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
            ui.colored_label(theme::WARN, format!("{selected} аккаунт(ов) разом — это заметно"));
        }

        let title = if feed_mode {
            "Круги, лимиты, прокси"
        } else {
            "Проверки и прокси"
        };
        extra(ui, title, "common_extra", |ui| {
            if feed_mode {
                ui.horizontal(|ui| {
                    ui.label("Общий лимит на аккаунт");
                    ui.add(egui::DragValue::new(&mut self.total_limit).range(0..=100_000));
                    ui.label("за круг");
                    ui.add(egui::DragValue::new(&mut self.round_limit).range(0..=100_000));
                });
                hint(
                    ui,
                    "0 = не ограничивать. Общий держит весь прогон целиком, включая повторные круги; лимит самого режима — один проход. Берётся меньшее.",
                );
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

// ─── Общие кусочки форм ─────────────────────────────────────────────────────

/// Блок «Уникальность текста». Один и тот же в трёх режимах, поэтому здесь.
pub fn uniq_block(ui: &mut Ui, mode: &mut UniqMode, latin: &mut bool) {
    ui.horizontal(|ui| {
        ui.label("Уникализация");
        seg(ui, mode, UniqMode::Off, "нет");
        seg(ui, mode, UniqMode::Light, "лёгкая");
        seg(ui, mode, UniqMode::Normal, "обычная");
        seg(ui, mode, UniqMode::Hard, "сильная");
    });
    hint(
        ui,
        "Мелкие правки, какие делает живой человек: точка в конце то есть, то нет, «ещё» вместо «еще», словечко в начале. Один и тот же текст с разных аккаунтов модерация ловит, слегка разный — уже нет.",
    );
    if *mode != UniqMode::Off {
        ui.checkbox(latin, "подменять похожие буквы латиницей");
        hint(
            ui,
            "Сильнее размывает текст, но смешанные алфавиты внутри слова — сами по себе признак спама. Включать осознанно.",
        );
    }
    if *mode == UniqMode::Hard {
        hint(ui, "«Сильная» ещё и дописывает в конец число — без решётки и разной длины.");
    }
}

/// Блок «Проверять, что отправилось». `what` — что именно проверяем.
pub fn verify_block(ui: &mut Ui, on: &mut bool, delay: &mut f64, what: &str) {
    ui.horizontal(|ui| {
        ui.checkbox(on, format!("проверять, что {what} остался на сайте"));
        ui.add_enabled(*on, egui::DragValue::new(delay).range(0.0..=120.0).speed(0.5).suffix(" сек"));
    });
    hint(
        ui,
        &format!("Сайт отвечает «принято» и на то, что через секунду снесёт автомодерация. Если {what}а на месте нет — бот его не засчитывает и пробует другим текстом."),
    );
}

/// Пара «от–до»: верхняя граница не опускается ниже нижней.
///
/// Перевёрнутый промежуток ядро молча схлопывает в одно число: при «от 50 до 10»
/// пауза всегда ровно 50 секунд. Молча — хуже всего, поэтому просто не даём его
/// задать.
pub fn range_row(ui: &mut Ui, label: &str, min: &mut f64, max: &mut f64, top: f64) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(min).range(0.0..=top).speed(0.5));
        ui.label("–");
        ui.add(egui::DragValue::new(max).range(0.0..=top).speed(0.5).suffix(" сек"));
    });
    if *max < *min {
        *max = *min;
    }
}

/// Поле для своего списка готовых фраз (по одной в строке).
pub fn list_edit(ui: &mut Ui, id: &str, text: &mut String, title: &str, hint_text: &str, placeholder: &str) {
    ui.label(title);
    boxed_multiline(ui, id, text, 4, placeholder);
    let n = split_lines(text).len();
    hint(ui, &if n == 0 { hint_text.to_string() } else { format!("{hint_text} Своих строк: {n}.") });
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
            "votes_links",
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

        block(ui, "Сколько и как часто");
        if self.target_kind == VoteTargetKind::Profile {
            ui.horizontal(|ui| {
                ui.label("Постов с профиля");
                ui.add(egui::DragValue::new(&mut self.limit).range(0..=100_000));
                ui.label("(0 — все)");
            });
        }
        ui.horizontal(|ui| {
            ui.label("Пауза между голосами");
            ui.add(egui::DragValue::new(&mut self.delay).range(0.0..=600.0).speed(0.1).suffix(" сек"));
        });
        hint(
            ui,
            "Тот же голос второй раз ничего не меняет — сайт оставляет его как есть, так что перезапуск не страшен. А вот противоположный знак перезаписывает: минус поверх плюса сделает минус.",
        );
    }

    pub fn to_params(&self, check_auth: bool) -> VoteParams {
        VoteParams {
            targets: split_lines(&self.links),
            vote: if self.plus { Vote::Plus } else { Vote::Minus },
            delay: self.delay,
            limit: self.limit,
            check_auth,
            progress: Progress::new(),
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
        links_edit(ui, "subs_links", &mut self.links, "https://otvet.mail.ru/profile/id000000000/");
        hint(ui, "По одной ссылке на профиль в строке.");

        block(ui, "Как часто");
        ui.horizontal(|ui| {
            ui.label("Пауза между подписками");
            ui.add(egui::DragValue::new(&mut self.delay).range(0.0..=600.0).speed(0.1).suffix(" сек"));
        });
        hint(ui, "Сколько подписок — столько и ссылок: подписаться на одного дважды нельзя.");
    }

    pub fn to_params(&self, check_auth: bool) -> SubParams {
        SubParams {
            profiles: split_lines(&self.links),
            action: if self.subscribe { SubAction::Subscribe } else { SubAction::Unsubscribe },
            delay: self.delay,
            // Лимита у режима нет: сколько ссылок, столько и подписок.
            limit: 0,
            check_auth,
            progress: Progress::new(),
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
            "complain_links",
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

        block(ui, "Сколько и как часто");
        ui.horizontal(|ui| {
            ui.label("Пауза между жалобами");
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
            progress: Progress::new(),
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

/// Многострочное поле постоянной высоты: список внутри листается, а само поле
/// не растёт. Иначе два десятка ссылок распирают форму, и кнопка «Старт»
/// уезжает за нижний край окна — приходится листать всю панель, чтобы её найти.
pub fn boxed_multiline(ui: &mut Ui, id: &str, text: &mut String, rows: usize, hint_text: &str) {
    // Плюс половина строки: столько же строк, сколько просили, плюс краешек
    // следующей — по нему сразу видно, что список длиннее и его можно листать.
    let h = ui.text_style_height(&egui::TextStyle::Monospace) * (rows as f32 + 0.5);
    // Рамку рисуем САМИ, а поле внутри делаем без своей.
    //
    // Иначе рамка принадлежит полю, растёт вместе с текстом и уезжает вместе с
    // ним: при прокрутке верхний и нижний края просто пропадали из виду, и
    // список повисал в воздухе. Наша рамка стоит на месте, а ездит только текст.
    //
    // Рамку задаём ДО раскладки, вместе с её толщиной: egui вычитает толщину из
    // места под содержимое в `begin`, а прибавляет обратно в `end`. Если
    // дорисовать рамку потом, поле займёт на 2 px больше, чем ему дали, — и
    // левая панель, упираясь в это, начинает расширяться сама, по два пикселя
    // за кадр, пока не упрётся в свой предел.
    let line = ui.visuals().widgets.inactive.bg_stroke;
    let frame = egui::Frame::default()
        .fill(theme::BG_INPUT)
        .stroke(line)
        .corner_radius(egui::CornerRadius::same(2))
        .inner_margin(egui::Margin::symmetric(6, 6));
    let mut prepared = frame.begin(ui);
    let inner = &mut prepared.content_ui;
    let resp = egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(h)
        // Без этого egui держит для прокручиваемой области свои 64 px: поле в
        // три строки разъезжалось до четырёх с половиной, и нижняя оказывалась
        // разрезанной пополам.
        .min_scrolled_height(h)
        .auto_shrink([false, false])
        .show(inner, |ui| {
            ui.add(
                egui::TextEdit::multiline(text)
                    .desired_rows(rows)
                    .desired_width(f32::INFINITY)
                    .hint_text(hint_text)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin::ZERO)
                    .font(egui::TextStyle::Monospace),
            )
        })
        .inner;
    // Подсветку фокуса поле больше не рисует — берём её на себя, иначе не видно,
    // куда именно попадёт набранное.
    // Меняем ТОЛЬКО цвет: толщина уже учтена в раскладке.
    let v = ui.visuals();
    prepared.frame.stroke.color = if resp.has_focus() {
        v.widgets.active.bg_stroke.color
    } else if resp.hovered() {
        v.widgets.hovered.bg_stroke.color
    } else {
        line.color
    };
    prepared.end(ui);
}

pub fn links_edit(ui: &mut Ui, id: &str, text: &mut String, hint_text: &str) {
    boxed_multiline(ui, id, text, 3, hint_text);
}

/// Длинную строку показываем с серединой в многоточии: хэш картинки или ссылка
/// целиком не нужны, а начало и конец опознаются с одного взгляда.
pub fn clip_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max / 2).collect();
    let tail: String = s.chars().skip(n - max / 2 + 1).collect();
    format!("{head}…{tail}")
}

pub fn split_lines(s: &str) -> Vec<String> {
    s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forms_ai::{AiForm, AnswersForm, CommentsForm, QuestionsForm};
    use otvet_core::journals::Styles;

    /// Ширина, на которую панель настроек имеет право рассчитывать.
    /// Это её минимум из `Panel::left(...).size_range(...)` в app.rs.
    const PANEL_MIN: f32 = 360.0;

    /// Сколько ширины содержимое заняло на самом деле, если дать ему `PANEL_MIN`.
    fn width_used(mut add: impl FnMut(&mut Ui)) -> f32 {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut used = 0.0;
        // Два кадра: на первом egui ещё не знает размеров и отвечает наугад.
        for _ in 0..2 {
            let _ = ctx.run_ui(Default::default(), |ui| {
                let rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(PANEL_MIN, 4000.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                add(&mut child);
                used = child.min_rect().width();
            });
        }
        used
    }

    /// Ни одна панель не должна просить больше ширины, чем ей дали.
    ///
    /// Иначе левая панель расширяется САМА: egui отдаёт ей ширину содержимого,
    /// на следующем кадре содержимое снова просит на столько же больше — и так
    /// до упора. Ровно это и случилось, когда у поля со ссылками появилась своя
    /// рамка: она добавляла два пикселя, и панель ползла вправо на глазах.
    #[test]
    fn panels_fit_the_width_they_are_given() {
        let styles = Styles::default();
        type Panel = Box<dyn FnMut(&mut Ui)>;
        let cases: Vec<(&str, Panel)> = vec![
            (
                "Голоса",
                Box::new({
                    let mut f = VotesForm {
                        links: "https://otvet.mail.ru/profile/x
"
                        .repeat(5),
                        ..Default::default()
                    };
                    move |ui: &mut Ui| f.ui(ui)
                }),
            ),
            (
                "Подписки",
                Box::new({
                    let mut f = SubsForm {
                        links: "https://otvet.mail.ru/profile/x
"
                        .repeat(5),
                        ..Default::default()
                    };
                    move |ui: &mut Ui| f.ui(ui)
                }),
            ),
            (
                "Жалобы",
                Box::new({
                    let mut f = ComplainForm::default();
                    move |ui: &mut Ui| f.ui(ui)
                }),
            ),
            (
                "Ответы",
                Box::new({
                    let (mut f, mut ai, styles) = (AnswersForm::default(), AiForm::default(), styles.clone());
                    move |ui: &mut Ui| {
                        f.ui(ui, &mut ai, &styles);
                    }
                }),
            ),
            (
                "Ответы по ссылкам",
                Box::new({
                    let (mut f, mut ai, styles) = (AnswersForm::default(), AiForm::default(), styles.clone());
                    f.source = crate::forms_ai::Source::Links;
                    move |ui: &mut Ui| {
                        f.ui(ui, &mut ai, &styles);
                    }
                }),
            ),
            (
                "Ответы по диапазону",
                Box::new({
                    let (mut f, mut ai, styles) = (AnswersForm::default(), AiForm::default(), styles.clone());
                    f.source = crate::forms_ai::Source::Range;
                    f.range_from = "https://otvet.mail.ru/question/270377181".into();
                    f.range_to = "https://otvet.mail.ru/question/270377999".into();
                    move |ui: &mut Ui| {
                        f.ui(ui, &mut ai, &styles);
                    }
                }),
            ),
            (
                "Вопросы",
                Box::new({
                    let (mut f, mut ai, styles) =
                        (QuestionsForm::default(), AiForm::default(), styles.clone());
                    move |ui: &mut Ui| {
                        f.ui(ui, &mut ai, &styles);
                    }
                }),
            ),
            (
                "Комменты",
                Box::new({
                    let (mut f, mut ai, styles) =
                        (CommentsForm::default(), AiForm::default(), styles.clone());
                    move |ui: &mut Ui| {
                        f.ui(ui, &mut ai, &styles);
                    }
                }),
            ),
            (
                "Общие настройки",
                Box::new({
                    let mut f = CommonForm::default();
                    move |ui: &mut Ui| f.ui(ui, 3, true)
                }),
            ),
        ];
        for (name, add) in cases {
            let w = width_used(add);
            assert!(w <= PANEL_MIN + 0.5, "«{name}» просит {w:.1} px при отведённых {PANEL_MIN}");
        }
    }
}
