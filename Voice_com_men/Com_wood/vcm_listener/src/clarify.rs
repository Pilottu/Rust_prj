#![allow(dead_code)]

// Уточняющие вопросы. Если фраза не нашла команду, а слова из команд в ней есть, программа
// выясняет, КАКИХ категорий не хватает (категория — примечание дерева: «Локация» — где?,
// «Объект» — что?, «Команда» — что сделать?), спрашивает об этом и ждёт ответ, как после
// кодового слова. Ответ подставляется в исходную фразу; не получилось — тогда уже
// «Такой команды нет.». Если же слов названо достаточно, но подходят несколько команд
// («одинаковых локаций несколько»: «спальня» и на втором этаже, и на мансарде) — спрашивается
// «Уточни», и ответ на него обрабатывается по образцу ответа на «Где?».
// Здесь только чистая логика: без звука и без SQL.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::Deserialize;
use vcm_norm::{CommandIndex, Match, SpeechForm};

use crate::dialog::{judge, match_forms, Evaluation, Outcome};

/// Секция [clarify] файла config.toml.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct ClarifyConfig {
    pub enabled: bool,
    /// Сколько секунд ждать ответ на уточняющий вопрос.
    pub window_s: f32,
    /// Сколько вопросов подряд можно задать про одну фразу.
    pub max_rounds: usize,
    /// В каком порядке спрашивать категории (названия — примечания деревьев).
    pub order: Vec<String>,
    /// Свои тексты вопросов: [clarify.questions] локация = "Где именно?"
    pub questions: HashMap<String, String>,
    /// Вопрос, когда подходит несколько команд («одинаковых локаций несколько»):
    /// [clarify] ambiguous_question = "Уточните: какая именно?"
    pub ambiguous_question: String,
}

impl Default for ClarifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_s: 8.0,
            max_rounds: 3,
            order: vec!["Команда".to_string(), "Объект".to_string(), "Локация".to_string()],
            questions: HashMap::new(),
            ambiguous_question: "Уточни".to_string(),
        }
    }
}

impl ClarifyConfig {
    /// Текст вопроса по категории: свой из конфига, иначе стандартный.
    pub fn question_for(&self, category: &str) -> String {
        let key = category.to_lowercase();
        if let Some((_, q)) = self.questions.iter().find(|(k, _)| k.to_lowercase() == key) {
            return q.clone();
        }
        match key.as_str() {
            "локация" => "Где?".to_string(),
            "объект" => "Что?".to_string(),
            "команда" => "Что сделать?".to_string(),
            _ => format!("Уточните: {category}."),
        }
    }

    /// Текст «Уточни», когда подходит несколько команд: свой из конфига, иначе стандартный.
    pub fn disambiguation_question(&self) -> String {
        if self.ambiguous_question.trim().is_empty() {
            "Уточни".to_string()
        } else {
            self.ambiguous_question.clone()
        }
    }
}

/// Слова команд по категориям (из таблицы dbo.CommandVocab).
#[derive(Debug, Clone, Default)]
pub struct Vocabulary {
    by_stem: HashMap<String, Vec<usize>>,
    categories: Vec<String>,
}

impl Vocabulary {
    pub fn new(entries: &[(String, String)], order: &[String]) -> Self {
        let mut names: Vec<String> = Vec::new();
        for (_, category) in entries {
            if !names.iter().any(|n| n.to_lowercase() == category.to_lowercase()) {
                names.push(category.clone());
            }
        }
        // порядок вопросов: сначала по настройке, остальные по алфавиту
        let mut categories: Vec<String> = Vec::new();
        for wanted in order {
            if let Some(n) = names.iter().find(|n| n.to_lowercase() == wanted.to_lowercase()) {
                if !categories.contains(n) {
                    categories.push(n.clone());
                }
            }
        }
        let mut rest: Vec<String> = names.into_iter().filter(|n| !categories.contains(n)).collect();
        rest.sort();
        categories.extend(rest);

        let mut by_stem: HashMap<String, Vec<usize>> = HashMap::new();
        for (stem, category) in entries {
            if let Some(i) = categories.iter().position(|c| c.to_lowercase() == category.to_lowercase()) {
                let list = by_stem.entry(stem.clone()).or_default();
                if !list.contains(&i) {
                    list.push(i);
                }
            }
        }
        Self { by_stem, categories }
    }

    pub fn is_empty(&self) -> bool {
        self.categories.is_empty()
    }

    pub fn categories(&self) -> &[String] {
        &self.categories
    }

    pub fn len(&self) -> usize {
        self.by_stem.len()
    }

    /// Номера категорий, слова которых есть хотя бы в одной из фраз.
    pub fn present(&self, forms: &[SpeechForm]) -> HashSet<usize> {
        forms
            .iter()
            .flat_map(|f| f.stems.iter())
            .filter_map(|s| self.by_stem.get(s))
            .flatten()
            .copied()
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClarifyPlan {
    Ask { category: String, question: String },
    /// Названо всё, но подходят несколько команд («одинаковых локаций несколько») —
    /// спросить «Уточни»; ответ обрабатывается по образцу ответа на «Где?».
    AskDisambiguation { question: String },
    /// Уточнять нечего или нельзя (лимит вопросов): остаётся «Такой команды нет.».
    GiveUp,
}

/// Подходят несколько команд: все слова фразы входят в несколько разных команд
/// (как «спальня» на втором этаже и на мансарде). Их различают слова, которых во
/// фразе ещё нет, — их можно досказать в ответ на «Уточни».
fn fits_several(index: &CommandIndex, forms: &[SpeechForm]) -> bool {
    forms.iter().any(|f| matches!(index.lookup_subset(f), Match::Ambiguous(_)))
}

/// Что спросить после того, как фраза `forms` не нашла команду. `asked` — сколько
/// вопросов уже задано. Сначала — про недостающую категорию; когда названо всё, но
/// подходят несколько команд, — «Уточни».
pub fn plan(
    index: &CommandIndex,
    vocab: &Vocabulary,
    cfg: &ClarifyConfig,
    forms: &[SpeechForm],
    asked: usize,
) -> ClarifyPlan {
    if !cfg.enabled || vocab.is_empty() || asked >= cfg.max_rounds {
        return ClarifyPlan::GiveUp;
    }
    let present = vocab.present(forms);
    if present.is_empty() {
        return ClarifyPlan::GiveUp; // слова из команд есть, но ни к одной категории они не относятся
    }
    match vocab.categories().iter().enumerate().find(|(i, _)| !present.contains(i)) {
        Some((_, category)) => {
            ClarifyPlan::Ask { category: category.clone(), question: cfg.question_for(category) }
        }
        None if fits_several(index, forms) => {
            ClarifyPlan::AskDisambiguation { question: cfg.disambiguation_question() }
        }
        None => ClarifyPlan::GiveUp, // названо всё, и такая команда одна — её просто нет
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingClarify {
    /// Исходная фраза каждого движка (с накопленными уточнениями).
    pub forms: Vec<SpeechForm>,
    /// Сколько вопросов уже задано.
    pub round: usize,
    /// Текст заданного вопроса («Где?», «Что сделать?», «Уточни», …).
    pub question: String,
    deadline: Instant,
}

#[derive(Default)]
pub struct Clarify {
    pending: Option<PendingClarify>,
}

impl Clarify {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ask(&mut self, forms: Vec<SpeechForm>, question: String, round: usize, now: Instant, window: Duration) {
        self.pending = Some(PendingClarify { forms, round, question, deadline: now + window });
    }

    pub fn active(&mut self, now: Instant) -> Option<PendingClarify> {
        if matches!(&self.pending, Some(p) if now >= p.deadline) {
            self.pending = None;
        }
        self.pending.clone()
    }

    pub fn clear(&mut self) {
        self.pending = None;
    }
}

/// Результат разбора ответа на уточняющий вопрос.
#[derive(Debug, Clone)]
pub enum AnswerResult {
    /// Ответ сам оказался готовой командой (пользователь не стал отвечать на вопрос).
    Fresh(Evaluation),
    /// Ответ подставлен в исходную фразу, и команда нашлась.
    Merged(Evaluation),
    /// Не получилось. `ambiguous` — подходит несколько команд (нужно ещё уточнение).
    Failed { merged: Vec<SpeechForm>, ambiguous: bool },
    /// Не ответ: тишина, шум или чужой разговор. Вопрос остаётся в силе.
    Noise,
}

fn succeeded(o: &Outcome) -> bool {
    matches!(o, Outcome::Accepted { .. } | Outcome::AskNumber { .. } | Outcome::OutOfRange { .. })
}

pub fn answer(
    index: &CommandIndex,
    vocab: &Vocabulary,
    pending: &PendingClarify,
    forms: &[SpeechForm],
    has_text: &[bool],
    min_agree: usize,
    max_extra: usize,
) -> AnswerResult {
    if !has_text.iter().any(|h| *h) {
        return AnswerResult::Noise;
    }
    let vocab_hit: Vec<bool> = forms.iter().map(|f| index.vocab_hit(f)).collect();
    // Ответ — это слова из команд или из категорий? Иначе это чужой разговор.
    if !vocab_hit.iter().any(|h| *h) && vocab.present(forms).is_empty() {
        return AnswerResult::Noise;
    }

    // 1) ответ сам по себе — готовая команда?
    let alone = judge(index, match_forms(index, forms, has_text, max_extra, false), has_text, &vocab_hit, min_agree);
    if succeeded(&alone.outcome) {
        return AnswerResult::Fresh(alone);
    }

    // 2) подставляем ответ в исходную фразу
    let merged: Vec<SpeechForm> = pending.forms.iter().zip(forms).map(|(a, b)| a.merged(b)).collect();
    let all_text = vec![true; merged.len()];
    let merged_hit: Vec<bool> = merged.iter().map(|f| index.vocab_hit(f)).collect();
    let matches = match_forms(index, &merged, &all_text, max_extra, true);
    let eval = judge(index, matches, &all_text, &merged_hit, min_agree);
    if succeeded(&eval.outcome) {
        return AnswerResult::Merged(eval);
    }
    let ambiguous = eval.matches.iter().any(|m| matches!(m, Match::Ambiguous(_)));
    AnswerResult::Failed { merged, ambiguous }
}

/// Что делать после неудачного ответа: спросить ещё или сдаться.
pub fn next_step(
    index: &CommandIndex,
    vocab: &Vocabulary,
    cfg: &ClarifyConfig,
    pending: &PendingClarify,
    merged: &[SpeechForm],
    ambiguous: bool,
) -> ClarifyPlan {
    if pending.round >= cfg.max_rounds {
        return ClarifyPlan::GiveUp;
    }
    if ambiguous {
        // Подходит несколько команд («одинаковых локаций несколько»): спрашиваем «Уточни»;
        // ответ обрабатывается по образцу ответа на «Где?» — см. `answer`.
        return ClarifyPlan::AskDisambiguation { question: cfg.disambiguation_question() };
    }
    plan(index, vocab, cfg, merged, pending.round)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voting::Verdict;
    use vcm_norm::{command_form, speech_form, IndexEntry};

    fn index(phrases: &[(&str, &str)]) -> CommandIndex {
        let mut idx = CommandIndex::new();
        for (cmd, phrase) in phrases {
            let f = command_form(phrase);
            idx.insert(IndexEntry { cmd_key: cmd.to_string(), match_key: f.key, slot: f.slot });
        }
        idx
    }

    fn real_index() -> CommandIndex {
        index(&[
            ("LAMP", "Включить Освещение Второй этаж спальня"),
            ("LAMP", "Включить Свет Второй этаж спальня"),
            ("HEAT", "Сделай Теплый пол Мансарда спальня 0-100"),
            ("HEAT", "Поставь Теплый пол Мансарда спальня 0-100"),
            ("VOL", "Сделай Громкость Телевизор Второй этаж зал 0-100"),
        ])
    }

    /// Словарь категорий, как его пишет основная программа: слова ВСЕХ узлов деревьев.
    fn real_vocab(cfg: &ClarifyConfig) -> Vocabulary {
        let mut entries: Vec<(String, String)> = Vec::new();
        for (category, words) in [
            ("Локация", vec!["Мансарда", "Второй этаж", "Первый этаж", "спальня", "зал", "кухня", "ванна", "туалет"]),
            ("Объект", vec!["Освещение", "свет", "Люстра", "Теплый пол", "Громкость", "Телевизор", "Кондиционер"]),
            ("Команда", vec!["Включить", "Выключить", "Сделай", "Поставь", "Установи", "Увеличить", "Уменьшить"]),
        ] {
            for w in words {
                for stem in command_form(w).key.split(' ') {
                    entries.push((stem.to_string(), category.to_string()));
                }
            }
        }
        Vocabulary::new(&entries, &cfg.order)
    }

    fn forms(texts: [&str; 3]) -> Vec<SpeechForm> {
        texts.iter().map(|t| speech_form(t)).collect()
    }

    fn has(texts: [&str; 3]) -> Vec<bool> {
        texts.iter().map(|t| !t.trim().is_empty()).collect()
    }

    fn pending(f: Vec<SpeechForm>, question: &str, round: usize) -> PendingClarify {
        let mut c = Clarify::new();
        c.ask(f, question.to_string(), round, Instant::now(), Duration::from_secs(60));
        c.active(Instant::now()).unwrap()
    }

    // --- что спрашивать ------------------------------------------------------------------

    #[test]
    fn missing_place_is_asked_where() {
        let cfg = ClarifyConfig::default();
        let v = real_vocab(&cfg);
        let f = forms(["включи свет", "включи свет", "включи свет"]);
        assert_eq!(
            plan(&real_index(), &v, &cfg, &f, 0),
            ClarifyPlan::Ask { category: "Локация".into(), question: "Где?".into() }
        );
    }

    #[test]
    fn missing_command_and_missing_object_follow_the_configured_order() {
        let cfg = ClarifyConfig::default();
        let v = real_vocab(&cfg);
        let idx = real_index();
        // только место: сначала «Что сделать?», потом «Что?»
        let only_place = forms(["в спальне", "в спальне", "в спальне"]);
        assert_eq!(plan(&idx, &v, &cfg, &only_place, 0), ClarifyPlan::Ask { category: "Команда".into(), question: "Что сделать?".into() });
        // команда и место названы, объекта нет
        let no_object = forms(["включи в спальне", "включи в спальне", "включи в спальне"]);
        assert_eq!(plan(&idx, &v, &cfg, &no_object, 0), ClarifyPlan::Ask { category: "Объект".into(), question: "Что?".into() });
        // только объект
        let only_object = forms(["громкость", "громкость", "громкость"]);
        assert_eq!(plan(&idx, &v, &cfg, &only_object, 0), ClarifyPlan::Ask { category: "Команда".into(), question: "Что сделать?".into() });
    }

    #[test]
    fn everything_named_but_no_such_command_gives_up() {
        let cfg = ClarifyConfig::default();
        let v = real_vocab(&cfg);
        // «ванна» есть в «Локации», хотя света в ванной нет: место названо, переспрашивать нечего
        let f = forms(["включи свет в ванной", "включи свет в ванной", "включи свет в ванной"]);
        assert_eq!(plan(&real_index(), &v, &cfg, &f, 0), ClarifyPlan::GiveUp);
    }

    #[test]
    fn words_union_across_engines_is_enough() {
        let cfg = ClarifyConfig::default();
        let v = real_vocab(&cfg);
        // один движок расслышал команду, другой — объект: вместе есть всё, кроме места
        let f = forms(["включи", "свет", "включи свет"]);
        assert_eq!(
            plan(&real_index(), &v, &cfg, &f, 0),
            ClarifyPlan::Ask { category: "Локация".into(), question: "Где?".into() }
        );
    }

    #[test]
    fn limits_and_switches() {
        let mut cfg = ClarifyConfig::default();
        let v = real_vocab(&cfg);
        let f = forms(["включи свет", "включи свет", "включи свет"]);
        assert_eq!(plan(&real_index(), &v, &cfg, &f, 3), ClarifyPlan::GiveUp, "лимит вопросов");
        cfg.enabled = false;
        assert_eq!(plan(&real_index(), &v, &cfg, &f, 0), ClarifyPlan::GiveUp);
        cfg.enabled = true;
        assert_eq!(plan(&real_index(), &Vocabulary::default(), &cfg, &f, 0), ClarifyPlan::GiveUp, "словаря нет — уточнять нечего");
        // слова из команд есть, но ни одна категория их не знает
        let unknown = forms(["а", "а", "а"]);
        assert_eq!(plan(&real_index(), &v, &cfg, &unknown, 0), ClarifyPlan::GiveUp);
    }

    #[test]
    fn custom_questions_and_unknown_categories() {
        let mut cfg = ClarifyConfig::default();
        cfg.questions.insert("ЛОКАЦИЯ".to_string(), "Где именно?".to_string());
        assert_eq!(cfg.question_for("Локация"), "Где именно?");
        assert_eq!(cfg.question_for("Объект"), "Что?");
        assert_eq!(cfg.question_for("Время"), "Уточните: Время.");
    }

    #[test]
    fn category_order_comes_from_config_then_alphabet() {
        let entries = vec![
            ("а".to_string(), "Зона".to_string()),
            ("б".to_string(), "Локация".to_string()),
            ("в".to_string(), "Команда".to_string()),
            ("г".to_string(), "Время".to_string()),
        ];
        let v = Vocabulary::new(&entries, &["команда".to_string(), "локация".to_string()]);
        assert_eq!(v.categories(), ["Команда", "Локация", "Время", "Зона"]);
    }

    // --- «Уточни»: несколько подходящих команд -------------------------------------------

    #[test]
    fn several_identical_rooms_ask_to_specify_even_when_all_words_are_named() {
        let cfg = ClarifyConfig::default();
        let v = real_vocab(&cfg);
        let idx = index(&[
            ("LAMP2", "Включить Свет Второй этаж спальня"),
            ("LAMP1", "Включить Свет Мансарда спальня"),
        ]);
        // всё названо, но какая спальня — неясно
        let f = forms(["включи свет в спальне", "включи свет в спальне", "включи свет в спальне"]);
        assert_eq!(plan(&idx, &v, &cfg, &f, 0), ClarifyPlan::AskDisambiguation { question: "Уточни".into() });
        // место ещё не названо — сначала обычный «Где?»
        let no_place = forms(["включи свет", "включи свет", "включи свет"]);
        assert_eq!(plan(&idx, &v, &cfg, &no_place, 0), ClarifyPlan::Ask { category: "Локация".into(), question: "Где?".into() });
        // ванной в командах нет — переспрашивать нечего
        let bath = forms(["включи свет в ванной", "включи свет в ванной", "включи свет в ванной"]);
        assert_eq!(plan(&idx, &v, &cfg, &bath, 0), ClarifyPlan::GiveUp);
    }

    #[test]
    fn the_specify_question_is_configurable() {
        let mut cfg = ClarifyConfig::default();
        assert_eq!(cfg.disambiguation_question(), "Уточни");
        cfg.ambiguous_question = "Уточните: какая именно?".to_string();
        assert_eq!(cfg.disambiguation_question(), "Уточните: какая именно?");
        cfg.ambiguous_question = "   ".to_string();
        assert_eq!(cfg.disambiguation_question(), "Уточни", "пустой текст — стандартный вопрос");
    }

    // --- ответ на вопрос ------------------------------------------------------------------

    #[test]
    fn the_answer_is_substituted_into_the_phrase() {
        let cfg = ClarifyConfig::default();
        let (idx, v) = (real_index(), real_vocab(&cfg));
        // «включи свет» -> «Где?» -> «в спальне» -> единственная спальня со светом
        let p = pending(forms(["включи свет", "включи свет", "включи свет"]), "Где?", 1);
        let ans = ["в спальне", "в спальне", "в спальне"];
        match answer(&idx, &v, &p, &forms(ans), &has(ans), 2, 2) {
            AnswerResult::Merged(e) => assert_eq!(e.outcome, Outcome::Accepted { cmd_key: "LAMP".into(), number: None }),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn one_engine_mishearing_the_answer_does_not_break_it() {
        let cfg = ClarifyConfig::default();
        let (idx, v) = (real_index(), real_vocab(&cfg));
        let p = pending(forms(["включи свет", "включи свет", "включи свет"]), "Где?", 1);
        let ans = ["в спальне", "в спалне", "в спальне"];
        assert!(matches!(answer(&idx, &v, &p, &forms(ans), &has(ans), 2, 2), AnswerResult::Merged(_)));
    }

    #[test]
    fn answering_with_a_full_command_runs_that_command_instead() {
        let cfg = ClarifyConfig::default();
        let (idx, v) = (real_index(), real_vocab(&cfg));
        let p = pending(forms(["включи свет", "включи свет", "включи свет"]), "Где?", 1);
        let ans = ["сделай теплый пол мансарда спальня сорок"; 3];
        match answer(&idx, &v, &p, &forms(ans), &has(ans), 2, 2) {
            AnswerResult::Fresh(e) => assert_eq!(e.outcome, Outcome::Accepted { cmd_key: "HEAT".into(), number: Some(40) }),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_answer_that_finds_no_command_fails_and_the_user_gets_no_such_command() {
        let cfg = ClarifyConfig::default();
        let (idx, v) = (real_index(), real_vocab(&cfg));
        // «включи свет» -> «Где?» -> «в зале»: света в зале нет
        let p = pending(forms(["включи свет", "включи свет", "включи свет"]), "Где?", 1);
        let ans = ["в зале", "в зале", "в зале"];
        match answer(&idx, &v, &p, &forms(ans), &has(ans), 2, 2) {
            AnswerResult::Failed { merged, ambiguous } => {
                assert!(!ambiguous);
                assert_eq!(next_step(&idx, &v, &cfg, &p, &merged, ambiguous), ClarifyPlan::GiveUp);
            }
            other => panic!("{other:?}"),
        }
        // место, которого нет ни в одной команде («ванная»), — тоже ответ, а не шум
        let ans = ["в ванной", "в ванной", "в ванной"];
        assert!(matches!(answer(&idx, &v, &p, &forms(ans), &has(ans), 2, 2), AnswerResult::Failed { .. }));
    }

    #[test]
    fn chatter_and_silence_are_not_answers_and_keep_the_question_open() {
        let cfg = ClarifyConfig::default();
        let (idx, v) = (real_index(), real_vocab(&cfg));
        let p = pending(forms(["включи свет", "включи свет", "включи свет"]), "Где?", 1);
        let chat = ["вчера купили новый диван", "вчера купили новый диван", "вчера купили новый диван"];
        assert!(matches!(answer(&idx, &v, &p, &forms(chat), &has(chat), 2, 2), AnswerResult::Noise));
        let silence = ["", "", ""];
        assert!(matches!(answer(&idx, &v, &p, &forms(silence), &has(silence), 2, 2), AnswerResult::Noise));
    }

    #[test]
    fn a_command_with_a_number_slot_continues_into_the_number_question() {
        let cfg = ClarifyConfig::default();
        let (idx, v) = (real_index(), real_vocab(&cfg));
        // «громкость» -> «Что сделать?» -> «сделай» -> команда одна, нужно число
        let p = pending(forms(["громкость", "громкость", "громкость"]), "Что сделать?", 1);
        let ans = ["сделай", "сделай", "сделай"];
        match answer(&idx, &v, &p, &forms(ans), &has(ans), 2, 2) {
            AnswerResult::Merged(e) => assert!(matches!(e.outcome, Outcome::AskNumber { ref cmd_key, .. } if cmd_key == "VOL")),
            other => panic!("{other:?}"),
        }
        // «сделай теплый пол» -> «Где?» -> «в спальне на мансарде сорок» -> сразу с числом
        let p = pending(forms(["сделай теплый пол"; 3]), "Где?", 1);
        let ans = ["в спальне мансарды сорок"; 3];
        match answer(&idx, &v, &p, &forms(ans), &has(ans), 2, 2) {
            AnswerResult::Merged(e) => assert_eq!(e.outcome, Outcome::Accepted { cmd_key: "HEAT".into(), number: Some(40) }),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn two_matching_rooms_are_asked_again_and_the_second_answer_decides() {
        let cfg = ClarifyConfig::default();
        let idx = index(&[
            ("LAMP2", "Включить Свет Второй этаж спальня"),
            ("LAMP1", "Включить Свет Мансарда спальня"),
        ]);
        let v = real_vocab(&cfg);
        let first = pending(forms(["включи свет"; 3]), "Где?", 1);
        let ans1 = ["в спальне"; 3];
        let merged = match answer(&idx, &v, &first, &forms(ans1), &has(ans1), 2, 2) {
            AnswerResult::Failed { merged, ambiguous } => {
                assert!(ambiguous, "две спальни — нужно ещё уточнение");
                assert_eq!(
                    next_step(&idx, &v, &cfg, &first, &merged, ambiguous),
                    ClarifyPlan::AskDisambiguation { question: "Уточни".into() }
                );
                merged
            }
            other => panic!("{other:?}"),
        };
        // ответ на «Уточни» обрабатывается по образцу ответа на «Где?»: подстановка в исходную фразу
        let second = pending(merged, "Уточни", 2);
        let ans2 = ["на втором этаже"; 3];
        match answer(&idx, &v, &second, &forms(ans2), &has(ans2), 2, 2) {
            AnswerResult::Merged(e) => assert_eq!(e.outcome, Outcome::Accepted { cmd_key: "LAMP2".into(), number: None }),
            other => panic!("{other:?}"),
        }
        // зеркально: «на мансарде» выбирает другую спальню
        let second = pending(forms(["включи свет в спальне"; 3]), "Уточни", 2);
        let ans2 = ["на мансарде"; 3];
        match answer(&idx, &v, &second, &forms(ans2), &has(ans2), 2, 2) {
            AnswerResult::Merged(e) => assert_eq!(e.outcome, Outcome::Accepted { cmd_key: "LAMP1".into(), number: None }),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_number_of_questions_is_limited() {
        let cfg = ClarifyConfig { max_rounds: 2, ..ClarifyConfig::default() };
        let v = real_vocab(&cfg);
        let p = pending(forms(["включи свет"; 3]), "Где?", 2);
        let merged = forms(["включи свет"; 3]);
        assert_eq!(next_step(&real_index(), &v, &cfg, &p, &merged, true), ClarifyPlan::GiveUp);
        assert_eq!(next_step(&real_index(), &v, &cfg, &p, &merged, false), ClarifyPlan::GiveUp);
    }

    #[test]
    fn the_question_expires() {
        let mut c = Clarify::new();
        let t0 = Instant::now();
        c.ask(vec![], "Где?".into(), 1, t0, Duration::from_secs(8));
        assert!(c.active(t0 + Duration::from_secs(3)).is_some());
        assert!(c.active(t0 + Duration::from_secs(9)).is_none());
        c.ask(vec![], "Где?".into(), 1, t0, Duration::from_secs(8));
        c.clear();
        assert!(c.active(t0).is_none());
    }

    #[test]
    fn verdict_type_is_the_voting_one() {
        // защита от случайной смены типов: исход строится на тех же вердиктах голосования
        let _ = Verdict::NothingRecognized;
    }

    #[test]
    fn defaults_are_what_the_user_asked_for() {
        let c = ClarifyConfig::default();
        assert!(c.enabled);
        assert_eq!(c.max_rounds, 3);
        assert_eq!(c.question_for("локация"), "Где?");
        assert_eq!(c.question_for("Объект"), "Что?");
        assert_eq!(c.question_for("Команда"), "Что сделать?");
    }
}