//! content.rs — контент постов в формате ProseMirror, как его ждёт mail.ru.
//!
//! Две тонкости, снятые с живого сайта:
//!  · «\n» внутри одного текстового узла НЕ рендерится — всё схлопывается в
//!    строку. Поэтому каждая строка получает свой `paragraph`.
//!  · после блочной картинки обязателен хвостовой пустой `paragraph`, иначе
//!    редактор считает документ невалидным.

use crate::journals::PoolImage;
use serde_json::{json, Value};

/// mail.ru держит до 10 картинок в одной галерее.
pub const MAX_GALLERY: usize = 10;

/// UUID v4 из обычного рандома: он тут — просто идентификатор галереи, к
/// криптографии отношения не имеет.
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    for x in b.iter_mut() {
        *x = crate::util::rand_range(0, 255) as u8;
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn gallery_src(url: &str) -> String {
    if url.contains("?size=") {
        url.to_string()
    } else {
        format!("{url}?size=origin")
    }
}

/// Картинка (или несколько) одной `imageGallery`-нодой.
pub fn image_gallery_node(items: &[(String, i64, i64)]) -> Option<Value> {
    if items.is_empty() {
        return None;
    }
    let gallery: Vec<Value> = items
        .iter()
        .take(MAX_GALLERY)
        .map(|(url, w, h)| {
            json!({
                "src": gallery_src(url),
                "dimensions": { "width": w, "height": h },
                "size": "origin",
            })
        })
        .collect();
    Some(json!({
        "type": "imageGallery",
        "attrs": { "imageGalleryID": uuid_v4(), "gallery": gallery },
    }))
}

pub fn gallery_from_pool(gifs: &[PoolImage]) -> Option<Value> {
    let items: Vec<(String, i64, i64)> =
        gifs.iter().map(|g| (format!("{}.jpg", g.hash), g.width, g.height)).collect();
    image_gallery_node(&items)
}

/// Документ из текста (+необязательная картинка).
pub fn doc_with_image(text: &str, image: Option<&Value>) -> Value {
    let mut content: Vec<Value> = Vec::new();
    if text.is_empty() {
        content.push(json!({ "type": "paragraph" }));
    } else {
        for line in text.split('\n') {
            if line.is_empty() {
                content.push(json!({ "type": "paragraph" }));
            } else {
                content.push(json!({
                    "type": "paragraph",
                    "content": [{ "type": "text", "text": line }],
                }));
            }
        }
    }
    if let Some(img) = image {
        content.push(img.clone());
        content.push(json!({ "type": "paragraph" }));
    }
    json!({ "type": "doc", "content": content })
}

pub fn text_to_doc(text: &str) -> Value {
    doc_with_image(text, None)
}

/// Обратное преобразование: текст из документа вопроса/ответа (рекурсивно).
pub fn doc_to_text(doc: &Value) -> String {
    let mut out: Vec<String> = Vec::new();
    fn walk(n: &Value, out: &mut Vec<String>) {
        if let Some(t) = n.get("text").and_then(|t| t.as_str()) {
            out.push(t.to_string());
        }
        if let Some(arr) = n.get("content").and_then(|c| c.as_array()) {
            for c in arr {
                walk(c, out);
            }
        }
    }
    walk(doc, &mut out);
    out.join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_text_becomes_paragraphs() {
        let d = text_to_doc("раз\n\nдва");
        let c = d["content"].as_array().unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0]["content"][0]["text"], "раз");
        assert!(c[1].get("content").is_none()); // пустая строка = пустой параграф
    }

    #[test]
    fn image_gets_trailing_paragraph() {
        let img = image_gallery_node(&[("abc.jpg".into(), 10, 20)]).unwrap();
        let d = doc_with_image("текст", Some(&img));
        let c = d["content"].as_array().unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[1]["type"], "imageGallery");
        assert_eq!(c[1]["attrs"]["gallery"][0]["src"], "abc.jpg?size=origin");
        assert_eq!(c[2]["type"], "paragraph");
    }

    #[test]
    fn doc_to_text_reads_back() {
        assert_eq!(doc_to_text(&text_to_doc("привет  мир")), "привет мир");
    }
}
