//! Общий нормализатор для менеджера голосовых команд.
//!
//! Один и тот же код используется и при сборке пула команд (основная программа),
//! и при разборе распознанной речи (vcm_listener), поэтому ключи гарантированно
//! совпадают.
//!
//! Ключ сравнения (`key`): слова приведены к нижнему регистру, `ё` -> `е`, без
//! пунктуации и служебных слов, каждое слово сведено стеммером Snowball (русский)
//! к основе, основы без повторов и по алфавиту. Поэтому порядок слов, регистр,
//! падеж («ванне»/«ванной»/«ванна») и предлоги на ключ не влияют.
//!
//! Слот-число: в тексте КОМАНДЫ узел вида `0-100` превращается в метку `#` и
//! запоминает границы; в тексте РЕЧИ число (цифрами или словами: «двадцать
//! четыре») тоже превращается в `#` и возвращается значением.

use std::collections::{HashMap, HashSet};

use rust_stemmers::{Algorithm, Stemmer};

/// Метка слота в ключе.
pub const SLOT_MARK: &str = "#";

/// Служебные слова, которые не влияют на смысл команды. Слова-«хвосты» после числа
/// («пятьдесят процентов», «двадцать градусов») тоже здесь: в справочных фразах их нет.
const STOP_WORDS: &[&str] = &[
    "в", "во", "на", "по", "у", "и", "с", "со", "к", "ко", "для", "из", "от", "до", "за", "о", "об",
    "а", "но", "же", "бы", "ли", "пожалуйста", "ка", "мне", "нам", "ну", "вот", "процент",
    "процента", "процентов", "градус", "градуса", "градусов",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub min: u32,
    pub max: u32,
}

impl Slot {
    pub fn contains(&self, value: u32) -> bool {
        (self.min..=self.max).contains(&value)
    }
}

/// Результат разбора фразы-команды из пула.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandForm {
    pub key: String,
    pub slot: Option<Slot>,
}

/// Результат разбора распознанной речи.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechForm {
    /// Ключ с меткой `#` на месте числа (если оно было).
    pub key: String,
    /// Услышанное число.
    pub number: Option<u32>,
    /// Тот же ключ без метки: для случая «число не расслышали».
    pub key_without_number: String,
}

// ---------------------------------------------------------------------------
// Токены
// ---------------------------------------------------------------------------

fn clean(text: &str) -> String {
    text.to_lowercase()
        .replace('ё', "е")
        .chars()
        .map(|c| match c {
            '–' | '—' | '−' => '-',
            c if c.is_alphanumeric() || c == '-' => c,
            _ => ' ',
        })
        .collect()
}

fn parse_range(token: &str) -> Option<Slot> {
    let (a, b) = token.split_once('-')?;
    if a.is_empty() || b.is_empty() || !a.bytes().all(|c| c.is_ascii_digit()) || !b.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (a, b): (u32, u32) = (a.parse().ok()?, b.parse().ok()?);
    Some(Slot { min: a.min(b), max: a.max(b) })
}

fn is_number(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|c| c.is_ascii_digit())
}

/// Разбивает текст на токены; диапазон `0-100` остаётся одним токеном,
/// остальное с дефисом («какая-то») режется на части.
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in clean(text).split_whitespace() {
        if parse_range(raw).is_some() {
            out.push(raw.to_string());
        } else {
            out.extend(raw.split('-').filter(|p| !p.is_empty()).map(str::to_string));
        }
    }
    out
}

fn stem(stemmer: &Stemmer, word: &str) -> String {
    if is_number(word) {
        word.to_string()
    } else {
        stemmer.stem(word).into_owned()
    }
}

fn finish(mut stems: Vec<String>) -> String {
    stems.sort_unstable();
    stems.dedup();
    stems.join(" ")
}

// ---------------------------------------------------------------------------
// Команда
// ---------------------------------------------------------------------------

/// Разбор фразы из пула. Первый узел-диапазон (`0-100`) становится слотом.
pub fn command_form(text: &str) -> CommandForm {
    let stemmer = Stemmer::create(Algorithm::Russian);
    let mut stems = Vec::new();
    let mut slot = None;
    for token in tokens(text) {
        if slot.is_none() {
            if let Some(s) = parse_range(&token) {
                slot = Some(s);
                stems.push(SLOT_MARK.to_string());
                continue;
            }
        }
        if STOP_WORDS.contains(&token.as_str()) {
            continue;
        }
        stems.push(stem(&stemmer, &token));
    }
    CommandForm { key: finish(stems), slot }
}

// ---------------------------------------------------------------------------
// Речь
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Zero,
    Units,
    Teens,
    Tens,
    Hundreds,
}

fn number_word(word: &str) -> Option<(u32, Kind)> {
    use Kind::*;
    Some(match word {
        "ноль" | "нуль" => (0, Zero),
        "один" | "одна" | "одно" | "одну" => (1, Units),
        "два" | "две" => (2, Units),
        "три" => (3, Units),
        "четыре" => (4, Units),
        "пять" => (5, Units),
        "шесть" => (6, Units),
        "семь" => (7, Units),
        "восемь" => (8, Units),
        "девять" => (9, Units),
        "десять" => (10, Teens),
        "одиннадцать" => (11, Teens),
        "двенадцать" => (12, Teens),
        "тринадцать" => (13, Teens),
        "четырнадцать" => (14, Teens),
        "пятнадцать" => (15, Teens),
        "шестнадцать" => (16, Teens),
        "семнадцать" => (17, Teens),
        "восемнадцать" => (18, Teens),
        "девятнадцать" => (19, Teens),
        "двадцать" => (20, Tens),
        "тридцать" => (30, Tens),
        "сорок" => (40, Tens),
        "пятьдесят" => (50, Tens),
        "шестьдесят" => (60, Tens),
        "семьдесят" => (70, Tens),
        "восемьдесят" => (80, Tens),
        "девяносто" => (90, Tens),
        "сто" => (100, Hundreds),
        "двести" => (200, Hundreds),
        "триста" => (300, Hundreds),
        "четыреста" => (400, Hundreds),
        "пятьсот" => (500, Hundreds),
        "шестьсот" => (600, Hundreds),
        "семьсот" => (700, Hundreds),
        "восемьсот" => (800, Hundreds),
        "девятьсот" => (900, Hundreds),
        _ => return None,
    })
}

/// Разбирает числительное с начала списка слов: возвращает значение и число
/// использованных слов. Порядок: сотни, потом десятки или «-надцать», потом единицы.
pub fn parse_number_words(words: &[&str]) -> Option<(u32, usize)> {
    use Kind::*;
    let mut value = 0u32;
    let mut used = 0usize;
    // 0 — начало, 1 — после сотен, 2 — после десятков, 3 — конец
    let mut stage = 0u8;
    for word in words {
        let Some((v, kind)) = number_word(word) else { break };
        let next = match (stage, kind) {
            (0, Zero) => 3,
            (0, Hundreds) => 1,
            (0, Tens) | (1, Tens) => 2,
            (0, Teens) | (1, Teens) | (0, Units) | (1, Units) | (2, Units) => 3,
            _ => break,
        };
        value += v;
        used += 1;
        stage = next;
        if stage == 3 {
            break;
        }
    }
    if used == 0 {
        None
    } else {
        Some((value, used))
    }
}

/// Разбор распознанной фразы. Первое число (цифрами или словами) становится слотом.
pub fn speech_form(text: &str) -> SpeechForm {
    let stemmer = Stemmer::create(Algorithm::Russian);
    let toks = tokens(text);
    let mut stems = Vec::new();
    let mut number = None;
    let mut i = 0;
    while i < toks.len() {
        let token = toks[i].as_str();
        if number.is_none() {
            if is_number(token) {
                if let Ok(v) = token.parse::<u32>() {
                    number = Some(v);
                    stems.push(SLOT_MARK.to_string());
                    i += 1;
                    continue;
                }
            } else {
                let rest: Vec<&str> = toks[i..].iter().map(String::as_str).collect();
                if let Some((v, used)) = parse_number_words(&rest) {
                    number = Some(v);
                    stems.push(SLOT_MARK.to_string());
                    i += used;
                    continue;
                }
            }
        }
        if !STOP_WORDS.contains(&token) {
            stems.push(stem(&stemmer, token));
        }
        i += 1;
    }
    let key_without_number = finish(stems.iter().filter(|s| s.as_str() != SLOT_MARK).cloned().collect());
    SpeechForm { key: finish(stems), number, key_without_number }
}

// ---------------------------------------------------------------------------
// Индекс команд
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub cmd_key: String,
    pub match_key: String,
    pub slot: Option<Slot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    /// Команда найдена; `number` — значение слота, если он есть.
    Exact { cmd_key: String, number: Option<u32> },
    /// Команда ясна, но у неё есть слот, а число не услышано.
    NeedNumber { cmd_key: String },
    /// Число вне допустимого диапазона слота.
    OutOfRange { cmd_key: String, number: u32, slot: Slot },
    /// Один и тот же ключ у нескольких разных команд.
    Ambiguous(Vec<String>),
    None,
}

#[derive(Default)]
pub struct CommandIndex {
    by_key: HashMap<String, Vec<IndexEntry>>,
    /// Слотовые команды по ключу без метки `#` (для «число не расслышали»).
    by_key_no_slot: HashMap<String, Vec<IndexEntry>>,
    slots: HashMap<String, Slot>,
    len: usize,
}

fn without_mark(key: &str) -> String {
    key.split(' ').filter(|w| *w != SLOT_MARK).collect::<Vec<_>>().join(" ")
}

impl CommandIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, entry: IndexEntry) {
        if entry.match_key.is_empty() {
            return;
        }
        if let Some(slot) = entry.slot {
            self.slots.insert(entry.cmd_key.clone(), slot);
            self.by_key_no_slot
                .entry(without_mark(&entry.match_key))
                .or_default()
                .push(entry.clone());
        }
        self.by_key.entry(entry.match_key.clone()).or_default().push(entry);
        self.len += 1;
    }

    /// Сколько вариантов фраз в индексе.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn slot(&self, cmd_key: &str) -> Option<Slot> {
        self.slots.get(cmd_key).copied()
    }

    fn distinct(entries: &[IndexEntry]) -> Vec<String> {
        let mut seen = HashSet::new();
        entries
            .iter()
            .filter(|e| seen.insert(e.cmd_key.clone()))
            .map(|e| e.cmd_key.clone())
            .collect()
    }

    pub fn lookup(&self, speech: &SpeechForm) -> Match {
        if let Some(entries) = self.by_key.get(&speech.key) {
            let keys = Self::distinct(entries);
            if keys.len() > 1 {
                return Match::Ambiguous(keys);
            }
            let entry = &entries[0];
            return match (entry.slot, speech.number) {
                (Some(slot), Some(n)) if slot.contains(n) => {
                    Match::Exact { cmd_key: entry.cmd_key.clone(), number: Some(n) }
                }
                (Some(slot), Some(n)) => {
                    Match::OutOfRange { cmd_key: entry.cmd_key.clone(), number: n, slot }
                }
                (Some(_), None) => Match::NeedNumber { cmd_key: entry.cmd_key.clone() },
                (None, _) => Match::Exact { cmd_key: entry.cmd_key.clone(), number: None },
            };
        }
        if speech.number.is_none() {
            if let Some(entries) = self.by_key_no_slot.get(&speech.key) {
                let keys = Self::distinct(entries);
                return if keys.len() == 1 {
                    Match::NeedNumber { cmd_key: keys[0].clone() }
                } else {
                    Match::Ambiguous(keys)
                };
            }
        }
        Match::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> String {
        speech_form(text).key
    }

    // --- ключ: порядок, регистр, падежи, предлоги ------------------------------

    #[test]
    fn word_order_case_and_punctuation_do_not_matter() {
        let a = key("Включи свет в ванной");
        assert_eq!(a, key("в ванной свет включи"));
        assert_eq!(a, key("СВЕТ, включи — в ванной!"));
        assert_eq!(a, key("Свет включи в ванной"));
    }

    #[test]
    fn cases_of_the_same_noun_give_the_same_stem() {
        assert_eq!(key("ванна"), key("в ванне"));
        assert_eq!(key("ванна"), key("в ванной"));
        assert_eq!(key("спальня"), key("в спальне"));
        assert_eq!(key("зал"), key("в зале"));
        assert_eq!(key("люстра"), key("люстру"));
        assert_eq!(key("телевизор"), key("телевизора"));
    }

    #[test]
    fn yo_and_hyphens() {
        assert_eq!(key("зелёный"), key("зеленый"));
        assert_eq!(key("какая-то ерунда"), key("какая то ерунда"));
    }

    #[test]
    fn empty_and_only_stop_words() {
        assert_eq!(key(""), "");
        assert_eq!(key("в на по"), "");
    }

    #[test]
    fn imperative_and_infinitive_of_regular_verbs_match() {
        // «включи» и «включить» сводятся к одной основе
        assert_eq!(key("включи"), key("включить"));
        assert_eq!(key("выключи"), key("выключить"));
        assert_eq!(key("сделай"), key("сделать"));
    }

    // --- числительные ----------------------------------------------------------

    fn num(text: &str) -> Option<u32> {
        speech_form(text).number
    }

    #[test]
    fn number_words_0_to_100() {
        for (text, n) in [
            ("ноль", 0), ("один", 1), ("две", 2), ("пять", 5), ("десять", 10),
            ("одиннадцать", 11), ("девятнадцать", 19), ("двадцать", 20),
            ("двадцать один", 21), ("двадцать четыре", 24), ("тридцать", 30),
            ("сорок восемь", 48), ("пятьдесят", 50), ("пятьдесят пять", 55),
            ("девяносто девять", 99), ("сто", 100),
        ] {
            assert_eq!(num(text), Some(n), "{text}");
        }
    }

    #[test]
    fn number_words_above_100_and_digits() {
        assert_eq!(num("сто пять"), Some(105));
        assert_eq!(num("двести"), Some(200));
        assert_eq!(num("триста двадцать пять"), Some(325));
        assert_eq!(num("громкость 50"), Some(50));
        assert_eq!(num("50 процентов"), Some(50));
    }

    #[test]
    fn number_grammar_stops_where_it_should() {
        assert_eq!(parse_number_words(&["пять", "двадцать"]), Some((5, 1)));
        assert_eq!(parse_number_words(&["пятнадцать", "три"]), Some((15, 1)));
        assert_eq!(parse_number_words(&["двадцать", "двадцать"]), Some((20, 1)));
        assert_eq!(parse_number_words(&["привет"]), None);
    }

    #[test]
    fn first_number_is_the_slot_and_units_are_ignored() {
        let a = speech_form("громкость пятьдесят процентов");
        let b = speech_form("громкость пятьдесят");
        assert_eq!(a, b);
        assert_eq!(a.number, Some(50));
        assert!(a.key.contains(SLOT_MARK));
        assert_eq!(a.key_without_number, key("громкость"));
        assert_eq!(speech_form("температура двадцать два градуса").number, Some(22));
    }

    // --- команда ---------------------------------------------------------------

    #[test]
    fn command_slot_is_detected() {
        let f = command_form("Сделай Громкость Телевизор Второй этаж зал 0-100");
        assert_eq!(f.slot, Some(Slot { min: 0, max: 100 }));
        assert!(f.key.split(' ').any(|w| w == SLOT_MARK));
        let g = command_form("Включить Люстра спальня");
        assert_eq!(g.slot, None);
        assert!(!g.key.contains(SLOT_MARK));
        // тире разной длины
        assert_eq!(command_form("громкость 0–100").slot, Some(Slot { min: 0, max: 100 }));
    }

    #[test]
    fn command_and_speech_keys_agree() {
        let cmd = command_form("Включить Люстра Мансарда спальня");
        let spoken = speech_form("включи люстру в спальне мансарды");
        assert_eq!(cmd.key, spoken.key);
    }

    // --- индекс ----------------------------------------------------------------

    fn index() -> CommandIndex {
        let mut idx = CommandIndex::new();
        for (cmd, phrase) in [
            ("L1", "Включить Люстра спальня"),
            ("L1", "Включить свет спальня"), // синоним той же команды
            ("L2", "Включить Люстра зал"),
            ("V1", "Сделай Громкость Телевизор зал 0-100"),
            ("V1", "Поставь Громкость Телевизор зал 0-100"),
        ] {
            let f = command_form(phrase);
            idx.insert(IndexEntry { cmd_key: cmd.into(), match_key: f.key, slot: f.slot });
        }
        idx
    }

    #[test]
    fn exact_match_with_synonym_and_free_order() {
        let idx = index();
        assert_eq!(
            idx.lookup(&speech_form("в спальне включи свет")),
            Match::Exact { cmd_key: "L1".into(), number: None }
        );
        assert_eq!(
            idx.lookup(&speech_form("включи люстру в зале")),
            Match::Exact { cmd_key: "L2".into(), number: None }
        );
    }

    #[test]
    fn slot_command_gets_its_number() {
        let idx = index();
        assert_eq!(
            idx.lookup(&speech_form("поставь громкость телевизора в зале на пятьдесят процентов")),
            Match::Exact { cmd_key: "V1".into(), number: Some(50) }
        );
        assert_eq!(idx.slot("V1"), Some(Slot { min: 0, max: 100 }));
        assert_eq!(idx.slot("L1"), None);
    }

    #[test]
    fn missing_number_asks_for_it() {
        let idx = index();
        assert_eq!(
            idx.lookup(&speech_form("сделай громкость телевизора в зале")),
            Match::NeedNumber { cmd_key: "V1".into() }
        );
    }

    #[test]
    fn number_out_of_range_is_reported() {
        let idx = index();
        match idx.lookup(&speech_form("сделай громкость телевизора в зале сто пятьдесят")) {
            Match::OutOfRange { cmd_key, number, slot } => {
                assert_eq!(cmd_key, "V1");
                assert_eq!(number, 150);
                assert_eq!(slot, Slot { min: 0, max: 100 });
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn number_for_a_command_without_slot_does_not_match() {
        let idx = index();
        assert_eq!(idx.lookup(&speech_form("включи люстру в зале пятьдесят")), Match::None);
    }

    #[test]
    fn unknown_phrase_and_partial_phrase_do_not_match() {
        let idx = index();
        assert_eq!(idx.lookup(&speech_form("какая то ерунда")), Match::None);
        // без комнаты команда неоднозначна по смыслу, но в индексе такого ключа нет
        assert_eq!(idx.lookup(&speech_form("включи свет")), Match::None);
    }

    #[test]
    fn same_key_for_different_commands_is_ambiguous() {
        let mut idx = CommandIndex::new();
        for cmd in ["A", "B"] {
            let f = command_form("Включить свет");
            idx.insert(IndexEntry { cmd_key: cmd.into(), match_key: f.key, slot: f.slot });
        }
        match idx.lookup(&speech_form("включи свет")) {
            Match::Ambiguous(keys) => assert_eq!(keys, vec!["A".to_string(), "B".to_string()]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn duplicate_entries_of_one_command_are_not_ambiguous() {
        let mut idx = CommandIndex::new();
        for phrase in ["Включить свет", "включи свет"] {
            let f = command_form(phrase);
            idx.insert(IndexEntry { cmd_key: "A".into(), match_key: f.key, slot: f.slot });
        }
        assert_eq!(
            idx.lookup(&speech_form("свет включи")),
            Match::Exact { cmd_key: "A".into(), number: None }
        );
    }
}
