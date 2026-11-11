use std::{cell::RefCell, rc::Rc};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::command_pool::tree_category;
use crate::repository::{self, TagRowData};
use crate::state::{AppState, CADTree, RawItem};
use crate::{AppWindow, TagRow};

fn find_node(state: &AppState, node_id: i32) -> Option<(&CADTree, &RawItem)> {
    state.trees.iter().find_map(|tree| {
        tree.items
            .iter()
            .find(|item| item.id == node_id)
            .map(|item| (tree, item))
    })
}

/// Локация от внутренней к внешней: «ванна Второй этаж».
fn place_text(tree: &CADTree, item: &RawItem) -> String {
    let mut parts = vec![item.text.trim().to_string()];
    let mut seen = vec![item.id];
    let mut current = item.parent_id;
    while let Some(parent) = tree.items.iter().find(|i| i.id == current) {
        if seen.contains(&parent.id) {
            break;
        }
        seen.push(parent.id);
        parts.push(parent.text.trim().to_string());
        current = parent.parent_id;
    }
    parts.join(" ")
}

/// Фраза в «человеческом» порядке: команда, объект, локация (изнутри наружу), число.
/// CmdKey — это id узлов цепочки через «-»; первый узел цепочки — сама команда.
fn natural_phrase(state: &AppState, cmd_key: &str) -> Option<String> {
    let (mut command, mut object, mut place, mut slot) = (vec![], vec![], vec![], vec![]);
    for (position, id) in cmd_key
        .split('-')
        .filter_map(|part| part.parse::<i32>().ok())
        .enumerate()
    {
        let (tree, item) = find_node(state, id)?;
        let text = item.text.trim().to_string();
        if vcm_norm::command_form(&text).slot.is_some() {
            slot.push(text);
            continue;
        }
        let category = tree_category(tree).unwrap_or_default().to_lowercase();
        if position == 0 || category.starts_with("ком") {
            command.push(text);
        } else if category.starts_with("лок") {
            place.push(place_text(tree, item));
        } else {
            object.push(text);
        }
    }
    let all: Vec<String> = command
        .into_iter()
        .chain(object)
        .chain(place)
        .chain(slot)
        .collect();
    (!all.is_empty()).then(|| all.join(" "))
}

pub fn wire_tag_callbacks(app: &AppWindow, state: &Rc<RefCell<AppState>>) {
    let model: Rc<VecModel<TagRow>> = Rc::new(VecModel::default());
    app.set_tag_rows(ModelRc::from(model.clone()));

    let w = app.as_weak();
    let m = model.clone();
    let s = state.clone();
    app.on_tags_open(move || {
        let ui = w.unwrap();
        match repository::load_tag_rows() {
            Ok(mut rows) => {
                {
                    let st = s.borrow();
                    for row in rows.iter_mut() {
                        if let Some(phrase) = natural_phrase(&st, &row.cmd_key) {
                            row.phrase = phrase;
                        }
                    }
                }
                rows.sort_by_key(|row| row.phrase.to_lowercase());
                let items: Vec<TagRow> = rows
                    .into_iter()
                    .map(|r| TagRow {
                        cmd_key: r.cmd_key.into(),
                        phrase: r.phrase.into(),
                        active: r.active,
                        vtype: r.vtype,
                    })
                    .collect();
                ui.set_tags_status(format!("Команд 0-го ранга: {}", items.len()).into());
                m.set_vec(items);
            }
            Err(error) => {
                crate::historian::log("ERROR", &format!("Не удалось загрузить список тегов: {error}"));
                m.set_vec(Vec::<TagRow>::new());
                ui.set_tags_status(format!("Ошибка: {error}").into());
            }
        }
        ui.set_tags_visible(true);
    });

    let m = model.clone();
    app.on_tag_row_changed(move |index, active, vtype| {
        let i = index as usize;
        if let Some(mut row) = m.row_data(i) {
            row.active = active;
            row.vtype = vtype;
            m.set_row_data(i, row);
        }
    });

    let w = app.as_weak();
    let m = model.clone();
    app.on_tags_save(move || {
        let ui = w.unwrap();
        let rows: Vec<TagRowData> = m
            .iter()
            .map(|r| TagRowData {
                cmd_key: r.cmd_key.to_string(),
                phrase: r.phrase.to_string(),
                active: r.active,
                vtype: r.vtype,
            })
            .collect();
        match repository::save_tag_rows(&rows) {
            Ok((total, active)) => {
                crate::historian::log(
                    "INFO",
                    &format!("Теги OPC сохранены: команд={total}, в работе={active}"),
                );
                ui.set_tags_status(format!("Сохранено: команд {total}, в работе {active}").into());
            }
            Err(error) => {
                crate::historian::log("ERROR", &format!("Не удалось сохранить теги: {error}"));
                ui.set_tags_status(format!("Ошибка сохранения: {error}").into());
            }
        }
    });
}