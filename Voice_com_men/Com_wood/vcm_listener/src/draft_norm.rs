#![allow(dead_code)]

// ЧЕРНОВОЙ нормализатор для живой проверки голосования до появления vcm_norm
// (со стеммером, слотом 0-100 и словарём команд). Ключ не зависит от порядка слов,
// регистра, пунктуации и служебных слов, но НЕ учитывает падежи: «ванне» и «ванной»
// для него разные слова.

const STOP_WORDS: &[&str] = &[
    "в", "во", "на", "по", "у", "и", "с", "к", "для", "из", "от", "до", "за", "о", "об",
    "пожалуйста", "ка",
];

pub fn draft_key(text: &str) -> String {
    let cleaned: String = text
        .to_lowercase()
        .replace('ё', "е")
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { ' ' })
        .collect();
    let mut words: Vec<&str> = cleaned
        .split_whitespace()
        .filter(|w| !STOP_WORDS.contains(w))
        .collect();
    words.sort_unstable();
    words.dedup();
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_order_case_and_punctuation_do_not_matter() {
        let a = draft_key("Включи свет в ванне");
        assert_eq!(a, draft_key("в ванне свет включи"));
        assert_eq!(a, draft_key("СВЕТ, включи — в ванне!"));
        assert_eq!(a, "ванне включи свет");
    }

    #[test]
    fn yo_is_folded() {
        assert_eq!(draft_key("зелёный"), draft_key("зеленый"));
    }

    #[test]
    fn empty_and_only_stop_words_give_empty_key() {
        assert_eq!(draft_key(""), "");
        assert_eq!(draft_key("в на по"), "");
    }

    #[test]
    fn hyphenated_slot_word_is_kept() {
        assert_eq!(draft_key("громкость 0-100"), "0-100 громкость");
    }
}
