#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use std::{cell::RefCell, rc::Rc};

use slint::ComponentHandle;

slint::include_modules!();

mod command_classifier;
mod historian;
mod repository;
mod state;
mod tree_ops;
mod ui_bridge;
mod win_native;

fn main() {
    let app = AppWindow::new().unwrap();

    let mut initial_state = state::load_initial_state();
    command_classifier::classify_state(&mut initial_state);
    repository::save_persisted_state(&initial_state);
    let state = Rc::new(RefCell::new(initial_state));
    tree_ops::update_ui_models(&app, &state.borrow());

    ui_bridge::wire_callbacks(&app, &state);

    win_native::fix_window_chrome(app.as_weak());
    win_native::spawn_hotkey_thread(app.as_weak());

    app.run().unwrap();
}
