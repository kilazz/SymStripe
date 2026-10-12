mod benchmark;
mod config;
mod crash_handler;
mod manifest;
mod scanner;
mod stripe_math;
mod striping;
mod telemetry;
mod ui_controller;
mod vfs_pool;
mod win32;
mod zstd_atg;

use config::{AppConfig, load_config};
use parking_lot::Mutex;
use slint::CloseRequestResponse;
use std::sync::Arc;
use vfs_pool::VfsPoolSession;

slint::include_modules!();

pub struct AppState {
    pub config: AppConfig,
    pub pool_session: Option<VfsPoolSession>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize native crash handler
    crash_handler::setup_crash_handler();

    // Initialize UI window and application state
    let app = AppWindow::new()?;
    let initial_config = load_config();

    let state = Arc::new(Mutex::new(AppState {
        config: initial_config,
        pool_session: None,
    }));

    // Initialize active profile and UI fields
    ui_controller::init_ui_state(&app, state.clone());

    // Initialize system tray icon and background telemetry loop
    let _tray_icon = telemetry::init_tray_and_telemetry(state.clone(), app.as_weak());

    // Bind all UI callbacks to the controller
    ui_controller::bind_ui_handlers(&app, state.clone());

    // Minimize to system tray on close request
    app.window()
        .on_close_requested(move || CloseRequestResponse::HideWindow);

    // Run main Slint event loop
    app.run()?;

    // Gracefully unmount and cleanup VFS session upon termination
    state.lock().pool_session = None;

    Ok(())
}
