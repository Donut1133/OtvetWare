//! persona.rs — порт fingerprint.js: детерминированные антидетект-«персоны».
//!
//! Ключевое требование — СОВМЕСТИМОСТЬ с JS-версией: тот же seed обязан давать
//! ту же персону, иначе аккаунт, залогиненный старым приложением, начнёт ходить
//! по API с другим «железом» под теми же куками. Поэтому PRNG (FNV-1a +
//! mulberry32) и ПОРЯДОК обращений к нему повторены один в один.
//!
//! personas.json читается и пишется в том же формате — оба приложения работают
//! с одним файлом.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const FALLBACK_VERSION: &str = "148.0.7778.96";

// ─── PRNG ───────────────────────────────────────────────────────────────────

/// FNV-1a 32 бита. Строка обходится по UTF-16 code units (как charCodeAt в JS),
/// а не по байтам UTF-8 — иначе кириллическое имя аккаунта дало бы другой seed.
pub fn hash32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for u in s.encode_utf16() {
        h ^= u as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

pub struct Mulberry32(u32);

impl Mulberry32 {
    pub fn new(seed: u32) -> Self {
        Self(seed)
    }
    #[allow(clippy::should_implement_trait)] // это ГПСЧ, а не итератор: имя из JS-оригинала
    pub fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6D2B_79F5);
        let a = self.0;
        let mut t = (a ^ (a >> 15)).wrapping_mul(1 | a);
        t = (t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t))) ^ t;
        (t ^ (t >> 14)) as f64 / 4_294_967_296.0
    }
    fn int(&mut self, a: i64, b: i64) -> i64 {
        a + (self.next() * (b - a + 1) as f64).floor() as i64
    }
    /// Взвешенный выбор: пары (значение, вес).
    fn pick_w<'a, T>(&mut self, items: &'a [(T, f64)]) -> &'a T {
        let total: f64 = items.iter().map(|x| x.1).sum();
        let mut x = self.next() * total;
        for it in items {
            x -= it.1;
            if x <= 0.0 {
                return &it.0;
            }
        }
        &items[items.len() - 1].0
    }
}

// ─── Таблицы по ОС ──────────────────────────────────────────────────────────

struct Screen {
    w: i64,
    h: i64,
    dpr: f64,
}

struct OsTable {
    platform: &'static str,
    ua_platform: &'static str,
    ch_platform: &'static str,
    platform_versions: Vec<(&'static str, f64)>,
    architecture: &'static str,
    bitness: &'static str,
    wow64: bool,
    taskbar: i64,
    screens: Vec<(Screen, f64)>,
    gpus: Vec<((&'static str, &'static str), f64)>,
}

fn sc(w: i64, h: i64, dpr: f64) -> Screen {
    Screen { w, h, dpr }
}

fn os_table(key: &str) -> OsTable {
    match key {
        "mac" => OsTable {
            platform: "MacIntel",
            ua_platform: "Macintosh; Intel Mac OS X 10_15_7",
            ch_platform: "macOS",
            platform_versions: vec![("14.5.0", 3.0), ("15.3.0", 4.0), ("15.6.0", 3.0)],
            architecture: "arm",
            bitness: "64",
            wow64: false,
            taskbar: 25,
            screens: vec![
                (sc(1512, 982, 2.0), 25.0),
                (sc(1440, 900, 2.0), 22.0),
                (sc(1728, 1117, 2.0), 15.0),
                (sc(1280, 800, 2.0), 12.0),
                (sc(1920, 1080, 1.0), 14.0),
                (sc(2560, 1440, 1.0), 12.0),
            ],
            gpus: vec![
                (("Google Inc. (Apple)", "ANGLE (Apple, ANGLE Metal Renderer: Apple M1, Unspecified Version)"), 10.0),
                (("Google Inc. (Apple)", "ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version)"), 9.0),
                (("Google Inc. (Apple)", "ANGLE (Apple, ANGLE Metal Renderer: Apple M3, Unspecified Version)"), 7.0),
                (("Google Inc. (Intel Inc.)", "ANGLE (Intel Inc., ANGLE Metal Renderer: Intel(R) Iris(TM) Plus Graphics 640, Unspecified Version)"), 5.0),
            ],
        },
        "linux" => OsTable {
            platform: "Linux x86_64",
            ua_platform: "X11; Linux x86_64",
            ch_platform: "Linux",
            platform_versions: vec![("6.8.0", 4.0), ("6.11.0", 3.0), ("5.15.0", 2.0)],
            architecture: "x86",
            bitness: "64",
            wow64: false,
            taskbar: 27,
            screens: vec![
                (sc(1920, 1080, 1.0), 40.0),
                (sc(1366, 768, 1.0), 20.0),
                (sc(1600, 900, 1.0), 12.0),
                (sc(2560, 1440, 1.0), 10.0),
            ],
            gpus: vec![
                (("Google Inc. (Intel)", "ANGLE (Intel, Mesa Intel(R) UHD Graphics 620 (KBL GT2), OpenGL 4.6)"), 10.0),
                (("Google Inc. (Intel)", "ANGLE (Intel, Mesa Intel(R) Graphics (RPL-P), OpenGL 4.6)"), 7.0),
                (("Google Inc. (AMD)", "ANGLE (AMD, AMD Radeon Graphics (radeonsi, renoir, LLVM 17.0.6), OpenGL 4.6)"), 6.0),
                (("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce GTX 1650/PCIe/SSE2, OpenGL 4.5)"), 5.0),
            ],
        },
        _ => OsTable {
            platform: "Win32",
            ua_platform: "Windows NT 10.0; Win64; x64",
            ch_platform: "Windows",
            platform_versions: vec![("10.0.0", 3.0), ("15.0.0", 4.0), ("19.0.0", 3.0)],
            architecture: "x86",
            bitness: "64",
            wow64: false,
            taskbar: 40,
            screens: vec![
                (sc(1920, 1080, 1.0), 30.0),
                (sc(1536, 864, 1.25), 22.0),
                (sc(1366, 768, 1.0), 14.0),
                (sc(1600, 900, 1.0), 8.0),
                (sc(2560, 1440, 1.0), 7.0),
                (sc(1280, 720, 1.5), 6.0),
                (sc(1920, 1200, 1.0), 5.0),
                (sc(1440, 900, 1.0), 5.0),
                (sc(1280, 1024, 1.0), 3.0),
            ],
            gpus: vec![
                (("Google Inc. (Intel)", "ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E92) Direct3D11 vs_5_0 ps_5_0, D3D11)"), 10.0),
                (("Google Inc. (Intel)", "ANGLE (Intel, Intel(R) UHD Graphics 620 (0x00005917) Direct3D11 vs_5_0 ps_5_0, D3D11)"), 9.0),
                (("Google Inc. (Intel)", "ANGLE (Intel, Intel(R) Iris(R) Xe Graphics (0x000046A8) Direct3D11 vs_5_0 ps_5_0, D3D11)"), 9.0),
                (("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce GTX 1650 (0x00001F82) Direct3D11 vs_5_0 ps_5_0, D3D11)"), 8.0),
                (("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 (0x00002504) Direct3D11 vs_5_0 ps_5_0, D3D11)"), 8.0),
                (("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce RTX 4060 (0x00002882) Direct3D11 vs_5_0 ps_5_0, D3D11)"), 6.0),
                (("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce GTX 1060 6GB (0x00001C03) Direct3D11 vs_5_0 ps_5_0, D3D11)"), 6.0),
                (("Google Inc. (AMD)", "ANGLE (AMD, AMD Radeon RX 580 (0x000067DF) Direct3D11 vs_5_0 ps_5_0, D3D11)"), 5.0),
                (("Google Inc. (AMD)", "ANGLE (AMD, AMD Radeon(TM) Graphics (0x00001638) Direct3D11 vs_5_0 ps_5_0, D3D11)"), 5.0),
                (("Google Inc. (AMD)", "ANGLE (AMD, AMD Radeon RX 6600 (0x000073FF) Direct3D11 vs_5_0 ps_5_0, D3D11)"), 4.0),
            ],
        },
    }
}

pub fn host_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "windows") {
        "win"
    } else {
        "linux"
    }
}

const LANG_VARIANTS: [(&str, &[&str]); 4] = [
    ("ru-RU,ru;q=0.9", &["ru-RU", "ru"]),
    ("ru-RU,ru;q=0.9,en;q=0.8", &["ru-RU", "ru", "en"]),
    ("ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7", &["ru-RU", "ru", "en-US", "en"]),
    ("ru,en;q=0.9", &["ru", "en"]),
];

// ─── Персона ────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Brand {
    pub brand: String,
    pub version: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UaMetadata {
    pub brands: Vec<Brand>,
    #[serde(rename = "fullVersionList", default)]
    pub full_version_list: Vec<Brand>,
    #[serde(rename = "fullVersion", default)]
    pub full_version: String,
    #[serde(default)]
    pub platform: String,
    #[serde(rename = "platformVersion", default)]
    pub platform_version: String,
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub bitness: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub mobile: bool,
    #[serde(default)]
    pub wow64: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ScreenInfo {
    pub width: i64,
    pub height: i64,
    #[serde(rename = "availWidth")]
    pub avail_width: i64,
    #[serde(rename = "availHeight")]
    pub avail_height: i64,
    #[serde(rename = "colorDepth")]
    pub color_depth: i64,
    #[serde(rename = "pixelDepth")]
    pub pixel_depth: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WindowInfo {
    pub width: i64,
    pub height: i64,
    pub left: i64,
    pub top: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Gpu {
    pub vendor: String,
    pub renderer: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Connection {
    #[serde(rename = "effectiveType")]
    pub effective_type: String,
    pub rtt: i64,
    pub downlink: f64,
    #[serde(rename = "saveData")]
    pub save_data: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Noise {
    pub canvas: u32,
    pub webgl: u32,
    pub audio: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Persona {
    pub v: u32,
    pub seed: String,
    pub os: String,
    pub ua: String,
    pub platform: String,
    #[serde(rename = "chromeMajor")]
    pub chrome_major: String,
    #[serde(rename = "chromeFullVersion")]
    pub chrome_full_version: String,
    #[serde(rename = "uaMetadata")]
    pub ua_metadata: UaMetadata,
    #[serde(rename = "acceptLanguage")]
    pub accept_language: String,
    pub languages: Vec<String>,
    pub locale: String,
    #[serde(rename = "timezoneId")]
    pub timezone_id: String,
    pub screen: ScreenInfo,
    pub dpr: f64,
    pub window: WindowInfo,
    #[serde(rename = "hardwareConcurrency")]
    pub hardware_concurrency: i64,
    #[serde(rename = "deviceMemory")]
    pub device_memory: i64,
    #[serde(rename = "maxTouchPoints")]
    pub max_touch_points: i64,
    pub gpu: Gpu,
    pub connection: Connection,
    #[serde(rename = "colorScheme")]
    pub color_scheme: String,
    #[serde(rename = "reducedMotion")]
    pub reduced_motion: String,
    pub noise: Noise,
    /// Неизвестные поля из JS-версии сохраняем, чтобы не терять их при записи.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Persona {
    /// Заголовки HTTP-канала из персоны (порт `fingerprint.httpHeaders`).
    pub fn http_headers(&self) -> Vec<(&'static str, String)> {
        let brands = self
            .ua_metadata
            .brands
            .iter()
            .map(|b| format!("\"{}\";v=\"{}\"", b.brand, b.version))
            .collect::<Vec<_>>()
            .join(", ");
        vec![
            ("user-agent", self.ua.clone()),
            ("accept-language", self.accept_language.clone()),
            ("sec-ch-ua", brands),
            ("sec-ch-ua-mobile", if self.ua_metadata.mobile { "?1".into() } else { "?0".into() }),
            ("sec-ch-ua-platform", format!("\"{}\"", self.ua_metadata.platform)),
        ]
    }
}

// ─── Версия браузера ────────────────────────────────────────────────────────

static CHROME_VERSION: OnceLock<String> = OnceLock::new();

/// Версия Chrome берётся из ТОЙ сборки, что реально лежит рядом (patchright
/// browsers.json), иначе — из константы. Пул версий «на глазок» протухает.
pub fn chrome_full_version(root: &Path) -> String {
    CHROME_VERSION
        .get_or_init(|| {
            if let Ok(v) = std::env::var("OTVET_CHROME_VERSION") {
                if !v.trim().is_empty() {
                    return v;
                }
            }
            // Версия ТОГО браузера, которым мы правда пойдём входить. Иначе
            // персона говорила бы «Chrome 148», а движок оказывался бы, скажем,
            // 151-м: UA против набора возможностей — то же расхождение, из-за
            // которого мы не берём Edge. Chrome и patchright кладут номер рядом
            // с chrome.exe: первый папкой, второй файлом `<версия>.manifest`.
            if let Some(v) = crate::cdp::chrome_path(root).as_deref().and_then(version_beside) {
                return v;
            }
            let p = root.join("node_modules/patchright-core/browsers.json");
            if let Ok(txt) = std::fs::read_to_string(&p) {
                if let Ok(j) = serde_json::from_str::<Value>(&txt) {
                    if let Some(arr) = j.get("browsers").and_then(|b| b.as_array()) {
                        for b in arr {
                            if b.get("name").and_then(|n| n.as_str()) == Some("chromium") {
                                if let Some(v) = b.get("browserVersion").and_then(|v| v.as_str()) {
                                    return v.to_string();
                                }
                            }
                        }
                    }
                }
            }
            FALLBACK_VERSION.to_string()
        })
        .clone()
}

/// Номер версии, лежащий рядом с `chrome.exe`: у Chrome это папка
/// `151.0.7922.174`, у сборки patchright — файл `148.0.7778.96.manifest`.
fn version_beside(exe: &Path) -> Option<String> {
    let dir = exe.parent()?;
    let looks_like_version = |s: &str| {
        let parts: Vec<&str> = s.split('.').collect();
        parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    };
    let mut found: Option<String> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let stem = name.strip_suffix(".manifest").unwrap_or(&name).to_string();
        if looks_like_version(&stem) {
            // Версий рядом может лежать несколько (Chrome не всегда убирает
            // старую после обновления) — берём старшую.
            if found.as_deref().is_none_or(|old| older(old, &stem)) {
                found = Some(stem);
            }
        }
    }
    found
}

/// Сравнение версий по числам, а не по строкам: «9.0.0.0» не старше «10.0.0.0».
fn older(a: &str, b: &str) -> bool {
    let nums = |s: &str| s.split('.').filter_map(|p| p.parse::<u32>().ok()).collect::<Vec<_>>();
    nums(a) < nums(b)
}

pub fn chrome_major(root: &Path) -> String {
    chrome_full_version(root).split('.').next().unwrap_or("148").to_string()
}

// ─── Сборка персоны ─────────────────────────────────────────────────────────

#[derive(Default, Clone)]
pub struct PersonaOpts {
    pub os: Option<String>,
    pub locale: Option<String>,
    pub timezone_id: Option<String>,
}

/// Чистая функция: один seed → одна и та же персона (совместимо с JS).
pub fn build_persona(seed: &str, opts: &PersonaOpts, root: &Path) -> Persona {
    let os_key = match opts.os.as_deref() {
        None | Some("host") => host_os().to_string(),
        Some(o) => o.to_string(),
    };
    let t = os_table(&os_key);
    let mut r = Mulberry32::new(hash32(seed));

    let full = chrome_full_version(root);
    let major = full.split('.').next().unwrap_or("148").to_string();
    // Современный Chrome отдаёт «урезанный» UA — минорные части всегда 0.0.0,
    // поэтому одинаковый UA у всех аккаунтов это норма, а не потеря энтропии.
    let ua = format!(
        "Mozilla/5.0 ({}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{}.0.0.0 Safari/537.36",
        t.ua_platform, major
    );

    // ПОРЯДОК обращений к ГПСЧ обязан совпадать с JS — иначе персона «поедет».
    let (screen_w, screen_h, dpr) = {
        let s = r.pick_w(&t.screens);
        (s.w, s.h, s.dpr)
    };
    let (gpu_vendor, gpu_renderer) = *r.pick_w(&t.gpus);
    let platform_version = (*r.pick_w(&t.platform_versions)).to_string();
    let lang = {
        let i = (r.next() * LANG_VARIANTS.len() as f64).floor() as usize % LANG_VARIANTS.len();
        LANG_VARIANTS[i]
    };
    // Окно обязано влезать в экран: «невозможные» пары antibot ловит.
    let win_w = (screen_w - 40).min(r.int(1100, 1400));
    let win_h = (screen_h - t.taskbar - 40).min(r.int(720, 900));
    let win_left = r.int(0, 120);
    let win_top = r.int(0, 90);
    // deviceMemory Chrome квантует и ограничивает восьмёркой — 16 не бывает.
    let hw = *r.pick_w(&[(4i64, 20.0), (6, 20.0), (8, 30.0), (12, 18.0), (16, 12.0)]);
    let mem = *r.pick_w(&[(4i64, 35.0), (8, 60.0), (2, 5.0)]);
    let rtt = r.int(2, 8) * 25; // Chrome округляет rtt до 25 мс
    let downlink = *r.pick_w(&[(1.5f64, 2.0), (3.0, 4.0), (5.0, 5.0), (7.5, 4.0), (10.0, 3.0)]);
    let color_scheme = (*r.pick_w(&[("light", 75.0), ("dark", 25.0)])).to_string();

    Persona {
        v: 2,
        seed: seed.to_string(),
        os: os_key,
        ua,
        platform: t.platform.into(),
        chrome_major: major.clone(),
        chrome_full_version: full.clone(),
        ua_metadata: UaMetadata {
            brands: vec![
                Brand { brand: "Chromium".into(), version: major.clone() },
                Brand { brand: "Google Chrome".into(), version: major.clone() },
                Brand { brand: "Not?A_Brand".into(), version: "24".into() },
            ],
            full_version_list: vec![
                Brand { brand: "Chromium".into(), version: full.clone() },
                Brand { brand: "Google Chrome".into(), version: full.clone() },
                Brand { brand: "Not?A_Brand".into(), version: "24.0.0.0".into() },
            ],
            full_version: full,
            platform: t.ch_platform.into(),
            platform_version,
            architecture: t.architecture.into(),
            bitness: t.bitness.into(),
            model: String::new(),
            mobile: false,
            wow64: t.wow64,
        },
        accept_language: lang.0.to_string(),
        languages: lang.1.iter().map(|s| s.to_string()).collect(),
        locale: opts.locale.clone().unwrap_or_else(|| "ru-RU".into()),
        // Таймзона обязана биться с гео прокси; без гео-базы честнее держать Москву.
        timezone_id: opts.timezone_id.clone().unwrap_or_else(|| "Europe/Moscow".into()),
        screen: ScreenInfo {
            width: screen_w,
            height: screen_h,
            avail_width: screen_w,
            avail_height: screen_h - t.taskbar,
            color_depth: 24,
            pixel_depth: 24,
        },
        dpr,
        window: WindowInfo { width: win_w, height: win_h, left: win_left, top: win_top },
        hardware_concurrency: hw,
        device_memory: mem,
        max_touch_points: 0,
        gpu: Gpu { vendor: gpu_vendor.into(), renderer: gpu_renderer.into() },
        connection: Connection {
            effective_type: "4g".into(),
            rtt,
            downlink: (downlink * 10.0).round() / 10.0,
            save_data: false,
        },
        color_scheme,
        reduced_motion: "no-preference".into(),
        // Шум обязан быть детерминированным: «нарисуй дважды — сравни» ловит
        // случайный шум мгновенно.
        noise: Noise {
            canvas: hash32(&format!("canvas:{seed}")),
            webgl: hash32(&format!("webgl:{seed}")),
            audio: hash32(&format!("audio:{seed}")),
        },
        extra: serde_json::Map::new(),
    }
}

// ─── Хранилище персон (personas.json) ───────────────────────────────────────

pub struct PersonaStore {
    writer: crate::store_io::FileWriter,
    root: PathBuf,
    map: Mutex<BTreeMap<String, Persona>>,
}

impl PersonaStore {
    pub fn new(root: &Path) -> Self {
        let path = root.join("personas.json");
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<BTreeMap<String, Persona>>(&t).ok())
            .unwrap_or_default();
        Self {
            writer: crate::store_io::FileWriter::new(path),
            root: root.to_path_buf(),
            map: Mutex::new(map),
        }
    }

    /// Персона аккаунта: из хранилища либо генерим и сохраняем. Пересобираем при
    /// смене мажорной версии браузера — иначе UA обещал бы Chrome 148 на движке 150.
    pub fn get(&self, account_name: &str, opts: &PersonaOpts) -> Persona {
        let key = if account_name.is_empty() { "default" } else { account_name };
        let major_now = chrome_major(&self.root);
        {
            let mut m = self.map.lock();
            if let Some(cur) = m.get_mut(key) {
                let os_ok = match opts.os.as_deref() {
                    None | Some("host") => true,
                    Some(o) => cur.os == o,
                };
                if cur.v == 2 && cur.chrome_major == major_now && os_ok {
                    // Таймзона/локаль — единственное, что меняем на лету (гео прокси).
                    if let Some(tz) = &opts.timezone_id {
                        if &cur.timezone_id != tz {
                            cur.timezone_id = tz.clone();
                        }
                    }
                    if let Some(lo) = &opts.locale {
                        if &cur.locale != lo {
                            cur.locale = lo.clone();
                        }
                    }
                    return cur.clone();
                }
            }
        }
        // Seed берём прежний (если персона пересобирается) — «железо» сохранится.
        let seed = {
            let m = self.map.lock();
            m.get(key).map(|p| p.seed.clone()).unwrap_or_else(|| key.to_string())
        };
        let p = build_persona(&seed, opts, &self.root);
        self.map.lock().insert(key.to_string(), p.clone());
        let _ = self.save();
        p
    }

    /// Перенести персону на новое имя (переименование аккаунта): отпечаток обязан
    /// остаться прежним — куки те же, а «железо» иначе сменилось бы целиком.
    pub fn rename(&self, old: &str, new: &str) -> bool {
        if old == new {
            return false;
        }
        let mut m = self.map.lock();
        if !m.contains_key(old) || m.contains_key(new) {
            return false; // занято — чужое не затираем
        }
        let p = m.remove(old).unwrap();
        m.insert(new.to_string(), p);
        drop(m);
        let _ = self.save();
        true
    }

    pub fn drop_persona(&self, name: &str) {
        let mut m = self.map.lock();
        if m.remove(name).is_some() {
            drop(m);
            let _ = self.save();
        }
    }

    fn save(&self) -> std::io::Result<()> {
        let txt = {
            let m = self.map.lock();
            serde_json::to_string_pretty(&*m).unwrap_or_else(|_| "{}".into())
        };
        self.writer.write(&txt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash32_matches_js() {
        assert_eq!(hash32(""), 0x811c_9dc5);
    }

    /// Главная проверка совместимости: персоны, сгенерированные JS-версией и
    /// лежащие в personas.json, должны воспроизводиться байт-в-байт. Если этот
    /// тест красный — аккаунт под теми же куками сменит «железо».
    #[test]
    fn reproduces_personas_written_by_js() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let file = root.join("personas.json");
        let Ok(txt) = std::fs::read_to_string(&file) else {
            eprintln!("personas.json рядом нет — проверка пропущена");
            return;
        };
        let map: BTreeMap<String, Persona> = serde_json::from_str(&txt).expect("personas.json разобран");
        let mut checked = 0;
        for (name, want) in map.iter() {
            if want.v != 2 || want.chrome_major != chrome_major(&root) {
                continue; // персона другой схемы/версии браузера — не сравниваем
            }
            let got = build_persona(
                &want.seed,
                &PersonaOpts { os: Some(want.os.clone()), ..Default::default() },
                &root,
            );
            assert_eq!(got.ua, want.ua, "UA разошёлся для {name}");
            assert_eq!(got.screen.width, want.screen.width, "экран разошёлся для {name}");
            assert_eq!(got.screen.height, want.screen.height, "экран разошёлся для {name}");
            assert_eq!(got.gpu.renderer, want.gpu.renderer, "GPU разошёлся для {name}");
            assert_eq!(got.accept_language, want.accept_language, "язык разошёлся для {name}");
            assert_eq!(got.hardware_concurrency, want.hardware_concurrency, "ядра разошлись для {name}");
            assert_eq!(got.device_memory, want.device_memory, "память разошлась для {name}");
            assert_eq!(got.window.width, want.window.width, "окно разошлось для {name}");
            assert_eq!(got.connection.rtt, want.connection.rtt, "rtt разошёлся для {name}");
            assert_eq!(got.noise.canvas, want.noise.canvas, "шум разошёлся для {name}");
            checked += 1;
        }
        eprintln!("сверено персон: {checked}");
    }

    /// Версия берётся у того браузера, которым и пойдём входить. Расхождение
    /// «UA говорит 148, движок 151» ловится обычным перебором возможностей, и
    /// это ровно та несостыковка, из-за которой мы не запускаемся под Edge.
    #[test]
    fn version_is_read_from_the_browser_we_will_launch() {
        // Имя с солью: одного pid мало — тестовых двоичных файлов бывает
        // несколько, и они гоняются разом, деля временную папку на двоих.
        let dir = std::env::temp_dir().join(format!(
            "otvetware-ver-{}-{}",
            std::process::id(),
            crate::util::rand_range(1, 1_000_000)
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // Chrome держит номер версии папкой рядом с chrome.exe.
        let chrome = dir.join("chrome");
        std::fs::create_dir_all(chrome.join("151.0.7922.174")).unwrap();
        std::fs::write(chrome.join("chrome.exe"), b"").unwrap();
        assert_eq!(version_beside(&chrome.join("chrome.exe")).as_deref(), Some("151.0.7922.174"));

        // Сборка patchright — файлом `<версия>.manifest`.
        let patched = dir.join("patched");
        std::fs::create_dir_all(&patched).unwrap();
        std::fs::write(patched.join("148.0.7778.96.manifest"), b"").unwrap();
        std::fs::write(patched.join("chrome.exe"), b"").unwrap();
        assert_eq!(version_beside(&patched.join("chrome.exe")).as_deref(), Some("148.0.7778.96"));

        // После обновления рядом может остаться старая — берём старшую, и
        // сравниваем числами: строкой «9» оказалась бы больше «10».
        std::fs::create_dir_all(chrome.join("149.0.9.9")).unwrap();
        assert_eq!(version_beside(&chrome.join("chrome.exe")).as_deref(), Some("151.0.7922.174"));
        assert!(older("149.0.9.9", "151.0.7922.174"));
        assert!(older("9.0.0.0", "10.0.0.0"), "версии сравниваются как строки");

        // Ничего похожего рядом нет — пусть решает тот, кто звал.
        let bare = dir.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(bare.join("chrome.exe"), b"").unwrap();
        assert_eq!(version_beside(&bare.join("chrome.exe")), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persona_is_deterministic() {
        let root = Path::new(".");
        let a = build_persona("Аккаунт 1", &PersonaOpts::default(), root);
        let b = build_persona("Аккаунт 1", &PersonaOpts::default(), root);
        assert_eq!(a.ua, b.ua);
        assert_eq!(a.screen.width, b.screen.width);
        assert_eq!(a.noise.canvas, b.noise.canvas);
    }
}
