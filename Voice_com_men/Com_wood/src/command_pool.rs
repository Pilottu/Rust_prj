use std::collections::{HashMap, HashSet};

use crate::state::{CADLink, CADTree, RawItem};

// Пул возможных голосовых команд.
//
// Для каждого узла, помеченного как команда (is_command), идём по связям
// (cad_links) и собираем ВСЕ максимальные цепочки узлов:
//   Включить -> Люстра -> Мансарда спальня
// Текст узлов цепочки, склеенный через пробел, — первичная команда (id "N_0").
// Вторичные команды (id "N_1", "N_2", ...) — те же цепочки, где у любого узла
// текст заменён на одно из его примечаний (синоним). Перебираются все
// комбинации, поэтому пул растёт как произведение числа вариантов у узлов.
//
// Правила обхода (чтобы цепочки не расползались):
//   * по одному узлу из каждого дерева — цепочка идёт «дерево -> дерево»;
//   * другие узлы-команды в цепочку не попадают;
//   * узел-команда без связей команды не даёт.
//
// Одинаковые тексты внутри одного дерева (две «спальня» на разных этажах)
// различаются добавлением родителей: «Мансарда спальня» / «Второй этаж спальня».
// Родители добавляются только тем узлам, у которых в дереве есть «двойник»,
// и ровно в таком количестве, чтобы фраза стала однозначной.

/// Верхняя граница числа вариантов (первичная + синонимы) на одну цепочку.
const MAX_VARIANTS_PER_CHAIN: usize = 500;

/// Ключ узла: (id дерева, id узла).
type Key = (i32, i32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandVariant {
    /// "1_0" — первичная, "1_1", "1_2", ... — синонимы.
    pub id: String,
    pub text: String,
}

struct TreeIndex<'a> {
    by_id: HashMap<i32, &'a RawItem>,
    by_text: HashMap<String, Vec<&'a RawItem>>,
}

/// Строит весь пул. Чистая функция: БД не трогает.
pub fn build_pool(trees: &[CADTree], links: &[CADLink]) -> Vec<CommandVariant> {
    let mut nodes: HashMap<Key, &RawItem> = HashMap::new();
    let mut indexes: HashMap<i32, TreeIndex> = HashMap::new();
    for tree in trees {
        let mut index = TreeIndex {
            by_id: HashMap::new(),
            by_text: HashMap::new(),
        };
        for item in &tree.items {
            nodes.insert((tree.id, item.id), item);
            index.by_id.insert(item.id, item);
            index
                .by_text
                .entry(item.text.trim().to_lowercase())
                .or_default()
                .push(item);
        }
        indexes.insert(tree.id, index);
    }

    // Связи неориентированные.
    let mut graph: HashMap<Key, Vec<Key>> = HashMap::new();
    for link in links {
        let a = (link.tree_id_1, link.id_obj1);
        let b = (link.tree_id_2, link.id_obj2);
        if nodes.contains_key(&a) && nodes.contains_key(&b) {
            graph.entry(a).or_default().push(b);
            graph.entry(b).or_default().push(a);
        }
    }
    for neighbours in graph.values_mut() {
        neighbours.sort_unstable();
        neighbours.dedup();
    }

    let mut starts: Vec<Key> = nodes
        .iter()
        .filter(|(_, item)| item.is_command)
        .map(|(key, _)| *key)
        .collect();
    starts.sort_unstable();

    let mut pool = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut chain_no = 0;

    for start in starts {
        let mut chains = Vec::new();
        let mut path = vec![start];
        let mut used_trees = HashSet::from([start.0]);
        walk(&mut path, &mut used_trees, &graph, &nodes, &mut chains);

        for chain in chains {
            chain_no += 1;
            let per_node: Vec<Vec<String>> = chain
                .iter()
                .map(|key| variants_of(nodes[key], &indexes[&key.0]))
                .collect();
            for (idx, phrase) in expand(&per_node).into_iter().enumerate() {
                if seen.insert(phrase.to_lowercase()) {
                    pool.push(CommandVariant {
                        id: format!("{chain_no}_{idx}"),
                        text: phrase,
                    });
                }
            }
        }
    }
    pool
}

fn walk(
    path: &mut Vec<Key>,
    used_trees: &mut HashSet<i32>,
    graph: &HashMap<Key, Vec<Key>>,
    nodes: &HashMap<Key, &RawItem>,
    out: &mut Vec<Vec<Key>>,
) {
    let last = *path.last().expect("path is never empty");
    let mut extended = false;

    if let Some(neighbours) = graph.get(&last) {
        for &next in neighbours {
            if used_trees.contains(&next.0) || nodes[&next].is_command {
                continue;
            }
            extended = true;
            path.push(next);
            used_trees.insert(next.0);
            walk(path, used_trees, graph, nodes, out);
            used_trees.remove(&next.0);
            path.pop();
        }
    }

    // Записываем только максимальные цепочки (дальше идти некуда).
    if !extended && path.len() > 1 {
        out.push(path.clone());
    }
}

/// Родители узла от ближнего к дальнему (защита от циклов по parent_id).
fn ancestors<'a>(item: &RawItem, index: &TreeIndex<'a>) -> Vec<&'a str> {
    let mut result = Vec::new();
    let mut seen = HashSet::from([item.id]);
    let mut current = item.parent_id;
    while let Some(&parent) = index.by_id.get(&current) {
        if !seen.insert(parent.id) {
            break;
        }
        result.push(parent.text.trim());
        current = parent.parent_id;
    }
    result
}

fn lowered_prefix(ancestors: &[&str], k: usize) -> Vec<String> {
    ancestors.iter().take(k).map(|a| a.to_lowercase()).collect()
}

/// Префикс из родителей, нужный чтобы отличить узел от «двойников» в дереве.
/// Пустая строка — если двойников нет.
fn disambiguation_prefix(item: &RawItem, index: &TreeIndex) -> String {
    let twins: Vec<&RawItem> = index
        .by_text
        .get(&item.text.trim().to_lowercase())
        .map(|same| same.iter().copied().filter(|c| c.id != item.id).collect())
        .unwrap_or_default();
    if twins.is_empty() {
        return String::new();
    }

    let mine = ancestors(item, index);
    let twin_ancestors: Vec<Vec<&str>> = twins.iter().map(|t| ancestors(t, index)).collect();

    for k in 1..=mine.len() {
        let mine_prefix = lowered_prefix(&mine, k);
        if twin_ancestors
            .iter()
            .all(|other| lowered_prefix(other, k) != mine_prefix)
        {
            // Ближайший родитель — последний в тексте: «Мансарда спальня».
            return mine.iter().take(k).rev().copied().collect::<Vec<_>>().join(" ");
        }
    }
    String::new() // полные дубликаты не различить — оставляем как есть
}

/// Текст узла первым, затем его примечания-синонимы (без пустых и повторов).
fn variants_of(item: &RawItem, index: &TreeIndex) -> Vec<String> {
    let prefix = disambiguation_prefix(item, index);
    let with_prefix = |text: &str| {
        if prefix.is_empty() {
            text.to_string()
        } else {
            format!("{prefix} {text}")
        }
    };

    let mut result = vec![with_prefix(item.text.trim())];
    for note in &item.notes {
        let note = note.trim();
        if note.is_empty() {
            continue;
        }
        let variant = with_prefix(note);
        if !result.iter().any(|v| v.to_lowercase() == variant.to_lowercase()) {
            result.push(variant);
        }
    }
    result
}

/// Декартово произведение вариантов. Элемент 0 — всегда первичная команда.
fn expand(per_node: &[Vec<String>]) -> Vec<String> {
    let mut combos: Vec<String> = vec![String::new()];
    for options in per_node {
        let mut next = Vec::new();
        'outer: for base in &combos {
            for option in options {
                if next.len() >= MAX_VARIANTS_PER_CHAIN {
                    break 'outer;
                }
                next.push(if base.is_empty() {
                    option.clone()
                } else {
                    format!("{base} {option}")
                });
            }
        }
        combos = next;
    }
    combos
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i32, parent_id: i32, text: &str, notes: &[&str], is_command: bool) -> RawItem {
        RawItem {
            id,
            text: text.to_string(),
            parent_id,
            is_expanded: true,
            is_editing: false,
            is_command,
            is_command_manual: false,
            notes: notes.iter().map(|n| n.to_string()).collect(),
            links: vec![],
        }
    }

    fn tree(id: i32, items: Vec<RawItem>) -> CADTree {
        CADTree {
            id,
            cad_x: 0.0,
            cad_y: 0.0,
            width: 250.0,
            count: items.len() as i32,
            items,
            notes: vec![],
        }
    }

    fn link(id: i32, t1: i32, n1: i32, t2: i32, n2: i32) -> CADLink {
        CADLink {
            id,
            id_obj1: n1,
            id_obj2: n2,
            x1: 0.0,
            y1: 0.0,
            x2: 0.0,
            y2: 0.0,
            tree_id_1: t1,
            tree_id_2: t2,
            side1: 0,
            side2: 0,
            mid_x: 0.0,
        }
    }

    fn texts(pool: Vec<CommandVariant>) -> Vec<String> {
        pool.into_iter().map(|v| v.text).collect()
    }

    #[test]
    fn chain_and_synonyms() {
        let trees = vec![
            tree(1, vec![item(1, 0, "Включить", &[], true)]),
            tree(2, vec![item(10, 0, "Люстра", &["свет"], false)]),
            tree(3, vec![item(20, 0, "спальня", &[], false)]),
        ];
        let links = vec![link(1, 1, 1, 2, 10), link(2, 2, 10, 3, 20)];
        let pool = build_pool(&trees, &links);
        assert_eq!(pool.len(), 2);
        assert_eq!(
            pool[0],
            CommandVariant { id: "1_0".into(), text: "Включить Люстра спальня".into() }
        );
        assert_eq!(
            pool[1],
            CommandVariant { id: "1_1".into(), text: "Включить свет спальня".into() }
        );
    }

    #[test]
    fn branching_and_no_other_commands() {
        let trees = vec![
            tree(1, vec![item(1, 0, "Включить", &[], true)]),
            tree(2, vec![item(10, 0, "Люстра", &[], false), item(11, 0, "Бра", &[], false)]),
            tree(3, vec![item(30, 0, "Выключить", &[], true)]),
        ];
        let links = vec![link(1, 1, 1, 2, 10), link(2, 1, 1, 2, 11), link(3, 3, 30, 2, 10)];
        let t = texts(build_pool(&trees, &links));
        assert!(t.contains(&"Включить Люстра".to_string()));
        assert!(t.contains(&"Включить Бра".to_string()));
        assert!(t.contains(&"Выключить Люстра".to_string()));
        assert!(!t.iter().any(|x| x.contains("Включить") && x.contains("Выключить")));
    }

    #[test]
    fn command_without_links_gives_nothing() {
        let trees = vec![tree(1, vec![item(1, 0, "Включить", &[], true)])];
        assert!(build_pool(&trees, &[]).is_empty());
    }

    #[test]
    fn same_text_gets_parent_prefix() {
        let trees = vec![
            tree(
                1,
                vec![
                    item(1, 0, "Мансарда", &[], false),
                    item(2, 1, "спальня", &["спальная"], false),
                    item(3, 0, "Второй этаж", &[], false),
                    item(4, 3, "спальня", &[], false),
                ],
            ),
            tree(2, vec![item(10, 0, "Включить", &[], true)]),
        ];
        let links = vec![link(1, 2, 10, 1, 2), link(2, 2, 10, 1, 4)];
        let t = texts(build_pool(&trees, &links));
        assert!(t.contains(&"Включить Мансарда спальня".to_string()));
        assert!(t.contains(&"Включить Мансарда спальная".to_string()));
        assert!(t.contains(&"Включить Второй этаж спальня".to_string()));
        assert!(!t.contains(&"Включить спальня".to_string()));
    }
}
