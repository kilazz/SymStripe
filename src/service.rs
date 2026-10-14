use crate::config::load_config;
use crate::vfs::mount_storage_pool;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

pub const SERVICE_NAME: &str = "SymStripeService";
pub const SERVICE_DISPLAY_NAME: &str = "SymStripe Virtual Storage Pool Driver";

define_windows_service!(ffi_service_main, symstripe_service_main);

/// Installs SymStripe as an automatic Windows Service running under LocalSystem.
pub fn install_service() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;

    let exe_path = std::env::current_exe()?;

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path,
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // LocalSystem account (mounts at system boot before logon)
        account_password: None,
    };

    let _service =
        manager.create_service(&service_info, ServiceAccess::START | ServiceAccess::STOP)?;
    Ok(())
}

/// Uninstalls and deletes the Windows Service registration.
pub fn uninstall_service() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;

    if let Ok(service) =
        manager.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE)
    {
        let _ = service.stop();
        std::thread::sleep(Duration::from_millis(500));
        service.delete()?;
    }
    Ok(())
}

/// Main entry point when invoked by Windows Service Control Manager.
pub fn run_service() -> Result<(), Box<dyn std::error::Error>> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn symstripe_service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service_payload() {
        eprintln!("[SymStripeService] Service terminated with error: {:?}", e);
    }
}

fn run_service_payload() -> Result<(), Box<dyn std::error::Error>> {
    let stop_signal = Arc::new(AtomicBool::new(false));
    let stop_clone = stop_signal.clone();

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                stop_clone.store(true, Ordering::Relaxed);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StartPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 1,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    })?;

    // Load persistent profile from ProgramData
    let cfg = load_config();
    let prof = &cfg.profiles[cfg.active_profile_index];

    let mut _active_session = None;

    if prof.mode_idx == 1 && !prof.pool_members.is_empty() {
        let members: Vec<PathBuf> = prof.pool_members.iter().map(PathBuf::from).collect();
        let stripe_bytes = (prof.pool_stripe_mb as u64) * 1024 * 1024;
        let custom_gb = prof.pool_custom_size_gb.trim().parse::<u64>().unwrap_or(0);

        if let Ok(session) = mount_storage_pool(
            &prof.pool_mount_point,
            &members,
            prof.pool_policy,
            stripe_bytes,
            custom_gb,
            prof.pool_codec,
            prof.tiered_storage,
            prof.pool_ram_cache_mb,
            None,
        ) {
            _active_session = Some(session);
        }
    }

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    while !stop_signal.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
    }

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StopPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 2,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    })?;

    drop(_active_session); // Flushes all dirty nodes and unmounts drive cleanly

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}
