//! accounts.rs — панель аккаунтов: таблица, массовые действия, диалоги.
//!
//! Статус значит ровно то же, что в веб-версии, и это важно:
//!   «жив» — сессия отвечает;  «разлогин» — ТОЧНО не залогинен (401/403);
//!   «не проверен» — неизвестно.
//! Антибот (418/429) и сетевые сбои статус НЕ меняют: иначе умерший прокси
//! пометил бы живые аккаунты мёртвыми, и их бы вручную перелогинивали зря.

use crate::state::{Bg, LogBuf};
use crate::theme;
use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};
use otvet_core::accounts::{normalize_cookies_input, Account};
use otvet_core::api;
use otvet_core::util::Stop;
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
    /// Первый показ: отмечаем все не-красные аккаунты, как делает веб-версия.
    initialized: bool,
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
            initialized: false,
            login_stop: None,
        }
    }
}

impl AccountsPanel {
    pub fn selected_accounts(&self, all: &[Account]) -> Vec<Account> {
        all.iter().filter(|a| self.selected.contains(&a.name) && a.has_cookies()).cloned().collect()
    }

    pub fn ui(&mut self, ui: &mut Ui, bg: &Arc<Bg>) {
        let all = bg.accounts();
        if !self.initialized && !all.is_empty() {
            self.initialized = true;
            // Не-красные = те, про кого не известно точно, что они разлогинены.
            for a in all.iter() {
                if a.auth_bad != Some(true) && a.has_cookies() {
                    self.selected.insert(a.name.clone());
                }
            }
        }

        self.toolbar(ui, bg, &all);
        // Файл аккаунтов не прочитался — об этом нужно сказать громко: иначе
        // человек увидит пустой список и решит, что аккаунты пропали.
        if let Some(err) = bg.core.accounts.load_error() {
            ui.add_space(4.0);
            ui.colored_label(theme::WARN, format!("⚠ {err}"));
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
                        "Нажми «Добавить аккаунт» — откроется браузер.\n\
                         Заходишь в почту как обычно, окно закроется само, аккаунт появится здесь.",
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
            let green = all.iter().filter(|a| a.auth_bad == Some(false)).count();
            let red = all.iter().filter(|a| a.auth_bad == Some(true)).count();
            ui.label(RichText::new(format!("Всего {}", all.len())).color(theme::FG_STRONG));
            ui.label(RichText::new(format!("живых {green}")).color(theme::FG));
            ui.label(RichText::new(format!("разлогин {red}")).color(theme::FG_DIM));
            ui.label(RichText::new(format!("отмечено {}", self.selected.len())).color(theme::FG_STRONG));
        });

        // Вторая строка — выбор и обслуживание.
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Отметить:").color(theme::FG_DIM));
            if ui.button("все").clicked() {
                self.selected = all.iter().filter(|a| a.has_cookies()).map(|a| a.name.clone()).collect();
            }
            if ui.button("живых").on_hover_text("Только те, чья сессия точно жива").clicked()
            {
                self.selected = all
                    .iter()
                    .filter(|a| a.auth_bad == Some(false) && a.has_cookies())
                    .map(|a| a.name.clone())
                    .collect();
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
        TableBuilder::new(ui)
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(26.0))
            .column(Column::initial(170.0).at_least(110.0).clip(true))
            .column(Column::exact(96.0))
            .column(Column::exact(74.0))
            .column(Column::initial(180.0).at_least(90.0).clip(true))
            .column(Column::remainder().at_least(150.0))
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
                        let (txt, color) = match (a.has_cookies(), a.auth_bad) {
                            (false, _) => ("нет кук", theme::FG_FAINT),
                            (true, Some(true)) => ("разлогин", theme::BAD),
                            (true, Some(false)) => ("жив", theme::FG_STRONG),
                            (true, None) => ("не проверен", theme::FG_FAINT),
                        };
                        ui.label(RichText::new(txt).color(color));
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
                        let list = a.proxy_list();
                        let txt = match list.len() {
                            0 => "прямое".to_string(),
                            1 => otvet_core::proxy::mask_proxy(&list[0]),
                            n => format!("{} (+{})", otvet_core::proxy::mask_proxy(&list[0]), n - 1),
                        };
                        ui.label(RichText::new(txt).color(if list.is_empty() {
                            theme::FG_FAINT
                        } else {
                            theme::FG_DIM
                        }))
                        .on_hover_text(list.join("\n"));
                    });
                    row.col(|ui| {
                        if ui.small_button("проверить").clicked() {
                            spawn_check_one(bg, self.log.clone(), a.clone());
                        }
                        if ui.small_button("вход").on_hover_text("Войти заново через браузер").clicked()
                        {
                            self.dialog = Dialog::Login {
                                name: a.name.clone(),
                                proxy: a.proxy.clone().unwrap_or_default(),
                                error: String::new(),
                                existing: true,
                            };
                        }
                        if ui.small_button("куки").on_hover_text("Скопировать куки в буфер").clicked()
                        {
                            ui.ctx().copy_text(a.cookies.clone().unwrap_or_default());
                            self.log.push(&format!("Куки «{}» скопированы", a.name));
                        }
                        if ui.small_button("прокси").clicked() {
                            self.dialog = Dialog::Proxy {
                                name: a.name.clone(),
                                value: a.proxy.clone().unwrap_or_default(),
                            };
                        }
                        if ui.small_button("имя").on_hover_text("Переименовать").clicked() {
                            self.dialog = Dialog::Rename {
                                old: a.name.clone(),
                                new: a.name.clone(),
                                error: String::new(),
                            };
                        }
                        if ui.small_button("×").on_hover_text("Удалить аккаунт").clicked() {
                            self.dialog = Dialog::Delete { name: a.name.clone() };
                        }
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
                egui::Window::new("🍪 Новый аккаунт по кукам")
                    .collapsible(false)
                    .resizable(true)
                    .default_width(560.0)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(&ctx, |ui| {
                        ui.label("Имя аккаунта");
                        ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                        ui.add_space(4.0);
                        ui.label("Куки (строка «a=1; b=2» или JSON из расширения)");
                        ui.add(
                            egui::TextEdit::multiline(cookies)
                                .desired_rows(6)
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );
                        ui.add_space(4.0);
                        ui.label("Прокси (необязательно; несколько — через | или с новой строки)");
                        ui.add(
                            egui::TextEdit::multiline(proxy)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .hint_text("socks5://user:pass@host:1080"),
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
                                        "В куках нет Mpop/Auth-Token — это гостевая сессия, аккаунт будет разлогинен"
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
                                            log.push(&format!("➕ Аккаунт «{}» добавлен, проверяю…", acc.name));
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
                egui::Window::new(format!("🌐 Прокси — {name}"))
                    .collapsible(false)
                    .default_width(520.0)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(&ctx, |ui| {
                        ui.small("Несколько прокси — через | или с новой строки: при сетевых сбоях бот переключится на следующий.");
                        ui.add(
                            egui::TextEdit::multiline(value)
                                .desired_rows(3)
                                .desired_width(f32::INFINITY)
                                .hint_text("socks5://user:pass@host:1080 | http://host:8080:user:pass"),
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
                                log.push(&format!("🌐 Прокси «{name}» обновлён"));
                                close = true;
                            }
                            if ui.button("Отмена").clicked() {
                                close = true;
                            }
                        });
                    });
            }
            Dialog::Rename { old, new, error } => {
                egui::Window::new(format!("✏ Переименовать — {old}"))
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
                                        log.push(&format!("✏ «{old}» → «{}»", new.trim()));
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
                                log.push(&format!("🗑 Аккаунт «{name}» удалён"));
                                bg.refresh_accounts();
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
            Err(e) => log.push(&format!("❌ Вход «{name}»: {e}")),
            Ok(h) => {
                let acc = match (existing, core.accounts.get(&name)) {
                    (true, Some(a)) => {
                        core.accounts.mutate(&name, |x| {
                            x.cookies = Some(h.cookies.clone());
                            x.ua = Some(h.ua.clone());
                            if !proxy.is_empty() {
                                x.proxy = Some(proxy.clone());
                            }
                            x.auth_bad = Some(false);
                        });
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
                            log.push(&format!("❌ {e}"));
                            return;
                        }
                        a
                    }
                };
                log.push(&format!("🍪 Куки сняты ({} шт.)", h.cookies.split(';').count()));
                // Сразу выясняем, кто вошёл: userId нужен для публикации вопросов.
                let v = api::validate_account(&core, &acc, &Stop::new()).await;
                api::persist_validation(&core, &name, &v);
                log.push(&fmt_validation(&name, &v));
                if let Some(u) = &v.username {
                    log.push(&format!(
                        "👤 {name} → {u}{}",
                        v.user_id.map(|i| format!(" (id{i})")).unwrap_or_default()
                    ));
                }
                // Временный профиль браузера больше не нужен.
                let _ = std::fs::remove_dir_all(&profile_dir);
            }
        }
        stop.stop(); // снимаем признак «идёт вход»
    });
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
        log.push("❌ Не отмечен ни один аккаунт с куками.");
        return;
    }
    let core = bg.core.clone();
    log.push(&format!("🔍 Проверяю {} аккаунт(ов)…", list.len()));
    bg.spawn(async move {
        let stop = Stop::new();
        // По четыре разом: проверка дёргает три эндпоинта на аккаунт, и пачка
        // в 50 параллельных запросов с одного IP — прямой путь к 429.
        let mut set = tokio::task::JoinSet::new();
        let mut it = list.into_iter();
        let mut alive = 0usize;
        let mut dead = 0usize;
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
                    if v.alive {
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
        log.push(&format!("🎉 Проверка завершена: 🟢 {alive}, 🔴 {dead}"));
    });
}

fn fmt_validation(name: &str, v: &api::Validation) -> String {
    let karma = v.karma.as_ref().map(|k| format!(", карма {}", k.total)).unwrap_or_default();
    if let Some(e) = &v.error {
        return format!("⚠️  {name}: сеть/прокси ({e}) — статус не трогаю");
    }
    if v.blocked {
        format!("🛑 {name}: антибот (418/429) — статус не трогаю")
    } else if v.alive {
        format!("✅ {name}: авторизован{karma}")
    } else if v.auth_bad {
        format!("🔒 {name}: НЕ авторизован — нужен новый вход")
    } else {
        format!("⚠️  {name}: непонятный ответ — статус не трогаю")
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
