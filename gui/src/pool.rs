//! pool.rs — окно пула картинок.
//!
//! Пул (`gif-pool.json`) — это список хэшей, которые УЖЕ лежат на CDN сайта.
//! Такую картинку можно приложить к любому посту без заливки, поэтому пул
//! наполняется один раз, а дальше только используется.
//!
//! Наполнить его можно двумя путями: залить свои файлы (тогда картинка сначала
//! уходит на сайт под конкретным аккаунтом — это обычный пользовательский
//! запрос, поэтому аккаунт нужен живой) или вставить готовую ссылку с CDN,
//! если хэш уже известен.

use crate::state::{Bg, LogBuf};
use crate::theme;
use egui::{RichText, Ui};
use otvet_core::accounts::Account;
use otvet_core::api;
use otvet_core::journals::{self, PoolImage};
use otvet_core::util::Stop;
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Default)]
pub struct PoolWindow {
    open: bool,
    /// Аккаунт, от имени которого заливаем.
    account: Option<Account>,
    accounts: Vec<Account>,
    items: Vec<PoolImage>,
    paste: String,
    tag: String,
    busy: Arc<Mutex<Option<String>>>,
    /// Заливка шла на прошлом кадре: по её окончанию список надо перечитать.
    was_busy: bool,
    pub log: Arc<LogBuf>,
}

impl PoolWindow {
    pub fn open(&mut self, bg: &Arc<Bg>, accounts: &[Account]) {
        self.open = true;
        self.accounts = accounts.to_vec();
        if self.account.as_ref().map(|a| !accounts.iter().any(|x| x.name == a.name)).unwrap_or(true) {
            self.account = accounts.first().cloned();
        }
        self.reload(bg);
    }

    fn reload(&mut self, bg: &Arc<Bg>) {
        self.items = journals::load_gif_pool(&bg.core.root);
    }

    /// `chosen` — хэши, отмеченные к использованию в текущем режиме. Окно их
    /// же и правит: выбор картинок принадлежит режиму, а не окну, поэтому у
    /// ответов и у вопросов он свой.
    pub fn ui(&mut self, ctx: &egui::Context, bg: &Arc<Bg>, chosen: &mut Vec<String>) {
        if !self.open {
            return;
        }
        let mut open = self.open;
        let mut reload = false;
        egui::Window::new("Пул картинок")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([620.0, 460.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                reload = self.body(ui, bg, chosen);
            });
        self.open = open;
        if reload {
            self.reload(bg);
        }
    }

    fn body(&mut self, ui: &mut Ui, bg: &Arc<Bg>, chosen: &mut Vec<String>) -> bool {
        let mut reload = false;
        ui.label(
            RichText::new("Картинки из пула прикладываются к постам без заливки — они уже на CDN сайта.")
                .color(theme::FG_DIM)
                .size(11.5),
        );
        ui.add_space(6.0);

        // Кем заливаем.
        ui.horizontal(|ui| {
            ui.label("Заливать от имени");
            let cur = self.account.as_ref().map(|a| a.name.clone()).unwrap_or_else(|| "—".into());
            egui::ComboBox::from_id_salt("pool_acc").selected_text(cur).width(220.0).show_ui(ui, |ui| {
                for a in &self.accounts {
                    let on = self.account.as_ref().map(|x| x.name == a.name).unwrap_or(false);
                    if ui.selectable_label(on, &a.name).clicked() {
                        self.account = Some(a.clone());
                    }
                }
            });
            if self.accounts.is_empty() {
                ui.colored_label(theme::WARN, "нет отмеченных аккаунтов");
            }
        });

        let busy = self.busy.lock().clone();
        // Заливка закончилась — показываем свежий пул, не дожидаясь действий.
        if self.was_busy && busy.is_none() {
            reload = true;
        }
        self.was_busy = busy.is_some();
        ui.horizontal(|ui| {
            let can = self.account.is_some() && busy.is_none();
            if ui.add_enabled(can, egui::Button::new("Загрузить файлы…")).clicked() {
                if let Some(files) = rfd::FileDialog::new()
                    .add_filter("Картинки", &["jpg", "jpeg", "png", "gif", "webp"])
                    .set_title("Выбери картинки для пула")
                    .pick_files()
                {
                    self.upload(bg, files);
                }
            }
            if ui.add_enabled(can, egui::Button::new("Загрузить папку…")).clicked() {
                if let Some(dir) = rfd::FileDialog::new().set_title("Папка с картинками").pick_folder()
                {
                    let files: Vec<std::path::PathBuf> = otvet_core::answerer::list_images(&dir)
                        .into_iter()
                        .map(std::path::PathBuf::from)
                        .collect();
                    if files.is_empty() {
                        self.log.push("[!] В папке нет картинок");
                    } else {
                        self.upload(bg, files);
                    }
                }
            }
            if let Some(msg) = &busy {
                ui.label(RichText::new(msg).color(theme::FG_STRONG));
            }
        });

        // Готовый хэш или ссылка с CDN.
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.paste)
                    .desired_width(ui.available_width() - 190.0)
                    .hint_text("ссылка с сайта или хэш картинки"),
            );
            ui.add(egui::TextEdit::singleline(&mut self.tag).desired_width(90.0).hint_text("подпись"));
            if ui.button("Добавить").clicked() {
                let raw = self.paste.trim().to_string();
                let hash = api::extract_cdn_hash(&raw).unwrap_or_else(|| {
                    raw.trim_end_matches(".jpg").trim_end_matches(".gif").trim().to_string()
                });
                if hash.len() < 8 {
                    self.log.push("[-] Не похоже на ссылку или хэш картинки");
                } else {
                    // Читаем файл заново: пул могла пополнить фоновая заливка,
                    // и запись «поверх» показанного списка её бы потеряла.
                    let mut items = journals::load_gif_pool(&bg.core.root);
                    if items.iter().any(|i| i.hash == hash) {
                        self.log.push("[!] Такая картинка в пуле уже есть");
                    } else {
                        items.push(PoolImage { hash, width: 0, height: 0, tag: self.tag.trim().to_string() });
                        save(bg, &items, &self.log);
                        self.paste.clear();
                    }
                    reload = true;
                }
            }
        });
        crate::forms::hint(
            ui,
            "Ссылку можно взять со страницы сайта: правой кнопкой по картинке → «Копировать адрес». Подпись — необязательная пометка для себя, на сайт она не уходит.",
        );

        ui.add_space(6.0);
        ui.separator();
        // Отметки на картинки, которых в пуле уже нет, только путают счётчик.
        // Чистим их, но не когда пул пуст: пустой файл — это ещё и «не
        // прочитался», и стирать по нему чужой выбор нельзя.
        if !self.items.is_empty() {
            chosen.retain(|h| self.items.iter().any(|i| &i.hash == h));
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("В пуле: {}", self.items.len())).color(theme::FG_STRONG));
            if chosen.is_empty() {
                ui.label(RichText::new("прикладывается любая").color(theme::FG_DIM));
            } else {
                ui.label(
                    RichText::new(format!("прикладываются только отмеченные: {}", chosen.len()))
                        .color(theme::FG_STRONG),
                );
                if ui.small_button("снять отметки").clicked() {
                    chosen.clear();
                }
            }
        });

        egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
            if self.items.is_empty() {
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Пул пуст").color(theme::FG_DIM));
                    ui.label(
                        RichText::new("Загрузи файлы — они уедут на сайт и станут доступны всем аккаунтам.")
                            .color(theme::FG_FAINT)
                            .size(11.0),
                    );
                });
                return;
            }
            for it in self.items.clone() {
                ui.horizontal(|ui| {
                    let mut on = chosen.contains(&it.hash);
                    if ui.checkbox(&mut on, "").on_hover_text("прикладывать эту").changed() {
                        if on {
                            chosen.push(it.hash.clone());
                        } else {
                            chosen.retain(|h| h != &it.hash);
                        }
                    }
                    ui.label(
                        RichText::new(crate::forms::clip_middle(&it.hash, 34)).monospace().color(theme::FG),
                    )
                    .on_hover_text(&it.hash);
                    if it.width > 0 {
                        ui.label(
                            RichText::new(format!("{}×{}", it.width, it.height))
                                .color(theme::FG_FAINT)
                                .small(),
                        );
                    }
                    if !it.tag.is_empty() {
                        ui.label(RichText::new(&it.tag).color(theme::FG_DIM).small());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("убрать").on_hover_text("удалить из пула").clicked()
                        {
                            // Читаем файл заново, а не правим показанный список:
                            // пул могла пополнить фоновая заливка.
                            let mut items = journals::load_gif_pool(&bg.core.root);
                            items.retain(|i| i.hash != it.hash);
                            save(bg, &items, &self.log);
                            self.log.push("[+] Картинка убрана из пула");
                            reload = true;
                        }
                        if ui.small_button("копировать хэш").clicked() {
                            ui.ctx().copy_text(it.hash.clone());
                        }
                    });
                });
            }
        });
        reload
    }

    /// Заливка файлов на сайт с добавлением в пул.
    fn upload(&mut self, bg: &Arc<Bg>, files: Vec<std::path::PathBuf>) {
        let Some(acc) = self.account.clone() else { return };
        let core = bg.core.clone();
        let log = self.log.clone();
        let busy = self.busy.clone();
        let tag = self.tag.trim().to_string();
        *busy.lock() = Some(format!("заливаю 0/{}", files.len()));
        log.push(&format!("[>] Заливаю {} картинок от «{}»", files.len(), acc.name));
        bg.spawn(async move {
            let stop = Stop::new();
            let total = files.len();
            let mut added = 0usize;
            for (i, f) in files.iter().enumerate() {
                *busy.lock() = Some(format!("заливаю {}/{total}", i + 1));
                match core.http.upload_picture(&acc, f, &stop).await {
                    Ok(up) => {
                        // Без хэша запись в пул бессмысленна: из неё соберётся
                        // битая ссылка на картинку.
                        let Some(hash) = api::extract_cdn_hash(&up.url) else {
                            log.push(&format!("[-] Непонятный ответ заливки: {}", up.url));
                            continue;
                        };
                        // Читаем пул заново на каждый файл: заливка долгая, а
                        // файл мог поменяться и снаружи.
                        let mut items = journals::load_gif_pool(&core.root);
                        if !items.iter().any(|x| x.hash == hash) {
                            items.push(PoolImage {
                                hash: hash.clone(),
                                width: up.width,
                                height: up.height,
                                tag: tag.clone(),
                            });
                            let _ = journals::save_gif_pool(&core.root, &items);
                            added += 1;
                        }
                        log.push(&format!(
                            "[+] {} → {}",
                            f.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                            crate::forms::clip_middle(&hash, 24)
                        ));
                    }
                    Err(e) => log.push(&format!(
                        "[-] {}: {e}",
                        f.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
                    )),
                }
            }
            *busy.lock() = None;
            log.push(&format!("[+] Готово: в пул добавлено {added} из {total}"));
        });
    }
}

fn save(bg: &Arc<Bg>, items: &[PoolImage], log: &Arc<LogBuf>) {
    match journals::save_gif_pool(&bg.core.root, items) {
        Ok(()) => log.push(&format!("[+] Пул сохранён: {} картинок", items.len())),
        Err(e) => log.push(&format!("[-] Не записать gif-pool.json: {e}")),
    }
}
