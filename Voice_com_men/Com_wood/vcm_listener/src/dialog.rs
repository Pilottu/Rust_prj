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
            // «Такой команды нет» — только если слова расслышали хотя бы двое.
            if views.iter().filter(|v| v.has_text).count() >= 2 {
                Outcome::NotACommand
            } else {
                Outcome::Silence
            }
        }
    }
}

/// Текст, который нужно произнести (None — промолчать).
pub fn prompt_for(outcome: &Outcome, confirm: bool) -> Option<String> {
    match outcome {
        Outcome::Accepted { .. } => confirm.then(|| "Принято.".to_string()),
        Outcome::AskNumber { slot, .. } => {
            Some(format!("Какое значение? От {} до {}.", slot.min, slot.max))
        }
        Outcome::OutOfRange { slot, .. } => {
            Some(format!("Значение вне диапазона. Назовите число от {} до {}.", slot.min, slot.max))
        }
        Outcome::NotACommand => Some("Такой команды нет.".to_string()),
        Outcome::Unclear => Some("Не понял. Повторите, пожалуйста.".to_string()),
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

    fn view(has_text: bool, matched: Match) -> EngineView {
        EngineView { has_text, matched }
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
        assert_eq!(prompt_for(&o, false), None);
        assert_eq!(prompt_for(&o, true), Some("Принято.".to_string()));
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
        assert_eq!(prompt_for(&o, false), Some("Какое значение? От 0 до 100.".to_string()));
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
        assert_eq!(prompt_for(&o, false), Some("Не понял. Повторите, пожалуйста.".to_string()));
    }

    #[test]
    fn heard_words_but_no_such_command() {
        let views = [view(true, Match::None), view(true, Match::None), view(true, Match::None)];
        let o = classify(&Verdict::NothingRecognized, &views, &slot_of);
        assert_eq!(o, Outcome::NotACommand);
        assert_eq!(prompt_for(&o, false), Some("Такой команды нет.".to_string()));
    }

    #[test]
    fn single_engine_noise_stays_silent() {
        let views = [view(true, Match::None), view(false, Match::None), view(false, Match::None)];
        let o = classify(&Verdict::NothingRecognized, &views, &slot_of);
        assert_eq!(o, Outcome::Silence);
        assert_eq!(prompt_for(&o, true), None);
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
            prompt_for(&o, false),
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
}
