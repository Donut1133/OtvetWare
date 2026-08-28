//! theme.rs — оформление OtvetWare: строгий серый однотон.
//!
//! Палитра намеренно бесцветная: приложение показывает много состояний сразу
//! (статусы аккаунтов, строки лога, активные вкладки), и если каждое из них
//! красить в свой цвет, окно превращается в ёлку. Здесь всё различается
//! ЯРКОСТЬЮ на общем сером фоне — глазу спокойнее, а важное всё равно видно.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Stroke, Visuals};
use std::sync::Arc;

/// Оттенки серого от самого тёмного к самому светлому.
pub const BG: Color32 = Color32::from_gray(24); // фон окна
pub const BG_PANEL: Color32 = Color32::from_gray(32); // панели
pub const BG_INPUT: Color32 = Color32::from_gray(42); // поля и кнопки
pub const BG_HOVER: Color32 = Color32::from_gray(54);
pub const BG_ACTIVE: Color32 = Color32::from_gray(72); // нажатая/выбранная
pub const LINE: Color32 = Color32::from_gray(60); // границы и разделители

pub const FG: Color32 = Color32::from_gray(196); // основной текст
pub const FG_STRONG: Color32 = Color32::from_gray(235); // заголовки, важное
pub const FG_DIM: Color32 = Color32::from_gray(128); // подписи, второстепенное
pub const FG_FAINT: Color32 = Color32::from_gray(96); // совсем блёклое

// Смысловые роли — те же серые, различаются яркостью. Отдельные имена нужны,
// чтобы в коде было видно намерение, а не «from_gray(235)».
pub const BAD: Color32 = Color32::from_gray(150);
pub const WARN: Color32 = Color32::from_gray(170);

const SYSTEM_FONTS: &[(&str, &[&str], bool)] = &[
    // (имя, кандидаты-файлы, моноширинный)
    ("segoe", &["C:/Windows/Fonts/segoeui.ttf"], false),
    ("consola", &["C:/Windows/Fonts/consola.ttf"], true),
    ("seguisym", &["C:/Windows/Fonts/seguisym.ttf"], false),
];

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    for (name, paths, mono) in SYSTEM_FONTS {
        let Some(bytes) = paths.iter().find_map(|p| std::fs::read(p).ok()) else {
            continue;
        };
        fonts.font_data.insert((*name).to_string(), Arc::new(FontData::from_owned(bytes)));
        // Системный шрифт ставим ПЕРВЫМ, встроенный остаётся запасным: если в
        // системном не нашлось глифа, egui подставит его из следующего.
        let fam = if *mono { FontFamily::Monospace } else { FontFamily::Proportional };
        fonts.families.entry(fam).or_default().insert(0, (*name).to_string());
        if !*mono {
            fonts.families.entry(FontFamily::Monospace).or_default().push((*name).to_string());
        }
    }
    ctx.set_fonts(fonts);

    let mut v = Visuals::dark();
    v.override_text_color = Some(FG);
    v.panel_fill = BG;
    v.window_fill = BG_PANEL;
    v.window_stroke = Stroke::new(1.0, LINE);
    v.extreme_bg_color = Color32::from_gray(18);
    v.faint_bg_color = Color32::from_gray(28);
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.popup_shadow = egui::epaint::Shadow::NONE;

    v.widgets.noninteractive.bg_fill = BG_PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, FG_DIM);

    v.widgets.inactive.bg_fill = BG_INPUT;
    v.widgets.inactive.weak_bg_fill = BG_INPUT;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, FG);

    v.widgets.hovered.bg_fill = BG_HOVER;
    v.widgets.hovered.weak_bg_fill = BG_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_gray(88));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, FG_STRONG);

    v.widgets.active.bg_fill = BG_ACTIVE;
    v.widgets.active.weak_bg_fill = BG_ACTIVE;
    v.widgets.active.bg_stroke = Stroke::new(1.0, Color32::from_gray(110));
    v.widgets.active.fg_stroke = Stroke::new(1.0, FG_STRONG);

    v.widgets.open.bg_fill = BG_HOVER;
    v.widgets.open.weak_bg_fill = BG_HOVER;

    v.selection.bg_fill = Color32::from_gray(70);
    v.selection.stroke = Stroke::new(1.0, FG);
    v.hyperlink_color = FG_STRONG;

    // Скруглений почти нет: инструмент, а не карточки.
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(2);
    }

    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style.spacing.interact_size.y = 24.0;
    });
}

/// Яркость строки лога по её смыслу. Цвета нет — есть «громкость».
pub fn log_color(line: &str) -> Color32 {
    let t = line.trim_start();
    let head = t.chars().next().unwrap_or(' ');
    match head {
        // успех и итоги — самое яркое
        '✅' | '🎉' => FG_STRONG,
        // проблемы — светлее фона, но глуше успеха
        '❌' | '🛑' | '🔒' | '⛔' => BAD,
        '⚠' | '⏭' | '🔄' | '⏳' => WARN,
        // служебное: заголовки аккаунтов, разделители
        '👤' | '═' | '─' | '🚀' | '🔁' | '📊' => FG_DIM,
        _ => FG,
    }
}

/// Название приложения — одно место на всё окно.
pub const APP_NAME: &str = "OtvetWare";
