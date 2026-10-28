#![allow(dead_code)]

// Логика диалога: что слушатель говорит в каждой ситуации и как принимает ответ-число
// на вопрос «какое значение?». Без звука и без зависимостей — чистые функции, их можно
// проверять тестами.

use std::time::{Duration, Instant};

use vcm_norm::{speech_form, Match, Slot};

use crate::voting::Verdict;

/// Итог разбора фразы с точки зрения пользователя.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Команда принята (число — если у команды есть слот).
    Accepted { cmd_key: String, number: Option<u32> },
    /// Команда ясна, но нужно число.
    AskNumber { cmd_key: String, slot: Slot },
    /// Число вне диапазона слота.
    OutOfRange { cmd_key: String, slot: Slot },
    /// Движки расслышали слова, но такой команды нет.
    NotACommand,
    /// Не поняли: движки разошлись или команда неоднозначна.
    Unclear,
    /// Шум или одиночное слово без команды: молчим, чтобы не болтать на каждый кашель.
    Silence,
}

/// Что услышал и как разобрал один движок.
#[derive(Debug, Clone)]
pub struct EngineView {
    pub has_text: bool,
    /// Среди услышанных слов есть хотя бы одно слово из команд в базе.
    pub vocab_hit: bool,
    pub matched: Match,
}

pub fn classify(
    verdict: &Verdict,
    views: &[EngineView],
    slot_of: &dyn Fn(&str) -> Option<Slot>,
) -> Outcome {
    match verdict {
        Verdict::Accepted { interpretation, .. } => {
            match (slot_of(&interpretation.cmd_key), interpretation.number) {
                (Some(slot), None) => {
                    Outcome::AskNumber { cmd_key: interpretation.cmd_key.clone(), slot }
                }
                (_, number) => Outcome::Accepted { cmd_key: interpretation.cmd_key.clone(), number },
            }
        }
        Verdict::NeedNumber { cmd_key } => match slot_of(cmd_key) {
            Some(slot) => Outcome::AskNumber { cmd_key: cmd_key.clone(), slot },
            None => Outcome::Unclear,
        },
        Verdict::NoAgreement => Outcome::Unclear,
        Verdict::NothingRecognized => {
            let out_of_range = views.iter().find_map(|v| match &v.matched {
                Match::OutOfRange { cmd_key, slot, .. } => Some((cmd_key.clone(), *slot)),
                _ => None,
            });
            if let Some((cmd_key, slot)) = out_of_range {
                return Outcome::OutOfRange { cmd_key, slot };
            }
            if views.iter().any(|v| matches!(v.matched, Match::Ambiguous(_))) {
                return Outcome::Unclear;
            }
            // «Такой команды нет» — только если слова расслышали хотя бы двое И среди них было
            // хоть одно слово из команд в базе; иначе это просто чужой разговор — молчим.
            if views.iter().filter(|v| v.has_text).count() >= 2 && views.iter().any(|v| v.vocab_hit) {
                Outcome::NotACommand
            } else {
                Outcome::Silence
            }
        }
    }
}

/// Что говорить вслух, а о чём молчать.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptPolicy {
    /// «Принято.» после выполненной команды.
    pub confirm: bool,
    /// «Такой команды нет.» На обычную болтовню в комнате лучше не отвечать.
    pub say_not_a_command: bool,
    /// «Не понял. Повторите, пожалуйста.»
    pub say_unclear: bool,
}

impl Default for PromptPolicy {
    fn default() -> Self {
        Self { confirm: false, say_not_a_command: false, say_unclear: true }
    }
}

/// Текст, который нужно произнести (None — промолчать).
pub fn prompt_for(outcome: &Outcome, policy: &PromptPolicy) -> Option<String> {
    match outcome {
        Outcome::Accepted { .. } => policy.confirm.then(|| "Принято.".to_string()),
        Outcome::AskNumber { slot, .. } => {
            Some(format!("Какое значение? От {} до {}.", slot.min, slot.max))
        }
        Outcome::OutOfRange { slot, .. } => {
            Some(format!("Значение вне диапазона. Назовите число от {} до {}.", slot.min, slot.max))
        }
        Outcome::NotACommand => policy.say_not_a_command.then(|| "Такой команды нет.".to_string()),
        Outcome::Unclear => policy.say_unclear.then(|| "Не понял. Повторите, пожалуйста.".to_string()),
        Outcome::Silence => None,
    }
}

// ---------------------------------------------------------------------------
// Ожидание ответа-числа
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub cmd_key: String,
    pub slot: Slot,
    deadline: Instant,
}

#[derive(Default)]
pub struct Dialog {
    pending: Option<Pending>,
}

impl Dialog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Запомнить, что ждём число для команды `cmd_key`.
    pub fn ask(&mut self, cmd_key: String, slot: Slot, now: Instant, window: Duration) {
        self.pending = Some(Pending { cmd_key, slot, deadline: now + window });
    }

    /// Действующий вопрос (просроченный сбрасывается).
    pub fn active(&mut self, now: Instant) -> Option<Pending> {
        if matches!(&self.pending, Some(p) if now >= p.deadline) {
            self.pending = None;
        }
        self.pending.clone()
    }

    pub fn clear(&mut self) {
        self.pending = None;
    }
}

/// Если как минимум два движка услышали ТОЛЬКО число («пятьдесят»), разбираем фразу как ответ
/// на вопрос; иначе None — это обычная новая команда.
pub fn followup_views(texts: &[&str], pending: &Pending) -> Option<Vec<EngineView>> {
    let forms: Vec<_> = texts.iter().map(|t| speech_form(t)).collect();
    let number_only = |f: &vcm_norm::SpeechForm| f.number.is_some() && f.key_without_number.is_empty();
    if forms.iter().filter(|f| number_only(f)).count() < 2 {
        return None;
    }
    Some(
        texts
            .iter()
            .zip(&forms)
            .map(|(t, f)| EngineView {
                has_text: !t.trim().is_empty(),
                vocab_hit: true,
                matched: match (number_only(f), f.number) {
                    (true, Some(n)) if pending.slot.contains(n) => {
                        Match::Exact { cmd_key: pending.cmd_key.clone(), number: Some(n) }
                    }
                    (true, Some(n)) => Match::OutOfRange {
                        cmd_key: pending.cmd_key.clone(),
                        number: n,
                        slot: pending.slot,
                    },
                    _ => Match::None,
                },
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voting::{vote, EngineVote, Interpretation};

    const SLOT: Slot = Slot { min: 0, max: 100 };

    fn slot_of(key: &str) -> Option<Slot> {
        (key == "VOL").then_some(SLOT)
    }

    /// Политика «говорить обо всём» — для проверок самих фраз.
    fn talk(confirm: bool) -> PromptPolicy {
        PromptPolicy { confirm, say_not_a_command: true, say_unclear: true }
    }

    fn view(has_text: bool, matched: Match) -> EngineView {
        EngineView { has_text, vocab_hit: true, matched }
    }

    fn view_v(has_text: bool, vocab_hit: bool, matched: Match) -> EngineView {
        EngineView { has_text, vocab_hit, matched }
    }

    fn accepted(cmd: &str, number: Option<u32>) -> Verdict {
        Verdict::Accepted {
            interpretation: Interpretation { cmd_key: cmd.into(), number },
            agreed: vec!["GigaAM", "Vosk"],
            dissent: vec![],
        }
    }

    // --- classify и prompt_for ---------------------------------------------------

    #[test]
    fn accepted_command_without_slot() {
        let o = classify(&accepted("LIGHT", None), &[], &slot_of);
        assert_eq!(o, Outcome::Accepted { cmd_key: "LIGHT".into(), number: None });
        assert_eq!(prompt_for(&o, &talk(false)), None);
        assert_eq!(prompt_for(&o, &talk(true)), Some("Принято.".to_string()));
    }

    #[test]
    fn accepted_slot_command_with_number() {
        let o = classify(&accepted("VOL", Some(50)), &[], &slot_of);
        assert_eq!(o, Outcome::Accepted { cmd_key: "VOL".into(), number: Some(50) });
    }

    #[test]
    fn slot_command_accepted_without_number_asks_for_it() {
        let o = classify(&accepted("VOL", None), &[], &slot_of);
        assert_eq!(o, Outcome::AskNumber { cmd_key: "VOL".into(), slot: SLOT });
        assert_eq!(prompt_for(&o, &talk(false)), Some("Какое значение? От 0 до 100.".to_string()));
    }

    #[test]
    fn engines_disagree_on_number_asks_for_it() {
        let o = classify(&Verdict::NeedNumber { cmd_key: "VOL".into() }, &[], &slot_of);
        assert!(matches!(o, Outcome::AskNumber { .. }));
        // ключ без слота — это не вопрос про число
        let o = classify(&Verdict::NeedNumber { cmd_key: "LIGHT".into() }, &[], &slot_of);
        assert_eq!(o, Outcome::Unclear);
    }

    #[test]
    fn no_agreement_means_please_repeat() {
        let o = classify(&Verdict::NoAgreement, &[], &slot_of);
        assert_eq!(o, Outcome::Unclear);
        assert_eq!(prompt_for(&o, &talk(false)), Some("Не понял. Повторите, пожалуйста.".to_string()));
    }

    #[test]
    fn heard_words_but_no_such_command() {
        let views = [view(true, Match::None), view(true, Match::None), view(true, Match::None)];
        let o = classify(&Verdict::NothingRecognized, &views, &slot_of);
        assert_eq!(o, Outcome::NotACommand);
        assert_eq!(prompt_for(&o, &talk(false)), Some("Такой команды нет.".to_string()));
    }

    #[test]
    fn single_engine_noise_stays_silent() {
        let views = [view(true, Match::None), view(false, Match::None), view(false, Match::None)];
        let o = classify(&Verdict::NothingRecognized, &views, &slot_of);
        assert_eq!(o, Outcome::Silence);
        assert_eq!(prompt_for(&o, &talk(true)), None);
        let none = [view(false, Match::None), view(false, Match::None), view(false, Match::None)];
        assert_eq!(classify(&Verdict::NothingRecognized, &none, &slot_of), Outcome::Silence);
    }

    #[test]
    fn out_of_range_is_reported_with_the_range() {
        let views = [
            view(true, Match::OutOfRange { cmd_key: "VOL".into(), number: 150, slot: SLOT }),
            view(true, Match::None),
            view(true, Match::None),
        ];
        let o = classify(&Verdict::NothingRecognized, &views, &slot_of);
        assert_eq!(o, Outcome::OutOfRange { cmd_key: "VOL".into(), slot: SLOT });
        assert_eq!(
            prompt_for(&o, &talk(false)),
            Some("Значение вне диапазона. Назовите число от 0 до 100.".to_string())
        );
    }

    #[test]
    fn ambiguous_command_is_unclear() {
        let views = [view(true, Match::Ambiguous(vec!["A".into(), "B".into()])), view(true, Match::None)];
        assert_eq!(classify(&Verdict::NothingRecognized, &views, &slot_of), Outcome::Unclear);
    }

    // --- диалог ----------------------------------------------------------------------

    #[test]
    fn pending_question_expires() {
        let mut d = Dialog::new();
        let t0 = Instant::now();
        d.ask("VOL".into(), SLOT, t0, Duration::from_secs(8));
        assert!(d.active(t0 + Duration::from_secs(3)).is_some());
        assert!(d.active(t0 + Duration::from_secs(9)).is_none());
        assert!(d.active(t0 + Duration::from_secs(1)).is_none()); // сброшен насовсем
    }

    #[test]
    fn clear_drops_the_question() {
        let mut d = Dialog::new();
        let t0 = Instant::now();
        d.ask("VOL".into(), SLOT, t0, Duration::from_secs(8));
        d.clear();
        assert!(d.active(t0).is_none());
    }

    fn pending() -> Pending {
        let mut d = Dialog::new();
        d.ask("VOL".into(), SLOT, Instant::now(), Duration::from_secs(60));
        d.active(Instant::now()).unwrap()
    }

    #[test]
    fn number_only_answer_is_attached_to_the_pending_command() {
        let p = pending();
        let views = followup_views(&["пятьдесят", "пятьдесят процентов", "пятьдесят"], &p).unwrap();
        assert!(views
            .iter()
            .all(|v| v.matched == Match::Exact { cmd_key: "VOL".into(), number: Some(50) }));
        // и голосование принимает это как команду с числом
        let votes: Vec<EngineVote> = ["GigaAM", "T-one", "Vosk"]
            .iter()
            .zip(&views)
            .map(|(e, v)| EngineVote {
                engine: e,
                interpretation: match &v.matched {
                    Match::Exact { cmd_key, number } => {
                        Some(Interpretation { cmd_key: cmd_key.clone(), number: *number })
                    }
                    _ => None,
                },
            })
            .collect();
        let verdict = vote(&votes, 2);
        assert_eq!(
            classify(&verdict, &views, &slot_of),
            Outcome::Accepted { cmd_key: "VOL".into(), number: Some(50) }
        );
    }

    #[test]
    fn two_of_three_hearing_a_number_is_enough() {
        let p = pending();
        let views = followup_views(&["сорок", "сорок", "в сорок восемь"], &p).unwrap();
        assert_eq!(views.len(), 3);
        assert_eq!(views[0].matched, Match::Exact { cmd_key: "VOL".into(), number: Some(40) });
    }

    #[test]
    fn a_new_command_is_not_an_answer() {
        let p = pending();
        assert!(followup_views(&["включи свет в зале", "включи свет в зале", "включи свет в зале"], &p).is_none());
        // число внутри обычной фразы — тоже не ответ
        assert!(followup_views(&["сделай громкость пятьдесят", "сделай громкость пятьдесят", "сделай громкость пятьдесят"], &p).is_none());
    }

    #[test]
    fn a_single_number_among_noise_is_not_enough() {
        let p = pending();
        assert!(followup_views(&["пятьдесят", "", ""], &p).is_none());
    }

    #[test]
    fn out_of_range_answer_is_reported() {
        let p = pending();
        let views = followup_views(&["сто пятьдесят", "сто пятьдесят", "сто пятьдесят"], &p).unwrap();
        let verdict = Verdict::NothingRecognized;
        assert_eq!(
            classify(&verdict, &views, &slot_of),
            Outcome::OutOfRange { cmd_key: "VOL".into(), slot: SLOT }
        );
    }

    // --- политика ответов ---------------------------------------------------------------

    #[test]
    fn default_policy_stays_silent_on_chatter_but_still_asks_about_numbers() {
        let p = PromptPolicy::default();
        assert_eq!(prompt_for(&Outcome::NotACommand, &p), None, "болтовня не должна получать ответ");
        assert_eq!(prompt_for(&Outcome::Silence, &p), None);
        assert_eq!(prompt_for(&Outcome::Accepted { cmd_key: "A".into(), number: None }, &p), None);
        assert_eq!(
            prompt_for(&Outcome::Unclear, &p),
            Some("Не понял. Повторите, пожалуйста.".to_string())
        );
        assert!(prompt_for(&Outcome::AskNumber { cmd_key: "VOL".into(), slot: SLOT }, &p).is_some());
        assert!(prompt_for(&Outcome::OutOfRange { cmd_key: "VOL".into(), slot: SLOT }, &p).is_some());
    }

    #[test]
    fn every_prompt_can_be_switched_off_or_on() {
        let off = PromptPolicy { confirm: false, say_not_a_command: false, say_unclear: false };
        assert_eq!(prompt_for(&Outcome::Unclear, &off), None);
        let on = PromptPolicy { confirm: true, say_not_a_command: true, say_unclear: true };
        assert_eq!(prompt_for(&Outcome::NotACommand, &on), Some("Такой команды нет.".to_string()));
        assert_eq!(prompt_for(&Outcome::Accepted { cmd_key: "A".into(), number: None }, &on), Some("Принято.".to_string()));
    }

    #[test]
    fn words_that_are_not_command_words_are_ignored_silently() {
        // все три услышали слова, но ни одного слова из команд в базе нет — чужой разговор
        let views = [view_v(true, false, Match::None), view_v(true, false, Match::None), view_v(true, false, Match::None)];
        let o = classify(&Verdict::NothingRecognized, &views, &slot_of);
        assert_eq!(o, Outcome::Silence);
        assert_eq!(prompt_for(&o, &talk(true)), None);
        // а если хоть один движок услышал слово из команд — «такой команды нет»
        let views = [view_v(true, false, Match::None), view_v(true, true, Match::None), view_v(true, false, Match::None)];
        assert_eq!(classify(&Verdict::NothingRecognized, &views, &slot_of), Outcome::NotACommand);
    }

    // --- сквозной сценарий на строках из настоящего лога --------------------------------------

    fn pool_like_the_real_one() -> vcm_norm::CommandIndex {
        let mut idx = vcm_norm::CommandIndex::new();
        for (cmd, phrase) in [
            ("9070-8003-9047", "Включить Освещение Второй этаж спальня"),
            ("9070-8003-9047", "Включить Свет Второй этаж спальня"),
            ("9085-9064-9041-9086", "Сделай Теплый пол Мансарда спальня 0-100"),
            ("9085-9064-9041-9086", "Поставь Теплый пол Мансарда спальня 0-100"),
            ("9085-9064-9041-9086", "Установи Теплый пол Мансарда спальня 0-100"),
            ("9085-9077-9066-9046-9086", "Сделай Громкость Телевизор Второй этаж зал 0-100"),
        ] {
            let f = vcm_norm::command_form(phrase);
            idx.insert(vcm_norm::IndexEntry { cmd_key: cmd.into(), match_key: f.key, slot: f.slot });
        }
        idx
    }

    /// Как listen.rs: формы -> разбор -> голосование -> исход.
    fn run_pipeline(idx: &vcm_norm::CommandIndex, texts: [&str; 3], max_extra: usize) -> Outcome {
        let forms: Vec<_> = texts.iter().map(|t| speech_form(t)).collect();
        let matches: Vec<Match> = forms.iter().map(|f| idx.lookup_tolerant(f, max_extra)).collect();
        let votes: Vec<EngineVote> = ["GigaAM", "T-one", "Vosk"]
            .iter()
            .zip(&matches)
            .map(|(e, m)| EngineVote {
                engine: e,
                interpretation: match m {
                    Match::Exact { cmd_key, number } => Some(Interpretation { cmd_key: cmd_key.clone(), number: *number }),
                    Match::NeedNumber { cmd_key } => Some(Interpretation { cmd_key: cmd_key.clone(), number: None }),
                    _ => None,
                },
            })
            .collect();
        let views: Vec<EngineView> = texts
            .iter()
            .zip(&forms)
            .zip(&matches)
            .map(|((t, f), m)| EngineView { has_text: !t.trim().is_empty(), vocab_hit: idx.vocab_hit(f), matched: m.clone() })
            .collect();
        classify(&vote(&votes, 2), &views, &|k| idx.slot(k))
    }

    #[test]
    fn the_phrase_from_the_log_is_now_accepted_even_when_one_engine_mishears() {
        let idx = pool_like_the_real_one();
        let o = run_pipeline(
            &idx,
            [
                "сделай на мансарде в спальне на втором этаже теплый пол сорок",
                "сделай на массаде в спальне на втором этаже теплый пол сорок", // T-one исказил «мансарде»
                "сделай на мансарде в спальне на втором этаже тёплый пол сорок",
            ],
            2,
        );
        assert_eq!(o, Outcome::Accepted { cmd_key: "9085-9064-9041-9086".into(), number: Some(40) });
        // со строгим разбором (допуск 0) та же фраза не находится — это и был «косяк»
        let strict = run_pipeline(
            &idx,
            [
                "сделай на мансарде в спальне на втором этаже теплый пол сорок",
                "сделай на массаде в спальне на втором этаже теплый пол сорок",
                "сделай на мансарде в спальне на втором этаже тёплый пол сорок",
            ],
            0,
        );
        assert_eq!(strict, Outcome::NotACommand);
    }

    #[test]
    fn small_talk_gets_no_answer_but_a_near_miss_does() {
        let idx = pool_like_the_real_one();
        // чужой разговор: слов из команд нет — молчим
        let chat = run_pipeline(&idx, ["вчера мы ездили к бабушке на дачу", "вчера мы ездили к бабушке на дачу", "вчера мы ездили к бабушке на дачу"], 2);
        assert_eq!(chat, Outcome::Silence);
        // слова команд есть (свет, спальня), но такой команды нет — отвечаем
        let near = run_pipeline(&idx, ["включи свет в ванной", "включи свет в ванной", "включи свет в ванной"], 2);
        assert_eq!(near, Outcome::NotACommand);
    }
}
