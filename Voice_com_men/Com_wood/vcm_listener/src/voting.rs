#![allow(dead_code)]

// Голосование трёх распознавателей.
//
// Сравниваем не тексты (у моделей разные формы слов, регистр, цифры/числительные),
// а РЕЗУЛЬТАТ РАЗБОРА: какую команду нашли (cmd_key) и какое число услышали.
// Команда принимается, если минимум `min_agree` движков дали одинаковый результат.

/// Результат разбора одной фразы одним движком.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interpretation {
    /// Стабильный ключ команды из таблицы пула (CmdKey).
    pub cmd_key: String,
    /// Число для слота 0-100, если команда его содержит и оно услышано.
    pub number: Option<u32>,
}

/// Что сказал один движок. `None` — фраза не сопоставилась ни с одной командой.
#[derive(Clone, Debug)]
pub struct EngineVote {
    pub engine: &'static str,
    pub interpretation: Option<Interpretation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Достаточно движков согласно. `dissent` — остальные (для лога и диагностики).
    Accepted {
        interpretation: Interpretation,
        agreed: Vec<&'static str>,
        dissent: Vec<&'static str>,
    },
    /// Сошлись на команде, но не на числе: достаточно переспросить только число.
    NeedNumber { cmd_key: String },
    /// Результаты есть, но большинства нет: переспросить команду целиком.
    NoAgreement,
    /// Ни один движок не нашёл команду.
    NothingRecognized,
}

pub fn vote(votes: &[EngineVote], min_agree: usize) -> Verdict {
    let min_agree = min_agree.max(1);

    // Группируем одинаковые разборы (порядок появления сохраняем).
    let mut groups: Vec<(Interpretation, Vec<&'static str>)> = Vec::new();
    for v in votes {
        let Some(interp) = &v.interpretation else {
            continue;
        };
        match groups.iter_mut().find(|(i, _)| i == interp) {
            Some((_, engines)) => engines.push(v.engine),
            None => groups.push((interp.clone(), vec![v.engine])),
        }
    }
    if groups.is_empty() {
        return Verdict::NothingRecognized;
    }

    let best = groups.iter().map(|(_, e)| e.len()).max().unwrap_or(0);
    if best >= min_agree {
        let mut leaders = groups.iter().filter(|(_, e)| e.len() == best);
        let first = leaders.next();
        if leaders.next().is_some() {
            return Verdict::NoAgreement; // ничья между разными разборами
        }
        if let Some((interpretation, agreed)) = first {
            let dissent = votes
                .iter()
                .map(|v| v.engine)
                .filter(|e| !agreed.contains(e))
                .collect();
            return Verdict::Accepted {
                interpretation: interpretation.clone(),
                agreed: agreed.clone(),
                dissent,
            };
        }
    }

    // Полного совпадения нет. Может, хотя бы команда общая (разошлись только по числу)?
    let mut by_cmd: Vec<(&str, usize)> = Vec::new();
    for (interp, engines) in &groups {
        match by_cmd.iter_mut().find(|(k, _)| *k == interp.cmd_key) {
            Some((_, n)) => *n += engines.len(),
            None => by_cmd.push((&interp.cmd_key, engines.len())),
        }
    }
    let best_cmd = by_cmd.iter().map(|(_, n)| *n).max().unwrap_or(0);
    if best_cmd >= min_agree {
        let mut leaders = by_cmd.iter().filter(|(_, n)| *n == best_cmd);
        if let (Some((key, _)), None) = (leaders.next(), leaders.next()) {
            return Verdict::NeedNumber { cmd_key: key.to_string() };
        }
    }
    Verdict::NoAgreement
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interp(key: &str, number: Option<u32>) -> Option<Interpretation> {
        Some(Interpretation { cmd_key: key.to_string(), number })
    }

    fn votes(g: Option<Interpretation>, t: Option<Interpretation>, v: Option<Interpretation>) -> Vec<EngineVote> {
        vec![
            EngineVote { engine: "GigaAM", interpretation: g },
            EngineVote { engine: "T-one", interpretation: t },
            EngineVote { engine: "Vosk", interpretation: v },
        ]
    }

    #[test]
    fn all_three_agree() {
        let r = vote(&votes(interp("20-11", None), interp("20-11", None), interp("20-11", None)), 2);
        match r {
            Verdict::Accepted { agreed, dissent, .. } => {
                assert_eq!(agreed.len(), 3);
                assert!(dissent.is_empty());
            }
            other => panic!("ожидали Accepted, получили {other:?}"),
        }
    }

    #[test]
    fn two_of_three_is_enough_and_dissent_is_reported() {
        let r = vote(&votes(interp("A", None), interp("B", None), interp("A", None)), 2);
        match r {
            Verdict::Accepted { interpretation, agreed, dissent } => {
                assert_eq!(interpretation.cmd_key, "A");
                assert_eq!(agreed, vec!["GigaAM", "Vosk"]);
                assert_eq!(dissent, vec!["T-one"]);
            }
            other => panic!("ожидали Accepted, получили {other:?}"),
        }
    }

    #[test]
    fn all_different_means_no_agreement() {
        let r = vote(&votes(interp("A", None), interp("B", None), interp("C", None)), 2);
        assert_eq!(r, Verdict::NoAgreement);
    }

    #[test]
    fn engine_that_found_nothing_does_not_block_the_other_two() {
        let r = vote(&votes(interp("A", None), None, interp("A", None)), 2);
        assert!(matches!(r, Verdict::Accepted { .. }));
    }

    #[test]
    fn single_command_vote_is_not_enough() {
        let r = vote(&votes(interp("A", None), None, None), 2);
        assert_eq!(r, Verdict::NoAgreement);
    }

    #[test]
    fn nothing_recognized() {
        assert_eq!(vote(&votes(None, None, None), 2), Verdict::NothingRecognized);
    }

    #[test]
    fn same_slot_number_two_of_three() {
        let r = vote(&votes(interp("VOL", Some(50)), interp("VOL", Some(15)), interp("VOL", Some(50))), 2);
        match r {
            Verdict::Accepted { interpretation, .. } => assert_eq!(interpretation.number, Some(50)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn same_command_but_numbers_differ_asks_only_for_number() {
        let r = vote(&votes(interp("VOL", Some(50)), interp("VOL", Some(15)), interp("VOL", Some(80))), 2);
        assert_eq!(r, Verdict::NeedNumber { cmd_key: "VOL".to_string() });
    }

    #[test]
    fn missing_number_is_treated_as_disagreement_on_number() {
        let r = vote(&votes(interp("VOL", Some(50)), interp("VOL", None), None), 2);
        assert_eq!(r, Verdict::NeedNumber { cmd_key: "VOL".to_string() });
    }

    #[test]
    fn tie_between_different_results_is_no_agreement() {
        // при min_agree = 1 два разных разбора набрали по одному голосу
        let r = vote(&votes(interp("A", None), interp("B", None), None), 1);
        assert_eq!(r, Verdict::NoAgreement);
    }

    // --- Сквозной пример: свободный порядок слов ---------------------------
    // Упрощённый нормализатор только для теста (в боевом коде — vcm_norm со стеммером):
    // без служебных слов, по алфавиту. Разные формулировки дают ОДИН ключ.
    fn toy_key(text: &str) -> String {
        let stop = ["в", "на", "по", "у", "и", "пожалуйста"];
        let mut words: Vec<String> = text
            .to_lowercase()
            .split_whitespace()
            .filter(|w| !stop.contains(w))
            .map(|w| w.to_string())
            .collect();
        words.sort();
        words.dedup();
        words.join(" ")
    }

    fn lookup(text: &str) -> Option<Interpretation> {
        // «словарь команд»: ключ сравнения -> CmdKey
        let dict = [(toy_key("включи свет в ванне"), "LIGHT_BATH_ON")];
        let key = toy_key(text);
        dict.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, cmd)| Interpretation { cmd_key: cmd.to_string(), number: None })
    }

    #[test]
    fn free_word_order_across_engines() {
        let v = votes(
            lookup("включи свет в ванне"),
            lookup("в ванне свет включи"),
            lookup("включи свет в ванной"), // этот движок ошибся в слове
        );
        match vote(&v, 2) {
            Verdict::Accepted { interpretation, agreed, dissent } => {
                assert_eq!(interpretation.cmd_key, "LIGHT_BATH_ON");
                assert_eq!(agreed, vec!["GigaAM", "T-one"]);
                assert_eq!(dissent, vec!["Vosk"]);
            }
            other => panic!("{other:?}"),
        }
    }
}
