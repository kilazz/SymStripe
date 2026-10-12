use crate::{AppState, AppWindow};
use parking_lot::Mutex;
use slint::ComponentHandle;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::{
    TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

/// Initializes the system tray menu and background telemetry polling thread.
pub fn init_tray_and_telemetry(
    state: Arc<Mutex<AppState>>,
    ui_weak: slint::Weak<AppWindow>,
) -> TrayIcon {
    let tray_menu = Menu::new();
    let quit_i = MenuItem::new("Quit SymStripe", true, None);
    let open_i = MenuItem::new("Open Dashboard", true, None);
    let unmount_i = MenuItem::new("Unmount Pool", true, None);

    tray_menu
        .append_items(&[
            &open_i,
            &unmount_i,
            &PredefinedMenuItem::separator(),
            &quit_i,
        ])
        .unwrap();

    let quit_id = quit_i.id().clone();
    let open_id = open_i.id().clone();
    let unmount_id = unmount_i.id().clone();

    let rgba = vec![255u8; 16 * 16 * 4];
    let icon = tray_icon::Icon::from_rgba(rgba, 16, 16).unwrap();

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("SymStripe RAM-Accelerated RAID-0")
        .with_icon(icon)
        .build()
        .unwrap();

    let state_telemetry = state;
    let ui_weak_telemetry = ui_weak;

    thread::spawn(move || {
        let mut last_tick = Instant::now();
        loop {
            thread::sleep(Duration::from_millis(300));
            let elapsed = last_tick.elapsed().as_secs_f64();
            last_tick = Instant::now();

            // Tray Event Polling
            if let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == quit_id {
                    std::process::exit(0);
                } else if event.id == open_id {
                    let ui = ui_weak_telemetry.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui.upgrade() {
                            let _ = ui.window().show();
                        }
                    });
                } else if event.id == unmount_id {
                    let st_clone = state_telemetry.clone();
                    let ui = ui_weak_telemetry.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let mut st = st_clone.lock();
                        st.pool_session = None;
                        if let Some(ui) = ui.upgrade() {
                            ui.set_is_pool_mounted(false);
                            ui.set_status_msg("Virtual Storage Pool unmounted from Tray.".into());
                        }
                    });
                }
            }

            if let Ok(TrayIconEvent::DoubleClick { .. }) = TrayIconEvent::receiver().try_recv() {
                let ui = ui_weak_telemetry.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui.upgrade() {
                        let _ = ui.window().show();
                    }
                });
            }

            // Real-Time I/O and RAM Cache Metrics Sampling
            let stats = {
                let st = state_telemetry.lock();
                if let Some(session) = &st.pool_session {
                    let deltas = session.sample_and_reset_io();
                    let names: Vec<String> = session
                        .member_paths
                        .iter()
                        .enumerate()
                        .map(|(i, p)| {
                            let drive = p.to_string_lossy();
                            format!("Drive {} ({})", i + 1, drive)
                        })
                        .collect();

                    let hits = session.cache_hits.load(Ordering::Relaxed);
                    let misses = session.cache_misses.load(Ordering::Relaxed);
                    let total_cache = hits + misses;
                    let hit_rate = if total_cache > 0 {
                        (hits as f64 / total_cache as f64) * 100.0
                    } else {
                        0.0
                    };

                    let raw = session.raw_bytes.load(Ordering::Relaxed);
                    let comp = session.compressed_bytes.load(Ordering::Relaxed);
                    let saved_gb = raw.saturating_sub(comp) as f64 / 1_073_741_824.0;

                    Some((deltas, names, hit_rate, saved_gb))
                } else {
                    None
                }
            };

            let _ = ui_weak_telemetry.upgrade_in_event_loop(move |ui| {
                if let Some((deltas, names, hit_rate, saved_gb)) = stats {
                    let d1_bytes = deltas.first().copied().unwrap_or(0);
                    let d2_bytes = deltas.get(1).copied().unwrap_or(0);

                    let d1_mbs = (d1_bytes as f64 / 1_048_576.0) / elapsed.max(0.001);
                    let d2_mbs = (d2_bytes as f64 / 1_048_576.0) / elapsed.max(0.001);
                    let total_mbs = d1_mbs + d2_mbs;

                    let max_scale = (d1_mbs.max(d2_mbs) * 1.25).max(100.0);
                    let d1_pct = (d1_mbs / max_scale).clamp(0.0, 1.0) as f32;
                    let d2_pct = (d2_mbs / max_scale).clamp(0.0, 1.0) as f32;

                    ui.set_drive1_speed_text(format!("{:.1} MB/s", d1_mbs).into());
                    ui.set_drive2_speed_text(format!("{:.1} MB/s", d2_mbs).into());
                    ui.set_drive1_load_pct(d1_pct);
                    ui.set_drive2_load_pct(d2_pct);
                    ui.set_total_io_text(format!("{:.1} MB/s", total_mbs).into());

                    ui.set_cache_hit_rate_text(format!("{:.1}%", hit_rate).into());
                    ui.set_saved_space_text(format!("{:.2} GB", saved_gb).into());

                    if let Some(n1) = names.first() {
                        ui.set_drive1_label(n1.as_str().into());
                    }
                    if let Some(n2) = names.get(1) {
                        ui.set_drive2_label(n2.as_str().into());
                    }

                    ui.set_is_io_active(total_mbs > 0.5);
                } else {
                    ui.set_drive1_speed_text("0.0 MB/s".into());
                    ui.set_drive2_speed_text("0.0 MB/s".into());
                    ui.set_drive1_load_pct(0.0);
                    ui.set_drive2_load_pct(0.0);
                    ui.set_total_io_text("0.0 MB/s".into());
                    ui.set_is_io_active(false);
                }
            });
        }
    });

    tray_icon
}
