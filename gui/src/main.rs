//! OtvetWare — нативный GUI (egui) для бота otvet.mail.ru.
//!
//! Рабочая папка = папка с данными (`accounts.json`, `personas.json`, журналы).
//! По умолчанию это папка `accounts` рядом с .exe: всё состояние лежит в одном
//! месте, программу можно перенести целиком вместе с аккаунтами. Если рядом
//! данных нет, ищем их так же, как раньше, — чтобы запуск из клона проекта и
//! общая папка данных продолжали работать.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod accounts;
mod app;
mod console;
mod forms;
mod forms_ai;
mod icons;
mod pool;
mod state;
mod theme;

use otvet_core::Core;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Портативная папка данных рядом с .exe.
const PORTABLE: &str = "accounts";

/// Что переносим в портативную папку, если данные лежали прямо рядом с .exe.
/// Список закрытый: всё остальное рядом с программой — не наше, трогать нельзя.
fn is_data_name(name: &str) -> bool {
    matches!(
        name,
        "accounts.json"
            | "personas.json"
            | "styles.json"
            | "styles.example.json"
            | "accounts.example.json"
            | "gif-pool.json"
            | "avatar-pool.json"
            | "convo.json"
            | "asked.json"
            | "profiles"
            | "images"
            | "browsers"
    ) || name.ends_with(".ndjson")
        || name.starts_with("accounts.json.")
}

/// Собрать данные, лежащие рядом с .exe, в портативную папку.
///
/// Переносим, а не копируем: две живые копии `accounts.json` — это разъезжающиеся
/// куки и разлогин на ровном месте. Если файл переехать не смог (занят, нет
/// прав), он остаётся на месте и мы просто идём дальше: потерять данные при
/// переезде страшнее, чем не переехать.
fn gather_into(from: &Path, to: &Path) {
    if std::fs::create_dir_all(to).is_err() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(from) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_data_name(name) {
            continue;
        }
        let dst = to.join(name);
        if dst.exists() {
            continue; // в папке уже есть своё — чужим не перетираем
        }
        let _ = std::fs::rename(e.path(), &dst);
    }
}

/// Где лежат данные: аккаунты, отпечатки, журналы, профили браузера.
///
/// Порядок поиска — от самого явного к самому общему:
///   1. переменная `OTVET_ROOT` (если указывает на существующую папку);
///   2. папка `accounts` рядом с .exe — портативная раскладка;
///   3. текущая папка, если в ней уже есть `accounts.json`;
///   4. папка выше по дереву от самого .exe с `accounts.json` — так работает
///      запуск из `target/release` внутри проекта;
///   5. `./data`, если такая папка есть — путь свежего клона с гитхаба;
///   6. новая папка `accounts` рядом с .exe.
fn pick_root(env: Option<PathBuf>, cwd: &Path, exe_dir: Option<&Path>) -> PathBuf {
    if let Some(p) = env {
        if p.is_dir() {
            return p;
        }
    }
    if let Some(dir) = exe_dir {
        let portable = dir.join(PORTABLE);
        // Данные лежат прямо рядом с .exe (так выглядит папка, скопированная от
        // старой версии) — сложим их в портативную папку.
        if !portable.exists() && dir.join("accounts.json").is_file() {
            gather_into(dir, &portable);
        }
        if portable.is_dir() {
            return portable;
        }
    }
    if cwd.join("accounts.json").exists() {
        return cwd.to_path_buf();
    }
    let mut up = exe_dir.map(|p| p.to_path_buf());
    while let Some(d) = up {
        if d.join("accounts.json").exists() {
            return d;
        }
        up = d.parent().map(|p| p.to_path_buf());
    }
    let data = cwd.join("data");
    if data.is_dir() {
        return data;
    }
    if let Some(dir) = exe_dir {
        let portable = dir.join(PORTABLE);
        if std::fs::create_dir_all(&portable).is_ok() {
            return portable;
        }
    }
    cwd.to_path_buf()
}

fn data_root() -> PathBuf {
    let env = std::env::var_os("OTVET_ROOT").map(PathBuf::from);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe = std::env::current_exe().ok();
    let exe_dir = exe.as_deref().and_then(|p| p.parent());
    pick_root(env, &cwd, exe_dir)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "{}").unwrap();
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("otv_root_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// Свежая установка: рядом с .exe нет ничего — заводим свою папку.
    #[test]
    fn fresh_install_gets_a_folder_next_to_the_exe() {
        let tmp = tmp_dir("fresh");
        let exe = tmp.join("bin");
        std::fs::create_dir_all(&exe).unwrap();

        let root = pick_root(None, &tmp, Some(&exe));
        assert_eq!(root, exe.join(PORTABLE));
        assert!(root.is_dir(), "папку надо и создать, а не только назвать");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Папка, скопированная от старой версии: .exe и данные вперемешку.
    /// Данные должны сами собраться в `accounts`, посторонние файлы — остаться.
    #[test]
    fn data_next_to_the_exe_moves_into_the_folder() {
        let tmp = tmp_dir("move");
        std::fs::create_dir_all(&tmp).unwrap();
        touch(&tmp.join("accounts.json"));
        touch(&tmp.join("personas.json"));
        touch(&tmp.join("answered_acc.ndjson"));
        touch(&tmp.join("profiles/acc/state"));
        touch(&tmp.join("otvetware.exe"));
        touch(&tmp.join("readme.txt"));

        let root = pick_root(None, &tmp, Some(&tmp));
        assert_eq!(root, tmp.join(PORTABLE));
        for name in ["accounts.json", "personas.json", "answered_acc.ndjson", "profiles"] {
            assert!(root.join(name).exists(), "{name} не переехал");
            assert!(!tmp.join(name).exists(), "{name} остался снаружи");
        }
        assert!(tmp.join("otvetware.exe").exists(), "программу трогать нельзя");
        assert!(tmp.join("readme.txt").exists(), "чужие файлы трогать нельзя");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Запуск из `target/release` внутри проекта: данные лежат выше по дереву,
    /// и переезжать они никуда не должны.
    #[test]
    fn project_layout_still_finds_the_root_above() {
        let tmp = tmp_dir("up");
        let exe = tmp.join("rust/target/release");
        std::fs::create_dir_all(&exe).unwrap();
        touch(&tmp.join("accounts.json"));

        let root = pick_root(None, &tmp.join("elsewhere"), Some(&exe));
        assert_eq!(root, tmp);
        assert!(tmp.join("accounts.json").exists());
        assert!(!exe.join(PORTABLE).exists(), "лишней папки рядом с .exe быть не должно");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Явный OTVET_ROOT сильнее всего остального.
    #[test]
    fn env_wins() {
        let tmp = tmp_dir("env");
        let exe = tmp.join("bin");
        let want = tmp.join("data");
        std::fs::create_dir_all(&exe).unwrap();
        std::fs::create_dir_all(&want).unwrap();
        std::fs::create_dir_all(exe.join(PORTABLE)).unwrap();

        assert_eq!(pick_root(Some(want.clone()), &tmp, Some(&exe)), want);
        // Несуществующий путь в переменной не должен уводить в никуда.
        assert_eq!(pick_root(Some(tmp.join("нет")), &tmp, Some(&exe)), exe.join(PORTABLE));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
