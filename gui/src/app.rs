//! app.rs — само окно: раскладка, вкладки режимов, запуск и остановка прогонов.

use crate::accounts::AccountsPanel;
use crate::console;
use crate::forms::{CommonForm, ComplainForm, SubsForm, VotesForm};
use crate::forms_ai::{AiForm, AnswersForm, CommentsForm, QuestionsForm};
use crate::state::{Bg, Mode, ModeState};
use crate::theme;
use egui::RichText;
use otvet_core::journals::Styles;
use otvet_core::runner::{self, RunOne, RunnerCfg};
use otvet_core::{answerer, asker, complain, replier, subscribe, votes};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

const STATE_KEY: &str = "otvetware_forms";

#[derive(Serialize, Deserialize, Default, Clone)]
struct PersistedForms {
    #[serde(default)]
    common: CommonForm,
    #[serde(default)]
    votes: VotesForm,
    #[serde(default)]
    subs: SubsForm,
    #[serde(default)]
    complain: ComplainForm,
    #[serde(default)]
    ai: AiForm,
    #[serde(default)]
    answers: AnswersForm,
    #[serde(default)]
    questions: QuestionsForm,
    #[serde(default)]
    comments: CommentsForm,
}

pub struct App {
    bg: Arc<Bg>,
    modes: HashMap<Mode, Arc<ModeState>>,
    mode: Mode,
    accounts: AccountsPanel,
    forms: PersistedForms,
    /// Стили из styles.json — читаются один раз на старте.
    styles: Styles,
    show_accounts_log: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, bg: Arc<Bg>) -> Self {
        theme::install(&cc.egui_ctx);
        bg.set_ctx(cc.egui_ctx.clone());

        let forms = cc
            .storage
            .and_then(|s| s.get_string(STATE_KEY))
            .and_then(|t| serde_json::from_str::<PersistedForms>(&t).ok())
            .unwrap_or_default();

        let modes = Mode::ALL.iter().map(|m| (*m, Arc::new(ModeState::default()))).collect();
        let styles = Styles::load(&bg.core.root);

        Self {
            bg,
            modes,
            mode: Mode::Votes,
            accounts: AccountsPanel::default(),
            forms,
            styles,
            show_accounts_log: true,
        }
    }

    fn state(&self, m: Mode) -> Arc<ModeState> {
        self.modes[&m].clone()
    }

    fn any_running(&self) -> bool {
        self.modes.values().any(|s| s.is_running())
    }

    // ─── запуск ─────────────────────────────────────────────────────────────

    fn start(&mut self) {
        let mode = self.mode;
        let st = self.state(mode);
        if st.is_running() {
            return;
        }
        let all = self.bg.accounts();
        let selected = self.accounts.selected_accounts(&all);
        let log = st.log.logger();
        if selected.is_empty() {
            log("❌ Не выбран ни один аккаунт с куками. Добавь аккаунт и отметь галочкой.");
            return;
        }

        let c = &self.forms.common;
        let cfg = RunnerCfg {
            concurrency: if c.all_at_once { selected.len() } else { c.concurrency },
            total_limit: c.total_limit,
            round_limit: c.round_limit,
            // Голоса/подписки/жалобы кругами не гоняем: повторно то же самое
            // сделать нельзя — второй голос снимает первый.
            repeat_rounds: c.repeat_rounds && rounds_supported(mode),
            round_pause_min: c.round_pause_min,
            proxy_rotate_fails: c.proxy_rotate_fails,
        };
        let check_auth = c.check_auth;
        let core = self.bg.core.clone();

        let run_one: RunOne = match mode {
            Mode::Votes => {
                let p = self.forms.votes.to_params(check_auth);
                let core = core.clone();
                Arc::new(move |acc, pass_limit, log, stop| {
                    let core = core.clone();
                    let mut p = p.clone();
                    p.limit = tighter(p.limit, pass_limit);
                    Box::pin(async move { votes::run_votes(&core, &acc, &p, &log, &stop).await })
                })
            }
            Mode::Subscribe => {
                let p = self.forms.subs.to_params(check_auth);
                let core = core.clone();
                Arc::new(move |acc, _pass_limit, log, stop| {
                    let core = core.clone();
                    let p = p.clone();
                    Box::pin(async move { subscribe::run_subscriber(&core, &acc, &p, &log, &stop).await })
                })
            }
            Mode::Complain => {
                let p = self.forms.complain.to_params(check_auth);
                let core = core.clone();
                Arc::new(move |acc, pass_limit, log, stop| {
                    let core = core.clone();
                    let mut p = p.clone();
                    p.limit = tighter(p.limit, pass_limit);
                    Box::pin(async move { complain::run_complainer(&core, &acc, &p, &log, &stop).await })
                })
            }
            Mode::Answers => {
                let p = self.forms.answers.to_params(&self.forms.ai, check_auth);
                let core = core.clone();
                Arc::new(move |acc, pass_limit, log, stop| {
                    let core = core.clone();
                    let mut p = p.clone();
                    p.limit = tighter(p.limit, pass_limit);
                    Box::pin(async move { answerer::run_answerer(&core, &acc, &p, &log, &stop).await })
                })
            }
            Mode::Questions => {
                let p = self.forms.questions.to_params(&self.forms.ai, check_auth);
                let core = core.clone();
                Arc::new(move |acc, pass_limit, log, stop| {
                    let core = core.clone();
                    let mut p = p.clone();
                    p.limit = tighter(p.limit, pass_limit);
                    Box::pin(async move { asker::run_asker(&core, &acc, &p, &log, &stop).await })
                })
            }
            Mode::Comments => {
                let p = self.forms.comments.to_params(&self.forms.ai, check_auth);
                let core = core.clone();
                Arc::new(move |acc, pass_limit, log, stop| {
                    let core = core.clone();
                    let mut p = p.clone();
                    p.limit = tighter(p.limit, pass_limit);
                    Box::pin(async move { replier::run_replier(&core, &acc, &p, &log, &stop).await })
                })
            }
        };

        // Счётчик «сделано» считаем по тому, что вернул бот. Раньше он брался из
        // лога по значку ✅ и приписывал к голосам ещё и «✅ Авторизован».
        st.done.store(0, Ordering::SeqCst);
        let counted: RunOne = {
            let inner = run_one;
            let done = st.done.clone();
            Arc::new(move |acc, limit, log, stop| {
                let inner = inner.clone();
                let done = done.clone();
                Box::pin(async move {
                    let res = inner(acc, limit, log, stop).await;
                    done.fetch_add(res.done, Ordering::SeqCst);
                    res
                })
            })
        };

        let stop = st.fresh_stop();
        st.running.store(true, Ordering::SeqCst);
        let running = st.running.clone();
        let core = self.bg.core.clone();
        self.bg.spawn(async move {
            runner::run(core, selected, cfg, counted, log, stop).await;
            running.store(false, Ordering::SeqCst);
        });
    }

    // ─── разметка ───────────────────────────────────────────────────────────

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(theme::APP_NAME).size(17.0).strong().color(theme::FG_STRONG));
                ui.label(RichText::new("otvet.mail.ru").size(12.0).color(theme::FG_FAINT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Статус справа одной строкой, без мигалок и спиннеров.
                    let (txt, col) = if self.any_running() {
                        ("прогон идёт", theme::FG_STRONG)
                    } else if self.bg.busy() > 0 {
                        ("работаю…", theme::FG_DIM)
                    } else {
                        ("готов", theme::FG_FAINT)
                    };
                    ui.label(RichText::new(txt).size(12.0).color(col));
                });
            });
            ui.add_space(3.0);
        });
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("settings")
            .resizable(true)
            .default_size(430.0)
            .size_range(egui::Rangef::new(360.0, 640.0))
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.heading("Настройки запуска");
                ui.add_space(6.0);

                // Вкладки режимов — два ряда по три, чтобы влезали подписи.
                egui::Grid::new("modes").num_columns(3).spacing([6.0, 6.0]).show(ui, |ui| {
                    for (i, m) in Mode::ALL.iter().enumerate() {
                        let on = self.mode == *m;
                        let running = self.state(*m).is_running();
                        let label = format!("{}{}", m.title(), if running { "  ·" } else { "" });
                        let btn = egui::Button::new(RichText::new(label).color(if on {
                            theme::FG_STRONG
                        } else {
                            theme::FG_DIM
                        }))
                        .fill(if on { theme::BG_ACTIVE } else { theme::BG_INPUT })
                        .min_size(egui::vec2(122.0, 28.0));
                        if ui.add(btn).clicked() {
                            self.mode = *m;
                        }
                        if i % 3 == 2 {
                            ui.end_row();
                        }
                    }
                });

                ui.add_space(10.0);
                // Кнопки запуска прибиты к низу панели: настроек много, и при
                // длинной форме «Запустить» иначе уезжает за край окна.
                egui::Panel::bottom("run_controls").resizable(false).show(ui, |ui| {
                    ui.add_space(6.0);
                    self.run_controls(ui);
                    ui.add_space(4.0);
                });
                egui::ScrollArea::vertical().id_salt("settings_scroll").show(ui, |ui| {
                    match self.mode {
                        Mode::Votes => self.forms.votes.ui(ui),
                        Mode::Subscribe => self.forms.subs.ui(ui),
                        Mode::Complain => self.forms.complain.ui(ui),
                        Mode::Answers => self.forms.answers.ui(ui, &mut self.forms.ai, &self.styles),
                        Mode::Questions => self.forms.questions.ui(ui, &mut self.forms.ai, &self.styles),
                        Mode::Comments => self.forms.comments.ui(ui, &mut self.forms.ai, &self.styles),
                    }

                    ui.add_space(10.0);
                    ui.separator();
                    let selected = self.accounts.selected.len();
                    self.forms.common.ui(ui, selected, rounds_supported(self.mode));
                    ui.add_space(6.0);
                });
            });
    }

    /// Что мешает запуститься прямо сейчас. Пустой список — можно жать.
    ///
    /// Проверяем ДО старта, а не в логе каждого аккаунта: раньше забытый ключ
    /// нейросети давал двадцать одинаковых ошибок в консоли и ноль понимания.
    fn problems(&self) -> Vec<String> {
        let mut v = Vec::new();
        let all = self.bg.accounts();
        if self.accounts.selected_accounts(&all).is_empty() {
            v.push(if all.is_empty() {
                "Нет ни одного аккаунта — добавь его кнопкой справа".to_string()
            } else {
                "Не отмечен ни один аккаунт с куками".to_string()
            });
        }
        let links_needed = |links: &str, what: &str| -> Option<String> {
            if crate::forms::split_lines(links).is_empty() {
                Some(format!("Не вставлены ссылки {what}"))
            } else {
                None
            }
        };
        match self.mode {
            Mode::Votes => v.extend(links_needed(&self.forms.votes.links, "на профили или посты")),
            Mode::Subscribe => v.extend(links_needed(&self.forms.subs.links, "на профили")),
            Mode::Complain => v.extend(links_needed(&self.forms.complain.links, "на цели жалоб")),
            Mode::Answers => v.extend(self.forms.answers.problems(&self.forms.ai)),
            Mode::Questions => v.extend(self.forms.questions.problems(&self.forms.ai)),
            Mode::Comments => v.extend(self.forms.comments.problems(&self.forms.ai)),
        }
        v
    }

    /// Человеческое «что сейчас произойдёт» — строка над кнопкой запуска.
    fn summary(&self) -> String {
        let n = self.accounts.selected_accounts(&self.bg.accounts()).len();
        let mode_part = match self.mode {
            Mode::Votes => self.forms.votes.summary(),
            Mode::Subscribe => self.forms.subs.summary(),
            Mode::Complain => self.forms.complain.summary(),
            Mode::Answers => self.forms.answers.summary(),
            Mode::Questions => self.forms.questions.summary(),
            Mode::Comments => self.forms.comments.summary(),
        };
        let how = if self.forms.common.all_at_once {
            "все разом".to_string()
        } else if self.forms.common.concurrency > 1 {
            format!("по {} за раз", self.forms.common.concurrency)
        } else {
            "по очереди".to_string()
        };
        format!("{n} аккаунт(ов) {how}: {mode_part}")
    }

    fn run_controls(&mut self, ui: &mut egui::Ui) {
        let st = self.state(self.mode);
        let running = st.is_running();
        let problems = self.problems();

        if running {
            ui.label(
                RichText::new(format!("Сделано: {}", st.done.load(Ordering::Relaxed)))
                    .color(theme::FG_STRONG),
            );
        } else if problems.is_empty() {
            ui.label(RichText::new(self.summary()).color(theme::FG_DIM).size(11.5));
        } else {
            for p in &problems {
                ui.label(RichText::new(format!("• {p}")).color(theme::WARN).size(11.5));
            }
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let can_start = !running && problems.is_empty();
            let start = egui::Button::new(
                RichText::new(if running { "Выполняется…" } else { "Запустить" })
                    .size(14.0)
                    .color(if can_start { theme::FG_STRONG } else { theme::FG_DIM }),
            )
            .fill(theme::BG_ACTIVE)
            .min_size(egui::vec2(190.0, 32.0));
            let resp = ui.add_enabled(can_start, start);
            if resp.clicked() {
                self.start();
            }
            if !problems.is_empty() && !running {
                resp.on_hover_text(problems.join("\n"));
            }
            let stop = egui::Button::new(RichText::new("Стоп").size(14.0).color(theme::FG))
                .fill(theme::BG_INPUT)
                .min_size(egui::vec2(110.0, 32.0));
            if ui.add_enabled(running, stop).clicked() {
                st.request_stop();
                st.log.push("Останавливаю — бот докрутит текущий шаг и выйдет.");
            }
        });
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        let st = self.state(self.mode);
        egui::Panel::top("accounts_panel")
            .resizable(true)
            .default_size(320.0)
            .size_range(egui::Rangef::new(140.0, 900.0))
            .show(ui, |ui| {
                ui.add_space(4.0);
                self.accounts.ui(ui, &self.bg);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Что делает бот — {}", self.mode.title())).color(theme::FG_STRONG),
                );
                ui.separator();
                ui.label(
                    RichText::new(format!(
                        "{}: {}",
                        self.mode.counter_label(),
                        st.done.load(Ordering::Relaxed)
                    ))
                    .color(theme::FG_STRONG),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut self.show_accounts_log, "лог аккаунтов");
                });
            });
            ui.add_space(2.0);

            if self.show_accounts_log && !self.accounts.log.is_empty() {
                let h = (ui.available_height() * 0.28).clamp(60.0, 220.0);
                ui.allocate_ui(egui::vec2(ui.available_width(), h), |ui| {
                    console::show(ui, &self.accounts.log, &st.follow, "acclog");
                });
                ui.add_space(4.0);
                ui.separator();
            }
            console::show(ui, &st.log, &st.follow, "runlog");
        });
    }
}

/// Строжайший из двух лимитов; 0 означает «без ограничения».
///
/// У режима свой лимит («ответов на аккаунт», «постов с профиля»), у прогона —
/// свой общий. Раньше общий просто затирал режимный, и лимит из панели молча
/// переставал работать, если общий стоял в 0.
fn tighter(mode_limit: i64, pass_limit: i64) -> i64 {
    match (mode_limit.max(0), pass_limit.max(0)) {
        (0, b) => b,
        (a, 0) => a,
        (a, b) => a.min(b),
    }
}

fn rounds_supported(m: Mode) -> bool {
    !matches!(m, Mode::Votes | Mode::Subscribe | Mode::Complain)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.top_bar(ui);
        self.left_panel(ui);
        self.central(ui);

        // Пока что-то работает — обновляем картинку 10 раз в секунду. В покое
        // окно не перерисовывается вообще: ноль нагрузки на простое.
        if self.any_running() || self.bg.busy() > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Ok(t) = serde_json::to_string(&self.forms) {
            storage.set_string(STATE_KEY, t);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for st in self.modes.values() {
            st.request_stop();
        }
    }
}
