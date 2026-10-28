#![allow(dead_code)]

// Кодовое слово («Дом», «Дача», «Нафаня»): без него слушатель игнорирует фразы, а после
// него разбирает команду. Бывает двух видов: в одной фразе («Дом, включи свет») и отдельным
// словом («Дом» — пауза — «включи свет»): тогда открывается окно на несколько секунд.
// Здесь только чистая логика — без звука, без SQL.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde::Deserialize;
use vcm_norm::{speech_form, SpeechForm, SLOT_MARK};

/// Секция [wake] файла config.toml.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct WakeConfig {
    pub enabled: bool,
    /// Кодовые слова. Движки по-разному пишут необычные слова, поэтому перечислите
    /// варианты написания: ["нафаня", "афаня", "на фаня"].
    pub words: Vec<String>,
    /// Сколько секунд ждать команду после одного кодового слова.
    pub window_s: f32,
    /// Что сказать, когда услышано только кодовое слово (пусто — молча).
    pub ack: String,
    /// Сколько движков из трёх должны расслышать кодовое слово.
    pub min_engines: usize,
}

impl Default for WakeConfig {
    fn default() -> Self {
        Self { enabled: false, words: Vec::new(), window_s: 6.0, ack: "Слушаю.".to_string(), min_engines: 1 }
    }
}

/// Секция [matching] файла config.toml.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct MatchingConfig {
    /// Сколько лишних слов допускать во фразе («…на втором этаже…»). 0 — только точное совпадение.
    pub max_extra_words: usize,
}

impl Default for MatchingConfig {
    fn default() -> Self {
        Self { max_extra_words: 2 }
    }
}

/// Кодовые слова в виде основ (так же, как нормализуются команды и речь).
#[derive(Debug, Clone, Default)]
pub struct WakeWords {
    words: Vec<Vec<String>>,
}

impl WakeWords {
    pub fn new(words: &[String]) -> Self {
        let words = words
            .iter()
            .map(|w| speech_form(w).stems.into_iter().filter(|s| s != SLOT_MARK).collect::<Vec<_>>())
            .filter(|stems| !stems.is_empty())
            .collect();
        Self { words }
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// Есть ли во фразе хотя бы одно кодовое слово (все его основы).
    pub fn heard_in(&self, form: &SpeechForm) -> bool {
        self.words.iter().any(|w| w.iter().all(|s| form.stems.contains(s)))
    }

    /// Фраза без услышанных в ней кодовых слов.
    pub fn strip(&self, form: &SpeechForm) -> SpeechForm {
        let remove: HashSet<String> = self
            .words
            .iter()
            .filter(|w| w.iter().all(|s| form.stems.contains(s)))
            .flatten()
            .cloned()
            .collect();
        form.without(&remove)
    }
}

/// Окно «слушаю» после одного кодового слова.
#[derive(Debug, Default)]
pub struct WakeState {
    awake_until: Option<Instant>,
}

impl WakeState {
    pub fn open(&mut self, now: Instant, window: Duration) {
        self.awake_until = Some(now + window);
    }

    pub fn is_awake(&self, now: Instant) -> bool {
        matches!(self.awake_until, Some(t) if now < t)
    }

    pub fn close(&mut self) {
        self.awake_until = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeDecision {
    /// Разбирать команду по этим фразам (кодовое слово уже вырезано, если оно было).
    Process { forms: Vec<SpeechForm>, woke: bool },
    /// Прозвучало только кодовое слово: открыть окно и ответить «Слушаю.».
    OpenWindow,
    /// Кодового слова нет и окно закрыто: фраза не нам.
    Ignore,
}

pub fn decide(wake: &WakeWords, cfg: &WakeConfig, awake: bool, forms: &[SpeechForm]) -> WakeDecision {
    if !cfg.enabled || wake.is_empty() {
        return WakeDecision::Process { forms: forms.to_vec(), woke: false };
    }
    let heard = forms.iter().filter(|f| wake.heard_in(f)).count();
    if heard >= cfg.min_engines.max(1) {
        let stripped: Vec<SpeechForm> = forms.iter().map(|f| wake.strip(f)).collect();
        if stripped.iter().all(|f| f.key.is_empty()) {
            WakeDecision::OpenWindow
        } else {
            WakeDecision::Process { forms: stripped, woke: true }
        }
    } else if awake {
        WakeDecision::Process { forms: forms.to_vec(), woke: false }
    } else {
        WakeDecision::Ignore
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forms(texts: &[&str]) -> Vec<SpeechForm> {
        texts.iter().map(|t| speech_form(t)).collect()
    }

    fn cfg(words: &[&str]) -> (WakeWords, WakeConfig) {
        let list: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        (WakeWords::new(&list), WakeConfig { enabled: true, words: list, ..WakeConfig::default() })
    }

    #[test]
    fn disabled_or_empty_wake_lets_everything_through() {
        let f = forms(&["включи свет", "включи свет", "включи свет"]);
        let (w, mut c) = cfg(&["дом"]);
        c.enabled = false;
        assert!(matches!(decide(&w, &c, false, &f), WakeDecision::Process { woke: false, .. }));
        let (w, c) = cfg(&[]);
        assert!(matches!(decide(&w, &c, false, &f), WakeDecision::Process { woke: false, .. }));
    }

    #[test]
    fn phrase_without_wake_word_is_ignored() {
        let (w, c) = cfg(&["нафаня", "дом"]);
        let f = forms(&["включи свет в спальне", "включи свет в спальне", "включи свет в спальне"]);
        assert_eq!(decide(&w, &c, false, &f), WakeDecision::Ignore);
        // обычная болтовня тоже
        let chat = forms(&["вчера купили новый диван", "вчера купили новый диван", "вчера купили новый диван"]);
        assert_eq!(decide(&w, &c, false, &chat), WakeDecision::Ignore);
    }

    #[test]
    fn wake_word_in_the_same_phrase_is_cut_out() {
        let (w, c) = cfg(&["нафаня"]);
        let f = forms(&["нафаня включи свет в спальне", "нафаня включи свет в спальне", "афаня включи свет в спальне"]);
        match decide(&w, &c, false, &f) {
            WakeDecision::Process { forms: out, woke } => {
                assert!(woke);
                let want = speech_form("включи свет в спальне").key;
                assert_eq!(out[0].key, want);
                assert_eq!(out[1].key, want);
                // третий движок не расслышал кодовое слово: у него лишних основ нет, но и вырезать нечего
                assert!(out[2].key.contains("афан"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn wake_position_in_the_phrase_does_not_matter() {
        let (w, c) = cfg(&["дом"]);
        let f = forms(&["включи свет дом", "включи свет дом", "включи свет дом"]);
        assert!(matches!(decide(&w, &c, false, &f), WakeDecision::Process { woke: true, .. }));
    }

    #[test]
    fn lone_wake_word_opens_the_window_and_the_next_phrase_goes_through() {
        let (w, c) = cfg(&["нафаня"]);
        let only = forms(&["нафаня", "нафаня", "нафаня"]);
        assert_eq!(decide(&w, &c, false, &only), WakeDecision::OpenWindow);
        let cmd = forms(&["включи свет", "включи свет", "включи свет"]);
        // окно открыто — команда без кодового слова принимается
        assert!(matches!(decide(&w, &c, true, &cmd), WakeDecision::Process { woke: false, .. }));
        // окно закрыто — игнор
        assert_eq!(decide(&w, &c, false, &cmd), WakeDecision::Ignore);
    }

    #[test]
    fn spelling_variants_and_multi_word_aliases() {
        let (w, c) = cfg(&["нафаня", "афаня", "эй дом"]);
        assert_eq!(w.len(), 3);
        let f = forms(&["афаня включи свет", "включи свет", ""]);
        assert!(matches!(decide(&w, &c, false, &f), WakeDecision::Process { woke: true, .. }));
        // «эй дом» — оба слова должны быть во фразе
        let only_dom = forms(&["дом это хорошо", "", ""]);
        let (w2, c2) = cfg(&["эй дом"]);
        assert_eq!(decide(&w2, &c2, false, &only_dom), WakeDecision::Ignore);
        let both = forms(&["эй дом включи свет", "", ""]);
        assert!(matches!(decide(&w2, &c2, false, &both), WakeDecision::Process { woke: true, .. }));
    }

    #[test]
    fn min_engines_requires_agreement_about_the_wake_word() {
        let (w, mut c) = cfg(&["нафаня"]);
        c.min_engines = 2;
        let one = forms(&["нафаня включи свет", "включи свет", "включи свет"]);
        assert_eq!(decide(&w, &c, false, &one), WakeDecision::Ignore);
        let two = forms(&["нафаня включи свет", "нафаня включи свет", "включи свет"]);
        assert!(matches!(decide(&w, &c, false, &two), WakeDecision::Process { woke: true, .. }));
    }

    #[test]
    fn number_survives_cutting_the_wake_word() {
        let (w, c) = cfg(&["дом"]);
        let f = forms(&["дом громкость пятьдесят", "дом громкость пятьдесят", "дом громкость пятьдесят"]);
        match decide(&w, &c, false, &f) {
            WakeDecision::Process { forms: out, .. } => assert_eq!(out[0].number, Some(50)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn window_expires() {
        let mut s = WakeState::default();
        let t0 = Instant::now();
        assert!(!s.is_awake(t0));
        s.open(t0, Duration::from_secs(6));
        assert!(s.is_awake(t0 + Duration::from_secs(5)));
        assert!(!s.is_awake(t0 + Duration::from_secs(7)));
        s.open(t0, Duration::from_secs(6));
        s.close();
        assert!(!s.is_awake(t0));
    }

    #[test]
    fn config_defaults_do_not_change_existing_behaviour() {
        let c = WakeConfig::default();
        assert!(!c.enabled);
        assert!(c.words.is_empty());
        assert_eq!(c.ack, "Слушаю.");
        assert_eq!(MatchingConfig::default().max_extra_words, 2);
    }
}
