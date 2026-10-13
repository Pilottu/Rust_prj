slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = LinePreview::new()?;
    app.set_line(CADLine {
        id: 1,
        id_obj1: 101,
        id_obj2: 202,
        x1: 180.0,
        y1: 220.0,
        x2: 760.0,
        y2: 390.0,
        tree_id_1: 1,
        tree_id_2: 2,
        side1: 0,
        side2: 1,
        mid_x: 470.0,
    });
    app.run()?;
    Ok(())
}
