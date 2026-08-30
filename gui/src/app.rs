//! app.rs — само окно: раскладка, вкладки режимов, запуск и остановка прогонов.

use crate::accounts::AccountsPanel;
use crate::console;
use crate::forms::{CommonForm, ComplainForm, SubsForm, VotesForm};
use crate::forms_ai::{AiForm, AnswersForm, CommentsForm, QuestionsForm};
use crate::icons;
use crate::pool::PoolWindow;
use crate::state::{Bg, Mode, ModeState};
use crate::theme;
use egui::RichText;
use otvet_core::journals::Styles;
use otvet_core::runner::{self, RunOne, RunnerCfg};
use otvet_core::util::{Progress, Stop};
use otvet_core::{answerer, asker, complain, replier, subscribe, votes};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

const STATE_KEY: &str = "otvetware_forms";

#[derive(Serialize, Deserialize, Default, Clone)]
struct PersistedForms {
    /// Общие настройки прогона — СВОИ у каждого режима: «голоса всеми сразу»
    /// и «ответы по одному» — это разные привычки, и одна пара полей на всех
    /// заставляла перещёлкивать их при каждом переключении вкладки.
    #[serde(default)]
    commons: HashMap<Mode, CommonForm>,
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
    /// Последний открытый режим: инструмент должен открываться там же, где его
    /// закрыли, а не всегда на «Голосах».
    #[serde(default)]
    mode: Option<Mode>,
}

impl PersistedForms {
    /// Дочитать то, что в прежних версиях называлось иначе.
    fn migrate(&mut self) {
        self.answers.migrate();
    }
    fn common(&self, m: Mode) -> CommonForm {
        self.commons.get(&m).cloned().unwrap_or_default()
    }
    fn common_mut(&mut self, m: Mode) -> &mut CommonForm {
        self.commons.entry(m).or_default()
    }
}

pub struct App {
    bg: Arc<Bg>,
    modes: HashMap<Mode, Arc<ModeState>>,
    mode: Mode,
    accounts: AccountsPanel,
    forms: PersistedForms,
    /// Стили из styles.json — читаются один раз на старте.
    styles: Styles,
    /// Окно пула картинок.
    pool: PoolWindow,
    show_accounts_log: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, bg: Arc<Bg>) -> Self {
        theme::install(&cc.egui_ctx);
        bg.set_ctx(cc.egui_ctx.clone());

        let mut forms = cc
            .storage
            .and_then(|s| s.get_string(STATE_KEY))
            .and_then(|t| serde_json::from_str::<PersistedForms>(&t).ok())
            .unwrap_or_default();
        forms.migrate();

        let modes = Mode::ALL.iter().map(|m| (*m, Arc::new(ModeState::default()))).collect();
        let styles = Styles::load(&bg.core.root);
        let mode = forms.mode.unwrap_or(Mode::Votes);

        Self {
            bg,
            modes,
            mode,
            accounts: AccountsPanel::default(),
            forms,
            styles,
            pool: PoolWindow::default(),
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
            log("[-] Не выбран ни один аккаунт с куками. Добавь аккаунт и отметь галочкой.");
            return;
        }

        let c = self.forms.common(mode);
        let (total_limit, round_limit) = run_limits(mode, &c);
        let cfg = RunnerCfg {
            concurrency: if c.all_at_once { selected.len() } else { c.concurrency },
            total_limit,
            round_limit,
            // Голоса/подписки/жалобы кругами не гоняем: повторно то же самое
            // сделать нельзя — второй голос снимает первый.
            repeat_rounds: c.repeat_rounds && feed_mode(mode),
            round_pause_min: c.round_pause_min,
            proxy_rotate_fails: c.proxy_rotate_fails,
            // Прогревать следующий аккаунт есть смысл только если его вообще
            // собираются проверять.
            prefetch_next: c.check_auth,
        };
        let check_auth = c.check_auth;
        let core = self.bg.core.clone();

        // Живой счётчик: режимы дёргают его на каждом успешном действии, и
        // «Сделано» растёт по ходу дела, а не после того, как аккаунт закончит
        // работу целиком (на длинном прогоне это были часы нуля на экране).
        st.done.store(0, Ordering::SeqCst);
        let progress = Progress::from_arc(st.done.clone());

        let run_one: RunOne = match mode {
            Mode::Votes => {
                let p = self.forms.votes.to_params(check_auth);
                let core = core.clone();
                Arc::new(move |acc, pass_limit, log, stop| {
                    let core = core.clone();
                    let mut p = p.clone();
                    p.limit = tighter(p.limit, pass_limit);
                    p.progress = progress.clone();
                    Box::pin(async move { votes::run_votes(&core, &acc, &p, &log, &stop).await })
                })
            }
            Mode::Subscribe => {
                let p = self.forms.subs.to_params(check_auth);
                let core = core.clone();
                Arc::new(move |acc, pass_limit, log, stop| {
                    let core = core.clone();
                    let mut p = p.clone();
                    p.limit = tighter(p.limit, pass_limit);
                    p.progress = progress.clone();
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
                    p.progress = progress.clone();
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
                    p.progress = progress.clone();
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
                    p.progress = progress.clone();
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
                    p.progress = progress.clone();
                    Box::pin(async move { replier::run_replier(&core, &acc, &p, &log, &stop).await })
                })
            }
        };

        let stop = st.fresh_stop();
        st.running.store(true, Ordering::SeqCst);
        let running = st.running.clone();
        let core = self.bg.core.clone();
        self.bg.spawn(async move {
            runner::run(core, selected, cfg, run_one, log, stop).await;
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
                    let (txt, col) = if self.modes.values().any(|s| s.is_paused()) {
                        ("на паузе", theme::WARN)
                    } else if self.any_running() {
                        ("прогон идёт", theme::FG_STRONG)
                    } else if self.bg.busy() > 0 {
                        ("фоновая задача…", theme::FG_DIM)
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
                // У работающего режима — кольцо слева и счётчик сделанного
                // справа: точка после названия ничего не говорила, а понять,
                // где что крутится, надо с одного взгляда.
                egui::Grid::new("modes").num_columns(3).spacing([6.0, 6.0]).show(ui, |ui| {
                    for (i, m) in Mode::ALL.iter().enumerate() {
                        let on = self.mode == *m;
                        let st = self.state(*m);
                        let running = st.is_running();
                        let done = st.done.load(Ordering::Relaxed);
                        let color = if on { theme::FG_STRONG } else { theme::FG_DIM };
                        let size = egui::vec2(122.0, 28.0);
                        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                        if ui.is_rect_visible(rect) {
                            let vis = ui.style().interact(&resp);
                            let fill = if on {
                                theme::BG_ACTIVE
                            } else if resp.hovered() {
                                theme::BG_HOVER
                            } else {
                                theme::BG_INPUT
                            };
                            ui.painter().rect(
                                rect,
                                egui::CornerRadius::same(2),
                                fill,
                                vis.bg_stroke,
                                egui::StrokeKind::Inside,
                            );
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                m.title(),
                                egui::FontId::proportional(13.0),
                                color,
                            );
                            if running {
                                icons::busy_bar(
                                    ui,
                                    rect,
                                    ui.input(|i| i.time),
                                    theme::LINE,
                                    if on { theme::FG_STRONG } else { theme::FG_DIM },
                                );
                                // Перерисовка только пока что-то работает и не
                                // чаще 30 раз в секунду: полоска должна ехать,
                                // а не греть слабый ноутбук.
                                ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
                                if done > 0 {
                                    icons::corner_text(ui, rect, &done.to_string(), theme::FG_DIM);
                                }
                            }
                        }
                        if resp.clicked() {
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
                let mut open_pool = false;
                egui::ScrollArea::vertical().id_salt("settings_scroll").show(ui, |ui| {
                    match self.mode {
                        Mode::Votes => self.forms.votes.ui(ui),
                        Mode::Subscribe => self.forms.subs.ui(ui),
                        Mode::Complain => self.forms.complain.ui(ui),
                        Mode::Answers => {
                            open_pool = self.forms.answers.ui(ui, &mut self.forms.ai, &self.styles)
                        }
                        Mode::Questions => {
                            open_pool = self.forms.questions.ui(ui, &mut self.forms.ai, &self.styles)
                        }
                        Mode::Comments => self.forms.comments.ui(ui, &mut self.forms.ai, &self.styles),
                    }
                    self.tools(ui);

                    ui.add_space(10.0);
                    ui.separator();
                    let selected = self.accounts.selected_count(&self.bg.accounts());
                    let mode = self.mode;
                    // Диапазон — единственная работа, общая на все аккаунты:
                    // лимиты там режут общую кучу, а не личную норму каждого.
                    let shared =
                        mode == Mode::Answers && self.forms.answers.source == crate::forms_ai::Source::Range;
                    self.forms.common_mut(mode).ui(ui, selected, feed_mode(mode), shared);
                    ui.add_space(6.0);
                });
                if open_pool {
                    self.pool.open(&self.bg, &self.accounts.selected_accounts(&self.bg.accounts()));
                }
            });
    }

    /// Инструменты режима: разовые проверки, которые не запускают прогон.
    /// Пишут в ту же консоль, что и бот, — отдельного окна не нужно.
    fn tools(&mut self, ui: &mut egui::Ui) {
        match self.mode {
            Mode::Subscribe => {
                crate::forms::block(ui, "Проверка");
                if ui
                    .button("Проверить подписки")
                    .on_hover_text("Пройтись по отмеченным аккаунтам и посмотреть, кто на кого уже подписан")
                    .clicked()
                {
                    self.check_subscriptions();
                }
                crate::forms::hint(ui, "Ничего не меняет — только смотрит.");
            }
            Mode::Votes => {
                crate::forms::block(ui, "Проверка");
                if ui
                    .button("Кто голосовал")
                    .on_hover_text("Показать плюсы и минусы под постом или ответом из списка ссылок")
                    .clicked()
                {
                    self.list_voters();
                }
                crate::forms::hint(ui, "Берёт первую ссылку вида /question/… из поля выше.");
            }
            _ => {}
        }
    }

    /// Кто уже подписан: матрица «аккаунт → профили».
    fn check_subscriptions(&self) {
        let st = self.state(self.mode);
        let log = st.log.clone();
        let all = self.bg.accounts();
        let accounts = self.accounts.selected_accounts(&all);
        let profiles = crate::forms::split_lines(&self.forms.subs.links);
        if accounts.is_empty() || profiles.is_empty() {
            log.push("Нужны отмеченные аккаунты и хотя бы одна ссылка на профиль.");
            return;
        }
        let core = self.bg.core.clone();
        log.push(&format!("Проверяю подписки: аккаунтов {}, профилей {}", accounts.len(), profiles.len()));
        self.bg.spawn(async move {
            let stop = Stop::new();
            for acc in accounts {
                let mut ok = 0;
                let mut parts = Vec::new();
                for p in &profiles {
                    match subscribe::subscription_status(&core, &acc, p, &stop).await {
                        Some((who, true)) => {
                            ok += 1;
                            parts.push(format!("{} +", who.name));
                        }
                        Some((who, false)) => parts.push(format!("{} —", who.name)),
                        None => parts.push("не понял профиль".into()),
                    }
                }
                log.push(&format!("{}: {ok}/{} — {}", acc.name, profiles.len(), parts.join(", ")));
            }
            log.push("Проверка подписок закончена.");
        });
    }

    /// Список проголосовавших под постом/ответом, с никами.
    fn list_voters(&self) {
        let st = self.state(self.mode);
        let log = st.log.clone();
        let all = self.bg.accounts();
        let Some(acc) = self.accounts.selected_accounts(&all).into_iter().next() else {
            log.push("Нужен хотя бы один отмеченный аккаунт — список смотрится из-под него.");
            return;
        };
        let Some(target) =
            crate::forms::split_lines(&self.forms.votes.links).iter().find_map(|l| votes::parse_target_id(l))
        else {
            log.push("В списке нет ссылки на пост или ответ (нужен /question/… или ?reply=…).");
            return;
        };

        let core = self.bg.core.clone();
        log.push(&format!("Смотрю голоса под {} #{}…", target.kind.ru(), target.id));
        self.bg.spawn(async move {
            let stop = Stop::new();
            match votes::list_voters(&core, &acc, &target, &stop).await {
                Err(e) => log.push(&format!("[-] {e}")),
                Ok(voters) if voters.is_empty() => log.push("Голосов пока нет."),
                Ok(voters) => {
                    let plus = voters.iter().filter(|v| v.reaction == 1).count();
                    let minus = voters.len() - plus;
                    log.push(&format!("Всего {}: плюсов {plus}, минусов {minus}", voters.len()));
                    // Ники резолвятся по одному: каждый заход проходит антибот,
                    // поэтому делаем это только для показанных строк.
                    for v in voters.iter().take(50) {
                        let nick = votes::resolve_nick(&core, &acc, v.author_id, &stop)
                            .await
                            .unwrap_or_else(|| format!("id{}", v.author_id));
                        log.push(&format!(
                            "  {} {}{}",
                            if v.reaction == 1 { "+" } else { "−" },
                            nick,
                            if v.mine { "  (это мы)" } else { "" }
                        ));
                    }
                    if voters.len() > 50 {
                        log.push(&format!("  …и ещё {}", voters.len() - 50));
                    }
                }
            }
        });
    }

    /// Что мешает запуститься прямо сейчас. Пустой список — можно жать.
    ///
    /// Проверяем ДО старта, а не в логе каждого аккаунта: раньше забытый ключ
    /// нейросети давал двадцать одинаковых ошибок в консоли и ноль понимания.
    fn problems(&self) -> Vec<String> {
        let mut v = Vec::new();
        let all = self.bg.accounts();
        if self.accounts.selected_count(&all) == 0 {
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
            Mode::Complain => v.extend(links_needed(
                &self.forms.complain.links,
                match self.forms.complain.kind {
                    crate::forms::ComplainKind::Single => "на посты или ответы",
                    _ => "на профили",
                },
            )),
            Mode::Answers => v.extend(self.forms.answers.problems(&self.forms.ai)),
            Mode::Questions => v.extend(self.forms.questions.problems(&self.forms.ai)),
            Mode::Comments => v.extend(self.forms.comments.problems(&self.forms.ai)),
        }
        v
    }

    /// Человеческое «что сейчас произойдёт» — строка над кнопкой запуска.
    fn summary(&self) -> String {
        let n = self.accounts.selected_count(&self.bg.accounts());
        let mode_part = match self.mode {
            Mode::Votes => self.forms.votes.summary(),
            Mode::Subscribe => self.forms.subs.summary(),
            Mode::Complain => self.forms.complain.summary(),
            Mode::Answers => self.forms.answers.summary(),
            Mode::Questions => self.forms.questions.summary(),
            Mode::Comments => self.forms.comments.summary(),
        };
        let c = self.forms.common(self.mode);
        let how = if c.all_at_once {
            "все разом".to_string()
        } else if c.concurrency > 1 {
            format!("по {} за раз", c.concurrency)
        } else {
            "по очереди".to_string()
        };
        format!("{n} аккаунт(ов) {how}: {mode_part}")
    }

    fn run_controls(&mut self, ui: &mut egui::Ui) {
        let st = self.state(self.mode);
        let running = st.is_running();
        let paused = st.is_paused();
        let problems = self.problems();

        if running {
            let done = st.done.load(Ordering::Relaxed);
            ui.label(if paused {
                RichText::new(format!("На паузе. Сделано: {done}")).color(theme::WARN)
            } else {
                RichText::new(format!("Сделано: {done}")).color(theme::FG_STRONG)
            });
        } else if problems.is_empty() {
            ui.label(RichText::new(self.summary()).color(theme::FG_DIM).size(11.5));
        } else {
            for p in &problems {
                ui.label(RichText::new(format!("• {p}")).color(theme::WARN).size(11.5));
            }
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            // Ширины считаем от того, что дали: три кнопки фиксированной ширины
            // не влезли бы в узкую панель и распирали бы её.
            let gap = ui.spacing().item_spacing.x;
            let free = (ui.available_width() - gap * 2.0).max(120.0);
            let w = free / 3.0;
            let can_start = !running && problems.is_empty();
            let label = match (running, paused) {
                (false, _) => "Запустить",
                (true, false) => "Работает",
                (true, true) => "На паузе",
            };
            let start = egui::Button::new(
                RichText::new(label)
                    .size(14.0)
                    .color(if can_start { theme::FG_STRONG } else { theme::FG_DIM }),
            )
            .fill(theme::BG_ACTIVE)
            .min_size(egui::vec2(w, 32.0));
            let resp = ui.add_enabled(can_start, start);
            if resp.clicked() {
                self.start();
            }
            if !problems.is_empty() && !running {
                resp.on_hover_text(problems.join("\n"));
            }
            // Пауза, а не «Стоп»: прогон остаётся на месте — очередь аккаунтов,
            // лимиты, счётчик сделанного. «Стоп» же заканчивает его совсем.
            let pause = egui::Button::new(
                RichText::new(if paused { "Продолжить" } else { "Пауза" }).size(14.0).color(theme::FG),
            )
            .fill(if paused { theme::BG_ACTIVE } else { theme::BG_INPUT })
            .min_size(egui::vec2(w, 32.0));
            let resp = ui.add_enabled(running, pause);
            if resp.clicked() {
                if st.toggle_pause() {
                    st.log.push("[=] Пауза. Начатый запрос дойдёт, дальше бот ждёт.");
                } else {
                    st.log.push("[=] Продолжаю.");
                }
            }
            resp.on_hover_text(
                "Прогон замирает: на сайт не уходит ни одного запроса, а очередь аккаунтов, лимиты и счётчик остаются на месте. «Стоп» заканчивает прогон совсем.",
            );

            let stop = egui::Button::new(RichText::new("Стоп").size(14.0).color(theme::FG))
                .fill(theme::BG_INPUT)
                .min_size(egui::vec2(w, 32.0));
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
                    console::show(ui, &self.accounts.log, &self.accounts.view, "acclog");
                });
                ui.add_space(4.0);
                ui.separator();
            }
            console::show(ui, &st.log, &st.view, "runlog");
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

/// Режим, где бот сам находит себе работу: лента вопросов, уведомления.
/// Остальные три идут строго по списку ссылок.
fn feed_mode(m: Mode) -> bool {
    !matches!(m, Mode::Votes | Mode::Subscribe | Mode::Complain)
}

/// Лимиты прогона по режиму: общий на аккаунт и на круг.
///
/// В режимах по ссылкам их нет вовсе — работу там ограничивает сам список, а
/// повторных кругов не бывает. Вдобавок общий лимит попадал в то же поле, что и
/// лимит режима, и в голосах с жалобами применялся к КАЖДОЙ ссылке отдельно:
/// «не больше 20» на трёх профилях означало до шестидесяти.
fn run_limits(mode: Mode, c: &CommonForm) -> (i64, i64) {
    if feed_mode(mode) {
        (c.total_limit, c.round_limit)
    } else {
        (0, 0)
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.top_bar(ui);
        self.left_panel(ui);
        self.central(ui);
        let bg = self.bg.clone();
        // Отметки «какие картинки прикладывать» принадлежат режиму, который
        // открыл окно: к ответам и к вопросам обычно идут разные.
        let mut spare = Vec::new();
        let chosen = match self.mode {
            Mode::Answers => &mut self.forms.answers.image.selected,
            Mode::Questions => &mut self.forms.questions.image.selected,
            _ => &mut spare,
        };
        self.pool.ui(&ctx, &bg, chosen);

        // Пока что-то работает — обновляем картинку 10 раз в секунду. В покое
        // окно не перерисовывается вообще: ноль нагрузки на простое.
        if self.any_running() || self.bg.busy() > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.forms.mode = Some(self.mode);
        if let Ok(t) = serde_json::to_string(&self.forms) {
            storage.set_string(STATE_KEY, t);
        }
        // eframe зовёт это периодически — заодно дописываем отложенную ротацию кук.
        self.bg.core.accounts.flush();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for st in self.modes.values() {
            st.request_stop();
        }
        self.bg.core.accounts.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forms_ai::Source;

    /// Настройки, сохранённые ПРОШЛОЙ версией, должны читаться и дальше.
    ///
    /// Тут легко потерять всё разом: у форм появились новые поля, а «общие
    /// настройки» переехали из одного объекта в набор «по режиму». Если serde
    /// на этом споткнётся, человек после обновления получит чистые формы —
    /// и ссылки, и ключ нейросети придётся вбивать заново.
    #[test]
    fn old_settings_still_load() {
        let legacy = r#"{
            "common": { "concurrency": 3, "all_at_once": true, "total_limit": 7,
                        "round_limit": 0, "repeat_rounds": false, "round_pause_min": 10.0,
                        "proxy_rotate_fails": 2, "check_auth": true },
            "votes": { "target_kind": "Profile", "links": "https://otvet.mail.ru/profile/id1",
                       "plus": true, "delay": 2.0, "limit": 0 },
            "answers": { "mode": "NoAi", "limit": 9, "from_links": true,
                         "links": "https://otvet.mail.ru/question/1",
                         "delay_min": 20.0, "delay_max": 45.0, "feed_min": 10.0, "feed_max": 20.0,
                         "recent_scan": 10, "repeat_per_question": 1, "batch_size": 1,
                         "parallel": false, "continuous_feed": false, "conversational": false,
                         "convo_budget_k": 60.0, "skip_others": false, "random_tag": true,
                         "signature": "подпись",
                         "image": { "kind": "Off", "folder": "images", "count": 1 } },
            "questions": { "mode": "Ai", "limit": 5, "delay_min": 30.0, "delay_max": 90.0,
                           "topic": "рыбалка", "noai_list": "",
                           "image": { "kind": "Off", "folder": "images", "count": 1 } }
        }"#;
        let mut f: PersistedForms = serde_json::from_str(legacy).expect("старые настройки не прочитались");
        f.migrate();
        // Пережило переезд.
        assert_eq!(f.votes.links, "https://otvet.mail.ru/profile/id1");
        assert_eq!(f.answers.limit, 9);
        assert_eq!(f.answers.signature, "подпись");
        // Галка «по ссылкам» стала источником, а не сбросилась на ленту.
        assert_eq!(f.answers.source, Source::Links);
        // Новые поля взяли значения по умолчанию, а не сломали разбор.
        assert!(f.answers.verify_posted);
        assert!(f.answers.skip_own_authors);
        assert_eq!(f.answers.noai_list, "");
        // Общие настройки теперь свои у каждого режима — начинаем с дефолтных.
        assert_eq!(f.common(Mode::Votes).concurrency, 1);
        // Однострочная тема из старой версии читается как список из одной темы.
        assert_eq!(f.questions.topics, "рыбалка");
    }

    /// Настройки «по режиму» должны переживать сохранение и чтение.
    #[test]
    fn per_mode_settings_round_trip() {
        let mut f = PersistedForms::default();
        f.common_mut(Mode::Votes).all_at_once = true;
        f.common_mut(Mode::Answers).total_limit = 20;
        let txt = serde_json::to_string(&f).unwrap();
        let back: PersistedForms = serde_json::from_str(&txt).unwrap();
        assert!(back.common(Mode::Votes).all_at_once);
        assert!(!back.common(Mode::Answers).all_at_once);
        assert_eq!(back.common(Mode::Answers).total_limit, 20);
        assert_eq!(back.common(Mode::Comments).total_limit, 0);
    }

    /// Режимы по ссылкам лимитов прогона не знают: список ссылок и есть лимит.
    /// Старые настройки могут хранить число — до раннера оно доходить не должно.
    #[test]
    fn link_modes_run_without_limits() {
        let c = CommonForm { total_limit: 20, round_limit: 5, ..Default::default() };
        for m in [Mode::Votes, Mode::Subscribe, Mode::Complain] {
            assert_eq!(run_limits(m, &c), (0, 0), "{m:?}: лимит прогона должен быть снят");
        }
        for m in [Mode::Answers, Mode::Questions, Mode::Comments] {
            assert_eq!(run_limits(m, &c), (20, 5), "{m:?}: лимит прогона должен работать");
        }
    }
}
