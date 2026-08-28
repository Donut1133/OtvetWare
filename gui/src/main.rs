//! OtvetWare — нативный GUI (egui) для бота otvet.mail.ru.
//!
//! Рабочая папка = папка с данными (`accounts.json`, `personas.json`, журналы).
//! По умолчанию берётся директория, где лежит .exe, а если данных там нет —
//! текущая рабочая директория. Так одно и то же приложение одинаково запускается
//! и двойным кликом из папки проекта, и из консоли.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod accounts;
mod app;
mod console;
mod forms;
mod forms_ai;
mod state;
mod theme;

use otvet_core::Core;
use std::path::PathBuf;
use std::sync::Arc;

/// Где лежат данные: аккаунты, персоны, журналы, стили.
///
/// Порядок поиска — от самого явного к самому общему:
///   1. переменная `OTVET_ROOT` (если указывает на существующую папку);
///   2. текущая папка, если в ней уже есть `accounts.json`;
///   3. папка выше по дереву от самого .exe с `accounts.json` — так работает
///      запуск из `target/release` внутри проекта;
///   4. `./data`, если такая папка есть — путь свежего клона с гитхаба;
///   5. текущая папка.
fn data_root() -> PathBuf {
    if let Ok(p) = std::env::var("OTVET_ROOT") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return p;
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join("accounts.json").exists() {
        return cwd;
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        while let Some(d) = dir {
            if d.join("accounts.json").exists() {
                return d;
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    let data = cwd.join("data");
    if data.is_dir() {
        return data;
    }
    cwd
}

fn main() -> eframe::Result<()> {
    let root = data_root();
    let core = Core::open(&root);
    let bg = state::Bg::new(core);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Меньше типичного экрана: окно во весь 1080p вылезает под панель
            // задач, и кнопки запуска оказываются за краем.
            .with_inner_size([1360.0, 820.0])
            .with_min_inner_size([1000.0, 620.0])
            .with_title(format!("{} — {}", theme::APP_NAME, root.display())),
        ..Default::default()
    };

    eframe::run_native(
        "otvetware",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, Arc::clone(&bg))))),
    )
}
