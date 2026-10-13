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
use std::collections::VecDeque;
use std::sync::Arc;
use vfs_pool::VfsPoolSession;

slint::include_modules!();

#[derive(Clone, Copy, Debug)]
pub struct MetricPoint {
    pub d1_mbs: f32,
    pub d2_mbs: f32,
    pub d1_lat: u64,
    pub d2_lat: u64,
}

pub struct AppState {
    pub config: AppConfig,
    pub pool_session: Option<VfsPoolSession>,
    pub metrics: VecDeque<MetricPoint>,
    pub graph_window_secs: usize,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Disable Windows 11 EcoQoS / Efficiency Mode and elevate process priority
    win32::boost_process_priority();

    // Initialize native crash handler (Rust panic hook + Win32 SEH filter)
    crash_handler::setup_crash_handler();

    // Initialize UI window and application state
    let app = AppWindow::new()?;
    let initial_config = load_config();

    let state = Arc::new(Mutex::new(AppState {
        config: initial_config,
        pool_session: None,
        metrics: VecDeque::with_capacity(300), // Stores up to 5 minutes of telemetry data
        graph_window_secs: 60,                 // Default graph scale: 60 seconds
    }));

    // Initialize active profile and UI fields
    ui_controller::init_ui_state(&app, state.clone());

    // Initialize system tray icon and background telemetry/graph loop
    let _tray_icon = telemetry::init_tray_and_telemetry(state.clone(), app.as_weak());

    // Bind all UI callbacks to domain logic
    ui_controller::bind_ui_handlers(&app, state.clone());

    // Minimize to system tray on close request
    app.window()
        .on_close_requested(move || CloseRequestResponse::HideWindow);

    // Run main Slint event loop
    app.run()?;

    // Gracefully unmount and clean up VFS session upon termination
    state.lock().pool_session = None;

    Ok(())
}
