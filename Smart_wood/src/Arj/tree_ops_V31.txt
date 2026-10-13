use std::{collections::HashMap, rc::Rc};
use slint::{ModelRc, VecModel};

use crate::state::{AppState, CADTree, ClipboardBranch, Point2D, RawItem, save_persisted_state};
use crate::{AppWindow, CADLine, MyTreeItem, NodeLinkInfo, NodeNoteInfo, SlintCADTree};

// Вся геометрия/логика построения дерева и связей: раскладка узлов,
// разрешение коллизий при переносе дерева, сборка данных для окна свойств,
// синхронизация Rust-состояния (AppState) с моделями на стороне Slint
// (update_ui_models — единственная точка, откуда UI реально перерисовывается),
// плюс копирование/вставка веток через буфер обмена (pack/unpack_branch).

pub fn check_collision(
    x1: f32,
    _y1: f32,
    w1: f32,
    _h1: f32,
    x2: f32,
    _y2: f32,
    w2: f32,
    _h2: f32,
) -> bool {
    let pad = 15.0_f32;
    x1 < x2 + w2 + pad && x1 + w1 + pad > x2
}
pub fn find_closest_free_pos(
    moved_id: i32,
    mx: f32,
    my: f32,
    mw: f32,
    mh: f32,
    trees: &[CADTree],
) -> (f32, f32) {
    let grid = 50.0_f32;
    let mut out_x = (mx / grid).round() * grid;
    let mut out_y = (my / grid).round() * grid;
    let total_all_nodes: usize = trees.iter().map(|t| t.items.len()).sum();
    let secret_value = (total_all_nodes as f32) * 10.0;
    for t in trees {
        if t.id == moved_id {
            continue;
        }
        let th = (t.count * 32 + 16) as f32;
        if check_collision(out_x, out_y, mw, mh, t.cad_x, t.cad_y, t.width, th) {
            let delta_x = (mw / 2.0) + (t.width / 2.0) + secret_value;
            let grid_delta_x = (delta_x / grid).round() * grid;
            if out_x + (mw / 2.0) < t.cad_x + (t.width / 2.0) {
                out_x = t.cad_x - grid_delta_x;
            } else {
                out_x = t.cad_x + grid_delta_x;
            }
            out_y = (out_y / grid).round() * grid;
        }
    }
    (out_x, out_y)
}
pub fn build_tree(
    pid: i32,
    depth: i32,
    db: &Vec<RawItem>,
    v_items: &mut Vec<MyTreeItem>,
    hidden: bool,
    parent_lasts: Vec<bool>,
    sel_id: i32,
    _parent_coord: Point2D,
    global_index: &mut i32,
) {
    let mut children: Vec<&RawItem> = db.iter().filter(|i| i.parent_id == pid).collect();
    children.sort_by_key(|i| i.id);
    let total_children = children.len();
    for (index, item) in children.into_iter().enumerate() {
        let is_last = index == total_children - 1;
        let has_children = db.iter().any(|i| i.parent_id == item.id);
        let current_row_idx = *global_index;
        if !hidden {
            *global_index += 1;
        }
        let current_node_coord = Point2D {
            x: 18.0 + 10.0 + (depth as f32 * 24.0) + 10.0,
            y: 13.0 + (current_row_idx as f32 * 32.0) + 16.0,
        };
        if !hidden {
            let computed_w = (item.text.chars().count() as f32 * 7.2) + 20.0;
            let smart_id = format!(
                "{}_{}_{}_{}_{}",
                item.parent_id,
                item.id,
                depth,
                if is_last { "L" } else { "M" },
                item.links.len()
            );
            let mut slint_mask = [0; 12];
            for i in 0..12 {
                if i < (depth - 1) as usize && i < parent_lasts.len() {
                    slint_mask[i] = if !parent_lasts[i] { 1 } else { 0 };
                }
            }
            let notes_str = item.notes.join(", ");
            v_items.push(MyTreeItem {
                id: item.id,
                text: item.text.clone().into(),
                depth,
                is_last,
                has_children,
                is_editing: item.is_editing,
                is_expanded: item.is_expanded,
                parent_is_last: depth > 1 && parent_lasts[(depth - 2) as usize],
                is_selected: item.id == sel_id,
                note: notes_str.into(),
                text_width: computed_w,
                smart_id: smart_id.into(),
                line_mask: ModelRc::from(Rc::new(VecModel::from(slint_mask.to_vec()))),
                parent_id: item.parent_id,
                parent_triangle_x: _parent_coord.x,
                parent_triangle_y: _parent_coord.y,
                self_triangle_x: current_node_coord.x,
                self_triangle_y: current_node_coord.y,
            });
        }
        if has_children {
            let mut current_lasts = parent_lasts.clone();
            current_lasts.push(is_last);
            build_tree(
                item.id,
                depth + 1,
                db,
                v_items,
                hidden || !item.is_expanded,
                current_lasts,
                sel_id,
                current_node_coord,
                global_index,
            );
        }
    }
}
pub fn build_node_links(state: &AppState, tree_id: i32, node_id: i32) -> Vec<NodeLinkInfo> {
    state
        .trees
        .iter()
        .find(|t| t.id == tree_id)
        .and_then(|t| t.items.iter().find(|i| i.id == node_id))
        .map(|i| {
            i.links
                .iter()
                .map(|l| NodeLinkInfo {
                    id: l.id,
                    target_node_id: l.target_node_id,
                    target_node_name: l.target_node_name.clone().into(),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn build_node_notes(state: &AppState, tree_id: i32, node_id: i32) -> Vec<NodeNoteInfo> {
    let mut result = Vec::new();
    if let Some(tree) = state.trees.iter().find(|t| t.id == tree_id) {
        if let Some(item) = tree.items.iter().find(|i| i.id == node_id) {
            let node_name = item.text.clone();
            for (idx, note) in item.notes.iter().enumerate() {
                result.push(NodeNoteInfo {
                    node_id: item.id,
                    node_name: node_name.clone().into(),
                    note_index: idx as i32,
                    note_text: note.clone().into(),
                });
            }
        }
    }
    result
}

// Если узел сейчас скрыт (какой-то его родитель свёрнут), поднимаемся вверх
// по цепочке parent_id, пока не найдём ближайшего ПРЕДКА, который реально
// отрисован в дереве (виден). Так линия связи "переезжает" на видимого
// родителя вместо того, чтобы зависать на старых координатах скрытого узла.
pub fn find_visible_node<'a>(
    tree_items: &[RawItem],
    views: &'a [MyTreeItem],
    node_id: i32,
) -> Option<&'a MyTreeItem> {
    if let Some(v) = views.iter().find(|i| i.id == node_id) {
        return Some(v);
    }
    let mut current_id = node_id;
    loop {
        let parent_id = tree_items.iter().find(|i| i.id == current_id)?.parent_id;
        if parent_id == 0 {
            return None;
        }
        if let Some(v) = views.iter().find(|i| i.id == parent_id) {
            return Some(v);
        }
        current_id = parent_id;
    }
}

// Визуальные допуски для точки, где линия связи касается узла — подберите
// на глаз под текущий стиль отрисовки. OVAL_BORDER_ADJUST — когда узел
// "дальняя" сторона (линия тянется к внешнему краю овала с текстом).
// TRIANGLE_ADJUST — когда узел "ближняя" сторона (линия идёт к треугольнику
// разворачивания слева от текста). Именно в эту логику раньше закрадывалась
// асимметрия: допуск был привязан к сегменту линии, а не к роли (side).
const OVAL_BORDER_ADJUST: f32 = 3.0;
const TRIANGLE_ADJUST: f32 = -5.0;

pub fn update_ui_models(app: &AppWindow, state: &AppState) {
    save_persisted_state(state);
    let mut slint_trees = Vec::new();
    let mut views_by_tree: HashMap<i32, Vec<MyTreeItem>> = HashMap::new();

    for t in &state.trees {
        let mut t_views = Vec::new();
        let mut g_idx = 0;
        build_tree(
            0,
            0,
            &t.items,
            &mut t_views,
            false,
            vec![],
            state.selected_node_id,
            Point2D { x: 30.0, y: 29.0 },
            &mut g_idx,
        );
        let mut max_w = 260.0_f32;
        for item in &t_views {
            max_w = max_w.max(10.0 + (item.depth as f32 * 24.0) + 20.0 + item.text_width + 50.0);
        }
        slint_trees.push(SlintCADTree {
            id: t.id,
            cad_x: t.cad_x,
            cad_y: t.cad_y,
            width: max_w,
            count: g_idx,
            data: ModelRc::from(Rc::new(VecModel::from(t_views.clone()))),
        });
        views_by_tree.insert(t.id, t_views);
    }
    app.set_trees_list(ModelRc::from(Rc::new(VecModel::from(slint_trees))));

    let mut updated_links = Vec::new();
    for link in &state.cad_links {
        let mut x1 = link.x1;
        let mut y1 = link.y1;
        let mut x2 = link.x2;
        let mut y2 = link.y2;
        let mut side1 = link.side1;
        let mut side2 = link.side2;
        let mut mid_x = link.mid_x;

        if let Some(t1) = state.trees.iter().find(|t| t.id == link.tree_id_1) {
            if let Some(t2) = state.trees.iter().find(|t| t.id == link.tree_id_2) {
                let new_side1 = if t1.cad_x < t2.cad_x { 0 } else { 1 };
                let new_side2 = if t2.cad_x < t1.cad_x { 0 } else { 1 };

                side1 = new_side1;
                side2 = new_side2;

                if let Some(views) = views_by_tree.get(&link.tree_id_1) {
                    if let Some(item) = find_visible_node(&t1.items, views, link.id_obj1) {
                        let node_width = item.text_width + 20.0;
                        let x1_base = t1.cad_x + item.self_triangle_x + 12.0;
                        // side == 0 — узел "дальняя" сторона связи, линия идёт к
                        // внешнему краю овала с текстом (OVAL_BORDER_ADJUST).
                        // side == 1 — узел "ближняя" сторона, линия идёт к
                        // треугольнику разворачивания (TRIANGLE_ADJUST).
                        // Допуск привязан к side, а не к тому, узел это 1 или 2 —
                        // иначе при перетаскивании, когда стороны меняются
                        // местами, допуск остаётся приклеен не к тому краю.
                        x1 = if side1 == 0 {
                            x1_base + node_width + OVAL_BORDER_ADJUST
                        } else {
                            x1_base + TRIANGLE_ADJUST
                        };
                        y1 = t1.cad_y + item.self_triangle_y + 10.0;
                    }
                }

                if let Some(views) = views_by_tree.get(&link.tree_id_2) {
                    if let Some(item) = find_visible_node(&t2.items, views, link.id_obj2) {
                        let node_width = item.text_width + 20.0;
                        let x2_base = t2.cad_x + item.self_triangle_x + 14.0;
                        x2 = if side2 == 0 {
                            x2_base + node_width + OVAL_BORDER_ADJUST
                        } else {
                            x2_base + TRIANGLE_ADJUST
                        };
                        y2 = t2.cad_y + item.self_triangle_y + 10.0;
                    }
                }

                mid_x = (x1 + x2) / 2.0;
            }
        }

        updated_links.push(CADLine {
            id: link.id,
            id_obj1: link.id_obj1,
            id_obj2: link.id_obj2,
            x1,
            y1,
            x2,
            y2,
            tree_id_1: link.tree_id_1,
            tree_id_2: link.tree_id_2,
            side1,
            side2,
            mid_x,
        });
    }
    app.set_active_cad_links(ModelRc::from(Rc::new(VecModel::from(updated_links))));

    let links_vec = build_node_links(state, state.active_focus_tree_id, state.selected_node_id);
    app.set_properties_links(ModelRc::from(Rc::new(VecModel::from(links_vec))));

    let notes_vec = build_node_notes(state, state.active_focus_tree_id, state.selected_node_id);
    app.set_properties_notes(ModelRc::from(Rc::new(VecModel::from(notes_vec))));

    app.set_selected_node_id(state.selected_node_id);
    app.set_selected_tree_id(state.active_focus_tree_id);
}
pub fn pack_branch(pid: i32, vec: &Vec<RawItem>) -> Vec<ClipboardBranch> {
    vec.iter()
        .filter(|i| i.parent_id == pid)
        .map(|i| ClipboardBranch {
            root_item: i.clone(),
            children: pack_branch(i.id, vec),
        })
        .collect()
}
pub fn unpack_branch(branch: ClipboardBranch, new_parent_id: i32, vec: &mut Vec<RawItem>) {
    let next_id = vec.iter().map(|i| i.id).max().unwrap_or(0) + 1;
    let mut new_item = branch.root_item;
    new_item.id = next_id;
    new_item.parent_id = new_parent_id;
    new_item.is_editing = false;
    vec.push(new_item);
    for child in branch.children {
        unpack_branch(child, next_id, vec);
    }
}
