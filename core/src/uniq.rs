//! uniq.rs — уникализация текста: два ответа бота не должны быть побайтово
//! одинаковыми.
//!
//! Старый способ — дописать в конец «#293508» — работал ровно наоборот:
//! решётка с шестизначным числом не встречается в живой переписке вообще, и
//! автомодерация цепляется именно за неё. Здесь вместо этого набор мелких
//! правок, каждая из которых выглядит как обычная человеческая небрежность:
//! другая точка в конце, «ещё/еще», словечко-паразит в начале, изредка число
//! в конце — но без решётки и не всегда.
//!
//! Подмена похожих букв латиницей вынесена в отдельный флаг и по умолчанию
//! выключена: приём рабочий, но смешанные алфавиты внутри слова — сами по себе
//! известный признак спама, и включать его вслепую было бы вредным советом.

use crate::util::{rand_f64, rand_range};
use serde::{Deserialize, Serialize};

/// Насколько сильно перетряхивать текст.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum UniqMode {
    /// Не трогать.
    #[default]
    Off,
    /// Одна незаметная правка.
    Light,
    /// Две-три правки.
    Normal,
    /// Всё, что есть, плюс число в конце.
    Hard,
}

impl UniqMode {
    pub fn ru(self) -> &'static str {
        match self {
            UniqMode::Off => "выключена",
            UniqMode::Light => "лёгкая",
            UniqMode::Normal => "обычная",
            UniqMode::Hard => "сильная",
        }
    }
    fn edits(self) -> usize {
        match self {
            UniqMode::Off => 0,
            UniqMode::Light => 1,
            UniqMode::Normal => 2,
            UniqMode::Hard => 4,
        }
    }
}

/// Словечки, с которых живые люди начинают ответ.
const OPENERS: &[&str] = &["ну", "вот", "короче", "честно", "кстати", "да", "вообще", "имхо"];
/// Хвосты, которые тоже выглядят естественно.
const TAILS: &[&str] = &["как-то так", "вот так", "имхо", "ну как-то так", "думаю так"];

/// Кириллица → визуально такая же латиница.
const LOOKALIKE: &[(char, char)] = &[
    ('а', 'a'),
    ('е', 'e'),
    ('о', 'o'),
    ('р', 'p'),
    ('с', 'c'),
    ('у', 'y'),
    ('х', 'x'),
    ('А', 'A'),
    ('В', 'B'),
    ('Е', 'E'),
    ('К', 'K'),
    ('М', 'M'),
    ('Н', 'H'),
    ('О', 'O'),
    ('Р', 'P'),
    ('С', 'C'),
    ('Т', 'T'),
    ('Х', 'X'),
];

/// Уникализировать текст. `homoglyphs` — подменять ли буквы латиницей.
pub fn uniquify(text: &str, mode: UniqMode, homoglyphs: bool) -> String {
    if mode == UniqMode::Off || text.trim().is_empty() {
        return text.to_string();
    }
    let want = mode.edits();
    // Несколько заходов: правки вероятностные, и с первого раза может не
    // измениться ничего — а пустая «уникализация» хуже отсутствующей.
    for _ in 0..6 {
        let mut out = text.to_string();
        let mut done = 0;
        for _ in 0..want * 3 {
            if done >= want {
                break;
            }
            let before = out.clone();
            out = match rand_range(0, if homoglyphs { 4 } else { 3 }) {
                0 => vary_ending(&out),
                1 => swap_yo(&out),
                2 => add_filler(&out),
                3 => vary_case(&out),
                _ => swap_lookalike(&out),
            };
            if out != before {
                done += 1;
            }
        }
        if mode == UniqMode::Hard {
            out = append_number(&out);
        }
        if out != text {
            return out;
        }
    }
    // Совсем не поддался (например, одно короткое слово без гласных) —
    // дописываем число: лучше так, чем два побайтово одинаковых ответа.
    append_number(text)
}

/// Точка в конце то есть, то нет — самая частая небрежность живого письма.
fn vary_ending(s: &str) -> String {
    let t = s.trim_end();
    let stripped = t.trim_end_matches(['.', '!', ')', '…']);
    // Вопросительный знак не трогаем: он несёт смысл.
    if t.ends_with('?') {
        return s.to_string();
    }
    let tail = match rand_range(0, 4) {
        0 => "",
        1 => ".",
        2 => ")",
        3 => "))",
        _ => "...",
    };
    format!("{stripped}{tail}")
}

/// «ещё» ↔ «еще»: обе формы одинаково живые.
fn swap_yo(s: &str) -> String {
    if s.contains('ё') {
        return s.replacen('ё', "е", 1);
    }
    if s.contains('Ё') {
        return s.replacen('Ё', "Е", 1);
    }
    // Обратная замена — только на словах, где «ё» действительно на месте.
    for (from, to) in [("еще", "ещё"), ("все ", "всё "), ("чем то", "чём то")] {
        if s.contains(from) {
            return s.replacen(from, to, 1);
        }
    }
    s.to_string()
}

/// Словечко в начале или хвостик в конце.
fn add_filler(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        return s.to_string();
    }
    // К длинному тексту хвост клеится естественнее, к короткому — начало.
    let long = t.chars().count() > 60;
    if long || rand_f64() < 0.5 {
        let tail = TAILS[rand_range(0, TAILS.len() as i64 - 1) as usize];
        if t.ends_with(tail) {
            return s.to_string();
        }
        let sep = if t.ends_with(['.', '!', '?', ')', '…']) { " " } else { ", " };
        return format!("{t}{sep}{tail}");
    }
    let opener = OPENERS[rand_range(0, OPENERS.len() as i64 - 1) as usize];
    if t.to_lowercase().starts_with(opener) {
        return s.to_string();
    }
    let mut rest = t.to_string();
    // Первая буква уезжает в строчную: «Ну Привет» никто не пишет.
    if let Some(c) = rest.chars().next() {
        if c.is_uppercase() {
            let lower: String = c.to_lowercase().collect();
            rest = format!("{lower}{}", &rest[c.len_utf8()..]);
        }
    }
    format!("{opener} {rest}")
}

/// Первая буква — заглавная или строчная. В переписке бывает и так, и так.
fn vary_case(s: &str) -> String {
    let mut cs = s.chars();
    let Some(first) = cs.next() else { return s.to_string() };
    if !first.is_alphabetic() {
        return s.to_string();
    }
    let rest = &s[first.len_utf8()..];
    let swapped: String =
        if first.is_uppercase() { first.to_lowercase().collect() } else { first.to_uppercase().collect() };
    format!("{swapped}{rest}")
}

/// Одна буква меняется на визуально такую же латинскую.
fn swap_lookalike(s: &str) -> String {
    let idxs: Vec<usize> = s
        .char_indices()
        .filter(|(_, c)| LOOKALIKE.iter().any(|(from, _)| from == c))
        .map(|(i, _)| i)
        .collect();
    if idxs.is_empty() {
        return s.to_string();
    }
    let at = idxs[rand_range(0, idxs.len() as i64 - 1) as usize];
    let ch = s[at..].chars().next().unwrap_or(' ');
    let Some((_, to)) = LOOKALIKE.iter().find(|(from, _)| *from == ch) else {
        return s.to_string();
    };
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..at]);
    out.push(*to);
    out.push_str(&s[at + ch.len_utf8()..]);
    out
}

/// Число в конце — БЕЗ решётки и не всегда одной длины.
fn append_number(s: &str) -> String {
    let digits = rand_range(3, 6);
    let mut n = String::new();
    for i in 0..digits {
        // Первая цифра не ноль: «0482» выглядит как артефакт, а не как число.
        let lo = if i == 0 { 1 } else { 0 };
        n.push(char::from(b'0' + rand_range(lo, 9) as u8));
    }
    format!("{} {n}", s.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_changes_nothing() {
        assert_eq!(uniquify("текст", UniqMode::Off, false), "текст");
    }

    #[test]
    fn always_changes_something() {
        for src in ["ну такое", "Согласен с тобой полностью", "лол", "а смысл"]
        {
            for _ in 0..40 {
                let out = uniquify(src, UniqMode::Normal, false);
                assert_ne!(out, src, "уникализация ничего не изменила: {src}");
                // Решётки с номером — главного признака бота — быть не должно.
                assert!(!out.contains('#'), "решётка вернулась: {out}");
            }
        }
    }

    #[test]
    fn results_vary_between_calls() {
        let src = "Обычный ответ про жизнь, ничего особенного";
        let set: std::collections::HashSet<String> =
            (0..60).map(|_| uniquify(src, UniqMode::Normal, false)).collect();
        assert!(set.len() > 5, "вариантов слишком мало: {}", set.len());
    }

    #[test]
    fn hard_adds_a_bare_number() {
        let out = uniquify("текст", UniqMode::Hard, false);
        let last = out.split_whitespace().last().unwrap_or("");
        assert!(last.chars().all(|c| c.is_ascii_digit()), "в конце не число: {out}");
        assert!(!out.contains('#'));
    }

    #[test]
    fn lookalikes_keep_the_shape() {
        let mut swapped = 0;
        for _ in 0..60 {
            let out = uniquify("хорошо", UniqMode::Light, true);
            if out.chars().any(|c| c.is_ascii_alphabetic()) {
                swapped += 1;
            }
        }
        assert!(swapped > 0, "подмена латиницей не сработала ни разу");
        // Без флага латиницы не появляется.
        for _ in 0..60 {
            let out = uniquify("хорошо", UniqMode::Light, false);
            assert!(
                !out.chars().any(|c| c.is_ascii_alphabetic() && c != ' '),
                "латиница пролезла без спроса: {out}"
            );
        }
    }

    #[test]
    fn question_mark_survives() {
        for _ in 0..40 {
            let out = uniquify("а ты сам как думаешь?", UniqMode::Normal, false);
            assert!(out.contains('?'), "потерян вопросительный знак: {out}");
        }
    }
}
