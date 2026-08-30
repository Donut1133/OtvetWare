//! icons.rs — векторные значки, нарисованные кистью egui.
//!
//! Эмодзи из интерфейса убраны намеренно: они цветные и растровые, в сером
//! однотонном окне выглядят наклейками на чертеже, а их вид зависит от шрифта
//! системы. Здесь вместо них несколько простых фигур — те же линии и та же
//! палитра, что у остального окна, любой размер без замыливания.

use egui::{Align2, Color32, FontId, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2};

/// Что рисуем.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// Галочка: получилось.
    Ok,
    /// Крест: не получилось.
    Bad,
    /// Треугольник с восклицанием: внимание.
    Warn,
    /// Квадрат: стоп.
    Stop,
    /// Стрелка вправо: шаг работы.
    Step,
    /// Полоска: заголовок, итог.
    Head,
}

impl Icon {
    /// Значок по маркеру строки лога (`[+]`, `[!]`, …).
    pub fn from_mark(mark: char) -> Icon {
        match mark {
            '+' => Icon::Ok,
            '-' => Icon::Bad,
            '!' => Icon::Warn,
            'x' => Icon::Stop,
            '=' => Icon::Head,
            _ => Icon::Step,
        }
    }
}

/// Нарисовать значок в заданном квадрате.
pub fn paint(ui: &Ui, rect: Rect, icon: Icon, color: Color32) {
    let p = ui.painter();
    let c = rect.center();
    let r = rect.width().min(rect.height()) * 0.5;
    // Толщина линии тянется за размером: значок 10px и значок 24px должны
    // выглядеть одним и тем же значком, а не разными.
    let w = (r * 0.32).clamp(1.0, 2.4);
    let stroke = Stroke::new(w, color);
    match icon {
        Icon::Ok => {
            // Галочка: две линии, длинная под 45°.
            p.line_segment([c + Vec2::new(-r * 0.62, 0.0), c + Vec2::new(-r * 0.16, r * 0.5)], stroke);
            p.line_segment([c + Vec2::new(-r * 0.16, r * 0.5), c + Vec2::new(r * 0.66, -r * 0.55)], stroke);
        }
        Icon::Bad => {
            let d = r * 0.55;
            p.line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], stroke);
            p.line_segment([c + Vec2::new(d, -d), c + Vec2::new(-d, d)], stroke);
        }
        Icon::Warn => {
            // Треугольник; восклицательный знак рисуем только если есть куда.
            let top = c + Vec2::new(0.0, -r * 0.75);
            let left = c + Vec2::new(-r * 0.8, r * 0.6);
            let right = c + Vec2::new(r * 0.8, r * 0.6);
            p.line_segment([top, left], stroke);
            p.line_segment([left, right], stroke);
            p.line_segment([right, top], stroke);
            if r >= 6.0 {
                p.line_segment(
                    [c + Vec2::new(0.0, -r * 0.28), c + Vec2::new(0.0, r * 0.12)],
                    Stroke::new(w * 0.9, color),
                );
                p.circle_filled(c + Vec2::new(0.0, r * 0.36), w * 0.55, color);
            }
        }
        Icon::Stop => {
            let d = r * 0.62;
            p.rect_stroke(
                Rect::from_center_size(c, Vec2::splat(d * 2.0)),
                egui::CornerRadius::same(1),
                stroke,
                egui::StrokeKind::Middle,
            );
            p.line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], stroke);
        }
        Icon::Step => {
            // Стрелка вправо.
            p.line_segment([c + Vec2::new(-r * 0.7, 0.0), c + Vec2::new(r * 0.55, 0.0)], stroke);
            p.line_segment([c + Vec2::new(r * 0.15, -r * 0.42), c + Vec2::new(r * 0.6, 0.0)], stroke);
            p.line_segment([c + Vec2::new(r * 0.15, r * 0.42), c + Vec2::new(r * 0.6, 0.0)], stroke);
        }
        Icon::Head => {
            p.line_segment([c + Vec2::new(-r * 0.75, 0.0), c + Vec2::new(r * 0.75, 0.0)], stroke);
        }
    }
}

/// Кнопка со значком вместо подписи.
pub fn button(ui: &mut Ui, icon: Icon, tip: &str) -> Response {
    let side = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
    if ui.is_rect_visible(rect) {
        let vis = ui.style().interact(&resp);
        ui.painter().rect_filled(rect, egui::CornerRadius::same(2), vis.weak_bg_fill);
        paint(ui, rect.shrink(side * 0.28), icon, vis.fg_stroke.color);
    }
    resp.on_hover_text(tip)
}

/// Маленькая цифра/значок в углу — для счётчиков на вкладках.
/// Счётчик в правом верхнем углу кнопки: мелко и блёкло, чтобы не спорить с
/// подписью.
pub fn corner_text(ui: &Ui, rect: Rect, text: &str, color: Color32) {
    ui.painter().text(
        rect.right_top() + Vec2::new(-2.0, 2.0),
        Align2::RIGHT_TOP,
        text,
        FontId::proportional(9.0),
        color,
    );
}

/// Полоска «идёт работа» у нижнего края кнопки: короткий отрезок неспешно
/// ездит из края в край.
///
/// Значок для этого не годился: подпись на вкладке по центру, и кольцо
/// приходилось ставить сбоку — получалась не кнопка, а кнопка с прилипшей
/// точкой. Полоска живёт на своей полосе, ничего не сдвигает, и движение
/// ловится боковым зрением, даже когда смотришь в лог.
pub fn busy_bar(ui: &Ui, rect: Rect, phase: f64, track: Color32, color: Color32) {
    let y = rect.bottom() - 3.0;
    let (x0, x1) = track_span(rect);
    let (from, to) = busy_span(rect, phase);
    let p = ui.painter();
    p.line_segment([Pos2::new(x0, y), Pos2::new(x1, y)], Stroke::new(2.0, track));
    p.line_segment([Pos2::new(from, y), Pos2::new(to, y)], Stroke::new(2.0, color));
}

/// Дорожка полоски: от края кнопки отступаем, чтобы не лезть в скругления.
fn track_span(rect: Rect) -> (f32, f32) {
    const PAD: f32 = 6.0;
    (rect.left() + PAD, rect.right() - PAD)
}

/// Где сейчас едущий отрезок.
///
/// Косинус вместо пилы: на разворотах отрезок замедляется, и движение читается
/// как дыхание, а не как дёрганье. Полный путь туда-обратно — около трёх секунд.
fn busy_span(rect: Rect, phase: f64) -> (f32, f32) {
    let (x0, x1) = track_span(rect);
    let seg = (x1 - x0) * 0.34;
    let k = ((1.0 - (phase * 2.2).cos()) * 0.5) as f32;
    let from = x0 + (x1 - x0 - seg) * k;
    (from, from + seg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Полоска обязана оставаться внутри кнопки при любой фазе — иначе на
    /// вкладке появится отрезок, вылезающий за рамку.
    #[test]
    fn busy_bar_stays_inside_the_button() {
        let rect = Rect::from_min_size(Pos2::new(10.0, 40.0), Vec2::new(122.0, 28.0));
        let (t0, t1) = track_span(rect);
        let mut seen_left = false;
        let mut seen_right = false;
        let mut width = None;
        for step in 0..400 {
            let (from, to) = busy_span(rect, step as f64 * 0.05);
            assert!(from >= t0 - 0.01, "уехал левее дорожки: {from} < {t0}");
            assert!(to <= t1 + 0.01, "уехал правее дорожки: {to} > {t1}");
            let w = to - from;
            match width {
                None => width = Some(w),
                Some(prev) => assert!((prev - w).abs() < 0.01, "отрезок меняет длину: {prev} → {w}"),
            }
            if from - t0 < 1.0 {
                seen_left = true;
            }
            if t1 - to < 1.0 {
                seen_right = true;
            }
        }
        assert!(seen_left && seen_right, "отрезок должен доезжать до обоих краёв");
    }
}
