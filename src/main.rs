mod benchmark;
mod config;
mod manifest;
mod stripe_math;
mod striping;
mod vfs_pool;
mod win32;

use benchmark::run_benchmark_test;
use config::{AppConfig, Profile, load_config, save_config};
use manifest::load_manifest;
use striping::{
    auto_detect_threshold, calculate_allocation_plan, execute_consolidate, execute_revert,
    execute_striping, parse_ext_list, verify_and_repair_integrity,
};
use vfs_pool::{VfsPoolSession, mount_storage_pool};
use win32::{get_logical_volumes, query_smart_disks};

use rfd::FileDialog;
use slint::{ModelRc, SharedString, StandardListViewItem, VecModel};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

slint::include_modules!();

struct AppState {
    config: AppConfig,
    pool_session: Option<VfsPoolSession>,
}

fn append_log(ui_handle: &slint::Weak<AppWindow>, msg: &str) {
    let msg_string = msg.to_string();
    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
        let mut logs = ui.get_log_text().to_string();
        logs.push_str(&format!("[SymStripe] {}\n", msg_string));
        ui.set_log_text(logs.into());
    });
}

fn update_status(ui_handle: &slint::Weak<AppWindow>, msg: &str) {
    let msg_string = msg.to_string();
    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
        ui.set_status_msg(msg_string.into());
    });
}

fn inspect_game_folder(src_path: &Path, ui: &slint::Weak<AppWindow>) {
    if let Some(manifest) = load_manifest(src_path) {
        let total_moved = manifest.files.len();
        let total_bytes: u64 = manifest.files.iter().map(|f| f.size_bytes).sum();
        let gb = total_bytes as f64 / 1e9;

        let mut rows: Vec<String> = Vec::new();
        rows.push(format!(
            "▼ 📁 [DRIVE 1: PRIMARY] — Root ({})",
            src_path.display()
        ));
        rows.push(format!(
            "▼ 📁 [ACTIVE STRIPING] — {} archives ({:.2} GB total)",
            total_moved, gb
        ));

        for f in &manifest.files {
            let rel = Path::new(&f.rel_path);
            let name = rel.file_name().unwrap_or_default().to_string_lossy();
            let parent = rel
                .parent()
                .map(|p| p.to_string_lossy())
                .unwrap_or_default();
            let mb = f.size_bytes as f64 / (1024.0 * 1024.0);
            let b_tag = if f.has_local_backup {
                " [.backup protected]"
            } else {
                ""
            };

            if parent.is_empty() {
                rows.push(format!(
                    "   ├─ [Drive {}] {} ({:.1} MB){}",
                    f.target_drive_idx + 1,
                    name,
                    mb,
                    b_tag
                ));
            } else {
                rows.push(format!(
                    "   ├─ [Drive {}] {} ({:.1} MB)  •  \\{}{}",
                    f.target_drive_idx + 1,
                    name,
                    mb,
                    parent,
                    b_tag
                ));
            }
        }

        let summary = format!(
            "Active Manifest: {} files relocated ({:.1} GB)",
            total_moved, gb
        );

        let _ = ui.upgrade_in_event_loop(move |app_ui| {
            app_ui.set_is_already_striped(true);
            app_ui.set_summary_text(summary.into());
            let items: Vec<StandardListViewItem> = rows
                .into_iter()
                .map(|s| StandardListViewItem::from(SharedString::from(s)))
                .collect();
            app_ui.set_file_list(ModelRc::new(VecModel::from(items)));
            app_ui.set_status_msg("Manifest detected: Directory is currently STRIPED.".into());
        });
    } else {
        let _ = ui.upgrade_in_event_loop(|app_ui| {
            app_ui.set_is_already_striped(false);
            app_ui.set_summary_text("Ready for analysis.".into());
            app_ui.set_file_list(ModelRc::new(VecModel::from(Vec::new())));
        });
    }
}

fn apply_profile_to_ui(profile: &Profile, app: &AppWindow, ui_weak: &slint::Weak<AppWindow>) {
    app.set_primary_path(profile.primary_path.clone().into());
    app.set_min_size_mb_text(profile.min_size_mb.clone().into());
    app.set_exclusions_text(profile.exclusions.clone().into());
    app.set_media_extensions_text(profile.media_extensions.clone().into());
    app.set_keep_backup(profile.keep_backup);
    app.set_aggressive_media(profile.aggressive_media);
    app.set_ue5_safe_mode(profile.ue5_safe_mode);
    app.set_active_mode_idx(profile.mode_idx);

    let target_rows: Vec<String> = profile
        .secondary_targets
        .iter()
        .enumerate()
        .map(|(idx, p)| format!("[Drive {}] {}", idx + 2, p))
        .collect();

    let items: Vec<StandardListViewItem> = target_rows
        .into_iter()
        .map(|s| StandardListViewItem::from(SharedString::from(s)))
        .collect();
    app.set_target_drives_list(ModelRc::new(VecModel::from(items)));

    let pool_rows: Vec<String> = profile
        .pool_members
        .iter()
        .enumerate()
        .map(|(idx, p)| format!("[Member {}] {}", idx + 1, p))
        .collect();

    let pool_items: Vec<StandardListViewItem> = pool_rows
        .into_iter()
        .map(|s| StandardListViewItem::from(SharedString::from(s)))
        .collect();
    app.set_pool_members_list(ModelRc::new(VecModel::from(pool_items)));
    app.set_pool_policy_idx(profile.pool_policy);
    app.set_pool_stripe_mb(profile.pool_stripe_mb as i32);
    app.set_pool_mount_point(profile.pool_mount_point.clone().into());
    app.set_pool_custom_size_gb_text(profile.pool_custom_size_gb.clone().into());

    if !profile.primary_path.is_empty() {
        inspect_game_folder(Path::new(&profile.primary_path), ui_weak);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = AppWindow::new()?;
    let ui_weak = app.as_weak();

    let initial_config = load_config();

    let profile_names: Vec<SharedString> = initial_config
        .profiles
        .iter()
        .map(|p| p.name.clone().into())
        .collect();
    app.set_profile_names(ModelRc::new(VecModel::from(profile_names)));

    let active_idx = initial_config
        .active_profile_index
        .min(initial_config.profiles.len().saturating_sub(1));
    app.set_active_profile_idx(active_idx as i32);

    if let Some(prof) = initial_config.profiles.get(active_idx) {
        apply_profile_to_ui(prof, &app, &ui_weak);
    }

    let state = Arc::new(Mutex::new(AppState {
        config: initial_config,
        pool_session: None,
    }));

    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_mode_switched(move |mode_idx| {
        let mut st = state_arc.lock().unwrap();
        let idx = st.config.active_profile_index;
        if let Some(prof) = st.config.profiles.get_mut(idx) {
            prof.mode_idx = mode_idx;
            save_config(&st.config);
        }
        let mode_name = if mode_idx == 0 {
            "In-Place Game Optimizer (Source -> Targets)"
        } else {
            "Virtual Storage Pool (Unified Multi-Drive VFS)"
        };
        append_log(&weak, &format!("Paradigm switched to: {}", mode_name));
    });

    let state_arc = state.clone();
    let weak = ui_weak.clone();
    app.on_browse_primary_folder(move || {
        if let Some(p) = FileDialog::new().pick_folder() {
            let p_str = p.to_string_lossy().to_string();
            let mut st = state_arc.lock().unwrap();
            let idx = st.config.active_profile_index;
            if let Some(prof) = st.config.profiles.get_mut(idx) {
                prof.primary_path = p_str.clone();
                save_config(&st.config);
            }
            inspect_game_folder(&p, &weak);
            p_str.into()
        } else {
            "".into()
        }
    });

    let weak = ui_weak.clone();
    app.on_primary_folder_changed(move |new_path| {
        let p = PathBuf::from(new_path.as_str());
        if p.exists() {
            inspect_game_folder(&p, &weak);
        }
    });

    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_add_target_drive(move || {
        if let Some(folder) = FileDialog::new().pick_folder() {
            let mut st = state_arc.lock().unwrap();
            let folder_str = folder.to_string_lossy().to_string();
            let idx = st.config.active_profile_index;

            if let Some(prof) = st.config.profiles.get_mut(idx) {
                prof.secondary_targets.push(folder_str);

                let rows: Vec<String> = prof
                    .secondary_targets
                    .iter()
                    .enumerate()
                    .map(|(i, p)| format!("[Drive {}] {}", i + 2, p))
                    .collect();

                save_config(&st.config);

                let _ = weak.upgrade_in_event_loop(move |ui| {
                    let items: Vec<StandardListViewItem> = rows
                        .into_iter()
                        .map(|s| StandardListViewItem::from(SharedString::from(s)))
                        .collect();
                    ui.set_target_drives_list(ModelRc::new(VecModel::from(items)));
                });
                append_log(&weak, "Target destination drive added to profile.");
            }
        }
    });

    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_clear_target_drives(move || {
        let mut st = state_arc.lock().unwrap();
        let idx = st.config.active_profile_index;
        if let Some(prof) = st.config.profiles.get_mut(idx) {
            prof.secondary_targets.clear();
            save_config(&st.config);
        }

        let _ = weak.upgrade_in_event_loop(|ui| {
            ui.set_target_drives_list(ModelRc::new(VecModel::from(Vec::new())));
        });
        append_log(&weak, "Secondary target drives cleared.");
    });

    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_consolidate_target_drive(move || {
        let w = weak.clone();
        let st_lock = state_arc.clone();

        thread::spawn(move || {
            let (src_dir, targets) = {
                let st = st_lock.lock().unwrap();
                let idx = st.config.active_profile_index;
                let prof = &st.config.profiles[idx];
                (prof.primary_path.clone(), prof.secondary_targets.clone())
            };

            if targets.is_empty() {
                append_log(&w, "No target drives to consolidate.");
                return;
            }

            let src_path = PathBuf::from(&src_dir);
            append_log(
                &w,
                "Select secondary target folder to consolidate & free...",
            );
            let Some(chosen_folder) = FileDialog::new().pick_folder() else {
                append_log(&w, "Consolidation canceled.");
                return;
            };

            let chosen_str = chosen_folder.to_string_lossy().to_string();
            append_log(&w, &format!("Consolidating files from: {}", chosen_str));

            let _ = w.upgrade_in_event_loop(|ui| {
                ui.set_is_processing(true);
                ui.set_progress(0.0);
            });

            match execute_consolidate(&src_path, &chosen_folder) {
                Ok(count) => {
                    {
                        let mut st = st_lock.lock().unwrap();
                        let idx = st.config.active_profile_index;
                        if let Some(prof) = st.config.profiles.get_mut(idx) {
                            prof.secondary_targets.retain(|t| t != &chosen_str);

                            let rows: Vec<String> = prof
                                .secondary_targets
                                .iter()
                                .enumerate()
                                .map(|(i, p)| format!("[Drive {}] {}", i + 2, p))
                                .collect();

                            save_config(&st.config);

                            let _ = w.upgrade_in_event_loop(move |ui| {
                                let items: Vec<StandardListViewItem> = rows
                                    .into_iter()
                                    .map(|s| StandardListViewItem::from(SharedString::from(s)))
                                    .collect();
                                ui.set_target_drives_list(ModelRc::new(VecModel::from(items)));
                            });
                        }
                    }

                    inspect_game_folder(&src_path, &w);
                    let _ = w.upgrade_in_event_loop(|ui| {
                        ui.set_progress(1.0);
                        ui.set_is_processing(false);
                        ui.set_status_msg("Target drive consolidated and freed.".into());
                    });
                    append_log(&w, &format!("Successfully consolidated {} file(s).", count));
                }
                Err(err) => {
                    append_log(&w, &format!("Error: {}", err));
                    update_status(&w, "Consolidation failed.");
                }
            }
        });
    });

    let weak = ui_weak.clone();
    app.on_auto_detect_threshold(move |src_dir, exclusions_str| {
        let src = src_dir.to_string();
        let excl = exclusions_str.to_string();
        let w = weak.clone();

        thread::spawn(move || {
            let src_path = PathBuf::from(&src);
            if !src_path.exists() {
                append_log(&w, "Error: Primary directory does not exist!");
                return;
            }

            let excl_list = parse_ext_list(&excl);
            update_status(&w, "Auto-detecting optimal threshold...");

            if let Some(optimal_mb) = auto_detect_threshold(&src_path, &excl_list) {
                append_log(
                    &w,
                    &format!(
                        "Auto-Detect: Set threshold to {} MB to cover ~80% of data.",
                        optimal_mb
                    ),
                );
                let _ = w.upgrade_in_event_loop(move |ui| {
                    ui.set_min_size_mb_text(optimal_mb.to_string().into());
                });
                update_status(&w, "Threshold optimized.");
            } else {
                append_log(&w, "Auto-Detect failed: No suitable files found.");
            }
        });
    });

    let weak = ui_weak.clone();
    app.on_verify_integrity(move |src_dir| {
        let src = src_dir.to_string();
        let w = weak.clone();

        thread::spawn(move || {
            let src_path = PathBuf::from(&src);
            append_log(&w, "--- VERIFYING STORAGE INTEGRITY ---");
            update_status(&w, "Verifying manifest and target files...");

            match verify_and_repair_integrity(&src_path) {
                Ok((intact, repaired)) => {
                    append_log(
                        &w,
                        &format!("Integrity check complete: {} file(s) intact.", intact),
                    );
                    if repaired > 0 {
                        append_log(&w, &format!("⚠️ [ALERT] {} missing file(s) detected! Auto-restored from .backup.", repaired));
                    }
                    inspect_game_folder(&src_path, &w);
                    update_status(&w, "Integrity verified.");
                }
                Err(err) => {
                    append_log(&w, &format!("Integrity error: {}", err));
                    update_status(&w, "Verification failed.");
                }
            }
        });
    });

    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_preview_striping(
        move |src_dir,
              min_mb,
              exclusions_str,
              media_exts_str,
              keep_backup,
              aggressive_media,
              ue5_safe_mode| {
            let src = src_dir.to_string();
            let excl = exclusions_str.to_string();
            let media = media_exts_str.to_string();
            let threshold = (min_mb as u64) * 1024 * 1024;
            let w = weak.clone();
            let st_lock = state_arc.clone();

            thread::spawn(move || {
                let targets: Vec<PathBuf> = {
                    let st = st_lock.lock().unwrap();
                    let idx = st.config.active_profile_index;
                    st.config.profiles[idx]
                        .secondary_targets
                        .iter()
                        .map(PathBuf::from)
                        .collect()
                };

                if targets.is_empty() {
                    append_log(&w, "Error: Add at least one secondary drive!");
                    update_status(&w, "Error: No secondary drives specified.");
                    return;
                }

                let src_path = PathBuf::from(&src);
                if !src_path.exists() {
                    append_log(&w, "Error: Primary directory does not exist!");
                    update_status(&w, "Error: Invalid primary directory.");
                    return;
                }

                let excl_list = parse_ext_list(&excl);
                let media_list = parse_ext_list(&media);
                let mode_str = if ue5_safe_mode {
                    "UE5 Subfolder Junctions"
                } else {
                    "File Symlinks"
                };
                append_log(
                    &w,
                    &format!("--- PREVIEW ANALYSIS (Strategy: {}) ---", mode_str),
                );

                let Some(plan) = calculate_allocation_plan(
                    &src_path,
                    &targets,
                    threshold,
                    &excl_list,
                    &media_list,
                    aggressive_media,
                ) else {
                    append_log(&w, "No files matching criteria found for striping.");
                    update_status(&w, "No eligible files found.");
                    return;
                };

                let mut list_rows = Vec::new();
                let d1_gb = plan.drive_bytes[0] as f64 / 1e9;
                list_rows.push(format!(
                    "▼ 📁 [DRIVE 1: PRIMARY] — {} archives ({:.2} GB retained)",
                    plan.drive_buckets[0].len(),
                    d1_gb
                ));
                for (f_path, size, _) in &plan.drive_buckets[0] {
                    if let Ok(rel) = f_path.strip_prefix(&src_path) {
                        let name = rel.file_name().unwrap_or_default().to_string_lossy();
                        let mb = *size as f64 / (1024.0 * 1024.0);
                        list_rows.push(format!("   ├─ [LOCAL] {} ({:.1} MB)", name, mb));
                    }
                }

                for (t_idx, target_files) in plan.drive_buckets.iter().enumerate().skip(1) {
                    let target_dir = &targets[t_idx - 1];
                    let dt_gb = plan.drive_bytes[t_idx] as f64 / 1e9;
                    list_rows.push(format!(
                        "▼ 📁 [DRIVE {}: TARGET ({})] — {} archives ({:.2} GB relocated)",
                        t_idx + 1,
                        target_dir.display(),
                        target_files.len(),
                        dt_gb
                    ));

                    for (f_path, size, _) in target_files {
                        if let Ok(rel) = f_path.strip_prefix(&src_path) {
                            let name = rel.file_name().unwrap_or_default().to_string_lossy();
                            let mb = *size as f64 / (1024.0 * 1024.0);
                            let tag = if ue5_safe_mode {
                                "[to UE5 Junction]"
                            } else {
                                "[to Symlink]"
                            };
                            let b_tag = if keep_backup { " [.backup]" } else { "" };
                            list_rows
                                .push(format!("   ├─ {} {} ({:.1} MB){}", tag, name, mb, b_tag));
                        }
                    }
                }

                let total_size: u64 = plan.drive_bytes.iter().sum();
                let summary = format!(
                    "Total: {} files ({:.1} GB) | Drive 1: {:.1} GB | Split: {:.1} GB",
                    plan.files.len(),
                    total_size as f64 / 1e9,
                    plan.drive_bytes[0] as f64 / 1e9,
                    (total_size - plan.drive_bytes[0]) as f64 / 1e9
                );

                let _ = w.upgrade_in_event_loop(move |ui| {
                    ui.set_summary_text(summary.into());
                    let items: Vec<StandardListViewItem> = list_rows
                        .into_iter()
                        .map(|s| StandardListViewItem::from(SharedString::from(s)))
                        .collect();
                    ui.set_file_list(ModelRc::new(VecModel::from(items)));
                });

                append_log(&w, "Preview complete. Ready to distribute.");
                update_status(&w, "Preview complete.");
            });
        },
    );

    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_apply_striping(
        move |src_dir,
              min_mb,
              exclusions_str,
              media_exts_str,
              keep_backup,
              aggressive_media,
              ue5_safe_mode| {
            let src = src_dir.to_string();
            let excl = exclusions_str.to_string();
            let media = media_exts_str.to_string();
            let threshold = (min_mb as u64) * 1024 * 1024;
            let w = weak.clone();
            let st_lock = state_arc.clone();

            thread::spawn(move || {
                {
                    let mut st = st_lock.lock().unwrap();
                    let idx = st.config.active_profile_index;
                    if let Some(prof) = st.config.profiles.get_mut(idx) {
                        prof.primary_path = src.clone();
                        prof.min_size_mb = min_mb.to_string();
                        prof.exclusions = excl.clone();
                        prof.media_extensions = media.clone();
                        prof.keep_backup = keep_backup;
                        prof.aggressive_media = aggressive_media;
                        prof.ue5_safe_mode = ue5_safe_mode;
                        save_config(&st.config);
                    }
                }

                let targets: Vec<PathBuf> = {
                    let st = st_lock.lock().unwrap();
                    let idx = st.config.active_profile_index;
                    st.config.profiles[idx]
                        .secondary_targets
                        .iter()
                        .map(PathBuf::from)
                        .collect()
                };

                if targets.is_empty() {
                    append_log(&w, "Error: Add at least one secondary drive!");
                    update_status(&w, "Error: No secondary drives specified.");
                    return;
                }

                let src_path = PathBuf::from(&src);
                let excl_list = parse_ext_list(&excl);
                let media_list = parse_ext_list(&media);

                let Some(plan) = calculate_allocation_plan(
                    &src_path,
                    &targets,
                    threshold,
                    &excl_list,
                    &media_list,
                    aggressive_media,
                ) else {
                    append_log(&w, "No files matching criteria found.");
                    update_status(&w, "No files found to stripe.");
                    return;
                };

                let _ = w.upgrade_in_event_loop(|ui| {
                    ui.set_is_processing(true);
                    ui.set_progress(0.0);
                });

                let w_cb = w.clone();
                let res = execute_striping(
                    &src_path,
                    &targets,
                    plan,
                    keep_backup,
                    ue5_safe_mode,
                    move |curr, total, name, pct| {
                        let status_text = format!(
                            "[{}/{}] Transferring {} ({:.0}%)...",
                            curr,
                            total,
                            name,
                            pct * 100.0
                        );
                        let _ = w_cb.upgrade_in_event_loop(move |ui| {
                            ui.set_progress(pct);
                            ui.set_status_msg(status_text.into());
                        });
                    },
                );

                match res {
                    Ok(moved_count) => {
                        inspect_game_folder(&src_path, &w);
                        let _ = w.upgrade_in_event_loop(|ui| {
                            ui.set_progress(1.0);
                            ui.set_is_processing(false);
                            ui.set_is_already_striped(true);
                            ui.set_status_msg("Striping complete! Manifest generated.".into());
                        });
                        append_log(
                            &w,
                            &format!("Operation complete! Relocated {} archive(s).", moved_count),
                        );
                    }
                    Err(err_msg) => {
                        append_log(&w, &err_msg);
                        let _ = w.upgrade_in_event_loop(|ui| {
                            ui.set_is_processing(false);
                            ui.set_status_msg("Striping aborted.".into());
                        });
                    }
                }
            });
        },
    );

    let weak = ui_weak.clone();
    app.on_revert_striping(move |src_dir| {
        let src = src_dir.to_string();
        let w = weak.clone();

        thread::spawn(move || {
            append_log(&w, &format!("Reverting all files to primary: {}", src));
            update_status(&w, "Reverting files...");
            let src_path = PathBuf::from(&src);

            let _ = w.upgrade_in_event_loop(|ui| {
                ui.set_is_processing(true);
                ui.set_progress(0.0);
            });

            let w_cb = w.clone();
            let count = execute_revert(&src_path, move |curr, total, pct| {
                let msg = format!("Restoring [{}/{}] ({:.0}%)...", curr, total, pct * 100.0);
                let _ = w_cb.upgrade_in_event_loop(move |ui| {
                    ui.set_progress(pct);
                    ui.set_status_msg(msg.into());
                });
            });

            let _ = w.upgrade_in_event_loop(|ui| {
                ui.set_progress(1.0);
                ui.set_is_processing(false);
                ui.set_is_already_striped(false);
                ui.set_summary_text("All files restored to primary drive.".into());
                ui.set_file_list(ModelRc::new(VecModel::from(Vec::new())));
                ui.set_status_msg("Reversion complete. Manifest deleted.".into());
            });

            append_log(
                &w,
                &format!("Reversion complete: {} file(s) restored.", count),
            );
        });
    });

    // ==========================================
    // PARADIGM 2: VIRTUAL STORAGE POOL (VFS) LOGIC
    // ==========================================

    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_add_pool_member(move || {
        if let Some(folder) = FileDialog::new().pick_folder() {
            let mut st = state_arc.lock().unwrap();
            let folder_str = folder.to_string_lossy().to_string();
            let idx = st.config.active_profile_index;

            if let Some(prof) = st.config.profiles.get_mut(idx) {
                prof.pool_members.push(folder_str);

                let rows: Vec<String> = prof
                    .pool_members
                    .iter()
                    .enumerate()
                    .map(|(i, p)| format!("[Member {}] {}", i + 1, p))
                    .collect();

                save_config(&st.config);

                let _ = weak.upgrade_in_event_loop(move |ui| {
                    let items: Vec<StandardListViewItem> = rows
                        .into_iter()
                        .map(|s| StandardListViewItem::from(SharedString::from(s)))
                        .collect();
                    ui.set_pool_members_list(ModelRc::new(VecModel::from(items)));
                });
                append_log(&weak, "Storage folder added to Virtual Pool.");
            }
        }
    });

    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_clear_pool_members(move || {
        let mut st = state_arc.lock().unwrap();
        let idx = st.config.active_profile_index;
        if let Some(prof) = st.config.profiles.get_mut(idx) {
            prof.pool_members.clear();
            save_config(&st.config);
        }

        let _ = weak.upgrade_in_event_loop(|ui| {
            ui.set_pool_members_list(ModelRc::new(VecModel::from(Vec::new())));
        });
        append_log(&weak, "All pool members cleared.");
    });

    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_pool_policy_switched(move |policy_idx| {
        let mut st = state_arc.lock().unwrap();
        let idx = st.config.active_profile_index;
        if let Some(prof) = st.config.profiles.get_mut(idx) {
            prof.pool_policy = policy_idx;
            save_config(&st.config);
        }
        let policy_name = if policy_idx == 0 {
            "Capacity (JBOD Merge Free Space)"
        } else {
            "Speed (RAID-0 Block Striping)"
        };
        append_log(
            &weak,
            &format!("Pool allocation strategy switched to: {}", policy_name),
        );
    });

    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_pool_stripe_changed(move |stripe_mb| {
        let mut st = state_arc.lock().unwrap();
        let idx = st.config.active_profile_index;
        if let Some(prof) = st.config.profiles.get_mut(idx) {
            prof.pool_stripe_mb = stripe_mb as u32;
            save_config(&st.config);
        }
        append_log(
            &weak,
            &format!("RAID-0 stripe chunk size set to: {} MB", stripe_mb),
        );
    });

    // Mount Virtual Storage Pool (Z:) with Custom or Auto Quota
    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_mount_pool(move |drive_letter, custom_size_str| {
        let w = weak.clone();
        let st_lock = state_arc.clone();
        let drive = drive_letter.to_string();
        let size_str = custom_size_str.to_string();

        thread::spawn(move || {
            let (members, policy, stripe_mb, custom_gb) = {
                let mut st = st_lock.lock().unwrap();
                let idx = st.config.active_profile_index;

                st.config.profiles[idx].pool_custom_size_gb = size_str.clone();
                save_config(&st.config);

                let prof = &st.config.profiles[idx];
                let parsed_gb = size_str.trim().parse::<u64>().unwrap_or(0);
                (
                    prof.pool_members
                        .iter()
                        .map(PathBuf::from)
                        .collect::<Vec<_>>(),
                    prof.pool_policy,
                    prof.pool_stripe_mb,
                    parsed_gb,
                )
            };

            if members.is_empty() {
                append_log(&w, "Error: Add at least one pool folder member!");
                update_status(&w, "Pool mount aborted: no members.");
                return;
            }

            let quota_msg = if custom_gb > 0 {
                format!("Fixed Quota: {} GB", custom_gb)
            } else {
                "Auto (Calculated from physical drives)".to_string()
            };

            append_log(
                &w,
                &format!(
                    "Mounting Virtual Storage Pool as drive '{}' ({})...",
                    drive, quota_msg
                ),
            );
            update_status(&w, "Mounting Virtual Storage Pool...");

            let stripe_bytes = (stripe_mb as u64) * 1024 * 1024;
            match mount_storage_pool(&drive, &members, policy, stripe_bytes, custom_gb) {
                Ok(session) => {
                    let mut st = st_lock.lock().unwrap();
                    st.pool_session = Some(session);

                    let drive_clone = drive.clone();
                    let _ = w.upgrade_in_event_loop(move |ui| {
                        ui.set_is_pool_mounted(true);
                        ui.set_status_msg(
                            format!("Virtual Storage Pool Active at {}", drive_clone).into(),
                        );
                    });
                    append_log(
                        &w,
                        &format!(
                            "🚀 SUCCESS: Virtual Storage Pool mounted at '{}' ({})!",
                            drive, quota_msg
                        ),
                    );
                }
                Err(err) => {
                    append_log(&w, &format!("Pool Mount Error: {}", err));
                    update_status(&w, "Pool Mount Failed.");
                }
            }
        });
    });

    // Unmount Virtual Storage Pool (Z:)
    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_unmount_pool(move || {
        let mut st = state_arc.lock().unwrap();
        st.pool_session = None;

        let _ = weak.upgrade_in_event_loop(|ui| {
            ui.set_is_pool_mounted(false);
            ui.set_status_msg("Virtual Storage Pool unmounted.".into());
        });
        append_log(&weak, "Virtual Storage Pool unmounted successfully.");
    });

    // Profile Management Callbacks
    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_profile_selected(move |idx| {
        let mut st = state_arc.lock().unwrap();
        let idx = (idx as usize).min(st.config.profiles.len().saturating_sub(1));
        st.config.active_profile_index = idx;
        save_config(&st.config);

        if let Some(prof) = st.config.profiles.get(idx).cloned() {
            let w = weak.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_primary_path(prof.primary_path.clone().into());
                ui.set_min_size_mb_text(prof.min_size_mb.clone().into());
                ui.set_exclusions_text(prof.exclusions.clone().into());
                ui.set_media_extensions_text(prof.media_extensions.clone().into());
                ui.set_keep_backup(prof.keep_backup);
                ui.set_aggressive_media(prof.aggressive_media);
                ui.set_ue5_safe_mode(prof.ue5_safe_mode);
                ui.set_active_mode_idx(prof.mode_idx);

                let target_rows: Vec<String> = prof
                    .secondary_targets
                    .iter()
                    .enumerate()
                    .map(|(i, p)| format!("[Drive {}] {}", i + 2, p))
                    .collect();

                let items: Vec<StandardListViewItem> = target_rows
                    .into_iter()
                    .map(|s| StandardListViewItem::from(SharedString::from(s)))
                    .collect();
                ui.set_target_drives_list(ModelRc::new(VecModel::from(items)));

                let pool_rows: Vec<String> = prof
                    .pool_members
                    .iter()
                    .enumerate()
                    .map(|(i, p)| format!("[Member {}] {}", i + 1, p))
                    .collect();

                let pool_items: Vec<StandardListViewItem> = pool_rows
                    .into_iter()
                    .map(|s| StandardListViewItem::from(SharedString::from(s)))
                    .collect();
                ui.set_pool_members_list(ModelRc::new(VecModel::from(pool_items)));
                ui.set_pool_policy_idx(prof.pool_policy);
                ui.set_pool_stripe_mb(prof.pool_stripe_mb as i32);
                ui.set_pool_mount_point(prof.pool_mount_point.clone().into());
                ui.set_pool_custom_size_gb_text(prof.pool_custom_size_gb.clone().into());

                if !prof.primary_path.is_empty() {
                    inspect_game_folder(Path::new(&prof.primary_path), &w);
                }
            });
            append_log(&weak, &format!("Switched to profile: {}", prof.name));
        }
    });

    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_create_profile(move || {
        let mut st = state_arc.lock().unwrap();
        let new_count = st.config.profiles.len() + 1;
        let new_profile = Profile {
            name: format!("Profile {}", new_count),
            ..Profile::default()
        };
        st.config.profiles.push(new_profile);
        let new_idx = st.config.profiles.len() - 1;
        st.config.active_profile_index = new_idx;
        save_config(&st.config);

        let names: Vec<SharedString> = st
            .config
            .profiles
            .iter()
            .map(|p| p.name.clone().into())
            .collect();
        let prof = st.config.profiles[new_idx].clone();

        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_profile_names(ModelRc::new(VecModel::from(names)));
            ui.set_active_profile_idx(new_idx as i32);
            ui.set_primary_path("".into());
            ui.set_target_drives_list(ModelRc::new(VecModel::from(Vec::new())));
            ui.set_pool_members_list(ModelRc::new(VecModel::from(Vec::new())));
            ui.set_file_list(ModelRc::new(VecModel::from(Vec::new())));
            ui.set_is_already_striped(false);
            ui.set_active_mode_idx(0);
        });
        append_log(&weak, &format!("Created new profile: {}", prof.name));
    });

    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_delete_profile(move || {
        let mut st = state_arc.lock().unwrap();
        if st.config.profiles.len() <= 1 {
            append_log(&weak, "Cannot delete the only remaining profile.");
            return;
        }

        let curr_idx = st.config.active_profile_index;
        let deleted_name = st.config.profiles.remove(curr_idx).name;
        st.config.active_profile_index = st
            .config
            .active_profile_index
            .min(st.config.profiles.len() - 1);
        save_config(&st.config);

        let names: Vec<SharedString> = st
            .config
            .profiles
            .iter()
            .map(|p| p.name.clone().into())
            .collect();
        let new_active_idx = st.config.active_profile_index;
        let prof = st.config.profiles[new_active_idx].clone();

        let w = weak.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_profile_names(ModelRc::new(VecModel::from(names)));
            ui.set_active_profile_idx(new_active_idx as i32);
            ui.set_primary_path(prof.primary_path.clone().into());
            ui.set_min_size_mb_text(prof.min_size_mb.clone().into());
            ui.set_exclusions_text(prof.exclusions.clone().into());
            ui.set_media_extensions_text(prof.media_extensions.clone().into());
            ui.set_keep_backup(prof.keep_backup);
            ui.set_aggressive_media(prof.aggressive_media);
            ui.set_ue5_safe_mode(prof.ue5_safe_mode);
            ui.set_active_mode_idx(prof.mode_idx);

            let target_rows: Vec<String> = prof
                .secondary_targets
                .iter()
                .enumerate()
                .map(|(i, p)| format!("[Drive {}] {}", i + 2, p))
                .collect();

            let items: Vec<StandardListViewItem> = target_rows
                .into_iter()
                .map(|s| StandardListViewItem::from(SharedString::from(s)))
                .collect();
            ui.set_target_drives_list(ModelRc::new(VecModel::from(items)));

            let pool_rows: Vec<String> = prof
                .pool_members
                .iter()
                .enumerate()
                .map(|(i, p)| format!("[Member {}] {}", i + 1, p))
                .collect();

            let pool_items: Vec<StandardListViewItem> = pool_rows
                .into_iter()
                .map(|s| StandardListViewItem::from(SharedString::from(s)))
                .collect();
            ui.set_pool_members_list(ModelRc::new(VecModel::from(pool_items)));
            ui.set_pool_policy_idx(prof.pool_policy);
            ui.set_pool_stripe_mb(prof.pool_stripe_mb as i32);
            ui.set_pool_mount_point(prof.pool_mount_point.clone().into());
            ui.set_pool_custom_size_gb_text(prof.pool_custom_size_gb.clone().into());

            if !prof.primary_path.is_empty() {
                inspect_game_folder(Path::new(&prof.primary_path), &w);
            }
        });
        append_log(&weak, &format!("Deleted profile '{}'.", deleted_name));
    });

    // Hardware Benchmarking
    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_run_benchmark(move || {
        let w = weak.clone();
        let st_lock = state_arc.clone();

        thread::spawn(move || {
            let (src_dir, targets) = {
                let st = st_lock.lock().unwrap();
                let idx = st.config.active_profile_index;
                let prof = &st.config.profiles[idx];
                (
                    prof.primary_path.clone(),
                    prof.secondary_targets
                        .iter()
                        .map(PathBuf::from)
                        .collect::<Vec<_>>(),
                )
            };

            if src_dir.is_empty() || targets.is_empty() {
                append_log(
                    &w,
                    "Error: Set a valid Primary folder and at least 1 Target drive.",
                );
                return;
            }

            let total_drives = 1 + targets.len();
            append_log(
                &w,
                &format!(
                    "=== STARTING HARDWARE BENCHMARK ({} DRIVES) ===",
                    total_drives
                ),
            );
            update_status(&w, "Running benchmark (128 MB blocks)...");

            let _ = w.upgrade_in_event_loop(|ui| {
                ui.set_is_processing(true);
                ui.set_progress(0.2);
            });

            match run_benchmark_test(Path::new(&src_dir), &targets) {
                Ok(res) => {
                    append_log(&w, "--- BENCHMARK RESULTS ---");
                    append_log(
                        &w,
                        &format!(
                            "• Single-Drive Read (Drive 1):               {:.1} MB/s",
                            res.single_speed_mbs
                        ),
                    );
                    append_log(
                        &w,
                        &format!(
                            "• Multi-Drive Concurrent Read ({} Drives):   {:.1} MB/s",
                            res.drives_tested, res.parallel_speed_mbs
                        ),
                    );
                    append_log(
                        &w,
                        &format!(
                            "• Measured Hardware Boost:                   {:+.1}% 🚀",
                            res.boost_percentage
                        ),
                    );

                    let _ = w.upgrade_in_event_loop(move |ui| {
                        ui.set_progress(1.0);
                        ui.set_is_processing(false);
                        ui.set_status_msg(
                            format!(
                                "Single: {:.0} MB/s | {} Drives: {:.0} MB/s ({:+.0}%)",
                                res.single_speed_mbs,
                                res.drives_tested,
                                res.parallel_speed_mbs,
                                res.boost_percentage
                            )
                            .into(),
                        );
                    });
                }
                Err(err) => {
                    append_log(&w, &format!("Benchmark error: {}", err));
                    let _ = w.upgrade_in_event_loop(|ui| {
                        ui.set_is_processing(false);
                    });
                }
            }
        });
    });

    // Hardware S.M.A.R.T. Disks Query
    let weak = ui_weak.clone();
    app.on_scan_drives(move || {
        let w = weak.clone();
        thread::spawn(move || {
            append_log(&w, "--- QUERYING S.M.A.R.T. PHYSICAL DISK HEALTH ---");
            update_status(&w, "Querying hardware S.M.A.R.T. status...");

            let smart_disks = query_smart_disks();
            let mut warning_found = false;

            for disk in smart_disks {
                let symbol = if disk.is_healthy {
                    "[OK]"
                } else {
                    warning_found = true;
                    "[ALERT]"
                };
                append_log(
                    &w,
                    &format!(
                        "{} [{}] {} - Status: {}",
                        symbol, disk.media_type, disk.name, disk.health_status
                    ),
                );
            }

            if warning_found {
                append_log(
                    &w,
                    "⚠️ WARNING: One or more physical disks reported non-healthy status!",
                );
                let _ = w.upgrade_in_event_loop(|ui| {
                    ui.set_smart_alert_text(
                        "⚠️ S.M.A.R.T. Warning detected! Keep Safe Mode (.backup) enabled.".into(),
                    );
                });
            } else {
                append_log(&w, "All connected storage drives reported HEALTHY status.");
                let _ = w.upgrade_in_event_loop(|ui| {
                    ui.set_smart_alert_text("".into());
                });
            }

            append_log(&w, "--- LOGICAL STORAGE VOLUMES ---");
            for (root, free_gb, total_gb) in get_logical_volumes() {
                append_log(
                    &w,
                    &format!(
                        "Volume [ {} ]: Free {:.1} GB / Total {:.1} GB",
                        root, free_gb, total_gb
                    ),
                );
            }
            update_status(&w, "Disk and S.M.A.R.T. scan complete.");
        });
    });

    app.run()?;
    Ok(())
}
