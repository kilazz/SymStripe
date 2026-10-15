mod aligned_buf;
mod benchmark;
mod config;
mod crash_handler;
mod manifest;
mod scanner;
mod service;
mod stripe_math;
mod striping;
mod telemetry;
mod ui_controller;
mod vfs;
mod win32;
mod zstd_atg;

use config::{AppConfig, load_config};
use mimalloc::MiMalloc;
use parking_lot::Mutex;
use slint::CloseRequestResponse;
use std::collections::VecDeque;
use std::sync::Arc;
use vfs::VfsPoolSession;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

slint::include_modules!();

#[derive(Clone, Debug)]
pub struct MetricPoint {
    pub drive_speeds_mbs: Vec<f32>,
    pub drive_lats_ms: Vec<u64>,
    pub total_mbs: f32,
    pub max_lat_ms: u64,
}

pub struct AppState {
    pub config: AppConfig,
    pub pool_session: Option<VfsPoolSession>,
    pub metrics: VecDeque<MetricPoint>,
    pub graph_window_secs: usize,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Process Windows Service CLI commands before initializing GUI
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        match args[1].as_str() {
            "--service" => {
                service::run_service()?;
                return Ok(());
            }
            "--install-service" => {
                service::install_service()?;
                println!("✅ SymStripe Windows Service successfully installed with Auto-Start!");
                println!("Virtual Storage Pool will mount at boot prior to user login.");
                return Ok(());
            }
            "--uninstall-service" => {
                service::uninstall_service()?;
                println!("✅ SymStripe Windows Service stopped and uninstalled successfully.");
                return Ok(());
            }
            _ => {}
        }
    }

    // Disable Windows 11 EcoQoS and elevate priority
    win32::boost_process_priority();

    // Initialize crash handler
    crash_handler::setup_crash_handler();

    // Initialize UI and application state
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
