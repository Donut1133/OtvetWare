//! console.rs — консоль прогона.
//!
//! Рисуется виртуализованно (`show_rows`): в буфере до 20 000 строк, но за кадр
//! в разметку уходит только видимая пара десятков. Иначе интерфейс на длинном
//! прогоне начинал бы вязнуть ровно тогда, когда на него смотрят.

use crate::state::LogBuf;
use crate::theme;
use egui::{RichText, Ui};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub fn show(ui: &mut Ui, log: &Arc<LogBuf>, follow: &AtomicBool, id: &str) {
    let total = log.len();
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 1.0;

    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("строк: {total}")).color(theme::FG_DIM).small());
        ui.separator();
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Очистить").clicked() {
                log.clear();
            }
            if ui.button("Копировать").on_hover_text("Весь лог в буфер обмена").clicked()
            {
                ui.ctx().copy_text(log.all_text());
            }
            let mut f = follow.load(Ordering::Relaxed);
            if ui.checkbox(&mut f, "следить").changed() {
                follow.store(f, Ordering::Relaxed);
            }
        });
    });

    let frame = egui::Frame::default()
        .fill(theme::BG)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .corner_radius(egui::CornerRadius::same(4));
    frame.show(ui, |ui| {
        let mut area = egui::ScrollArea::vertical()
            .id_salt(id)
            .auto_shrink([false, false])
            .stick_to_bottom(follow.load(Ordering::Relaxed));
        if total == 0 {
            area = area.max_height(f32::INFINITY);
        }
        area.show_rows(ui, row_h, total, |ui, range| {
            ui.set_min_width(ui.available_width());
            log.with_range(range, |lines| {
                for line in lines {
                    if line.is_empty() {
                        ui.add_space(row_h * 0.4);
                        continue;
                    }
                    ui.label(RichText::new(line).monospace().color(theme::log_color(line)));
                }
            });
        });
    });
}
