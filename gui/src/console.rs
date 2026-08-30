//! console.rs — консоль прогона.
//!
//! Рисуется виртуализованно (`show_rows`): в буфере до 20 000 строк, но за кадр
//! в разметку уходит только видимая пара десятков. Иначе интерфейс на длинном
//! прогоне начинал бы вязнуть ровно тогда, когда на него смотрят.
//!
//! Строка разложена по колонкам — время, значок, текст, — потому что читают её
//! не подряд, а глазами по вертикали: «где тут ошибки». Значок берётся из
//! маркера, который ставит ядро (`[+]`, `[!]`, …), и рисуется кистью: эмодзи в
//! сером окне выглядели наклейками и зависели от шрифта системы.

use crate::icons::{self, Icon};
use crate::state::{hhmmss, ConsoleView, LogBuf};
use crate::theme;
use egui::{RichText, Ui};
use otvet_core::util::split_mark;
use parking_lot::Mutex;
use std::sync::Arc;

pub fn show(ui: &mut Ui, log: &Arc<LogBuf>, view: &Mutex<ConsoleView>, id: &str) {
    let mut v = view.lock();
    let total = log.len();

    // Список строк под фильтр пересобираем, только если лог или фильтр менялись.
    // Без фильтра список не нужен вовсе — строки идут подряд.
    let filtered = !v.filter.trim().is_empty();
    let version = log.version();
    if filtered && (v.cache.0 != version || v.cache.1 != v.filter) {
        let idx = log.matching(v.filter.trim());
        v.cache = (version, v.filter.clone(), idx);
    }
    let rows = if filtered { v.cache.2.len() } else { total };

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(if v.filter.trim().is_empty() {
                format!("строк: {total}")
            } else {
                format!("найдено {rows} из {total}")
            })
            .color(theme::FG_DIM)
            .small(),
        );
        ui.add(
            egui::TextEdit::singleline(&mut v.filter)
                .desired_width(160.0)
                .hint_text("фильтр по тексту")
                .font(egui::TextStyle::Small),
        );
        if !v.filter.is_empty() && icons::button(ui, Icon::Bad, "Сбросить фильтр").clicked() {
            v.filter.clear();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Очистить").clicked() {
                log.clear();
            }
            if ui.small_button("Копировать").on_hover_text("Весь лог в буфер обмена").clicked()
            {
                ui.ctx().copy_text(log.all_text());
            }
            ui.checkbox(&mut v.show_time, "время").on_hover_text("Показывать время каждой строки");
            let was = v.follow;
            ui.checkbox(&mut v.follow, "вниз").on_hover_text(
                "Автопрокрутка. Снимается сама, если увести список вверх, и возвращается у нижнего края",
            );
            if !was && v.follow {
                // Ручное включение — сразу к последней строке.
                ui.ctx().request_repaint();
            }
        });
    });

    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 2.0;
    let time_w = if v.show_time { 54.0 } else { 0.0 };
    let icon_w = 14.0;

    let frame = egui::Frame::default()
        .fill(theme::BG)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .corner_radius(egui::CornerRadius::same(4));
    frame.show(ui, |ui| {
        if rows == 0 {
            ui.allocate_space(egui::vec2(ui.available_width(), 0.0));
            ui.label(
                RichText::new(if total == 0 {
                    "Здесь будет видно, что делает бот."
                } else {
                    "Под фильтр ничего не подошло."
                })
                .color(theme::FG_FAINT)
                .small(),
            );
            return;
        }
        // Плавный доезд вместо телепорта: за кадр сдвигаемся к нижнему краю на
        // часть оставшегося пути. Быстро, пока далеко, и мягко у самого низа —
        // так видно, что список поехал, а не подменился.
        let dt = ui.input(|i| i.stable_dt).clamp(1.0 / 240.0, 1.0 / 20.0);
        let mut area = egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]);
        if v.follow {
            let next = glide(v.glide_to, v.max_offset, dt);
            if (next - v.max_offset).abs() > 0.5 {
                // Ещё едем — просим следующий кадр, иначе замрём на полпути.
                ui.ctx().request_repaint();
            }
            v.glide_to = next;
            area = area.vertical_scroll_offset(next);
        }
        let out = area.show_rows(ui, row_h, rows, |ui, range| {
            ui.set_min_width(ui.available_width());
            // Виртуализация считает, что строка ровно `row_h` высотой:
            // вертикальный зазор между строками сдвинул бы список.
            ui.spacing_mut().item_spacing.y = 0.0;
            let draw = |line: &crate::state::LogLine| {
                let (mark, text) = split_mark(&line.text);
                if text.is_empty() && mark.is_none() {
                    ui.add_space(row_h * 0.35);
                    return;
                }
                let color = theme::log_color(mark);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if v.show_time {
                        ui.add_sized(
                            [time_w, row_h],
                            egui::Label::new(
                                RichText::new(hhmmss(line.secs)).monospace().small().color(theme::FG_FAINT),
                            ),
                        );
                    }
                    let (r, _) = ui.allocate_exact_size(egui::vec2(icon_w, row_h), egui::Sense::hover());
                    if let Some(m) = mark {
                        icons::paint(ui, r.shrink(3.0), Icon::from_mark(m), color);
                    }
                    // Отступ вложенных шагов сохраняем: по нему видно, что
                    // строка относится к предыдущей, а не идёт сама по себе.
                    let indent = line.text.len() - line.text.trim_start_matches(' ').len();
                    if indent > 0 {
                        ui.add_space(10.0);
                    }
                    let resp = ui.add(
                        egui::Label::new(RichText::new(text).monospace().color(color))
                            .truncate()
                            .selectable(true),
                    );
                    // Подсказка — только если строка не поместилась. Раньше она
                    // всплывала над любой строкой и просто дублировала её.
                    if clipped(&resp) {
                        resp.on_hover_text(text);
                    }
                });
            };
            if filtered {
                let visible: Vec<usize> = v.cache.2[range].to_vec();
                log.with_indices(&visible, draw);
            } else {
                log.with_range(range, draw);
            }
        });

        // Автопрокрутка не должна драться с человеком: увёл список вверх —
        // слежение снимается, вернулся к низу — включается обратно.
        //
        // Смотрим именно на УМЕНЬШЕНИЕ прокрутки, а не просто на расстояние до
        // низа: лог прирастает пачками, и по одному расстоянию слежение
        // снималось бы само, без всякого участия человека.
        let off = out.state.offset.y;
        let max_off = (out.content_size.y - out.inner_rect.height()).max(0.0);
        let from_bottom = max_off - off;
        if off + 1.0 < v.last_offset && from_bottom > row_h {
            v.follow = false;
        } else if from_bottom <= 1.0 {
            v.follow = true;
        }
        v.last_offset = off;
        v.max_offset = max_off;
        // Едем всегда от того места, где список оказался на самом деле: колесо
        // мыши в тот же кадр сдвигает его поверх нашего значения.
        v.glide_to = off;
    });
}

/// Строка не поместилась целиком — значит, есть что показать подсказкой.
/// `intrinsic_size` у обрезанной подписи — это её полная, необрезанная ширина.
fn clipped(resp: &egui::Response) -> bool {
    resp.intrinsic_size().is_some_and(|s| s.x > resp.rect.width() + 1.0)
}

/// Шаг плавной прокрутки: сколько от `from` до `to` проходим за кадр `dt`.
///
/// Экспоненциальное приближение — быстро, пока далеко, и мягко у цели. Ползти
/// последний пиксель бесконечно нельзя, поэтому у шага есть нижний порог: с ним
/// доезд всегда конечный.
fn glide(from: f32, to: f32, dt: f32) -> f32 {
    /// Насколько быстро подтягиваемся, 1/сек. 14 — примерно четверть секунды до
    /// цели: заметно глазу, но не «жду, пока доедет».
    const SETTLE: f32 = 14.0;
    let diff = to - from;
    if diff.abs() <= 0.5 {
        return to;
    }
    let step = diff * (1.0 - (-dt * SETTLE).exp());
    let floor = diff.abs().min(2.0);
    from + if step.abs() < floor { diff.signum() * floor } else { step }
}

#[cfg(test)]
mod tests {
    use super::{clipped, glide};
    use egui::{Label, RichText};

    /// Ширина подписи в узком поле: помещается или нет.
    fn is_clipped(text: &str, width: f32) -> bool {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut out = false;
        // Два кадра: на первом egui ещё не знает размеров шрифта.
        for _ in 0..2 {
            let _ = ctx.run_ui(Default::default(), |ui| {
                let rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, 100.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                let resp = child.add(Label::new(RichText::new(text).monospace()).truncate());
                out = clipped(&resp);
            });
        }
        out
    }

    /// Подсказка над строкой лога должна всплывать только тогда, когда строка
    /// обрезана. Иначе она просто дублирует то, что и так видно, — и мешает.
    #[test]
    fn tooltip_only_for_lines_that_do_not_fit() {
        assert!(!is_clipped("короткая строка", 400.0), "поместилась, а подсказка есть");
        assert!(is_clipped(&"очень длинная строка ".repeat(20), 200.0), "обрезана, а подсказки нет");
    }

    #[test]
    fn glide_moves_toward_the_target_and_stops_there() {
        let dt = 1.0 / 60.0;
        let mut at = 0.0;
        let mut frames = 0;
        while at < 1000.0 && frames < 600 {
            let next = glide(at, 1000.0, dt);
            assert!(next > at, "прокрутка должна двигаться: {at} → {next}");
            assert!(next <= 1000.0, "мимо цели: {next}");
            at = next;
            frames += 1;
        }
        assert_eq!(at, 1000.0, "не доехали за {frames} кадров");
        assert!(frames < 120, "доезд не должен занимать больше двух секунд: {frames} кадров");
        // Первый кадр — заметный кусок пути, последний — мелкий шаг.
        assert!(glide(0.0, 1000.0, dt) > 100.0, "начало доезда слишком вялое");
        assert!(glide(999.0, 1000.0, dt) - 999.0 <= 1.0, "у цели шаг должен быть мелким");
    }

    #[test]
    fn glide_works_upward_too() {
        let mut at = 500.0;
        for _ in 0..200 {
            at = glide(at, 0.0, 1.0 / 60.0);
        }
        assert_eq!(at, 0.0);
    }
}
