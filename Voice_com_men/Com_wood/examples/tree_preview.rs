use slint::{ModelRc, VecModel};
use std::rc::Rc;

slint::include_modules!();

fn item(
    id: i32,
    text: &str,
    depth: i32,
    parent_id: i32,
    has_children: bool,
    is_last: bool,
    parent_x: f32,
    parent_y: f32,
    self_x: f32,
    self_y: f32,
) -> MyTreeItem {
    MyTreeItem {
        id,
        text: text.into(),
        depth,
        is_last,
        has_children,
        is_expanded: true,
        is_editing: false,
        parent_is_last: false,
        is_selected: id == 6,
        note: "".into(),
        text_width: text.chars().count() as f32 * 7.2 + 20.0,
        smart_id: format!("preview-{id}").into(),
        line_mask: ModelRc::from(Rc::new(VecModel::from(vec![0; 12]))),
        parent_id,
        parent_triangle_x: parent_x,
        parent_triangle_y: parent_y,
        self_triangle_x: self_x,
        self_triangle_y: self_y,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = TreePreview::new()?;
    let data = vec![
        item(1, "Основной узел", 0, 0, true, false, 0.0, 0.0, 40.0, 26.0),
        item(
            2,
            "Команда запуска",
            1,
            1,
            true,
            false,
            40.0,
            26.0,
            64.0,
            58.0,
        ),
        item(
            3,
            "Проверка доступа",
            2,
            2,
            false,
            true,
            64.0,
            58.0,
            88.0,
            90.0,
        ),
        item(
            4,
            "Команда остановки",
            1,
            1,
            false,
            false,
            40.0,
            26.0,
            64.0,
            122.0,
        ),
        item(5, "Настройки", 0, 0, true, true, 0.0, 0.0, 40.0, 154.0),
        item(
            6,
            "Профиль пользователя",
            1,
            5,
            false,
            true,
            40.0,
            154.0,
            64.0,
            186.0,
        ),
    ];
    let tree = SlintCADTree {
        id: 1,
        cad_x: 40.0,
        cad_y: 95.0,
        width: 430.0,
        count: data.len() as i32,
        data: ModelRc::from(Rc::new(VecModel::from(data))),
    };
    app.set_tree(tree);
    app.run()?;
    Ok(())
}
