use crate::{AppState, AppWindow, MetricPoint};
use parking_lot::Mutex;
use slint::ComponentHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::{
    TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

pub const MAX_TRACKED_DRIVES: usize = 8;

pub static BENCH_BYTES: [AtomicU64; MAX_TRACKED_DRIVES] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

pub static BENCH_LATS: [AtomicU64; MAX_TRACKED_DRIVES] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

#[inline]
pub fn report_benchmark_io(drive_idx: usize, bytes: usize, latency_ms: u64) {
    if drive_idx < MAX_TRACKED_DRIVES {
        BENCH_BYTES[drive_idx].fetch_add(bytes as u64, Ordering::Relaxed);
        BENCH_LATS[drive_idx].fetch_max(latency_ms, Ordering::Relaxed);
    }
}

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
        .with_tooltip("SymStripe Multi-Spindle Accelerator")
        .with_icon(icon)
        .build()
        .unwrap();

    let state_telemetry = state;
    let ui_weak_telemetry = ui_weak;

    thread::spawn(move || {
        let mut last_tick = Instant::now();
        let mut last_sec_tick = Instant::now();

        let mut accum_bytes: [u64; MAX_TRACKED_DRIVES] = [0; MAX_TRACKED_DRIVES];
        let mut max_lats: [u64; MAX_TRACKED_DRIVES] = [0; MAX_TRACKED_DRIVES];

        loop {
            thread::sleep(Duration::from_millis(250));
            let elapsed = last_tick.elapsed().as_secs_f64();
            last_tick = Instant::now();

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

            let (stats_io, stats_graph) = {
                let mut st = state_telemetry.lock();

                let mut current_bytes = [0u64; MAX_TRACKED_DRIVES];
                let mut current_lats = [0u64; MAX_TRACKED_DRIVES];

                for i in 0..MAX_TRACKED_DRIVES {
                    current_bytes[i] += BENCH_BYTES[i].swap(0, Ordering::Relaxed);
                    current_lats[i] = current_lats[i].max(BENCH_LATS[i].swap(0, Ordering::Relaxed));
                }

                let (names, hit_rate, saved_gb) = if let Some(session) = &st.pool_session {
                    let (deltas, lats) = session.sample_and_reset_io();
                    for (i, &d) in deltas.iter().enumerate().take(MAX_TRACKED_DRIVES) {
                        current_bytes[i] += d;
                    }
                    for (i, &l) in lats.iter().enumerate().take(MAX_TRACKED_DRIVES) {
                        current_lats[i] = current_lats[i].max(l);
                    }

                    let n: Vec<String> = session
                        .member_paths
                        .iter()
                        .enumerate()
                        .map(|(i, p)| format!("Drive {} ({})", i + 1, p.to_string_lossy()))
                        .collect();

                    let hits = session.cache_hits.load(Ordering::Relaxed);
                    let misses = session.cache_misses.load(Ordering::Relaxed);
                    let total_cache = hits + misses;
                    let hr = if total_cache > 0 {
                        (hits as f64 / total_cache as f64) * 100.0
                    } else {
                        0.0
                    };
                    let raw = session.raw_bytes.load(Ordering::Relaxed);
                    let comp = session.compressed_bytes.load(Ordering::Relaxed);
                    let sgb = raw.saturating_sub(comp) as f64 / 1_073_741_824.0;
                    (n, hr, sgb)
                } else {
                    (
                        vec![
                            "Drive 1 (Primary)".to_string(),
                            "Drive 2 (Secondary)".to_string(),
                        ],
                        0.0,
                        0.0,
                    )
                };

                for i in 0..MAX_TRACKED_DRIVES {
                    accum_bytes[i] += current_bytes[i];
                    max_lats[i] = max_lats[i].max(current_lats[i]);
                }

                let mut g_update = None;
                let sec_elapsed = last_sec_tick.elapsed().as_secs_f64();

                if sec_elapsed >= 1.0 {
                    let mut speeds = Vec::with_capacity(MAX_TRACKED_DRIVES);
                    let mut total_sec_bytes = 0u64;
                    let mut overall_max_lat = 0u64;

                    for i in 0..MAX_TRACKED_DRIVES {
                        let spd = (accum_bytes[i] as f32 / 1_048_576.0) / sec_elapsed as f32;
                        speeds.push(spd);
                        total_sec_bytes += accum_bytes[i];
                        overall_max_lat = overall_max_lat.max(max_lats[i]);
                    }

                    let total_mbs = (total_sec_bytes as f32 / 1_048_576.0) / sec_elapsed as f32;

                    st.metrics.push_back(MetricPoint {
                        drive_speeds_mbs: speeds,
                        drive_lats_ms: max_lats.to_vec(),
                        total_mbs,
                        max_lat_ms: overall_max_lat,
                    });

                    if st.metrics.len() > 300 {
                        st.metrics.pop_front();
                    }

                    let win = st.graph_window_secs;
                    let skip = st.metrics.len().saturating_sub(win);
                    let mut dynamic_max = 50.0_f32;
                    for pt in st.metrics.iter().skip(skip) {
                        for &spd in &pt.drive_speeds_mbs {
                            dynamic_max = dynamic_max.max(spd);
                        }
                    }

                    let mut graph_data = Vec::new();
                    for pt in st.metrics.iter().skip(skip) {
                        let d1 = pt.drive_speeds_mbs.first().copied().unwrap_or(0.0);
                        let d2 = pt.drive_speeds_mbs.get(1).copied().unwrap_or(0.0);
                        graph_data.push(crate::GraphPoint {
                            d1_val: (d1 / dynamic_max).clamp(0.0, 1.0),
                            d2_val: (d2 / dynamic_max).clamp(0.0, 1.0),
                            is_stall: pt.max_lat_ms > 250,
                        });
                    }

                    let d1_sec = (accum_bytes[0] as f32 / 1_048_576.0) / sec_elapsed as f32;
                    let d2_sec = (accum_bytes[1] as f32 / 1_048_576.0) / sec_elapsed as f32;

                    let insights = if overall_max_lat > 250 {
                        "🔴 Spindle Bottleneck: High latency (>250ms) detected. Micro-stalls likely."
                    } else if d1_sec > 15.0 && d2_sec > 15.0 {
                        "🟢 Optimal Spindle Parallelism: Multi-drive concurrent streaming active."
                    } else if (d1_sec > 15.0 && d2_sec < 2.0) || (d2_sec > 15.0 && d1_sec < 2.0) {
                        "🟡 Asymmetric Spindle Load Detected."
                    } else {
                        "⚪ System Idle or Low Activity."
                    };

                    g_update = Some((graph_data, insights.to_string()));

                    accum_bytes = [0; MAX_TRACKED_DRIVES];
                    max_lats = [0; MAX_TRACKED_DRIVES];
                    last_sec_tick = Instant::now();
                }

                (
                    Some((
                        current_bytes[0],
                        current_bytes[1],
                        names,
                        hit_rate,
                        saved_gb,
                    )),
                    g_update,
                )
            };

            let _ = ui_weak_telemetry.upgrade_in_event_loop(move |ui| {
                if let Some((d1_b, d2_b, names, hit_rate, saved_gb)) = stats_io {
                    let d1_mbs = (d1_b as f64 / 1_048_576.0) / elapsed.max(0.001);
                    let d2_mbs = (d2_b as f64 / 1_048_576.0) / elapsed.max(0.001);
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
                }

                if let Some((g_data, insights)) = stats_graph {
                    use slint::{ModelRc, VecModel};
                    ui.set_graph_data(ModelRc::new(VecModel::from(g_data)));
                    ui.set_insights_text(insights.into());
                }
            });
        }
    });

    tray_icon
}
