//! ai.rs — клиент OpenAI-совместимого API (DeepSeek, OpenRouter и т.п.) + «директивы».
//!
//! Про директивы отдельно, потому что это неочевидно: разброс ответов — свойство
//! НАБОРА, а не одного ответа. Внутри вызова модель прошлых ответов не помнит и
//! «разнообразить» не может: из списка вариантов она стабильно берёт первый.
//! Поэтому заход/длину/небрежность выбирает КОД и подставляет в промпт на каждый
//! вызов через маркер `{{ДИРЕКТИВА}}`, а промпт задаёт только голос.

use crate::util::{pick_weighted, Log, Stop};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

/// Сайт «Ответы Mail.ru» не рендерит markdown: `**жирный**` и `## заголовки`
/// попадают в пост как есть — выглядит неестественно и пахнет ботом.
pub const NO_MARKDOWN: &str = " НЕ ИСПОЛЬЗУЙ markdown: никаких **жирных**, ## заголовков, `кода`, списков через дефис/номер, таблиц или разметки вообще. Пиши простым текстом, как в мессенджере.";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AiCfg {
    pub url: String,
    pub model: String,
    pub api_key: String,
    pub temperature: f64,
    pub max_tokens: i64,
    /// Таймаут одного запроса, сек.
    pub timeout_sec: u64,
    /// Сколько ПОВТОРНЫХ попыток (всего запросов = retries + 1).
    pub retries: u32,
}

impl AiCfg {
    pub fn preset() -> Self {
        Self {
            url: "https://api.deepseek.com/chat/completions".into(),
            model: "deepseek-chat".into(),
            api_key: String::new(),
            temperature: 0.7,
            max_tokens: 5000,
            timeout_sec: 15,
            retries: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Msg {
    pub role: String,
    pub content: String,
    /// Картинки к этому сообщению, уже готовыми `data:`-ссылками. В историю
    /// разговора не пишутся: файл распух бы на мегабайты, а модель к следующему
    /// вопросу всё равно смотрит новые картинки.
    #[serde(skip)]
    pub images: Vec<String>,
}

impl Msg {
    pub fn system(c: impl Into<String>) -> Self {
        Self { role: "system".into(), content: c.into(), images: vec![] }
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self { role: "user".into(), content: c.into(), images: vec![] }
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: c.into(), images: vec![] }
    }

    /// Показать модели картинки вместе с этим сообщением.
    pub fn with_images(mut self, images: Vec<String>) -> Self {
        self.images = images;
        self
    }

    /// Сообщение в том виде, в каком его ждёт API. Без картинок это обычная
    /// строка — так понимают все провайдеры; с картинками приходится
    /// раскладывать содержимое на части, иначе текст и фото не соединить.
    fn to_api(&self) -> serde_json::Value {
        if self.images.is_empty() {
            return json!({ "role": self.role, "content": self.content });
        }
        let mut parts = vec![json!({ "type": "text", "text": self.content })];
        for url in &self.images {
            parts.push(json!({ "type": "image_url", "image_url": { "url": url } }));
        }
        json!({ "role": self.role, "content": parts })
    }
}

/// Байты картинки в `data:`-ссылку, как её ждёт OpenAI-совместимый API.
pub fn data_url(bytes: &[u8], mime: &str) -> String {
    use base64::Engine;
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[derive(Debug)]
pub enum AiError {
    Aborted,
    Http(u16, String),
    Network(String),
    Empty,
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AiError::Aborted => write!(f, "остановлено"),
            AiError::Http(code, m) => {
                write!(f, "HTTP {code}{}", if m.is_empty() { String::new() } else { format!(": {m}") })
            }
            AiError::Network(m) => write!(f, "{m}"),
            AiError::Empty => write!(f, "пустой ответ"),
        }
    }
}

/// Клиент нейросети. Отдельный от бота HTTP-клиент: сюда не идут ни куки
/// аккаунта, ни его прокси — это другой сервис и другая личность запроса.
pub struct AiClient {
    http: reqwest::Client,
}

impl Default for AiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AiClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Проверка ключа/модели перед прогоном: понятные сообщения вместо «HTTP 4xx».
    pub async fn check(&self, cfg: &AiCfg, stop: &Stop) -> Result<String, String> {
        let body = json!({
            "model": cfg.model,
            "messages": [{"role": "user", "content": "тест"}],
            "max_tokens": 5,
        });
        let req = self
            .http
            .post(&cfg.url)
            .bearer_auth(&cfg.api_key)
            .header("content-type", "application/json")
            .json(&body);
        let fut = async {
            let r = req.send().await.map_err(|e| e.to_string())?;
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            Ok::<_, String>((status, text))
        };
        let (status, text) = tokio::select! {
            biased;
            _ = stop.wait() => return Err("остановлено".into()),
            r = tokio::time::timeout(Duration::from_secs(45), fut) => match r {
                Ok(v) => v.map_err(|e| format!("Ошибка подключения: {e}"))?,
                Err(_) => return Err("API не ответил — таймаут".into()),
            },
        };
        match status {
            200 => Ok(format!("API ОК, модель «{}»", cfg.model)),
            401 => Err("Неверный API ключ".into()),
            402 => Err("Баланс пуст — пополни счёт у провайдера".into()),
            403 => Err("Доступ запрещён — проверь ключ и баланс".into()),
            429 => Err("Слишком много запросов — подожди пару минут".into()),
            503 => Err("API перегружен — попробуй позже".into()),
            s => Err(format!("HTTP {s}: {}", err_message(&text))),
        }
    }

    /// Один запрос без ретраев.
    async fn once(
        &self,
        cfg: &AiCfg,
        msgs: &[Msg],
        temperature: f64,
        max_tokens: i64,
        stop: &Stop,
    ) -> Result<String, AiError> {
        // Свежая директива на КАЖДЫЙ вызов, в том числе на каждый ретрай.
        let msgs = apply_directives(msgs);
        let msgs: Vec<serde_json::Value> = msgs.iter().map(Msg::to_api).collect();
        let body = json!({
            "model": cfg.model,
            "messages": msgs,
            "temperature": temperature,
            "max_tokens": if max_tokens > 0 { max_tokens } else { 2000 },
        });
        let req = self
            .http
            .post(&cfg.url)
            .bearer_auth(&cfg.api_key)
            .header("content-type", "application/json")
            .json(&body);
        let fut = async {
            let r = req.send().await.map_err(|e| AiError::Network(crate::util::clip(&e.to_string(), 120)))?;
            let status = r.status().as_u16();
            let text = r.text().await.map_err(|e| AiError::Network(e.to_string()))?;
            Ok::<_, AiError>((status, text))
        };
        let timeout = Duration::from_secs(cfg.timeout_sec.max(3));
        let (status, text) = tokio::select! {
            biased;
            _ = stop.wait() => return Err(AiError::Aborted),
            r = tokio::time::timeout(timeout, fut) => match r {
                Ok(v) => v?,
                Err(_) => return Err(AiError::Network("таймаут".into())),
            },
        };
        if !(200..300).contains(&status) {
            return Err(AiError::Http(status, err_message(&text)));
        }
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| AiError::Empty)?;
        Ok(v.pointer("/choices/0/message/content").and_then(|c| c.as_str()).unwrap_or("").trim().to_string())
    }

    /// Генерация с ретраями: повторяем при ЛЮБОЙ ошибке и при ПУСТОМ ответе.
    /// «Стоп» рвёт сразу, без ретрая.
    pub async fn generate(
        &self,
        cfg: &AiCfg,
        msgs: &[Msg],
        log: &Log,
        stop: &Stop,
    ) -> Result<String, AiError> {
        self.generate_with(cfg, msgs, cfg.temperature, cfg.max_tokens, log, stop).await
    }

    pub async fn generate_with(
        &self,
        cfg: &AiCfg,
        msgs: &[Msg],
        temperature: f64,
        max_tokens: i64,
        log: &Log,
        stop: &Stop,
    ) -> Result<String, AiError> {
        let attempts = cfg.retries + 1;
        for i in 0..attempts {
            match self.once(cfg, msgs, temperature, max_tokens, stop).await {
                Ok(a) if !a.is_empty() => return Ok(a),
                Ok(a) => {
                    if i + 1 < attempts {
                        log(&format!("   [!] Пустой ответ — повторю ({}/{attempts})", i + 1));
                        if stop.sleep_ms(1500).await {
                            return Err(AiError::Aborted);
                        }
                        continue;
                    }
                    return Ok(a);
                }
                Err(AiError::Aborted) => return Err(AiError::Aborted),
                Err(e) => {
                    if i + 1 < attempts {
                        log(&format!("   [!] {e} — повторю ({}/{attempts})", i + 1));
                        if stop.sleep_ms(1500).await {
                            return Err(AiError::Aborted);
                        }
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(AiError::Empty)
    }
}

fn err_message(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| crate::util::clip(text.trim(), 140))
}

// ─── Маркеры промпта ────────────────────────────────────────────────────────

pub const DIRECTIVE_MARK: &str = "{{ДИРЕКТИВА}}";
pub const PELMENI_MARK: &str = "{{ПЕЛЬМЕНИ}}";

// [вес, текст] — на форуме преобладают короткие ответы и небрежное письмо,
// поэтому у них вес выше.
fn d_angles() -> Vec<(f64, &'static str)> {
    vec![
        (3.0, "вспомни СВОЙ случай по теме и расскажи только его, с конкретикой (возраст, срок, сколько стоило)"),
        (2.0, "просто согласись или не согласись, без единого объяснения"),
        (2.0, "задай автору встречный вопрос вместо ответа"),
        (2.0, "дай один конкретный совет одной строкой, без обоснования"),
        (1.0, "отмахнись: ерунда, забей, само пройдёт"),
        (1.0, "зацепись за мелкую деталь вопроса и уйди от темы"),
        (1.0, "беззлобно подколи автора"),
        (1.0, "усомнись в самом вопросе: так ли всё было, как он пишет"),
        (1.0, "признайся, что не знаешь, и всё равно скажи, что думаешь"),
        (1.0, "ответь не на главный вопрос, а на побочную деталь, которая тебя зацепила"),
    ]
}

fn d_lengths() -> Vec<(f64, &'static str)> {
    vec![
        (4.0, "одна строка, 3-8 слов, больше не пиши"),
        (3.0, "две коротких фразы"),
        (2.0, "две-три строки, можно сбивчиво"),
        (1.0, "обрубок в 2-4 слова"),
    ]
}

fn d_registers() -> Vec<(f64, &'static str)> {
    vec![
        (5.0, "со строчной буквы, точку в конце не ставь, пару запятых пропусти"),
        (2.0, "торопливо и криво, будто одной рукой в транспорте"),
        (2.0, "более-менее аккуратно, с заглавной и точками, ты не всегда неряшлив"),
    ]
}

pub fn roll_directive() -> String {
    format!(
        "ДИРЕКТИВА НА ЭТОТ ОТВЕТ (выполняй ровно её, сам не выбирай):\n- заход: {}\n- длина: {}\n- письмо: {}\nДиректива важнее темы: даже если вопрос сложный и хочется расписать, не расписывай.",
        pick_weighted(&d_angles()),
        pick_weighted(&d_lengths()),
        pick_weighted(&d_registers())
    )
}

/// Фирменная приблуда стиля «Смех». Написание каждый раз другое: высыпь весь
/// список в промпт — модель стабильно возьмёт первый вариант.
pub fn roll_pelmeni() -> String {
    let phrases = [
        (1.0, "пельмешки с укропчиком"),
        (1.0, "укроп с пельменями"),
        (1.0, "пельмешки с укропом"),
        (1.0, "укропчик к пельмешкам"),
        (1.0, "пельмени с укропчиком"),
        (1.0, "укропом присыпанные пельмешки"),
        (1.0, "пельмени, укропа сверху побольше"),
        (1.0, "укропчик и пельмешки"),
        (1.0, "пельмени под укропом"),
        (1.0, "пельмешек с укропом"),
        (1.0, "укроп поверх пельмешек"),
        (1.0, "пельмешки, укропа не жалей"),
        (1.0, "пельмени в укропе"),
    ];
    let manners = [
        (2.0, "вверни как совет ни к селу ни к городу"),
        (2.0, "закончи этим свой текст, вообще без связи с темой"),
        (1.0, "подай это как очевидное решение любой проблемы"),
        (1.0, "вспомни про них посреди фразы и сбейся"),
        (1.0, "скажи, что как раз их ешь прямо сейчас"),
        (1.0, "сравни происходящее с ними"),
        (1.0, "предложи их собеседнику ни с того ни с сего"),
    ];
    format!(
        "В ЭТОТ РАЗ обязательно приплети «{}»: {}. Формулировку бери ровно эту, не переписывай.",
        pick_weighted(&phrases),
        pick_weighted(&manners)
    )
}

/// Подставить маркеры в текст.
pub fn apply_markers(text: &str) -> String {
    let mut out = text.to_string();
    if out.contains(DIRECTIVE_MARK) {
        out = out.replace(DIRECTIVE_MARK, &roll_directive());
    }
    if out.contains(PELMENI_MARK) {
        out = out.replace(PELMENI_MARK, &roll_pelmeni());
    }
    out
}

/// Подставить маркеры во все системные сообщения (промпты без маркеров не трогаем).
pub fn apply_directives(msgs: &[Msg]) -> Vec<Msg> {
    msgs.iter()
        .map(|m| {
            if m.role == "system" {
                let c = apply_markers(&m.content);
                if c != m.content {
                    return Msg { role: m.role.clone(), content: c, images: m.images.clone() };
                }
            }
            m.clone()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_replaced_and_vary() {
        let src = format!("Голос. {DIRECTIVE_MARK}");
        let a = apply_markers(&src);
        assert!(!a.contains(DIRECTIVE_MARK));
        assert!(a.contains("ДИРЕКТИВА НА ЭТОТ ОТВЕТ"));
        // Промпт без маркера не меняется вообще.
        assert_eq!(apply_markers("просто текст"), "просто текст");
    }

    #[test]
    fn directives_touch_only_system_messages() {
        let msgs = vec![Msg::system(format!("x {DIRECTIVE_MARK}")), Msg::user(format!("y {DIRECTIVE_MARK}"))];
        let out = apply_directives(&msgs);
        assert!(!out[0].content.contains(DIRECTIVE_MARK));
        assert!(out[1].content.contains(DIRECTIVE_MARK));
    }
}
