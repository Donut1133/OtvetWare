//! forms_ai.rs — панели режимов с нейросетью: Ответы, Вопросы, Комменты.
//!
//! Настройки нейросети (ключ, модель, стиль) общие для всех трёх режимов и живут
//! в одной форме: три копии одного и того же — верный способ однажды поменять
//! ключ не там, где нужно.
//!
//! Раскладка та же, что и в остальных панелях: сверху то, ради чего запускают
//! прогон, ниже темп и лимиты, редкое — под сворачивающимися заголовками.

use crate::forms::{block, extra, hint, links_edit, seg, split_lines};
use crate::theme;
use egui::Ui;
use otvet_core::ai::AiCfg;
use otvet_core::answerer::{AnswerMode, AnswerParams, ImageMode, TargetMode};
use otvet_core::asker::{AskMode, AskParams};
use otvet_core::journals::Styles;
use otvet_core::replier::{NotifTypes, ReplyMode, ReplyParams};
use serde::{Deserialize, Serialize};

/// Разумный лимит по умолчанию: постить без ограничения — худшее, что может
/// сделать бот на свежем аккаунте.
fn default_limit() -> i64 {
    5
}

/// Общие настройки нейросети.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AiForm {
    pub url: String,
    pub model: String,
    pub api_key: String,
    pub temperature: f64,
    pub max_tokens: i64,
    pub timeout_sec: u64,
    pub retries: u32,
    pub style: String,
    pub custom_prompt: String,
    pub use_custom: bool,
    pub mention: String,
}

impl Default for AiForm {
    fn default() -> Self {
        let p = AiCfg::preset();
        Self {
            url: p.url,
            model: p.model,
            api_key: String::new(),
            temperature: 0.7,
            max_tokens: 5000,
            timeout_sec: 15,
            retries: 3,
            style: "Обычный чел".into(),
            custom_prompt: String::new(),
            use_custom: false,
            mention: String::new(),
        }
    }
}

impl AiForm {
    pub fn cfg(&self) -> AiCfg {
        AiCfg {
            url: self.url.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            timeout_sec: self.timeout_sec,
            retries: self.retries,
        }
    }

    pub fn prompt(&self) -> String {
        if self.use_custom {
            self.custom_prompt.clone()
        } else {
            String::new()
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, styles: &Styles) {
        block(ui, "Нейросеть");
        ui.horizontal(|ui| {
            ui.label("Ключ");
            ui.add(
                egui::TextEdit::singleline(&mut self.api_key)
                    .password(true)
                    .desired_width(f32::INFINITY)
                    .hint_text("sk-..."),
            );
        });
        if self.api_key.trim().is_empty() {
            hint(ui, "Ключ берётся у провайдера (например, openrouter.ai). Без него нейросеть не ответит.");
        }
        ui.horizontal(|ui| {
            ui.label("Модель");
            ui.add(egui::TextEdit::singleline(&mut self.model).desired_width(f32::INFINITY));
        });
        ui.horizontal(|ui| {
            ui.label("Стиль");
            let names = styles.names();
            egui::ComboBox::from_id_salt("ai_style")
                .selected_text(if self.style.is_empty() { "Обычный чел" } else { &self.style })
                .width(210.0)
                .show_ui(ui, |ui| {
                    for n in &names {
                        if ui.selectable_label(&self.style == n, n).clicked() {
                            self.style = n.clone();
                        }
                    }
                });
            ui.checkbox(&mut self.use_custom, "свой промпт");
        });
        if self.use_custom {
            ui.add(
                egui::TextEdit::multiline(&mut self.custom_prompt)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .hint_text("Опиши, как бот должен писать"),
            );
        }

        extra(ui, "Тонкая настройка нейросети", "ai_extra", |ui| {
            ui.label("Адрес API (OpenAI-совместимый)");
            ui.add(egui::TextEdit::singleline(&mut self.url).desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                ui.label("Живость (t°)");
                ui.add(egui::DragValue::new(&mut self.temperature).range(0.0..=2.0).speed(0.05));
                ui.label("длина ответа");
                ui.add(egui::DragValue::new(&mut self.max_tokens).range(1..=100_000).speed(10.0));
                ui.label("токенов");
            });
            hint(ui, "Живость 0 — сухо и предсказуемо, 1.5 — развязно и непредсказуемо.");
            ui.horizontal(|ui| {
                ui.label("Ждать ответа");
                ui.add(egui::DragValue::new(&mut self.timeout_sec).range(3..=300).suffix(" сек"));
                ui.label("повторов при сбое");
                ui.add(egui::DragValue::new(&mut self.retries).range(0..=10));
            });
            ui.horizontal(|ui| {
                ui.label("Упоминание");
                ui.add(
                    egui::TextEdit::singleline(&mut self.mention)
                        .desired_width(f32::INFINITY)
                        .hint_text("например, название канала"),
                );
            });
            hint(ui, "Впишется в текст к месту, а не в лоб.");
            hint(ui, "Маркеры {{ДИРЕКТИВА}} и {{ПЕЛЬМЕНИ}} в промпте подставляются заново на каждый запрос — это и даёт разброс ответов.");
        });
    }

    /// Есть ли всё, чтобы нейросеть вообще ответила.
    pub fn problems(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.api_key.trim().is_empty() {
            v.push("Не вписан ключ нейросети".into());
        }
        if self.model.trim().is_empty() {
            v.push("Не выбрана модель".into());
        }
        if self.url.trim().is_empty() {
            v.push("Пустой адрес API".into());
        }
        v
    }
}

// ─── Картинки ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    Off,
    Pool,
    Folder,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ImageForm {
    pub kind: ImageKind,
    pub folder: String,
    pub count: i64,
}

impl Default for ImageForm {
    fn default() -> Self {
        Self { kind: ImageKind::Off, folder: "images".into(), count: 1 }
    }
}

impl ImageForm {
    pub fn to_core(&self) -> ImageMode {
        match self.kind {
            ImageKind::Off => ImageMode::Off,
            ImageKind::Pool => ImageMode::Gif { selected: vec![] },
            ImageKind::Folder => ImageMode::Upload { dir: self.folder.clone() },
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            seg(ui, &mut self.kind, ImageKind::Off, "без картинки");
            seg(ui, &mut self.kind, ImageKind::Pool, "из пула");
            seg(ui, &mut self.kind, ImageKind::Folder, "из папки");
            if self.kind != ImageKind::Off {
                ui.label("по");
                ui.add(egui::DragValue::new(&mut self.count).range(1..=10));
                ui.label("шт.");
            }
        });
        match self.kind {
            ImageKind::Folder => {
                ui.add(
                    egui::TextEdit::singleline(&mut self.folder)
                        .desired_width(f32::INFINITY)
                        .hint_text("images"),
                );
                hint(ui, "Файлы из папки заливаются на сайт при каждой отправке.");
            }
            ImageKind::Pool => hint(ui, "Берутся из gif-pool.json — они уже на CDN, перезаливать не нужно."),
            ImageKind::Off => {}
        }
    }
}

// ─── Ответы ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AnswersForm {
    pub mode: AnswerMode,
    /// Сколько ответов на аккаунт за прогон (0 = без лимита).
    #[serde(default = "default_limit")]
    pub limit: i64,
    pub from_links: bool,
    pub links: String,
    pub delay_min: f64,
    pub delay_max: f64,
    pub feed_min: f64,
    pub feed_max: f64,
    pub recent_scan: i64,
    pub repeat_per_question: i64,
    pub batch_size: i64,
    pub parallel: bool,
    pub continuous_feed: bool,
    pub conversational: bool,
    pub convo_budget_k: f64,
    pub skip_others: bool,
    pub random_tag: bool,
    pub signature: String,
    pub image: ImageForm,
}

impl Default for AnswersForm {
    fn default() -> Self {
        Self {
            mode: AnswerMode::Ai,
            limit: default_limit(),
            from_links: false,
            links: String::new(),
            delay_min: 20.0,
            delay_max: 45.0,
            feed_min: 10.0,
            feed_max: 20.0,
            recent_scan: 10,
            repeat_per_question: 1,
            batch_size: 1,
            parallel: false,
            continuous_feed: false,
            conversational: false,
            convo_budget_k: 60.0,
            skip_others: false,
            random_tag: false,
            signature: String::new(),
            image: ImageForm::default(),
        }
    }
}

impl AnswersForm {
    pub fn ui(&mut self, ui: &mut Ui, ai: &mut AiForm, styles: &Styles) {
        block(ui, "Откуда берём вопросы");
        ui.horizontal(|ui| {
            seg(ui, &mut self.from_links, false, "Из ленты");
            seg(ui, &mut self.from_links, true, "По ссылкам");
        });
        if self.from_links {
            links_edit(ui, &mut self.links, "https://otvet.mail.ru/question/123456789");
            hint(ui, "По одной ссылке в строке. На что уже отвечали — пропустится.");
        } else {
            ui.horizontal(|ui| {
                ui.label("Смотреть последних");
                ui.add(egui::DragValue::new(&mut self.recent_scan).range(1..=50));
                ui.label("вопросов");
            });
            hint(ui, "Бот берёт только самые свежие вопросы: в старые не лезет, а ждёт новых.");
        }

        block(ui, "Чем отвечаем");
        ui.horizontal(|ui| {
            seg(ui, &mut self.mode, AnswerMode::Ai, "Нейросетью");
            seg(ui, &mut self.mode, AnswerMode::NoAi, "Готовыми фразами");
            seg(ui, &mut self.mode, AnswerMode::Mangle, "Коверканьем");
        });
        hint(
            ui,
            match self.mode {
                AnswerMode::Ai => "Настоящий ответ по смыслу вопроса. Нужен ключ нейросети.",
                AnswerMode::NoAi => "Короткие реплики вроде «согласен» и «жиза». Бесплатно и быстро.",
                AnswerMode::Mangle => "Слова вопроса в случайном порядке. Для кармы, а не для смысла.",
            },
        );
        if self.mode == AnswerMode::Ai {
            ai.ui(ui, styles);
        }

        block(ui, "Сколько и как часто");
        ui.horizontal(|ui| {
            ui.label("Ответов на аккаунт");
            ui.add(egui::DragValue::new(&mut self.limit).range(0..=100_000));
            ui.label("(0 — без лимита)");
        });
        ui.horizontal(|ui| {
            ui.label("Пауза между ответами");
            ui.add(egui::DragValue::new(&mut self.delay_min).range(0.0..=3600.0).speed(0.5));
            ui.label("–");
            ui.add(egui::DragValue::new(&mut self.delay_max).range(0.0..=3600.0).speed(0.5).suffix(" сек"));
        });
        hint(ui, "Случайное значение из промежутка — ровные паузы выглядят машинно.");

        extra(ui, "Лента и пачки", "ans_feed", |ui| {
            if !self.from_links {
                ui.horizontal(|ui| {
                    ui.label("Обновлять ленту через");
                    ui.add(egui::DragValue::new(&mut self.feed_min).range(0.0..=600.0).speed(0.5));
                    ui.label("–");
                    ui.add(
                        egui::DragValue::new(&mut self.feed_max).range(0.0..=600.0).speed(0.5).suffix(" сек"),
                    );
                });
                hint(ui, "Пауза, когда отвечать не на что — все свежие вопросы уже разобраны.");
            }
            ui.horizontal(|ui| {
                ui.label("Брать за проход");
                ui.add(egui::DragValue::new(&mut self.batch_size).range(1..=50));
                ui.label("вопрос(ов)");
            });
            ui.checkbox(&mut self.parallel, "отправлять пачку разом");
            hint(ui, "Быстро, но несколько ответов в одну секунду с одного аккаунта — заметный след.");
            ui.checkbox(&mut self.continuous_feed, "не ждать конца пачки");
            hint(ui, "Новые вопросы уходят в работу сразу, как появились в ленте.");
            ui.horizontal(|ui| {
                ui.label("Ответов на один вопрос");
                ui.add(egui::DragValue::new(&mut self.repeat_per_question).range(1..=20));
            });
        });

        extra(ui, "Единый чат с памятью", "ans_convo", |ui| {
            ui.checkbox(&mut self.conversational, "помнить прошлые вопросы и ответы");
            hint(
                ui,
                "Нейросеть держит один разговор и общий характер. История общая на все аккаунты, поэтому работает один аккаунт за раз.",
            );
            ui.horizontal(|ui| {
                ui.label("Сжимать историю после");
                ui.add_enabled(
                    self.conversational,
                    egui::DragValue::new(&mut self.convo_budget_k).range(0.0..=1000.0).suffix(" тыс. знаков"),
                );
            });
            hint(ui, "0 — не сжимать (для моделей с большим контекстом).");
        });

        extra(ui, "Уникальность и подпись", "ans_uniq", |ui| {
            ui.checkbox(&mut self.skip_others, "не отвечать туда, где уже был другой мой аккаунт");
            ui.checkbox(&mut self.random_tag, "дописывать в конец #случайные-цифры");
            hint(ui, "Спасает от «такой ответ уже есть», но виден в тексте.");
            ui.label("Подпись в конце");
            ui.add(
                egui::TextEdit::singleline(&mut self.signature)
                    .desired_width(f32::INFINITY)
                    .hint_text("необязательно"),
            );
        });

        extra(ui, "Картинка к ответу", "ans_img", |ui| self.image.ui(ui));
    }

    pub fn to_params(&self, ai: &AiForm, check_auth: bool) -> AnswerParams {
        AnswerParams {
            mode: self.mode,
            target: if self.from_links { TargetMode::Links } else { TargetMode::Feed },
            links: split_lines(&self.links),
            limit: self.limit,
            delay_min: self.delay_min,
            delay_max: self.delay_max,
            feed_min: self.feed_min,
            feed_max: self.feed_max,
            recent_scan: self.recent_scan,
            repeat_per_question: self.repeat_per_question,
            batch_size: self.batch_size,
            parallel: self.parallel,
            continuous_feed: self.continuous_feed,
            conversational: self.conversational,
            convo_budget_k: self.convo_budget_k,
            skip_others: self.skip_others,
            random_tag: self.random_tag,
            signature: self.signature.clone(),
            noai_answers: vec![],
            image: self.image.to_core(),
            image_count: self.image.count,
            ai: ai.cfg(),
            style: ai.style.clone(),
            custom_prompt: ai.prompt(),
            mention: ai.mention.clone(),
            check_auth,
        }
    }

    pub fn summary(&self) -> String {
        let how = match self.mode {
            AnswerMode::Ai => "нейросетью",
            AnswerMode::NoAi => "готовыми фразами",
            AnswerMode::Mangle => "коверканьем",
        };
        let src = if self.from_links {
            format!("по {} ссылк(ам)", split_lines(&self.links).len())
        } else {
            "из ленты".to_string()
        };
        format!(
            "{} {how} {src}, пауза {:.0}–{:.0} с",
            if self.limit > 0 {
                format!("по {} ответ(ов)", self.limit)
            } else {
                "без лимита".into()
            },
            self.delay_min,
            self.delay_max
        )
    }

    pub fn problems(&self, ai: &AiForm) -> Vec<String> {
        let mut v = Vec::new();
        if self.from_links && split_lines(&self.links).is_empty() {
            v.push("Не вставлены ссылки на вопросы".into());
        }
        if self.mode == AnswerMode::Ai {
            v.extend(ai.problems());
        }
        v
    }
}

// ─── Вопросы ────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct QuestionsForm {
    pub mode: AskMode,
    #[serde(default = "default_limit")]
    pub limit: i64,
    pub delay_min: f64,
    pub delay_max: f64,
    pub topic: String,
    pub random_tag: bool,
    pub image: ImageForm,
}

impl Default for QuestionsForm {
    fn default() -> Self {
        Self {
            mode: AskMode::Ai,
            limit: default_limit(),
            delay_min: 30.0,
            delay_max: 90.0,
            topic: String::new(),
            random_tag: false,
            image: ImageForm::default(),
        }
    }
}

impl QuestionsForm {
    pub fn ui(&mut self, ui: &mut Ui, ai: &mut AiForm, styles: &Styles) {
        block(ui, "Откуда берём вопросы");
        ui.horizontal(|ui| {
            seg(ui, &mut self.mode, AskMode::Ai, "Придумывает нейросеть");
            seg(ui, &mut self.mode, AskMode::NoAi, "Из готового списка");
        });
        if self.mode == AskMode::Ai {
            ui.horizontal(|ui| {
                ui.label("Тема");
                ui.add(
                    egui::TextEdit::singleline(&mut self.topic)
                        .desired_width(f32::INFINITY)
                        .hint_text("пусто — случайная тема из styles.json"),
                );
            });
            ai.ui(ui, styles);
        } else {
            hint(ui, "Простые бытовые вопросы из встроенного списка; уже заданные не повторяются.");
        }

        block(ui, "Сколько и как часто");
        ui.horizontal(|ui| {
            ui.label("Вопросов на аккаунт");
            ui.add(egui::DragValue::new(&mut self.limit).range(0..=100_000));
            ui.label("(0 — без лимита)");
        });
        ui.horizontal(|ui| {
            ui.label("Пауза");
            ui.add(egui::DragValue::new(&mut self.delay_min).range(0.0..=3600.0).speed(0.5));
            ui.label("–");
            ui.add(egui::DragValue::new(&mut self.delay_max).range(0.0..=3600.0).speed(0.5).suffix(" сек"));
        });
        hint(ui, "Заголовок длиннее 120 знаков сайт не принимает — хвост уедет в тело вопроса сам.");

        extra(ui, "Уникальность", "q_uniq", |ui| {
            ui.checkbox(&mut self.random_tag, "дописывать #случайные-цифры");
        });
        extra(ui, "Картинка к вопросу", "q_img", |ui| self.image.ui(ui));
    }

    pub fn to_params(&self, ai: &AiForm, check_auth: bool) -> AskParams {
        AskParams {
            mode: self.mode,
            limit: self.limit,
            delay_min: self.delay_min,
            delay_max: self.delay_max,
            ai: ai.cfg(),
            style: ai.style.clone(),
            custom_prompt: ai.prompt(),
            mention: ai.mention.clone(),
            topic: self.topic.clone(),
            noai_questions: vec![],
            random_tag: self.random_tag,
            image: self.image.to_core(),
            image_count: self.image.count,
            check_auth,
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} {}, пауза {:.0}–{:.0} с",
            if self.limit > 0 {
                format!("по {} вопрос(ов)", self.limit)
            } else {
                "без лимита".into()
            },
            if self.mode == AskMode::Ai { "от нейросети" } else { "из списка" },
            self.delay_min,
            self.delay_max
        )
    }

    pub fn problems(&self, ai: &AiForm) -> Vec<String> {
        if self.mode == AskMode::Ai {
            ai.problems()
        } else {
            vec![]
        }
    }
}

// ─── Комменты ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CommentsForm {
    pub mode: ReplyMode,
    #[serde(default = "default_limit")]
    pub limit: i64,
    pub types: NotifTypes,
    pub delay_min: f64,
    pub delay_max: f64,
    pub max_age_hours: f64,
    pub max_per_thread: i64,
    pub pages: i64,
    pub use_question: bool,
    pub use_chain: bool,
    pub max_chain: usize,
    pub skip_own: bool,
    pub mark_read: bool,
    pub random_tag: bool,
    pub signature: String,
}

impl Default for CommentsForm {
    fn default() -> Self {
        Self {
            mode: ReplyMode::Ai,
            limit: default_limit(),
            types: NotifTypes::default(),
            delay_min: 25.0,
            delay_max: 60.0,
            max_age_hours: 24.0,
            max_per_thread: 1,
            pages: 1,
            use_question: true,
            use_chain: true,
            max_chain: 6,
            skip_own: true,
            mark_read: false,
            random_tag: false,
            signature: String::new(),
        }
    }
}

impl CommentsForm {
    pub fn ui(&mut self, ui: &mut Ui, ai: &mut AiForm, styles: &Styles) {
        block(ui, "На что отвечаем");
        ui.checkbox(&mut self.types.reply, "написали под моим ответом");
        ui.checkbox(&mut self.types.topic, "ответили на мой вопрос");
        ui.checkbox(&mut self.types.mention, "упомянули меня");
        hint(ui, "Берётся из уведомлений аккаунта — то же, что видно в колокольчике на сайте.");

        block(ui, "Чем отвечаем");
        ui.horizontal(|ui| {
            seg(ui, &mut self.mode, ReplyMode::Ai, "Нейросетью");
            seg(ui, &mut self.mode, ReplyMode::NoAi, "Готовыми фразами");
        });
        if self.mode == ReplyMode::Ai {
            ai.ui(ui, styles);
        }

        block(ui, "Сколько и как часто");
        ui.horizontal(|ui| {
            ui.label("Ответов на аккаунт");
            ui.add(egui::DragValue::new(&mut self.limit).range(0..=100_000));
            ui.label("(0 — без лимита)");
        });
        ui.horizontal(|ui| {
            ui.label("Пауза");
            ui.add(egui::DragValue::new(&mut self.delay_min).range(0.0..=3600.0).speed(0.5));
            ui.label("–");
            ui.add(egui::DragValue::new(&mut self.delay_max).range(0.0..=3600.0).speed(0.5).suffix(" сек"));
        });

        extra(ui, "Кого пропускать", "cm_filter", |ui| {
            ui.checkbox(&mut self.skip_own, "не отвечать своим же аккаунтам");
            ui.horizontal(|ui| {
                ui.label("Не старше");
                ui.add(egui::DragValue::new(&mut self.max_age_hours).range(0.0..=8760.0).suffix(" ч"));
                ui.label("(0 — любые)");
            });
            ui.horizontal(|ui| {
                ui.label("Не больше");
                ui.add(egui::DragValue::new(&mut self.max_per_thread).range(1..=20));
                ui.label("ответ(ов) в одну ветку");
            });
            ui.horizontal(|ui| {
                ui.label("Просмотреть");
                ui.add(egui::DragValue::new(&mut self.pages).range(1..=10));
                ui.label("стр. уведомлений");
            });
            hint(ui, "Сайт отдаёт по 20 уведомлений на страницу.");
        });

        extra(ui, "Что показывать нейросети", "cm_ctx", |ui| {
            ui.checkbox(&mut self.use_question, "текст самого вопроса");
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.use_chain, "всю переписку в ветке, до");
                ui.add_enabled(self.use_chain, egui::DragValue::new(&mut self.max_chain).range(2..=20));
                ui.label("реплик");
            });
            hint(ui, "С перепиской ответы попадают в контекст разговора, но каждый запрос дороже.");
        });

        extra(ui, "Уникальность и подпись", "cm_uniq", |ui| {
            ui.checkbox(&mut self.random_tag, "дописывать #случайные-цифры");
            ui.label("Подпись в конце");
            ui.add(
                egui::TextEdit::singleline(&mut self.signature)
                    .desired_width(f32::INFINITY)
                    .hint_text("необязательно"),
            );
            ui.checkbox(&mut self.mark_read, "в конце пометить уведомления прочитанными");
            if self.mark_read {
                ui.colored_label(
                    theme::WARN,
                    "Сайт помечает прочитанным ВСЁ разом — не только то, на что ответили.",
                );
            }
        });
    }

    pub fn to_params(&self, ai: &AiForm, check_auth: bool) -> ReplyParams {
        ReplyParams {
            mode: self.mode,
            limit: self.limit,
            delay_min: self.delay_min,
            delay_max: self.delay_max,
            types: self.types,
            skip_own: self.skip_own,
            max_age_hours: self.max_age_hours,
            max_per_thread: self.max_per_thread,
            pages: self.pages,
            use_question: self.use_question,
            use_chain: self.use_chain,
            max_chain: self.max_chain,
            ai: ai.cfg(),
            style: ai.style.clone(),
            custom_prompt: ai.prompt(),
            mention: ai.mention.clone(),
            noai_replies: vec![],
            random_tag: self.random_tag,
            signature: self.signature.clone(),
            mark_read: self.mark_read,
            check_auth,
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} {}, пауза {:.0}–{:.0} с",
            if self.limit > 0 {
                format!("по {} ответ(ов)", self.limit)
            } else {
                "без лимита".into()
            },
            if self.mode == ReplyMode::Ai {
                "нейросетью"
            } else {
                "готовыми фразами"
            },
            self.delay_min,
            self.delay_max
        )
    }

    pub fn problems(&self, ai: &AiForm) -> Vec<String> {
        let mut v = Vec::new();
        if !self.types.reply && !self.types.topic && !self.types.mention {
            v.push("Не выбрано ни одного вида уведомлений".into());
        }
        if self.mode == ReplyMode::Ai {
            v.extend(ai.problems());
        }
        v
    }
}
