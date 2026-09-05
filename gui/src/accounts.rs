//! accounts.rs — панель аккаунтов: таблица, массовые действия, диалоги.
//!
//! Статус значит ровно то же, что в веб-версии, и это важно:
//!   «жив» — сессия отвечает;  «разлогин» — ТОЧНО не залогинен (401/403);
//!   «бан» — сайт заблокировал аккаунт (user_status < 0): сессия при этом
//!   живая, и без отдельной проверки такой аккаунт годами числился бы живым,
//!   молча тратя прогон впустую;
//!   «не проверен» — неизвестно.
//! Антибот (418/429) и сетевые сбои статус НЕ меняют: иначе умерший прокси
//! пометил бы живые аккаунты мёртвыми, и их бы вручную перелогинивали зря.

use crate::state::{Bg, ConsoleView, LogBuf};
use crate::theme;
use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};
use otvet_core::accounts::{normalize_cookies_input, Account};
use otvet_core::api;
use otvet_core::util::Stop;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Default)]
pub enum Dialog {
    #[default]
    None,
    Add {
        name: String,
        cookies: String,
        proxy: String,
        error: String,
    },
    Proxy {
        name: String,
        value: String,
    },
    Rename {
        old: String,
        new: String,
        error: String,
    },
    Delete {
        name: String,
    },
    /// Смена ника и имени НА САЙТЕ (не в программе).
    Profile {
        name: String,
        username: String,
        nick: String,
        error: String,
        /// Текущие значения уже подтянуты с сайта.
        loaded: bool,
    },
    /// Смена аватара: файл или ссылка на картинку с сайта.
    Avatar {
        name: String,
        file: String,
    },
    /// Вход через настоящий браузер: имя нового аккаунта (или существующего) и прокси.
    Login {
        name: String,
        proxy: String,
        error: String,
        /// Аккаунт уже есть — обновляем ему куки, а не создаём новый.
        existing: bool,
    },
}

pub struct AccountsPanel {
    pub selected: HashSet<String>,
    pub filter: String,
    pub dialog: Dialog,
    pub log: Arc<LogBuf>,
    /// Как показан лог аккаунтов (прокрутка, фильтр, время).
    pub view: Mutex<ConsoleView>,
    /// Первый показ: отмечаем все не-красные аккаунты, как делает веб-версия.
    initialized: bool,
    /// Таблицу один раз проматываем в начало.
    scrolled_once: bool,
    /// Идёт вход через браузер: этим же сигналом его и отменяем.
    login_stop: Option<Stop>,
}

impl Default for AccountsPanel {
    fn default() -> Self {
        Self {
            selected: HashSet::new(),
            filter: String::new(),
            dialog: Dialog::None,
            log: Arc::new(LogBuf::default()),
            view: Mutex::new(ConsoleView::default()),
            initialized: false,
            scrolled_once: false,
            login_stop: None,
        }
    }
}

impl AccountsPanel {
    pub fn selected_accounts(&self, all: &[Account]) -> Vec<Account> {
        all.iter().filter(|a| self.selected.contains(&a.name) && a.has_cookies()).cloned().collect()
    }

    /// Сколько отмечено — без копирования самих аккаунтов. Кадр рисуется по
    /// десять раз в секунду, а в аккаунте лежат куки: копировать их ради
    /// одного числа незачем.
    pub fn selected_count(&self, all: &[Account]) -> usize {
        all.iter().filter(|a| self.selected.contains(&a.name) && a.has_cookies()).count()
    }

    pub fn ui(&mut self, ui: &mut Ui, bg: &Arc<Bg>) {
        let all = bg.accounts();
        if !self.initialized && !all.is_empty() {
            self.initialized = true;
            // Не-красные = те, про кого не известно точно, что они разлогинены.
            for a in all.iter() {
                // Не «не разлогинен», а «можно работать»: заблокированный
                // аккаунт отвечает на проверку как живой, но всё, что он
                // сделает за прогон, сайт молча выбросит.
                if a.banned != Some(true) && a.auth_bad != Some(true) && a.has_cookies() {
                    self.selected.insert(a.name.clone());
                }
            }
        }

        self.toolbar(ui, bg, &all);
        // Файл аккаунтов не прочитался — об этом нужно сказать громко: иначе
        // человек увидит пустой список и решит, что аккаунты пропали.
        if let Some(err) = bg.core.accounts.load_error() {
            ui.add_space(4.0);
            ui.colored_label(theme::WARN, format!("[!] {err}"));
            ui.colored_label(
                theme::FG_DIM,
                "Данные из копии никуда не делись. Почини файл (или удали его) и нажми «Перечитать файл».",
            );
        }
        ui.add_space(4.0);
        if all.is_empty() {
            // Пустой список — самое частое первое впечатление. Пусть он объясняет,
            // что делать, а не молчит серой таблицей.
            ui.add_space(18.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Аккаунтов пока нет").color(theme::FG_STRONG).size(15.0));
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Нажми «Добавить аккаунт» — откроется браузер: Ответы и страница входа.\n\
                         Заходишь в почту как обычно, доделываешь что нужно и закрываешь\n\
                         окно — аккаунт появится здесь.",
                    )
                    .color(theme::FG_DIM),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("Данные лежат рядом с программой: {}", bg.core.root.display()))
                        .color(theme::FG_FAINT)
                        .size(11.0),
                );
            });
        } else {
            self.table(ui, bg, &all);
        }
        self.dialogs(ui, bg);
    }

    fn toolbar(&mut self, ui: &mut Ui, bg: &Arc<Bg>, all: &[Account]) {
        // Первая строка — «добавить аккаунт»: с этого начинается работа.
        ui.horizontal_wrapped(|ui| {
            let logging_in = self.login_stop.as_ref().map(|s| !s.is_stopped()).unwrap_or(false);
            if logging_in {
                ui.label(RichText::new("Открыт браузер — войди в аккаунт").color(theme::FG_STRONG));
                if ui.button("Отменить вход").clicked() {
                    if let Some(s) = &self.login_stop {
                        s.stop();
                    }
                }
            } else {
                let add = egui::Button::new(RichText::new("Добавить аккаунт").color(theme::FG_STRONG))
                    .fill(theme::BG_ACTIVE);
                if ui
                    .add(add)
                    .on_hover_text("Откроется браузер: логинишься руками, куки снимутся сами")
                    .clicked()
                {
                    self.dialog = Dialog::Login {
                        name: suggest_name(all),
                        proxy: String::new(),
                        error: String::new(),
                        existing: false,
                    };
                }
                if ui
                    .button("Вставить куки")
                    .on_hover_text("Если куки уже есть — из другого приложения или расширения браузера")
                    .clicked()
                {
                    self.dialog = Dialog::Add {
                        name: suggest_name(all),
                        cookies: String::new(),
                        proxy: String::new(),
                        error: String::new(),
                    };
                }
            }

            ui.separator();
            let banned = all.iter().filter(|a| a.banned == Some(true)).count();
            let green = all.iter().filter(|a| a.auth_bad == Some(false) && a.banned != Some(true)).count();
            let red = all.iter().filter(|a| a.auth_bad == Some(true)).count();
            ui.label(RichText::new(format!("Всего {}", all.len())).color(theme::FG_STRONG));
            ui.label(RichText::new(format!("живых {green}")).color(theme::FG));
            ui.label(RichText::new(format!("разлогин {red}")).color(theme::FG_DIM));
            if banned > 0 {
                ui.label(RichText::new(format!("бан {banned}")).color(theme::BAD))
                    .on_hover_text("Сессия жива, но сайт заблокировал аккаунт: действия молча не проходят");
            }
            ui.label(RichText::new(format!("отмечено {}", self.selected.len())).color(theme::FG_STRONG));
        });

        // Вторая строка — выбор и обслуживание.
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Отметить:").color(theme::FG_DIM));
            if ui.button("все").clicked() {
                self.selected = all.iter().filter(|a| a.has_cookies()).map(|a| a.name.clone()).collect();
            }
            if ui
                .button("живых")
                .on_hover_text(
                    "Те, чья сессия точно жива. Заблокированные сюда не входят — работать ими нельзя",
                )
                .clicked()
            {
                self.selected = all.iter().filter(|a| is_workable(a)).map(|a| a.name.clone()).collect();
            }
            if ui.button("никого").clicked() {
                self.selected.clear();
            }
            ui.separator();
            if ui
                .button("Проверить отмеченные")
                .on_hover_text("Жива ли сессия и какая сейчас карма")
                .clicked()
            {
                spawn_check_selected(bg, self.log.clone(), self.selected_accounts(all));
            }
            if ui.button("Перечитать файл").on_hover_text("accounts.json мог поправить кто-то ещё").clicked()
            {
                bg.reload_accounts();
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.filter).desired_width(150.0).hint_text("поиск по имени"),
            );
        });
    }

    fn table(&mut self, ui: &mut Ui, bg: &Arc<Bg>, all: &[Account]) {
        let filter = self.filter.trim().to_lowercase();
        let rows: Vec<&Account> = all
            .iter()
            .filter(|a| {
                filter.is_empty()
                    || a.name.to_lowercase().contains(&filter)
                    || a.username.as_deref().unwrap_or("").to_lowercase().contains(&filter)
            })
            .collect();

        let text_h = ui.text_style_height(&egui::TextStyle::Body) + 10.0;
        // egui запоминает прокрутку между запусками, и список аккаунтов
        // открывался там, где его оставили в прошлый раз — обычно в самом низу.
        // Первый показ всегда сверху: это список, а не лента.
        let to_top = !std::mem::replace(&mut self.scrolled_once, true);
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(26.0))
            .column(Column::initial(170.0).at_least(110.0).clip(true))
            .column(Column::exact(96.0))
            .column(Column::exact(74.0))
            .column(Column::initial(180.0).at_least(90.0).clip(true))
            .column(Column::remainder().at_least(150.0));
        if to_top && !rows.is_empty() {
            table = table.scroll_to_row(0, Some(egui::Align::TOP));
        }
        table
            .header(22.0, |mut h| {
                h.col(|ui| {
                    ui.label("");
                });
                h.col(|ui| {
                    ui.label(RichText::new("Аккаунт").strong());
                });
                h.col(|ui| {
                    ui.label(RichText::new("Статус").strong());
                });
                h.col(|ui| {
                    ui.label(RichText::new("Карма").strong());
                });
                h.col(|ui| {
                    ui.label(RichText::new("Прокси").strong());
                });
                h.col(|ui| {
                    ui.label(RichText::new("Действия").strong());
                });
            })
            .body(|body| {
                body.rows(text_h, rows.len(), |mut row| {
                    let a = rows[row.index()];
                    row.col(|ui| {
                        let mut on = self.selected.contains(&a.name);
                        if ui.add_enabled(a.has_cookies(), egui::Checkbox::without_text(&mut on)).changed() {
                            if on {
                                self.selected.insert(a.name.clone());
                            } else {
                                self.selected.remove(&a.name);
                            }
                        }
                    });
                    row.col(|ui| {
                        let mut t = RichText::new(&a.name);
                        if !a.has_cookies() {
                            t = t.color(theme::FG_DIM);
                        }
                        let resp = ui.label(t);
                        if let Some(u) = &a.username {
                            resp.on_hover_text(format!(
                                "{u}{}",
                                a.user_id.map(|i| format!(" · id{i}")).unwrap_or_default()
                            ));
                        }
                    });
                    row.col(|ui| {
                        // Статус различаем яркостью, а не цветом: живой — светлый,
                        // разлогиненный — приглушённый, неизвестный — почти фон.
                        let (txt, color) = match (a.has_cookies(), a.auth_bad, a.banned) {
                            (false, _, _) => ("нет кук", theme::FG_FAINT),
                            (true, Some(true), _) => ("разлогин", theme::BAD),
                            // Бан важнее «жив»: сессия рабочая, а толку ноль.
                            (true, _, Some(true)) => ("бан", theme::BAD),
                            (true, Some(false), _) => ("жив", theme::FG_STRONG),
                            (true, None, _) => ("не проверен", theme::FG_FAINT),
                        };
                        let resp = ui.label(RichText::new(txt).color(color));
                        if txt == "бан" {
                            resp.on_hover_text(
                                "Сайт заблокировал аккаунт: сессия жива, но голоса и ответы молча не проходят. Бот такие пропускает.",
                            );
                        }
                    });
                    row.col(|ui| match a.karma_total() {
                        Some(k) => {
                            let c = if k < 0 { theme::FG_DIM } else { theme::FG_STRONG };
                            ui.label(RichText::new(k.to_string()).color(c));
                        }
                        None => {
                            ui.label(RichText::new("—").color(theme::FG_FAINT));
                        }
                    });
                    row.col(|ui| {
                        // В подсказке — ТО ЖЕ, что в строке, только целиком и по
                        // одному на строку. Раньше сюда уходил сырой список: пароль
                        // от прокси показывался открытым, да ещё и дублировал
                        // строку таблицы.
                        let list = a.proxy_list();
                        let masked: Vec<String> =
                            list.iter().map(|p| otvet_core::proxy::mask_proxy(p)).collect();
                        let txt = match masked.len() {
                            0 => "без прокси".to_string(),
                            1 => masked[0].clone(),
                            n => format!("{} (+{})", masked[0], n - 1),
                        };
                        let resp = ui.label(RichText::new(txt).color(if list.is_empty() {
                            theme::FG_FAINT
                        } else {
                            theme::FG_DIM
                        }));
                        // Подсказка нужна, только если в строку всё не поместилось.
                        if masked.len() > 1 {
                            resp.on_hover_text(masked.join("\n"));
                        }
                    });
                    row.col(|ui| {
                        ui.spacing_mut().item_spacing.x = 3.0;
                        if ui.small_button("проверить").clicked() {
                            spawn_check_one(bg, self.log.clone(), a.clone());
                        }
                        if ui
                            .small_button("браузер")
                            .on_hover_text("Открыть сайт под этим аккаунтом: те же куки, отпечаток и прокси")
                            .clicked()
                        {
                            spawn_open_browser(bg, self.log.clone(), a.clone());
                        }
                        if ui.small_button("прокси").clicked() {
                            self.dialog = Dialog::Proxy {
                                name: a.name.clone(),
                                value: a.proxy.clone().unwrap_or_default(),
                            };
                        }
                        // Остальное — под «ещё»: в строке и так тесно, а этими
                        // действиями пользуются раз в жизни аккаунта.
                        // Не `menu_button`: обычная кнопка выше остальных в
                        // строке, а ряд должен быть ровным. Поэтому маленькая
                        // кнопка, к которой меню подвешено вручную.
                        let more = ui.small_button("ещё");
                        egui::Popup::menu(&more).show(|ui| {
                            ui.set_min_width(200.0);
                            if ui.button("Войти заново через браузер").clicked() {
                                self.dialog = Dialog::Login {
                                    name: a.name.clone(),
                                    proxy: a.proxy.clone().unwrap_or_default(),
                                    error: String::new(),
                                    existing: true,
                                };
                                ui.close();
                            }
                            if ui.button("Скопировать куки").clicked() {
                                ui.ctx().copy_text(a.cookies.clone().unwrap_or_default());
                                self.log.push(&format!("[+] Куки «{}» скопированы", a.name));
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Сменить ник и имя на сайте…").clicked() {
                                self.dialog = Dialog::Profile {
                                    name: a.name.clone(),
                                    username: a.username.clone().unwrap_or_default(),
                                    nick: String::new(),
                                    error: String::new(),
                                    loaded: false,
                                };
                                ui.close();
                            }
                            if ui.button("Сменить аватар…").clicked() {
                                self.dialog = Dialog::Avatar { name: a.name.clone(), file: String::new() };
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Переименовать в программе…").clicked() {
                                self.dialog = Dialog::Rename {
                                    old: a.name.clone(),
                                    new: a.name.clone(),
                                    error: String::new(),
                                };
                                ui.close();
                            }
                            if ui.button("Удалить аккаунт…").clicked() {
                                self.dialog = Dialog::Delete { name: a.name.clone() };
                                ui.close();
                            }
                        });
                    });
                });
            });
    }

    fn dialogs(&mut self, ui: &mut Ui, bg: &Arc<Bg>) {
        let ctx = ui.ctx().clone();
        let mut close = false;
        // Диалог вынимаем из self: иначе замыкание окна держало бы весь `self`
        // и не дало бы тронуть ни лог, ни список отмеченных.
        let mut dlg = std::mem::take(&mut self.dialog);
        let log = self.log.clone();
        let selected = &mut self.selected;
        let login_stop = &mut self.login_stop;
        match &mut dlg {
            Dialog::None => {}
            Dialog::Add { name, cookies, proxy, error } => {
                egui::Window::new("Новый аккаунт по кукам")
                    .collapsible(false)
                    .resizable(true)
                    .default_width(560.0)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(&ctx, |ui| {
                        ui.label("Имя аккаунта");
                        ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                        ui.add_space(4.0);
                        ui.label("Куки (строка «a=1; b=2» или JSON из расширения)");
                        crate::forms::boxed_multiline(ui, "add_cookies", cookies, 6, "");
                        ui.add_space(4.0);
                        ui.label("Прокси (необязательно; несколько — через | или с новой строки)");
                        crate::forms::boxed_multiline(
                            ui,
                            "add_proxy",
                            proxy,
                            2,
                            "socks5://user:pass@host:1080",
                        );
                        if !error.is_empty() {
                            ui.colored_label(theme::WARN, error.as_str());
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("Добавить").clicked() {
                                let jar = normalize_cookies_input(cookies);
                                if name.trim().is_empty() {
                                    *error = "Задай имя аккаунта".into();
                                } else if jar.is_empty() {
                                    *error = "Пустые куки".into();
                                } else if !otvet_core::accounts::looks_logged_in(&jar) {
                                    *error =
                                        "В куках нет Auth-Token — это ещё не сессия, аккаунт будет разлогинен"
                                            .into();
                                } else {
                                    let mut acc = Account::new(name.trim());
                                    acc.cookies = Some(jar);
                                    let p = proxy.trim();
                                    if !p.is_empty() {
                                        acc.proxy = Some(p.to_string());
                                    }
                                    match bg.core.accounts.add(acc.clone()) {
                                        Ok(()) => {
                                            log.push(&format!("Аккаунт «{}» добавлен, проверяю…", acc.name));
                                            selected.insert(acc.name.clone());
                                            spawn_check_one(bg, log.clone(), acc);
                                            close = true;
                                        }
                                        Err(e) => *error = e,
                                    }
                                }
                            }
                            if ui.button("Отмена").clicked() {
                                close = true;
                            }
                        });
                    });
            }
            Dialog::Proxy { name, value } => {
                egui::Window::new(format!("Прокси — {name}"))
                    .collapsible(false)
                    .default_width(520.0)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(&ctx, |ui| {
                        ui.small("Несколько прокси — через | или с новой строки: при сетевых сбоях бот переключится на следующий.");
                        crate::forms::boxed_multiline(
                            ui,
                            "proxy_value",
                            value,
                            3,
                            "socks5://user:pass@host:1080 | http://host:8080:user:pass",
                        );
                        for line in otvet_core::proxy::split_proxies(value) {
                            match otvet_core::proxy::parse_proxy(&line) {
                                Some(cfg) => {
                                    ui.colored_label(theme::FG_STRONG, format!("ok  {}", cfg.server));
                                }
                                None => {
                                    ui.colored_label(theme::WARN, format!("не разобрать: {line}"));
                                }
                            }
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Сохранить").clicked() {
                                bg.core.accounts.set_proxy(name, value);
                                bg.refresh_accounts();
                                log.push(&format!("Прокси «{name}» обновлён"));
                                close = true;
                            }
                            if ui.button("Отмена").clicked() {
                                close = true;
                            }
                        });
                    });
            }
            Dialog::Rename { old, new, error } => {
                egui::Window::new(format!("Переименовать — {old}"))
                    .collapsible(false)
                    .default_width(420.0)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(&ctx, |ui| {
                        ui.add(egui::TextEdit::singleline(new).desired_width(f32::INFINITY));
                        ui.small(
                            "Отпечаток (персона) переедет вместе с именем — «железо» аккаунта не сменится.",
                        );
                        if !error.is_empty() {
                            ui.colored_label(theme::WARN, error.as_str());
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Переименовать").clicked() {
                                // Ядро переносит и персону, и журналы — иначе
                                // аккаунт теряет отпечаток и память о сделанном.
                                match bg.core.rename_account(old, new.trim()) {
                                    Ok(()) => {
                                        if selected.remove(old) {
                                            selected.insert(new.trim().to_string());
                                        }
                                        log.push(&format!("Переименован: «{old}» → «{}»", new.trim()));
                                        bg.refresh_accounts();
                                        close = true;
                                    }
                                    Err(e) => *error = e,
                                }
                            }
                            if ui.button("Отмена").clicked() {
                                close = true;
                            }
                        });
                    });
            }
            Dialog::Login { name, proxy, error, existing } => {
                egui::Window::new(if *existing { "Вход через браузер" } else { "Новый аккаунт — вход через браузер" })
                    .collapsible(false)
                    .default_width(520.0)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(&ctx, |ui| {
                        ui.label("Имя аккаунта");
                        ui.add_enabled(
                            !*existing,
                            egui::TextEdit::singleline(name).desired_width(f32::INFINITY),
                        );
                        ui.add_space(4.0);
                        ui.label("Прокси (необязательно)");
                        ui.add(
                            egui::TextEdit::singleline(proxy)
                                .desired_width(f32::INFINITY)
                                .hint_text("socks5://user:pass@host:1080"),
                        );
                        ui.small(
                            "Входить лучше через тот же прокси, с которого потом работать: сессия, снятая с одного IP и \
используемая с другого, для антифрода выглядит как угон.",
                        );
                        if !error.is_empty() {
                            ui.colored_label(theme::WARN, error.as_str());
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("Открыть браузер").clicked() {
                                if name.trim().is_empty() {
                                    *error = "Задай имя аккаунта".into();
                                } else if !*existing && bg.core.accounts.get(name.trim()).is_some() {
                                    *error = format!("Аккаунт «{}» уже есть", name.trim());
                                } else {
                                    let stop = Stop::new();
                                    *login_stop = Some(stop.clone());
                                    selected.insert(name.trim().to_string());
                                    spawn_login(
                                        bg,
                                        log.clone(),
                                        name.trim().to_string(),
                                        proxy.trim().to_string(),
                                        *existing,
                                        stop,
                                    );
                                    close = true;
                                }
                            }
                            if ui.button("Отмена").clicked() {
                                close = true;
                            }
                        });
                    });
            }
            Dialog::Delete { name } => {
                egui::Window::new("Удалить аккаунт?")
                    .collapsible(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(&ctx, |ui| {
                        ui.label(format!("«{name}» будет удалён из accounts.json вместе с куками."));
                        ui.horizontal(|ui| {
                            if ui.button(RichText::new("Удалить").color(theme::WARN)).clicked() {
                                bg.core.accounts.remove(name);
                                bg.core.personas.drop_persona(name);
                                selected.remove(name);
                                log.push(&format!("Аккаунт «{name}» удалён"));
                                bg.refresh_accounts();
                                close = true;
                            }
                            if ui.button("Отмена").clicked() {
                                close = true;
                            }
                        });
                    });
            }
            Dialog::Profile { name, username, nick, error, loaded } => {
                // Текущие значения тянем с сайта один раз при открытии: писать
                // их вслепую нельзя — PUT требует ВСЕ поля профиля.
                if !*loaded {
                    *loaded = true;
                    let core = bg.core.clone();
                    let log2 = log.clone();
                    let who = name.clone();
                    bg.spawn(async move {
                        if let Some(acc) = core.accounts.get(&who) {
                            match api::get_profile(&core, &acc, &Stop::new()).await {
                                Ok(p) => log2.push(&format!(
                                    "[>] {who}: сейчас ник «{}», имя «{}»",
                                    p.username, p.nick
                                )),
                                Err(e) => log2.push(&format!("[-] {who}: не прочитать профиль ({e})")),
                            }
                        }
                    });
                }
                egui::Window::new(format!("Профиль на сайте — {name}"))
                    .collapsible(false)
                    .default_width(460.0)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(&ctx, |ui| {
                        ui.small("Текущие значения появятся в логе аккаунтов. Пустое поле — не менять.");
                        ui.add_space(4.0);
                        ui.label("Ник (латиница, в адресе профиля)");
                        ui.add(egui::TextEdit::singleline(username).desired_width(f32::INFINITY));
                        ui.label("Имя (буквы и пробелы, без цифр)");
                        ui.add(egui::TextEdit::singleline(nick).desired_width(f32::INFINITY));
                        ui.small("Сайт разрешает менять их не всем и не всегда — если откажет, будет видно в логе.");
                        if !error.is_empty() {
                            ui.colored_label(theme::WARN, error.as_str());
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("Сменить").clicked() {
                                if username.trim().is_empty() && nick.trim().is_empty() {
                                    *error = "Заполни хотя бы одно поле".into();
                                } else {
                                    spawn_change_name(
                                        bg,
                                        log.clone(),
                                        name.clone(),
                                        username.trim().to_string(),
                                        nick.trim().to_string(),
                                    );
                                    close = true;
                                }
                            }
                            if ui.button("Отмена").clicked() {
                                close = true;
                            }
                        });
                    });
            }
            Dialog::Avatar { name, file } => {
                egui::Window::new(format!("Аватар — {name}"))
                    .collapsible(false)
                    .default_width(520.0)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(&ctx, |ui| {
                        ui.small("Файл с диска или ссылка на картинку, которая уже есть на сайте.");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(file)
                                    .desired_width(ui.available_width() - 90.0)
                                    .hint_text("C:\\фото\\avatar.jpg"),
                            );
                            if ui.button("Выбрать…").clicked() {
                                if let Some(f) = rfd::FileDialog::new()
                                    .add_filter("Картинки", &["jpg", "jpeg", "png", "gif", "webp"])
                                    .pick_file()
                                {
                                    *file = f.display().to_string();
                                }
                            }
                        });
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(!file.trim().is_empty(), egui::Button::new("Поставить"))
                                .clicked()
                            {
                                spawn_change_avatar(bg, log.clone(), name.clone(), file.trim().to_string());
                                close = true;
                            }
                            if ui.button("Отмена").clicked() {
                                close = true;
                            }
                        });
                    });
            }
        }
        self.dialog = if close { Dialog::None } else { dlg };
    }

    // ─── фоновые действия ───────────────────────────────────────────────────
}

/// Вход через браузер: окно Chromium → куки → аккаунт → проверка.
fn spawn_login(bg: &Arc<Bg>, log: Arc<LogBuf>, name: String, proxy: String, existing: bool, stop: Stop) {
    let core = bg.core.clone();
    bg.spawn(async move {
        // Персона обязана быть ТА ЖЕ, что у HTTP-клиента: если аккаунт уже носит
        // свою (`_persona` из старой версии), браузер должен логиниться ею же.
        // Иначе вход придёт с одного «железа», а работа пойдёт с другого — ровно
        // то противоречие, ради которого персоны и заведены.
        let persona = core
            .accounts
            .get(&name)
            .and_then(|a| a.stored_persona())
            .filter(|p| p.v == 2 && !p.ua.is_empty())
            .unwrap_or_else(|| core.personas.get(&name, &otvet_core::persona::PersonaOpts::default()));
        // Профиль браузера отдельный на аккаунт: общий профиль означал бы общие
        // куки, и второй вход перетирал бы первый.
        let profile_dir =
            core.root.join("profiles").join(format!("_login_{}", otvet_core::util::safe_name(&name)));
        let proxy_opt = if proxy.is_empty() { None } else { Some(proxy.as_str()) };
        let logger: otvet_core::util::Log = {
            let l = log.clone();
            Arc::new(move |line: &str| l.push(line))
        };

        match otvet_core::cdp::login_and_harvest(
            &core.root,
            &persona,
            proxy_opt,
            &profile_dir,
            &logger,
            &stop,
        )
        .await
        {
            Err(e) => log.push(&format!("[-] Вход «{name}»: {e}")),
            Ok(h) => {
                let acc = match (existing, core.accounts.get(&name)) {
                    (true, Some(a)) => {
                        core.accounts.mutate(&name, |x| {
                            x.cookies = Some(h.cookies.clone());
                            x.ua = Some(h.ua.clone());
                            x.auth_bad = Some(false);
                            // Сессия новая — прежняя отметка бана к ней не
                            // относится: проверка расставит всё заново.
                            x.banned = None;
                        });
                        // Прокси ставим ОТДЕЛЬНО: set_proxy заодно сбрасывает
                        // рантайм-состояние ротации. Без этого активным
                        // оставался прежний прокси, и свежеснятая сессия тут же
                        // уходила с чужого IP — то самое, о чём предупреждает
                        // подсказка в диалоге входа.
                        if !proxy.is_empty() {
                            core.accounts.set_proxy(&name, &proxy);
                        }
                        core.accounts.get(&name).unwrap_or(a)
                    }
                    _ => {
                        let mut a = Account::new(&name);
                        a.cookies = Some(h.cookies.clone());
                        a.ua = Some(h.ua.clone());
                        if !proxy.is_empty() {
                            a.proxy = Some(proxy.clone());
                        }
                        if let Err(e) = core.accounts.add(a.clone()) {
                            log.push(&format!("[-] {e}"));
                            return;
                        }
                        a
                    }
                };
                log.push(&format!("Куки сняты ({} шт.)", h.cookies.split(';').count()));
                // Сразу выясняем, кто вошёл: userId нужен для публикации вопросов.
                let v = api::validate_account(&core, &acc, &Stop::new()).await;
                api::persist_validation(&core, &name, &v);
                log.push(&fmt_validation(&name, &v));
                if let Some(u) = &v.username {
                    log.push(&format!(
                        "{name} → {u}{}",
                        v.user_id.map(|i| format!(" (id{i})")).unwrap_or_default()
                    ));
                }
                // Профиль НЕ трогаем: окно после входа остаётся открытым, и
                // папку держит работающий браузер. Её убирает сам вход, когда
                // человек закроет окно.
            }
        }
        stop.stop(); // снимаем признак «идёт вход»
    });
}

/// Открыть сайт в настоящем браузере под этим аккаунтом.
fn spawn_open_browser(bg: &Arc<Bg>, log: Arc<LogBuf>, acc: Account) {
    let core = bg.core.clone();
    log.push(&format!("[>] Открываю браузер под «{}»…", acc.name));
    bg.spawn(async move {
        let persona = core.http.persona_for(&acc);
        // Профиль браузера постоянный на аккаунт: второй заход откроется там же,
        // где был первый, и сайт увидит ту же машину.
        let dir = core.root.join("profiles").join(otvet_core::util::safe_name(&acc.name));
        let logger: otvet_core::util::Log = {
            let l = log.clone();
            Arc::new(move |line: &str| l.push(line))
        };
        let cookies = acc.cookie_header().unwrap_or_default();
        if let Err(e) = otvet_core::cdp::open_as(
            &core.root,
            &persona,
            acc.active_proxy().as_deref(),
            &dir,
            &cookies,
            "https://otvet.mail.ru/",
            &logger,
        )
        .await
        {
            log.push(&format!("[-] Браузер «{}»: {e}", acc.name));
        }
    });
}

/// Смена ника/имени НА САЙТЕ.
fn spawn_change_name(bg: &Arc<Bg>, log: Arc<LogBuf>, name: String, username: String, nick: String) {
    let core = bg.core.clone();
    bg.spawn(async move {
        let Some(acc) = core.accounts.get(&name) else { return };
        let u = if username.is_empty() { None } else { Some(username.as_str()) };
        let n = if nick.is_empty() { None } else { Some(nick.as_str()) };
        match api::change_name(&core, &acc, u, n, &Stop::new()).await {
            Ok(p) => {
                log.push(&format!("[+] {name}: теперь ник «{}», имя «{}»", p.username, p.nick));
                core.accounts.set_ident(&name, None, Some(p.username));
            }
            Err(e) => log.push(&format!("[-] {name}: не сменить ({e})")),
        }
    });
}

/// Смена аватара: файл заливается на сайт, затем прописывается в профиль.
fn spawn_change_avatar(bg: &Arc<Bg>, log: Arc<LogBuf>, name: String, file: String) {
    let core = bg.core.clone();
    log.push(&format!("[>] {name}: ставлю аватар…"));
    bg.spawn(async move {
        let Some(acc) = core.accounts.get(&name) else { return };
        match api::change_avatar(&core, &acc, &file, &Stop::new()).await {
            Ok(hash) => {
                log.push(&format!("[+] {name}: аватар обновлён ({})", crate::forms::clip_middle(&hash, 20)))
            }
            Err(e) => log.push(&format!("[-] {name}: аватар не поставился ({e})")),
        }
    });
}

/// С аккаунтом можно работать: есть куки, он не разлогинен и не заблокирован.
///
/// Бан — отдельное состояние: сессия жива и сайт отвечает как обычно, поэтому
/// по одному `authBad` такой аккаунт выглядит здоровым, а на деле ни одно его
/// действие не проходит.
fn is_workable(a: &Account) -> bool {
    a.has_cookies() && a.auth_bad == Some(false) && a.banned != Some(true)
}

fn spawn_check_one(bg: &Arc<Bg>, log: Arc<LogBuf>, acc: Account) {
    let core = bg.core.clone();
    bg.spawn(async move {
        let stop = Stop::new();
        let v = api::validate_account(&core, &acc, &stop).await;
        api::persist_validation(&core, &acc.name, &v);
        log.push(&fmt_validation(&acc.name, &v));
    });
}

fn spawn_check_selected(bg: &Arc<Bg>, log: Arc<LogBuf>, list: Vec<Account>) {
    if list.is_empty() {
        log.push("[-] Не отмечен ни один аккаунт с куками.");
        return;
    }
    let core = bg.core.clone();
    log.push(&format!("[>] Проверяю {} аккаунт(ов)…", list.len()));
    bg.spawn(async move {
        let stop = Stop::new();
        // По четыре разом: проверка дёргает три эндпоинта на аккаунт, и пачка
        // в 50 параллельных запросов с одного IP — прямой путь к 429.
        let mut set = tokio::task::JoinSet::new();
        let mut it = list.into_iter();
        let mut alive = 0usize;
        let mut dead = 0usize;
        let mut banned = 0usize;
        loop {
            while set.len() < 4 {
                match it.next() {
                    Some(acc) => {
                        let core = core.clone();
                        let stop = stop.clone();
                        set.spawn(async move {
                            let v = api::validate_account(&core, &acc, &stop).await;
                            api::persist_validation(&core, &acc.name, &v);
                            (acc.name.clone(), v)
                        });
                    }
                    None => break,
                }
            }
            match set.join_next().await {
                Some(Ok((name, v))) => {
                    if v.banned {
                        banned += 1;
                    } else if v.alive {
                        alive += 1;
                    } else if v.auth_bad {
                        dead += 1;
                    }
                    log.push(&fmt_validation(&name, &v));
                }
                Some(Err(_)) => {}
                None => break,
            }
        }
        let ban_part = if banned > 0 { format!(", бан {banned}") } else { String::new() };
        log.push(&format!("[+] Проверка завершена: живых {alive}, разлогин {dead}{ban_part}"));
    });
}

fn fmt_validation(name: &str, v: &api::Validation) -> String {
    let karma = v.karma.as_ref().map(|k| format!(", карма {}", k.total)).unwrap_or_default();
    if let Some(e) = &v.error {
        return format!("[!] {name}: сеть/прокси ({e}) — статус не трогаю");
    }
    if v.blocked {
        format!("[x] {name}: антибот (418/429) — статус не трогаю")
    } else if v.banned {
        format!("[x] {name}: ЗАБЛОКИРОВАН сайтом — сессия жива, но действия не проходят{karma}")
    } else if v.alive {
        format!("[+] {name}: авторизован{karma}")
    } else if v.auth_bad {
        format!("[x] {name}: НЕ авторизован — нужен новый вход")
    } else {
        format!("[!] {name}: непонятный ответ — статус не трогаю")
    }
}

fn suggest_name(all: &[Account]) -> String {
    for i in 1..1000 {
        let n = format!("Аккаунт {i}");
        if !all.iter().any(|a| a.name == n) {
            return n;
        }
    }
    "Аккаунт".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(auth_bad: Option<bool>, banned: Option<bool>) -> Account {
        let mut a = Account::new("тест");
        a.cookies = Some("Mpop=x".into());
        a.auth_bad = auth_bad;
        a.banned = banned;
        a
    }

    /// Заблокированный аккаунт живым не считается ни при отметке «живых», ни при
    /// первом показе списка. Сайт отвечает на его проверку как обычно, поэтому
    /// по одному `authBad` он выглядит здоровым — и молча съедал бы прогон.
    #[test]
    fn banned_account_is_not_workable() {
        assert!(is_workable(&acc(Some(false), None)), "обычный живой аккаунт");
        assert!(is_workable(&acc(Some(false), Some(false))), "бан снят — снова рабочий");
        assert!(!is_workable(&acc(Some(false), Some(true))), "заблокированный не рабочий");
        assert!(!is_workable(&acc(Some(true), None)), "разлогиненный не рабочий");
        assert!(!is_workable(&acc(None, None)), "непроверенный в «живых» не попадает");

        let mut no_cookies = acc(Some(false), None);
        no_cookies.cookies = None;
        assert!(!is_workable(&no_cookies), "без кук работать нечем");
    }
}
