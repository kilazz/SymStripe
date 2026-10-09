mod benchmark;
mod config;
mod manifest;
mod striping;
mod win32;

use benchmark::run_benchmark_test;
use config::{AppConfig, Profile, load_config, save_config};
use manifest::load_manifest;
use striping::{
    auto_detect_threshold, calculate_allocation_plan, execute_consolidate, execute_revert,
    execute_striping, parse_ext_list, verify_and_repair_integrity,
};
use win32::{get_logical_volumes, query_smart_disks};

use rfd::FileDialog;
use slint::{ModelRc, SharedString, StandardListViewItem, VecModel};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

slint::include_modules!();

struct AppState {
    config: AppConfig,
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
    }));

    // Mode Switcher Callback
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
            "NTFS File Symlinks (Universal)"
        } else {
            "UE5 Subfolder Junctions (DirectStorage Safe)"
        };
        append_log(
            &weak,
            &format!("Engine architecture switched to: {}", mode_name),
        );
    });

    // Auto-Detect Optimal Threshold
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

    // Verify Storage Integrity
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

    // Profile Selection
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

                if !prof.primary_path.is_empty() {
                    inspect_game_folder(Path::new(&prof.primary_path), &w);
                }
            });
            append_log(&weak, &format!("Switched to profile: {}", prof.name));
        }
    });

    // Create Profile
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
            ui.set_file_list(ModelRc::new(VecModel::from(Vec::new())));
            ui.set_is_already_striped(false);
            ui.set_active_mode_idx(0);
        });
        append_log(&weak, &format!("Created new profile: {}", prof.name));
    });

    // Delete Profile
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

            if !prof.primary_path.is_empty() {
                inspect_game_folder(Path::new(&prof.primary_path), &w);
            }
        });
        append_log(&weak, &format!("Deleted profile '{}'.", deleted_name));
    });

    // Primary Folder Picker
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

    // Add Secondary Target Drive
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
                append_log(&weak, "Target drive added to profile.");
            }
        }
    });

    // Clear Target Drives
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
        append_log(&weak, "All secondary target drives cleared.");
    });

    // Consolidate Target Drive
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
                "Select the secondary folder you want to consolidate & free...",
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
                    append_log(
                        &w,
                        &format!(
                            "Successfully consolidated {} files. Empty folders removed.",
                            count
                        ),
                    );
                }
                Err(err) => {
                    append_log(&w, &format!("Error: {}", err));
                    update_status(&w, "Consolidation failed.");
                }
            }
        });
    });

    // Preview Striping (Dry Run)
    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_preview_striping(
        move |src_dir,
              min_mb,
              exclusions_str,
              media_exts_str,
              keep_backup,
              aggressive_media,
              mode_idx| {
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
                let mode_str = if mode_idx == 0 {
                    "File Symlinks"
                } else {
                    "Subfolder Junctions"
                };
                append_log(
                    &w,
                    &format!("--- PREVIEW ANALYSIS (Engine: {}) ---", mode_str),
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
                            let tag = if mode_idx == 0 {
                                "[to Symlink]"
                            } else {
                                "[to UE5 Junction]"
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

    // Apply Striping Relocation
    let weak = ui_weak.clone();
    let state_arc = state.clone();
    app.on_apply_striping(
        move |src_dir,
              min_mb,
              exclusions_str,
              media_exts_str,
              keep_backup,
              aggressive_media,
              mode_idx| {
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
                    mode_idx,
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

    // Revert Striping Back to Primary
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

    // Speed Benchmark Test
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

    // Scan S.M.A.R.T. Disks
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
