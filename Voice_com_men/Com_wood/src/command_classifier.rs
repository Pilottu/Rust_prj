use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};

use crate::state::{AppState, RawItem};

static COMMAND_WORDS: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

pub fn initialize_dictionary(words: impl IntoIterator<Item = String>) {
    let phrases = words
        .into_iter()
        .map(|phrase| normalize_phrase(&phrase))
        .filter(|phrase| !phrase.is_empty())
        .collect();
    if let Ok(mut current) = dictionary().write() {
        *current = phrases;
    }
}

fn dictionary() -> &'static RwLock<HashSet<String>> {
    COMMAND_WORDS.get_or_init(|| RwLock::new(HashSet::new()))
}

pub fn has_command_phrase(phrase: &str) -> bool {
    let normalized = normalize_phrase(phrase);
    dictionary()
        .read()
        .is_ok_and(|words| words.contains(&normalized))
}

pub fn add_command_phrase(phrase: &str) {
    let phrase = normalize_phrase(phrase);
    if !phrase.is_empty() {
        if let Ok(mut words) = dictionary().write() {
            words.insert(phrase);
        }
    }
}

pub fn remove_command_phrase(phrase: &str) {
    if let Ok(mut words) = dictionary().write() {
        words.remove(&normalize_phrase(phrase));
    }
}

fn normalize_phrase(text: &str) -> String {
    text.split_whitespace()
        .map(|token| {
            token
                .trim_matches(|character: char| !character.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn contains_command_phrase(text: &str, words: &HashSet<String>) -> bool {
    let normalized = normalize_phrase(text);
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    (0..tokens.len()).any(|start| {
        (start + 1..=tokens.len()).any(|end| words.contains(&tokens[start..end].join(" ")))
    })
}

fn directly_marked(item: &RawItem, words: &HashSet<String>) -> bool {
    contains_command_phrase(&item.text, words)
        || item
            .notes
            .iter()
            .any(|note| contains_command_phrase(note, words))
}

pub fn classify_items(items: &mut [RawItem]) {
    let Ok(words) = dictionary().read() else {
        return;
    };
    classify_items_with_dictionary(items, &words);
}

fn classify_items_with_dictionary(items: &mut [RawItem], words: &HashSet<String>) {
    let direct: Vec<bool> = items
        .iter()
        .map(|item| directly_marked(item, words))
        .collect();
    let mut children: HashMap<i32, Vec<usize>> = HashMap::new();
    let ids: HashSet<i32> = items.iter().map(|item| item.id).collect();
    for (index, item) in items.iter().enumerate() {
        if ids.contains(&item.parent_id) {
            children.entry(item.parent_id).or_default().push(index);
        }
    }

    fn mark_branch(
        index: usize,
        inherited: bool,
        items: &mut [RawItem],
        direct: &[bool],
        children: &HashMap<i32, Vec<usize>>,
        visited: &mut HashSet<usize>,
    ) {
        if !visited.insert(index) {
            return;
        }
        let is_command = if items[index].is_command_manual {
            items[index].is_command
        } else {
            inherited || direct[index]
        };
        items[index].is_command = is_command;
        if let Some(child_indexes) = children.get(&items[index].id) {
            for child_index in child_indexes {
                mark_branch(*child_index, is_command, items, direct, children, visited);
            }
        }
    }

    let roots: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.parent_id == 0 || !ids.contains(&item.parent_id))
        .map(|(index, _)| index)
        .collect();
    let mut visited = HashSet::new();
    for root in roots {
        mark_branch(root, false, items, &direct, &children, &mut visited);
    }
    for index in 0..items.len() {
        if !visited.contains(&index) {
            mark_branch(index, false, items, &direct, &children, &mut visited);
        }
    }
}

pub fn classify_state(state: &mut AppState) {
    for tree in &mut state.trees {
        classify_items(&mut tree.items);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{classify_items_with_dictionary, normalize_phrase};
    use crate::state::RawItem;

    fn item(id: i32, parent_id: i32, text: &str) -> RawItem {
        RawItem {
            id,
            text: text.to_string(),
            parent_id,
            is_expanded: true,
            is_editing: false,
            is_command: false,
            is_command_manual: false,
            notes: Vec::new(),
            links: Vec::new(),
        }
    }

    #[test]
    fn marks_phrase_with_number_and_all_descendants() {
        let words = HashSet::from([normalize_phrase("Курс 10")]);
        let mut items = vec![item(1, 0, "Курс 10"), item(2, 1, "Устройство"), item(3, 0, "Обычный узел")];

        classify_items_with_dictionary(&mut items, &words);

        assert!(items[0].is_command);
        assert!(items[1].is_command);
        assert!(!items[2].is_command);
    }

    #[test]
    fn manual_uncheck_overrides_dictionary_match() {
        let words = HashSet::from([normalize_phrase("Курс 10")]);
        let mut items = vec![item(1, 0, "Курс 10"), item(2, 1, "Дочерний узел")];
        items[0].is_command_manual = true;

        classify_items_with_dictionary(&mut items, &words);

        assert!(!items[0].is_command);
        assert!(!items[1].is_command);
    }

    #[test]
    fn manual_check_marks_arbitrary_phrase_without_dictionary_entry() {
        let words = HashSet::new();
        let mut items = vec![item(1, 0, "Курс 10")];
        items[0].is_command = true;
        items[0].is_command_manual = true;

        classify_items_with_dictionary(&mut items, &words);

        assert!(items[0].is_command);
    }
}