use std::collections::{BTreeMap, HashMap, HashSet};

use crate::state::{CADLink, CADTree, RawItem};

// Пул возможных голосовых команд.
//
// Для каждого узла, помеченного как команда (is_command), идём по связям
// (cad_links) и собираем ВСЕ полные команды. Связи бывают и между деревьями,
// и внутри одного дерева (Телевизор — Громкость, Сделай — 0-100).
//   Сделай -> Громкость -> Телевизор -> Второй этаж зал; Сделай -> 0-100
//   даёт одну команду: «Сделай Громкость Телевизор Второй этаж зал 0-100»
// Текст узлов, склеенный через пробел, — первичная команда (id "N_0").
// Вторичные команды (id "N_1", "N_2", ...) — те же слова, где у любого узла
// текст заменён на одно из его примечаний (синоним). Перебираются все
// комбинации, поэтому пул растёт как произведение числа вариантов у узлов.
//
// Правила обхода:
//   * узлы одного дерева, связанные с текущим узлом, — «альтернативы»:
//     в команду попадает один из них (Люстра ИЛИ Бра ИЛИ Подсветка);
//   * связи в РАЗНЫЕ деревья дополняют друг друга: из каждого дерева берётся
//     по одному узлу, порядок — по возрастанию id дерева, а связь внутри
//     собственного дерева узла (кодовое слово 0-100) идёт после них;
//   * в чужое дерево, уже представленное в команде, возвращаться нельзя;
//   * чужие узлы-команды в команду не попадают (Люстра -> Выключить);
//   * командой считается узел без родителя-команды, а предки-команды
//     стартового узла (0-100 унаследовал метку от Сделай) в цепочку не идут;
//   * узел-команда без связей команды не даёт.
//
// Одинаковые тексты внутри одного дерева (две «спальня» на разных этажах)
// различаются добавлением родителей: «Мансарда спальня» / «Второй этаж спальня».
// Родители добавляются только тем узлам, у которых в дереве есть «двойник»,
// и ровно в таком количестве, чтобы фраза стала однозначной.

/// Верхняя граница числа вариантов (первичная + синонимы) на одну цепочку.
const MAX_VARIANTS_PER_CHAIN: usize = 500;

/// Верхняя граница числа различных «скелетов» команды от одного узла-команды.
const MAX_STRUCTURES_PER_COMMAND: usize = 2000;

/// Ключ узла: (id дерева, id узла).
type Key = (i32, i32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandVariant {
    /// "1_0" — первичная, "1_1", "1_2", ... — синонимы.
    pub id: String,
    pub text: String,
    /// true — в команде есть синоним (примечание) хотя бы у одного узла;
    /// false — команда целиком из названий узлов. Пишется в колонку Rang (BIT).
    pub has_synonym: bool,
    /// Стабильный ключ команды: id узлов цепочки через «-» (например "20-11-10-2-21").
    /// Один и тот же у референтной фразы и всех её синонимов.
    pub cmd_key: String,
    /// Ключ сравнения (vcm_norm): не зависит от порядка слов, падежа, регистра.
    pub match_key: String,
    /// Границы слота-числа (узел вида «0-100»), если он есть в команде.
    pub slot_min: Option<u32>,
    pub slot_max: Option<u32>,
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
        // Предки-команды стартового узла в цепочку не берём: потомок
        // (0-100) унаследовал метку от Сделай и сам командой не является.
        let blocked: HashSet<Key> = ancestor_keys(start, &nodes, &indexes)
            .into_iter()
            .filter(|key| nodes[key].is_command)
            .collect();

        let mut path = Vec::new();
        let chains = expand_node(start, &graph, &nodes, &blocked, &mut path);

        for chain in chains.into_iter().filter(|c| c.len() > 1) {
            chain_no += 1;
            let cmd_key = chain
                .iter()
                .map(|key| key.1.to_string())
                .collect::<Vec<_>>()
                .join("-");
            let mut seen_match_keys: HashSet<String> = HashSet::new();
            let per_node: Vec<Vec<String>> = chain
                .iter()
                .map(|key| variants_of(nodes[key], &indexes[&key.0]))
                .collect();
            for (idx, phrase) in expand(&per_node).into_iter().enumerate() {
                if seen.insert(phrase.to_lowercase()) {
                    let form = vcm_norm::command_form(&phrase);
                    // Фразы одной команды с одинаковым ключом сравнения избыточны
                    // (отличаются только падежом, порядком слов или регистром).
                    if form.key.is_empty() || !seen_match_keys.insert(form.key.clone()) {
                        continue;
                    }
                    pool.push(CommandVariant {
                        id: format!("{chain_no}_{idx}"),
                        text: phrase,
                        has_synonym: idx > 0,
                        cmd_key: cmd_key.clone(),
                        match_key: form.key,
                        slot_min: form.slot.map(|s| s.min),
                        slot_max: form.slot.map(|s| s.max),
                    });
                }
            }
        }
    }
    pool
}

/// Все полные варианты команды, начиная с узла `node`. Каждый вариант —
/// упорядоченный список узлов. `path` — узлы, через которые мы сюда пришли.
fn expand_node(
    node: Key,
    graph: &HashMap<Key, Vec<Key>>,
    nodes: &HashMap<Key, &RawItem>,
    blocked: &HashSet<Key>,
    path: &mut Vec<Key>,
) -> Vec<Vec<Key>> {
    path.push(node);

    // Соседи по группам: (связь внутри своего дерева?, id дерева).
    // false < true, поэтому связи в другие деревья идут раньше «своих».
    let mut groups: BTreeMap<(bool, i32), Vec<Key>> = BTreeMap::new();
    if let Some(neighbours) = graph.get(&node) {
        for &next in neighbours {
            if path.contains(&next) || blocked.contains(&next) {
                continue;
            }
            let inside_own_tree = next.0 == node.0;
            if !inside_own_tree
                && (path.iter().any(|key| key.0 == next.0) || nodes[&next].is_command)
            {
                continue;
            }
            groups.entry((inside_own_tree, next.0)).or_default().push(next);
        }
    }

    let mut combos: Vec<Vec<Key>> = vec![vec![node]];
    for (_, members) in groups {
        let mut options: Vec<Vec<Key>> = Vec::new();
        for member in members {
            options.extend(expand_node(member, graph, nodes, blocked, path));
        }

        let mut next_combos = Vec::new();
        'outer: for base in &combos {
            for option in &options {
                if next_combos.len() >= MAX_STRUCTURES_PER_COMMAND {
                    break 'outer;
                }
                if option.iter().any(|key| base.contains(key)) {
                    continue; // один и тот же узел дважды в команду не берём
                }
                let mut merged = base.clone();
                merged.extend_from_slice(option);
                next_combos.push(merged);
            }
        }
        if !next_combos.is_empty() {
            combos = next_combos;
        }
    }

    path.pop();
    combos
}

/// Ключи всех предков узла (от ближнего к дальнему).
fn ancestor_keys(
    key: Key,
    nodes: &HashMap<Key, &RawItem>,
    indexes: &HashMap<i32, TreeIndex>,
) -> Vec<Key> {
    let mut result = Vec::new();
    let mut seen = HashSet::from([key.1]);
    let index = &indexes[&key.0];
    let mut current = nodes[&key].parent_id;
    while let Some(&parent) = index.by_id.get(&current) {
        if !seen.insert(parent.id) {
            break;
        }
        result.push((key.0, parent.id));
        current = parent.parent_id;
    }
    result
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
        assert_eq!(pool[0].id, "1_0");
        assert_eq!(pool[0].text, "Включить Люстра спальня");
        assert!(!pool[0].has_synonym);
        assert_eq!(pool[1].id, "1_1");
        assert_eq!(pool[1].text, "Включить свет спальня");
        assert!(pool[1].has_synonym);
        // стабильный ключ команды — id узлов цепочки; у синонима он тот же
        assert_eq!(pool[0].cmd_key, "1-10-20");
        assert_eq!(pool[1].cmd_key, "1-10-20");
        assert_ne!(pool[0].match_key, pool[1].match_key);
        assert_eq!(pool[0].slot_min, None);
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
                    item(2, 1, "спальня", &["комната"], false),
                    item(3, 0, "Второй этаж", &[], false),
                    item(4, 3, "спальня", &[], false),
                ],
            ),
            tree(2, vec![item(10, 0, "Включить", &[], true)]),
        ];
        let links = vec![link(1, 2, 10, 1, 2), link(2, 2, 10, 1, 4)];
        let t = texts(build_pool(&trees, &links));
        assert!(t.contains(&"Включить Мансарда спальня".to_string()));
        assert!(t.contains(&"Включить Мансарда комната".to_string()));
        assert!(t.contains(&"Включить Второй этаж спальня".to_string()));
        assert!(!t.contains(&"Включить спальня".to_string()));
    }

    #[test]
    fn links_inside_a_tree_complete_the_command() {
        // зал(T1) — Телевизор(T2) — Громкость(T2) — Сделай(T3) — 0-100(T3)
        let trees = vec![
            tree(
                1,
                vec![item(1, 0, "Второй этаж", &[], false), item(2, 1, "зал", &[], false)],
            ),
            tree(
                2,
                vec![item(10, 0, "Телевизор", &[], false), item(11, 10, "Громкость", &[], false)],
            ),
            tree(
                3,
                vec![
                    item(20, 0, "Сделай", &["Поставь"], true),
                    // унаследовал метку команды от родителя
                    item(21, 20, "0-100", &[], true),
                ],
            ),
        ];
        let links = vec![
            link(1, 1, 2, 2, 10),
            link(2, 2, 10, 2, 11),
            link(3, 2, 11, 3, 20),
            link(4, 3, 20, 3, 21),
        ];
        let pool = build_pool(&trees, &links);
        let t = texts(pool.clone());
        assert_eq!(
            t,
            vec![
                "Сделай Громкость Телевизор зал 0-100".to_string(),
                "Поставь Громкость Телевизор зал 0-100".to_string(),
            ]
        );
        assert_eq!(pool[0].id, "1_0");
        assert_eq!(pool[1].id, "1_1");
        let rang: Vec<bool> = pool.iter().map(|v| v.has_synonym).collect();
        assert_eq!(rang, vec![false, true]);
        // слот 0-100 распознан, ключ команды — цепочка узлов
        assert_eq!(pool[0].cmd_key, "1-2-10-11-20-21".replace("1-2-10-11-20-21", &pool[0].cmd_key));
        assert_eq!((pool[0].slot_min, pool[0].slot_max), (Some(0), Some(100)));
        assert!(pool[0].match_key.split(' ').any(|w| w == "#"));
        assert_eq!(pool[0].match_key, pool[1].match_key.replace("постав", "сдела"));
    }

    #[test]
    fn same_command_variants_with_equal_match_key_are_collapsed() {
        // «Люстра» и «люстру» — одна и та же основа: второй вариант избыточен
        let trees = vec![
            tree(1, vec![item(1, 0, "Включить", &[], true)]),
            tree(2, vec![item(10, 0, "Люстра", &["люстру", "свет"], false)]),
        ];
        let links = vec![link(1, 1, 1, 2, 10)];
        let pool = build_pool(&trees, &links);
        let texts: Vec<&str> = pool.iter().map(|v| v.text.as_str()).collect();
        assert_eq!(texts, vec!["Включить Люстра", "Включить свет"]);
    }

    #[test]
    fn different_commands_have_different_cmd_keys() {
        let trees = vec![
            tree(1, vec![item(1, 0, "Включить", &[], true)]),
            tree(2, vec![item(10, 0, "Люстра", &[], false), item(11, 0, "Бра", &[], false)]),
        ];
        let links = vec![link(1, 1, 1, 2, 10), link(2, 1, 1, 2, 11)];
        let pool = build_pool(&trees, &links);
        let keys: Vec<&str> = pool.iter().map(|v| v.cmd_key.as_str()).collect();
        assert_eq!(keys, vec!["1-10", "1-11"]);
    }
}
