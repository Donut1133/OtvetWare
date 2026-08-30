//! forms_ai.rs — панели режимов с нейросетью: Ответы, Вопросы, Комменты.
//!
//! Настройки нейросети (ключ, модель, стиль) общие для всех трёх режимов и живут
//! в одной форме: три копии одного и того же — верный способ однажды поменять
//! ключ не там, где нужно.
//!
//! Раскладка та же, что и в остальных панелях: сверху то, ради чего запускают
//! прогон, ниже темп и лимиты, редкое — под сворачивающимися заголовками.

use crate::forms::{
    block, extra, hint, links_edit, list_edit, range_row, seg, split_lines, uniq_block, verify_block,
};
use crate::theme;
use egui::Ui;
use otvet_core::ai::AiCfg;
use otvet_core::answerer::{AnswerMode, AnswerParams, ImageMode, TargetMode};
use otvet_core::asker::{AskMode, AskParams};
use otvet_core::journals::Styles;
use otvet_core::replier::{NotifTypes, ReplyMode, ReplyParams};
use otvet_core::uniq::UniqMode;
use otvet_core::util::Progress;
use serde::{Deserialize, Serialize};

/// Разумный лимит по умолчанию: постить без ограничения — худшее, что может
/// сделать бот на свежем аккаунте.
fn default_limit() -> i64 {
    5
}

/// Значения по умолчанию для полей, которых нет в старых сохранённых
/// настройках: без них serde не прочитает файл, оставшийся от прошлой версии.
fn yes() -> bool {
    true
}

fn default_verify_delay() -> f64 {
    6.0
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
            hint(
                ui,
                "Ключ берётся у провайдера — по умолчанию тут DeepSeek. Без ключа нейросеть не ответит.",
            );
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
                // Не шире: строка «Стиль + свой промпт» — самая широкая в панели,
                // и на крупном системном шрифте именно она упирала бы левую
                // панель в предел, не давая её сузить.
                .width(170.0)
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
            crate::forms::boxed_multiline(
                ui,
                "ai_prompt",
                &mut self.custom_prompt,
                3,
                "Опиши, как бот должен писать",
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
            hint(ui, "Маркеры для своего промпта. {{ДИРЕКТИВА}} — заход, длина и тон ответа, выбираются заново на каждый запрос: это и даёт разброс, иначе модель отвечает по одному шаблону. {{ПЕЛЬМЕНИ}} — фирменная фраза стиля «Смех», каждый раз написанная по-другому.");
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
    /// Хэши картинок пула, которые разрешено прикладывать. Пусто — годится
    /// любая. Отмечаются в окне пула, хранятся вместе с настройками режима:
    /// к ответам и к вопросам обычно идут разные картинки.
    #[serde(default)]
    pub selected: Vec<String>,
}

impl Default for ImageForm {
    fn default() -> Self {
        Self { kind: ImageKind::Off, folder: "images".into(), count: 1, selected: vec![] }
    }
}

impl ImageForm {
    pub fn to_core(&self) -> ImageMode {
        match self.kind {
            ImageKind::Off => ImageMode::Off,
            ImageKind::Pool => ImageMode::Gif { selected: self.selected.clone() },
            ImageKind::Folder => ImageMode::Upload { dir: self.folder.clone() },
        }
    }

    /// `true` — попросили открыть окно управления пулом.
    pub fn ui(&mut self, ui: &mut Ui) -> bool {
        let mut open_pool = false;
        ui.horizontal(|ui| {
            seg(ui, &mut self.kind, ImageKind::Off, "без картинки");
            seg(ui, &mut self.kind, ImageKind::Pool, "из пула");
            seg(ui, &mut self.kind, ImageKind::Folder, "из папки");
            if self.kind != ImageKind::Off {
                ui.label("картинок в посте");
                ui.add(egui::DragValue::new(&mut self.count).range(1..=10));
            }
        });
        match self.kind {
            ImageKind::Folder => {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.folder)
                            .desired_width(ui.available_width() - 90.0)
                            .hint_text("images"),
                    );
                    if ui.button("Выбрать…").clicked() {
                        if let Some(d) = rfd::FileDialog::new().set_title("Папка с картинками").pick_folder()
                        {
                            self.folder = d.display().to_string();
                        }
                    }
                });
                hint(ui, "Файлы из папки заливаются на сайт при каждой отправке.");
            }
            ImageKind::Pool => {
                ui.horizontal(|ui| {
                    if ui.button("Пул картинок…").clicked() {
                        open_pool = true;
                    }
                    ui.label(match self.selected.len() {
                        0 => "берётся любая из пула".to_string(),
                        n => format!("берутся только отмеченные: {n}"),
                    });
                });
                hint(
                    ui,
                    "Какие именно прикладывать — отмечается галочками в окне пула. Без отметок бот берёт любую.",
                );
                hint(ui, "Картинки из пула уже лежат на CDN сайта — перезаливать их не нужно.");
            }
            ImageKind::Off => {}
        }
        open_pool
    }
}

// ─── Ответы ─────────────────────────────────────────────────────────────────

/// Откуда бот берёт вопросы.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Source {
    /// Свежие вопросы из ленты.
    #[default]
    Feed,
    /// Готовый список ссылок.
    Links,
    /// Диапазон номеров — в том числе тех вопросов, которых ещё нет.
    Range,
}

/// Номер вопроса из ссылки или просто из числа.
fn topic_number(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    otvet_core::votes::topic_id_from_url(s)
        .and_then(|t| t.parse::<i64>().ok())
        .or_else(|| s.parse::<i64>().ok())
        .filter(|n| *n > 0)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AnswersForm {
    pub mode: AnswerMode,
    /// Сколько ответов на аккаунт за прогон (0 = без лимита).
    #[serde(default = "default_limit")]
    pub limit: i64,
    /// Откуда берём вопросы.
    #[serde(default)]
    pub source: Source,
    /// Прежние настройки хранили вместо трёх источников одну галку `from_links`.
    /// Читаем её, чтобы после обновления не сбрасывать выбор человека на ленту;
    /// обратно не пишем — см. [`AnswersForm::migrate`].
    #[serde(default, rename = "from_links", skip_serializing)]
    legacy_from_links: bool,
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
    /// Не отвечать на вопросы своих же аккаунтов.
    #[serde(default = "yes")]
    pub skip_own_authors: bool,
    /// Показывать нейросети картинки из вопроса.
    #[serde(default)]
    pub see_images: bool,
    #[serde(default)]
    pub uniq: UniqMode,
    #[serde(default)]
    pub uniq_latin: bool,
    #[serde(default = "yes")]
    pub verify_posted: bool,
    #[serde(default = "default_verify_delay")]
    pub verify_delay_sec: f64,
    /// Слова-приметы: отвечаем только на вопросы, где они встретились.
    #[serde(default)]
    pub keywords: String,
    /// Границы диапазона: ссылка на вопрос или просто номер.
    #[serde(default)]
    pub range_from: String,
    #[serde(default)]
    pub range_to: String,
    /// Свои готовые фразы (по одной в строке); пусто — встроенный набор.
    #[serde(default)]
    pub noai_list: String,
    pub signature: String,
    pub image: ImageForm,
}

impl Default for AnswersForm {
    fn default() -> Self {
        Self {
            mode: AnswerMode::Ai,
            limit: default_limit(),
            source: Source::default(),
            legacy_from_links: false,
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
            skip_own_authors: true,
            see_images: false,
            uniq: UniqMode::Off,
            uniq_latin: false,
            verify_posted: true,
            verify_delay_sec: default_verify_delay(),
            keywords: String::new(),
            range_from: String::new(),
            range_to: String::new(),
            noai_list: String::new(),
            signature: String::new(),
            image: ImageForm::default(),
        }
    }
}

impl AnswersForm {
    /// Перенести выбор из прежних настроек: галка «по ссылкам» → источник.
    /// Новое поле, если оно есть в файле, важнее старой галки.
    pub fn migrate(&mut self) {
        if std::mem::take(&mut self.legacy_from_links) && self.source == Source::Feed {
            self.source = Source::Links;
        }
    }

    /// Разобранный диапазон: меньший номер, больший. `None` — задан не полностью.
    pub fn range(&self) -> Option<(i64, i64)> {
        let a = topic_number(&self.range_from)?;
        let b = topic_number(&self.range_to)?;
        Some((a.min(b), a.max(b)))
    }

    /// `true` — форма просит открыть окно пула картинок.
    pub fn ui(&mut self, ui: &mut Ui, ai: &mut AiForm, styles: &Styles) -> bool {
        block(ui, "Откуда берём вопросы");
        ui.horizontal(|ui| {
            seg(ui, &mut self.source, Source::Feed, "Из ленты");
            seg(ui, &mut self.source, Source::Links, "По ссылкам");
            seg(ui, &mut self.source, Source::Range, "По диапазону");
        });
        if self.source == Source::Links {
            links_edit(ui, "answers_links", &mut self.links, "https://otvet.mail.ru/question/123456789");
            hint(ui, "По одной ссылке в строке. На что уже отвечали — пропустится.");
        } else if self.source == Source::Range {
            ui.horizontal(|ui| {
                ui.label("От");
                ui.add(
                    egui::TextEdit::singleline(&mut self.range_from)
                        .desired_width(f32::INFINITY)
                        .hint_text("ссылка или номер"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("До");
                ui.add(
                    egui::TextEdit::singleline(&mut self.range_to)
                        .desired_width(f32::INFINITY)
                        .hint_text("ссылка или номер"),
                );
            });
            hint(
                ui,
                &match self.range() {
                    Some((a, b)) => format!(
                        "Вопросов в диапазоне: {}. Номера идут подряд, и сайт принимает ответ даже на ещё не заданный вопрос — он дождётся автора.",
                        b - a + 1
                    ),
                    None => "Вставь ссылку на первый и последний вопрос — или просто их номера.".to_string(),
                },
            );
            hint(ui, "Текст здесь только готовыми фразами: вопроса ещё нет, нейросети не о чем писать. Проверка отправки по той же причине не работает.");
            hint(ui, "Номера общие на всех: каждый достаётся одному аккаунту, и под каждым окажется ровно один ответ. Чем больше аккаунтов работает разом, тем быстрее разбирается диапазон.");
        } else {
            ui.horizontal(|ui| {
                ui.label("Смотреть последних");
                ui.add(egui::DragValue::new(&mut self.recent_scan).range(1..=20));
                ui.label("вопросов");
            });
            hint(
                ui,
                "Бот берёт только самые свежие вопросы: в старые не лезет, а ждёт новых. Больше 20 сайт за раз не отдаёт.",
            );
        }

        block(ui, "Чем отвечаем");
        // В диапазоне вопроса ещё нет: ни нейросети, ни коверканью не из чего
        // исходить, поэтому выбора не показываем вовсе. Выбранный для ленты и
        // ссылок режим при этом сохраняется — вернётся вместе с источником.
        let how = self.effective_mode();
        if self.source == Source::Range {
            ui.label(egui::RichText::new("Готовыми фразами").color(theme::FG_STRONG));
        } else {
            ui.horizontal(|ui| {
                seg(ui, &mut self.mode, AnswerMode::Ai, "Нейросетью");
                seg(ui, &mut self.mode, AnswerMode::NoAi, "Готовыми фразами");
                seg(ui, &mut self.mode, AnswerMode::Mangle, "Коверканьем");
            });
        }
        hint(
            ui,
            match how {
                AnswerMode::Ai => "Настоящий ответ по смыслу вопроса. Нужен ключ нейросети.",
                AnswerMode::NoAi => "Короткие реплики вроде «согласен» и «жиза». Бесплатно и быстро.",
                AnswerMode::Mangle => "Слова вопроса в случайном порядке. Для кармы, а не для смысла.",
            },
        );
        if how == AnswerMode::Ai {
            ai.ui(ui, styles);
            ui.checkbox(&mut self.see_images, "показывать нейросети картинки из вопроса");
            hint(
                ui,
                "Половина вопросов на сайте — это фото с подписью «как вам?»: без картинки текст пустой. Нужна модель, которая умеет смотреть, и каждая картинка стоит токенов.",
            );
        }
        if how == AnswerMode::NoAi {
            list_edit(
                ui,
                "answers_noai",
                &mut self.noai_list,
                "Свои фразы",
                "Пусто — берётся встроенный набор коротких реплик.",
                "согласен\nну такое\nжиза",
            );
        }

        block(ui, "Сколько и как часто");
        ui.horizontal(|ui| {
            ui.label("Ответов на аккаунт");
            ui.add(egui::DragValue::new(&mut self.limit).range(0..=100_000));
            ui.label("(0 — без лимита)");
        });
        range_row(ui, "Пауза между ответами", &mut self.delay_min, &mut self.delay_max, 3600.0);
        hint(ui, "Случайное значение из промежутка — ровные паузы выглядят машинно.");

        // Пачки и лента — только когда вопросы бот берёт сам. По ссылкам он идёт
        // строго по списку, одну цель за другой: эти настройки там ни на что не
        // влияли, но стояли на виду и обещали обратное.
        if self.source != Source::Feed {
            ui.horizontal(|ui| {
                ui.label("Ответов на один вопрос");
                ui.add(egui::DragValue::new(&mut self.repeat_per_question).range(1..=20));
            });
            hint(ui, "Сайт разрешает отвечать на один вопрос несколько раз подряд. Больше одного — заметно.");
            // В диапазоне вопросы не надо ни искать, ни читать — узкое место
            // только в том, как быстро уходят сами ответы. Поэтому пачка тут
            // на виду, а не спрятана под заголовком, как у ленты.
            if self.source == Source::Range {
                ui.horizontal(|ui| {
                    ui.label("Брать за проход по аккаунту");
                    ui.add(egui::DragValue::new(&mut self.batch_size).range(1..=50));
                    ui.label("номер(ов)");
                });
                ui.checkbox(&mut self.parallel, "отвечать на них разом");
                hint(
                    ui,
                    "Без галки номера идут по одному. С галкой пачка уходит одновременно, и пауза считается между пачками, а не между ответами: так десять тысяч номеров разбираются за минуты — но нагрузка видна антиботу.",
                );
            }
        } else {
            extra(ui, "Лента и пачки", "ans_feed", |ui| {
                range_row(ui, "Обновлять ленту через", &mut self.feed_min, &mut self.feed_max, 600.0);
                hint(ui, "Пауза, когда отвечать не на что — все свежие вопросы уже разобраны.");
                ui.horizontal(|ui| {
                    ui.label("Брать за проход по аккаунту");
                    ui.add(egui::DragValue::new(&mut self.batch_size).range(1..=50));
                    ui.label("вопрос(ов)");
                });
                ui.checkbox(&mut self.continuous_feed, "не ждать конца пачки");
                hint(
                    ui,
                    "Новые вопросы уходят в работу сразу, как появились в ленте; одновременно — не больше размера пачки.",
                );
                // «Разом» имеет смысл только для настоящей пачки и только там,
                // где ядро действительно распараллеливает. Иначе галочка стояла
                // бы включённой и не делала ничего.
                let can_parallel = self.batch_size > 1 && !self.continuous_feed && !self.conversational;
                ui.add_enabled_ui(can_parallel, |ui| {
                    ui.checkbox(&mut self.parallel, "отправлять пачку разом");
                });
                hint(
                    ui,
                    if self.conversational {
                        "Не работает с единым чатом: история одна на всех, ответы идут по очереди."
                    } else if self.continuous_feed {
                        "Не нужно: «не ждать конца пачки» и так работает в несколько потоков."
                    } else if self.batch_size <= 1 {
                        "Нужна пачка больше одного вопроса."
                    } else {
                        "Быстро, но несколько ответов в одну секунду с одного аккаунта — заметный след."
                    },
                );
                ui.horizontal(|ui| {
                    ui.label("Ответов на один вопрос");
                    ui.add(egui::DragValue::new(&mut self.repeat_per_question).range(1..=20));
                });
                hint(
                    ui,
                    "Сайт разрешает отвечать на один вопрос несколько раз подряд. Больше одного — заметно.",
                );
            });
        }

        // Память разговора — свойство нейросети. Готовым фразам и коверканью
        // помнить нечего, и блок только сбивал бы с толку.
        if how == AnswerMode::Ai {
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
                        egui::DragValue::new(&mut self.convo_budget_k)
                            .range(0.0..=1000.0)
                            .suffix(" тыс. знаков"),
                    );
                });
                hint(ui, "0 — не сжимать (для моделей с большим контекстом).");
            });
        }

        extra(ui, "Уникальность и подпись", "ans_uniq", |ui| {
            ui.checkbox(&mut self.skip_others, "не отвечать туда, где уже был другой мой аккаунт");
            // Автор вопроса известен только в ленте: по ссылкам и в диапазоне
            // бот идёт по готовым номерам и ничьих вопросов не разбирает.
            if self.source == Source::Feed {
                ui.checkbox(&mut self.skip_own_authors, "не отвечать на вопросы своих аккаунтов");
                hint(ui, "Свой аккаунт под своим же вопросом — готовая связка для модерации.");
            }
            uniq_block(ui, &mut self.uniq, &mut self.uniq_latin);
            ui.label("Подпись в конце");
            ui.add(
                egui::TextEdit::singleline(&mut self.signature)
                    .desired_width(f32::INFINITY)
                    .hint_text("необязательно"),
            );
        });

        // Отбор по словам и проверка отправки читают сам вопрос — в диапазоне
        // его ещё нет. Ядро их там и не зовёт, так что показывать нечего.
        if self.source != Source::Range {
            extra(ui, "Только вопросы со словами", "ans_words", |ui| {
                crate::forms::boxed_multiline(
                    ui,
                    "ans_words_edit",
                    &mut self.keywords,
                    2,
                    "vpn, впн, обход блокировок",
                );
                let words = otvet_core::answerer::parse_keywords(&self.keywords);
                hint(
                    ui,
                    "Через запятую или с новой строки. Регистр не важен, слово ищется внутри заголовка и текста вопроса: «vpn» найдётся и в «VPN-сервис».",
                );
                hint(
                    ui,
                    &match words.len() {
                        0 => "Пусто — бот отвечает на всё подряд.".to_string(),
                        n => format!(
                            "Слов: {n}. Остальные вопросы бот из ленты даже не возьмёт — ни лимита, ни запроса к нейросети на них не потратит."
                        ),
                    },
                );
            });

            extra(ui, "Проверка отправки", "ans_verify", |ui| {
                verify_block(ui, &mut self.verify_posted, &mut self.verify_delay_sec, "ответ");
            });
        }

        let mut open_pool = false;
        extra(ui, "Картинка к ответу", "ans_img", |ui| {
            open_pool = self.image.ui(ui);
        });
        open_pool
    }

    pub fn to_params(&self, ai: &AiForm, check_auth: bool) -> AnswerParams {
        AnswerParams {
            mode: self.effective_mode(),
            target: match self.source {
                Source::Feed => TargetMode::Feed,
                Source::Links => TargetMode::Links,
                Source::Range => TargetMode::Range,
            },
            range_from: self.range().map(|(a, _)| a).unwrap_or(0),
            range_to: self.range().map(|(_, b)| b).unwrap_or(0),
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
            skip_own_authors: self.skip_own_authors,
            see_images: self.see_images,
            keywords: otvet_core::answerer::parse_keywords(&self.keywords),
            uniq: self.uniq,
            uniq_latin: self.uniq_latin,
            verify_posted: self.verify_posted,
            verify_delay_sec: self.verify_delay_sec,
            signature: self.signature.clone(),
            noai_answers: split_lines(&self.noai_list),
            image: self.image.to_core(),
            image_count: self.image.count,
            ai: ai.cfg(),
            style: ai.style.clone(),
            custom_prompt: ai.prompt(),
            mention: ai.mention.clone(),
            check_auth,
            progress: Progress::new(),
            // Одна очередь номеров на прогон: параметры дальше клонируются под
            // каждый аккаунт, и все получают ссылку на неё же.
            range_queue: Default::default(),
        }
    }

    /// Чем на самом деле пишется текст. В диапазон отвечаем заготовками при
    /// любом выбранном режиме: вопроса ещё нет, читать нечего.
    fn effective_mode(&self) -> AnswerMode {
        if self.source == Source::Range {
            AnswerMode::NoAi
        } else {
            self.mode
        }
    }

    pub fn summary(&self) -> String {
        let how = match self.effective_mode() {
            AnswerMode::Ai => "нейросетью",
            AnswerMode::NoAi => "готовыми фразами",
            AnswerMode::Mangle => "коверканьем",
        };
        let src = match (self.source, self.range()) {
            (Source::Links, _) => format!("по {} ссылк(ам)", split_lines(&self.links).len()),
            (Source::Range, Some((a, b))) => format!("в диапазон {a}–{b} ({} шт.)", b - a + 1),
            (Source::Range, None) => "в диапазон (не задан)".to_string(),
            _ => "из ленты".to_string(),
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
        if self.source == Source::Links && split_lines(&self.links).is_empty() {
            v.push("Не вставлены ссылки на вопросы".into());
        }
        if self.source == Source::Range && self.range().is_none() {
            v.push("Не задан диапазон: нужны номера первого и последнего вопроса".into());
        }
        // Ключ нейросети спрашиваем только если она правда понадобится: в
        // диапазоне её не зовут, и требовать ключ значило бы не пускать в
        // работу из-за того, чем не пользуются.
        if self.effective_mode() == AnswerMode::Ai {
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
    /// Свои темы, по одной в строке. В файле настроек ключ остался прежним —
    /// однострочная тема из старой версии читается как список из одной строки.
    #[serde(rename = "topic", default)]
    pub topics: String,
    #[serde(default)]
    pub uniq: UniqMode,
    #[serde(default)]
    pub uniq_latin: bool,
    #[serde(default = "yes")]
    pub verify_posted: bool,
    #[serde(default = "default_verify_delay")]
    pub verify_delay_sec: f64,
    /// Свои готовые вопросы (по одной строке; «Заголовок | тело»).
    #[serde(default)]
    pub noai_list: String,
    pub image: ImageForm,
}

impl Default for QuestionsForm {
    fn default() -> Self {
        Self {
            mode: AskMode::Ai,
            limit: default_limit(),
            delay_min: 30.0,
            delay_max: 90.0,
            topics: String::new(),
            uniq: UniqMode::Off,
            uniq_latin: false,
            verify_posted: true,
            verify_delay_sec: default_verify_delay(),
            noai_list: String::new(),
            image: ImageForm::default(),
        }
    }
}

impl QuestionsForm {
    /// `true` — форма просит открыть окно пула картинок.
    pub fn ui(&mut self, ui: &mut Ui, ai: &mut AiForm, styles: &Styles) -> bool {
        block(ui, "Откуда берём вопросы");
        ui.horizontal(|ui| {
            seg(ui, &mut self.mode, AskMode::Ai, "Придумывает нейросеть");
            seg(ui, &mut self.mode, AskMode::NoAi, "Из готового списка");
        });
        if self.mode == AskMode::Ai {
            ui.label("Темы");
            crate::forms::boxed_multiline(
                ui,
                "q_topics",
                &mut self.topics,
                2,
                "пусто — случайные темы из styles.json",
            );
            let n = split_lines(&self.topics).len();
            hint(
                ui,
                &match n {
                    0 => "Пусто — тема берётся случайной из styles.json.".to_string(),
                    1 => "Все вопросы будут на эту тему.".to_string(),
                    n => format!(
                        "По одной теме в строке. Своих тем: {n} — для каждого вопроса берётся случайная."
                    ),
                },
            );
            ai.ui(ui, styles);
        } else {
            hint(ui, "Уже заданные этим аккаунтом не повторяются — журнал ведётся сам.");
            list_edit(
                ui,
                "asks_noai",
                &mut self.noai_list,
                "Свои вопросы",
                "Пусто — берётся встроенный список бытовых вопросов. Можно «Заголовок | текст вопроса».",
                "как дела у всех?\nчто посмотреть вечером? | сериал или фильм, без разницы",
            );
        }

        block(ui, "Сколько и как часто");
        ui.horizontal(|ui| {
            ui.label("Вопросов на аккаунт");
            ui.add(egui::DragValue::new(&mut self.limit).range(0..=100_000));
            ui.label("(0 — без лимита)");
        });
        range_row(ui, "Пауза", &mut self.delay_min, &mut self.delay_max, 3600.0);
        hint(ui, "Заголовок длиннее 120 знаков сайт не принимает — хвост уедет в тело вопроса сам.");

        extra(ui, "Уникальность", "q_uniq", |ui| {
            uniq_block(ui, &mut self.uniq, &mut self.uniq_latin);
        });
        extra(ui, "Проверка публикации", "q_verify", |ui| {
            verify_block(ui, &mut self.verify_posted, &mut self.verify_delay_sec, "вопрос");
        });
        let mut open_pool = false;
        extra(ui, "Картинка к вопросу", "q_img", |ui| {
            open_pool = self.image.ui(ui);
        });
        open_pool
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
            topics: split_lines(&self.topics),
            noai_questions: split_lines(&self.noai_list),
            uniq: self.uniq,
            uniq_latin: self.uniq_latin,
            image: self.image.to_core(),
            image_count: self.image.count,
            verify_posted: self.verify_posted,
            verify_delay_sec: self.verify_delay_sec,
            check_auth,
            progress: Progress::new(),
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
    /// Показывать нейросети картинки из реплики собеседника.
    #[serde(default)]
    pub see_images: bool,
    pub max_chain: usize,
    pub skip_own: bool,
    pub mark_read: bool,
    #[serde(default)]
    pub uniq: UniqMode,
    #[serde(default)]
    pub uniq_latin: bool,
    #[serde(default = "yes")]
    pub verify_posted: bool,
    #[serde(default = "default_verify_delay")]
    pub verify_delay_sec: f64,
    /// Свои готовые реплики (по одной в строке).
    #[serde(default)]
    pub noai_list: String,
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
            see_images: false,
            max_chain: 6,
            skip_own: true,
            mark_read: false,
            uniq: UniqMode::Off,
            uniq_latin: false,
            verify_posted: true,
            verify_delay_sec: default_verify_delay(),
            noai_list: String::new(),
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
        } else {
            list_edit(
                ui,
                "replies_noai",
                &mut self.noai_list,
                "Свои реплики",
                "Пусто — берётся встроенный набор коротких ответов.",
                "согласен\nну хз\nда ладно тебе",
            );
        }

        block(ui, "Сколько и как часто");
        ui.horizontal(|ui| {
            ui.label("Ответов на аккаунт");
            ui.add(egui::DragValue::new(&mut self.limit).range(0..=100_000));
            ui.label("(0 — без лимита)");
        });
        range_row(ui, "Пауза", &mut self.delay_min, &mut self.delay_max, 3600.0);

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
            ui.checkbox(&mut self.see_images, "картинки из его реплики");
            hint(
                ui,
                "Мемом отвечают не реже, чем словами, а текста у такой реплики нет вовсе. Нужна модель, которая умеет смотреть.",
            );
        });

        extra(ui, "Уникальность и подпись", "cm_uniq", |ui| {
            uniq_block(ui, &mut self.uniq, &mut self.uniq_latin);
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

        extra(ui, "Проверка отправки", "cm_verify", |ui| {
            verify_block(ui, &mut self.verify_posted, &mut self.verify_delay_sec, "ответ");
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
            see_images: self.see_images,
            max_chain: self.max_chain,
            ai: ai.cfg(),
            style: ai.style.clone(),
            custom_prompt: ai.prompt(),
            mention: ai.mention.clone(),
            noai_replies: split_lines(&self.noai_list),
            uniq: self.uniq,
            uniq_latin: self.uniq_latin,
            verify_posted: self.verify_posted,
            verify_delay_sec: self.verify_delay_sec,
            signature: self.signature.clone(),
            mark_read: self.mark_read,
            check_auth,
            progress: Progress::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// В диапазон бот отвечает заготовками, даже если в форме выбрана
    /// нейросеть: вопроса ещё нет, читать ей нечего. И ключ за это спрашивать
    /// не за что — иначе кнопка «Запустить» не пускала бы в работу из-за того,
    /// чем не пользуются.
    #[test]
    fn range_answers_with_canned_text_and_needs_no_ai_key() {
        let ai = AiForm::default();
        let mut f = AnswersForm { mode: AnswerMode::Ai, ..Default::default() };

        // Из ленты нейросеть остаётся нейросетью, и ключ ей нужен.
        assert_eq!(f.to_params(&ai, false).mode, AnswerMode::Ai);
        assert!(!f.problems(&ai).is_empty(), "лента без ключа нейросети запускаться не должна");

        f.source = Source::Range;
        f.range_from = "1000".into();
        f.range_to = "1010".into();
        assert_eq!(f.to_params(&ai, false).mode, AnswerMode::NoAi, "в диапазон ушла нейросеть");
        assert!(f.problems(&ai).is_empty(), "диапазону ключ ни к чему: {:?}", f.problems(&ai));
        assert!(f.summary().contains("готовыми фразами"), "сводка врёт: {}", f.summary());

        // Возврат к ленте не должен стирать выбор человека.
        f.source = Source::Feed;
        assert_eq!(f.to_params(&ai, false).mode, AnswerMode::Ai);
    }

    /// Отмеченные в пуле картинки должны доезжать до прогона: без них ядро
    /// берёт из пула любую, и выбор человека ни на что не влиял бы.
    #[test]
    fn chosen_pool_images_reach_the_run() {
        let mut img = ImageForm { kind: ImageKind::Pool, ..Default::default() };
        match img.to_core() {
            ImageMode::Gif { selected } => assert!(selected.is_empty(), "по умолчанию годится любая"),
            other => panic!("не пул: {other:?}"),
        }

        img.selected = vec!["aaa".into(), "bbb".into()];
        match img.to_core() {
            ImageMode::Gif { selected } => assert_eq!(selected, vec!["aaa", "bbb"]),
            other => panic!("не пул: {other:?}"),
        }

        // Из папки отметки не при делах — там файлы, а не хэши пула.
        img.kind = ImageKind::Folder;
        assert!(matches!(img.to_core(), ImageMode::Upload { .. }));
    }
}
