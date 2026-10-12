use crate::manifest::{
    ManifestFileEntry, StripingManifest, delete_manifest, load_manifest, save_manifest,
};
use crate::win32::{copy_file_with_progress, get_free_disk_space_bytes, test_symlink_privilege};
use std::fs;
use std::os::windows::fs::symlink_file;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const COMPANION_EXTENSIONS: &[&str] = &["utoc", "pak", "sig"];
// Keep a safety reserve on destination target disks (512 MB)
const DISK_SAFETY_RESERVE_BYTES: u64 = 512 * 1024 * 1024;

pub fn is_ext_match(path: &Path, exts: &[String]) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_lowercase();
        return exts.iter().any(|e| e == &ext_lower);
    }
    false
}

pub fn parse_ext_list(ext_str: &str) -> Vec<String> {
    ext_str
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn is_iostore_metadata_companion(path: &Path) -> bool {
    if let Some(file_name) = path.file_name().and_then(|f| f.to_str()) {
        let name_lower = file_name.to_lowercase();
        if name_lower.starts_with("pakchunk0-") || name_lower.starts_with("global.") {
            return true;
        }
    }

    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_lowercase();
        if matches!(ext_lower.as_str(), "utoc" | "sig" | "toc" | "idx") {
            return true;
        }
        if ext_lower == "pak" {
            let ucas_companion = path.with_extension("ucas");
            let utoc_companion = path.with_extension("utoc");
            if ucas_companion.exists() || utoc_companion.exists() {
                return true;
            }
        }
    }
    false
}

/// Safely traverses up and removes empty parent directories, bounded strictly by stop_at_root.
fn clean_empty_parents(file_path: &Path, stop_at_root: &Path) {
    let mut current = file_path.to_path_buf();
    while let Some(parent) = current.parent() {
        if parent == stop_at_root || !parent.starts_with(stop_at_root) {
            break;
        }
        if fs::remove_dir(parent).is_ok() {
            current = parent.to_path_buf();
        } else {
            break;
        }
    }
}

pub fn auto_detect_threshold(src_path: &Path, exclusions: &[String]) -> Option<u64> {
    let mut files = Vec::new();
    let mut total_size: u64 = 0;

    for entry in WalkDir::new(src_path).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if is_ext_match(p, exclusions) || is_iostore_metadata_companion(p) {
            continue;
        }
        if let Some(meta) = fs::symlink_metadata(p).ok().filter(|m| m.is_file()) {
            let size = meta.len();
            files.push(size);
            total_size += size;
        }
    }

    if files.is_empty() {
        return None;
    }

    files.sort_by_key(|item| std::cmp::Reverse(*item));

    let target_size = (total_size as f64 * 0.8) as u64;
    let mut accumulated = 0;
    let mut threshold = files[0];

    for size in files {
        accumulated += size;
        threshold = size;
        if accumulated >= target_size {
            break;
        }
    }

    Some((threshold / 1_048_576).max(1))
}

pub fn verify_and_repair_integrity(src_path: &Path) -> Result<(usize, usize), String> {
    let mut manifest = load_manifest(src_path).ok_or("No active manifest found.")?;
    let mut intact = 0;
    let mut repaired = 0;
    let mut valid_entries = Vec::new();

    for entry in manifest.files {
        let link_path = src_path.join(&entry.rel_path);
        let target_path = PathBuf::from(&entry.target_file_path);

        if target_path.exists() {
            intact += 1;
            valid_entries.push(entry);
        } else {
            let _ = fs::remove_file(&link_path);
            let backup_path = PathBuf::from(format!("{}.backup", link_path.to_string_lossy()));

            if entry.has_local_backup
                && backup_path.exists()
                && fs::rename(&backup_path, &link_path).is_ok()
            {
                repaired += 1;
            }
            if let Some(target_parent) = target_path.parent() {
                clean_empty_parents(&target_path, target_parent);
            }
        }
    }

    if valid_entries.is_empty() {
        delete_manifest(src_path);
    } else {
        manifest.files = valid_entries;
        save_manifest(src_path, &manifest);
    }

    Ok((intact, repaired))
}

pub struct SimulationPlan {
    pub files: Vec<(PathBuf, u64, bool)>,
    pub drive_bytes: Vec<u64>,
    pub drive_buckets: Vec<Vec<(PathBuf, u64, bool)>>,
    pub total_drives: usize,
}

/// Calculates balanced multi-drive distribution strictly constrained by physical available space per spindle.
pub fn calculate_allocation_plan(
    src_path: &Path,
    targets: &[PathBuf],
    min_size_bytes: u64,
    exclusions: &[String],
    media_exts: &[String],
    aggressive_media: bool,
) -> Option<SimulationPlan> {
    let mut files = Vec::new();

    for entry in WalkDir::new(src_path).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if is_ext_match(p, exclusions) || is_iostore_metadata_companion(p) {
            continue;
        }

        if let Some(meta) = fs::symlink_metadata(p).ok().filter(|m| m.is_file()) {
            let size = meta.len();
            let is_media = aggressive_media && is_ext_match(p, media_exts);

            if is_media || size >= min_size_bytes {
                files.push((p.to_path_buf(), size, is_media));
            }
        }
    }

    if files.is_empty() {
        return None;
    }

    files.sort_by_key(|item| std::cmp::Reverse(item.1));

    let total_drives = 1 + targets.len();
    let mut drive_bytes: Vec<u64> = vec![0; total_drives];
    let mut drive_buckets: Vec<Vec<(PathBuf, u64, bool)>> = vec![Vec::new(); total_drives];

    // Query real physical free bytes for each spindle
    let mut initial_free_space: Vec<u64> = Vec::with_capacity(total_drives);
    initial_free_space.push(get_free_disk_space_bytes(src_path).unwrap_or(u64::MAX / 2));
    for t in targets {
        initial_free_space.push(get_free_disk_space_bytes(t).unwrap_or(0));
    }

    for (f_path, size, is_media) in files.clone() {
        // Target drives must have enough remaining space + safety threshold
        let target_has_space = |idx: usize| -> bool {
            let planned_alloc = drive_bytes[idx];
            let free = initial_free_space[idx];
            free.saturating_sub(planned_alloc) >= size + DISK_SAFETY_RESERVE_BYTES
        };

        let best_drive = if is_media && total_drives > 1 {
            // Aggressive media routing: pick destination drive among targets with enough room,
            // prioritizing the drive with greatest remaining capacity to balance spindles
            let eligible_targets: Vec<usize> = (1..total_drives)
                .filter(|&idx| target_has_space(idx))
                .collect();

            if !eligible_targets.is_empty() {
                eligible_targets
                    .into_iter()
                    .max_by_key(|&idx| initial_free_space[idx].saturating_sub(drive_bytes[idx]))
                    .unwrap_or(1)
            } else {
                0 // Fallback to Primary local drive if all targets lack capacity
            }
        } else {
            // Standard files: consider all candidate drives that can host the block
            let mut eligible_drives = Vec::new();
            // Drive 0 (Primary) already holds the file locally
            eligible_drives.push(0);

            for idx in 1..total_drives {
                if target_has_space(idx) {
                    eligible_drives.push(idx);
                }
            }

            // Among eligible drives, pick the one with least currently allocated bytes (round-robin / striping)
            eligible_drives
                .into_iter()
                .min_by_key(|&idx| drive_bytes[idx])
                .unwrap_or(0)
        };

        drive_bytes[best_drive] += size;
        drive_buckets[best_drive].push((f_path, size, is_media));
    }

    Some(SimulationPlan {
        files,
        drive_bytes,
        drive_buckets,
        total_drives,
    })
}

pub fn execute_striping(
    src_path: &Path,
    targets: &[PathBuf],
    plan: SimulationPlan,
    keep_backup: bool,
    ue5_safe_mode: bool,
    mut on_progress: impl FnMut(usize, usize, &str, f32),
) -> Result<usize, String> {
    // Verify unprivileged file symlink creation privilege
    if let Err((os_err, dev_mode)) = test_symlink_privilege() {
        let dev_status = if dev_mode {
            "Developer Mode: ENABLED (Policy requires application restart or administrative elevation)"
        } else {
            "Developer Mode: DISABLED in Windows Settings"
        };

        return Err(format!(
            "🛑 ABORTED BEFORE TRANSFER: Cannot create file symlinks (Win32 Error {}).\n\
            {}\n\n\
            HOW TO FIX:\n\
            1. Enable Developer Mode: Press Win+I -> System -> For developers -> Turn 'Developer Mode' ON.\n\
            2. OR Run SymStripe as Administrator.",
            os_err, dev_status
        ));
    }

    for (idx, target) in targets.iter().enumerate() {
        let required = plan.drive_bytes[idx + 1];
        if let Some(free) = get_free_disk_space_bytes(target)
            .filter(|&free| free < required + DISK_SAFETY_RESERVE_BYTES)
        {
            return Err(format!(
                "ABORTED: Insufficient space on Drive {} ({})! Required: {:.2} GB, Available: {:.2} GB",
                idx + 2,
                target.display(),
                (required + DISK_SAFETY_RESERVE_BYTES) as f64 / 1e9,
                free as f64 / 1e9
            ));
        }
    }

    let files_to_move: usize = plan.drive_buckets.iter().skip(1).map(|b| b.len()).sum();
    let total_bytes_to_move: u64 = plan
        .drive_buckets
        .iter()
        .skip(1)
        .flat_map(|b| b.iter().map(|f| f.1))
        .sum();

    let mut manifest = load_manifest(src_path).unwrap_or_else(|| StripingManifest {
        game_path: src_path.to_string_lossy().to_string(),
        total_drives: plan.total_drives,
        files: Vec::new(),
    });

    let mut moved_count = 0;
    let mut bytes_moved_so_far: u64 = 0;

    for (target_idx, bucket) in plan.drive_buckets.into_iter().enumerate().skip(1) {
        let target_root = &targets[target_idx - 1];

        for (f_path, size, _) in bucket {
            let rel = match f_path.strip_prefix(src_path) {
                Ok(r) => r,
                Err(_) => continue,
            };

            let filename = rel
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            if filename.to_lowercase().starts_with("pakchunk0-")
                || filename.to_lowercase().starts_with("global.")
            {
                continue;
            }

            let target_file = target_root.join(rel);
            let target_parent = target_file.parent().unwrap();
            let _ = fs::create_dir_all(target_parent);

            let current_file_idx = moved_count + 1;

            // Step 1: Smooth CopyFileExW with byte-level progress reporting to prevent UI freezes
            let target_tmp = target_file.with_extension("symstripe_tmp");
            let file_copied_bytes = std::sync::atomic::AtomicU64::new(0);

            let copy_res = copy_file_with_progress(&f_path, &target_tmp, |transferred, _total| {
                file_copied_bytes.store(transferred, std::sync::atomic::Ordering::Relaxed);
                let current_total = bytes_moved_so_far + transferred;
                let global_pct = if total_bytes_to_move > 0 {
                    (current_total as f64 / total_bytes_to_move as f64).clamp(0.0, 1.0) as f32
                } else {
                    1.0
                };
                on_progress(current_file_idx, files_to_move, &filename, global_pct);
                true
            });

            if let Err(e) = copy_res {
                let _ = fs::remove_file(&target_tmp);
                return Err(format!(
                    "Failed to copy file '{}' to target drive: {e}",
                    f_path.display()
                ));
            }

            if let Err(e) = fs::rename(&target_tmp, &target_file) {
                let _ = fs::remove_file(&target_tmp);
                return Err(format!(
                    "Failed to finalize target file '{}': {e}",
                    target_file.display()
                ));
            }

            // Step 2: In UE5 Safe Mode, co-locate IoStore companion files (.utoc, .pak, .sig) atomically to the same destination
            let is_ucas = f_path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ucas"));

            let mut migrated_companions = Vec::new();

            if ue5_safe_mode && is_ucas {
                for ext in COMPANION_EXTENSIONS {
                    let comp_src = f_path.with_extension(ext);
                    let comp_dst = target_file.with_extension(ext);
                    let comp_tmp = comp_dst.with_extension("symstripe_tmp");

                    if comp_src.exists()
                        && copy_file_with_progress(&comp_src, &comp_tmp, |_, _| true).is_ok()
                    {
                        if fs::rename(&comp_tmp, &comp_dst).is_ok() {
                            let comp_stage = PathBuf::from(format!(
                                "{}.symstripe_stage",
                                comp_src.to_string_lossy()
                            ));
                            let comp_backup =
                                PathBuf::from(format!("{}.backup", comp_src.to_string_lossy()));

                            let comp_dest = if keep_backup {
                                &comp_backup
                            } else {
                                &comp_stage
                            };

                            if fs::rename(&comp_src, comp_dest).is_ok() {
                                if symlink_file(&comp_dst, &comp_src).is_ok() {
                                    if !keep_backup {
                                        let _ = fs::remove_file(&comp_stage);
                                    }
                                    migrated_companions.push((
                                        comp_src,
                                        comp_dst,
                                        comp_dest.clone(),
                                    ));
                                } else {
                                    let _ = fs::rename(comp_dest, &comp_src);
                                    let _ = fs::remove_file(&comp_dst);
                                }
                            }
                        } else {
                            let _ = fs::remove_file(&comp_tmp);
                        }
                    }
                }
            }

            // Step 3: Rename original archive to stage or backup
            let stage_path = PathBuf::from(format!("{}.symstripe_stage", f_path.to_string_lossy()));
            let backup_path = PathBuf::from(format!("{}.backup", f_path.to_string_lossy()));

            let stage_destination = if keep_backup {
                &backup_path
            } else {
                &stage_path
            };

            if let Err(e) = fs::rename(&f_path, stage_destination) {
                let _ = fs::remove_file(&target_file);
                clean_empty_parents(&target_file, target_root);

                // Roll back companions
                for (c_src, c_dst, c_dest) in migrated_companions {
                    let _ = fs::remove_file(&c_src);
                    let _ = fs::rename(c_dest, &c_src);
                    let _ = fs::remove_file(c_dst);
                }

                return Err(format!(
                    "Failed to rename original file '{}': {e}",
                    f_path.display()
                ));
            }

            // Step 4: Transparent in-place file symlink creation directly where the engine expects it
            if let Err(sym_err) = symlink_file(&target_file, &f_path) {
                let _ = fs::rename(stage_destination, &f_path);
                let _ = fs::remove_file(&target_file);
                clean_empty_parents(&target_file, target_root);

                for (c_src, c_dst, c_dest) in migrated_companions {
                    let _ = fs::remove_file(&c_src);
                    let _ = fs::rename(c_dest, &c_src);
                    let _ = fs::remove_file(c_dst);
                }

                return Err(format!(
                    "Symlink creation failed for '{}': {}",
                    f_path.display(),
                    sym_err
                ));
            }

            if !keep_backup {
                let _ = fs::remove_file(&stage_path);
            }

            manifest.files.push(ManifestFileEntry {
                rel_path: rel.to_string_lossy().to_string(),
                target_drive_idx: target_idx,
                target_file_path: target_file.to_string_lossy().to_string(),
                size_bytes: size,
                has_local_backup: keep_backup,
            });
            save_manifest(src_path, &manifest);

            bytes_moved_so_far += size;
            moved_count += 1;

            let final_pct = if total_bytes_to_move > 0 {
                (bytes_moved_so_far as f64 / total_bytes_to_move as f64).clamp(0.0, 1.0) as f32
            } else {
                1.0
            };
            on_progress(moved_count, files_to_move, &filename, final_pct);
        }
    }

    Ok(moved_count)
}

pub fn execute_revert(src_path: &Path, mut on_progress: impl FnMut(usize, usize, f32)) -> usize {
    let mut reverted = 0;

    if let Some(mut manifest) = load_manifest(src_path) {
        let total = manifest.files.len();

        while !manifest.files.is_empty() {
            let entry = manifest.files.remove(0);
            let link_path = src_path.join(&entry.rel_path);
            let target_path = PathBuf::from(&entry.target_file_path);
            let backup_path = PathBuf::from(format!("{}.backup", link_path.to_string_lossy()));

            let mut restored = false;

            if entry.has_local_backup && backup_path.exists() {
                let _ = fs::remove_file(&link_path);
                if fs::rename(&backup_path, &link_path).is_ok() {
                    let _ = fs::remove_file(&target_path);
                    if let Some(target_parent) = target_path.parent() {
                        clean_empty_parents(&target_path, target_parent);
                    }
                    restored = true;
                }
            } else if target_path.exists() {
                let _ = fs::remove_file(&link_path);
                if copy_file_with_progress(&target_path, &link_path, |_, _| true).is_ok() {
                    let _ = fs::remove_file(&target_path);
                    if let Some(target_parent) = target_path.parent() {
                        clean_empty_parents(&target_path, target_parent);
                    }
                    restored = true;
                }
            }

            if link_path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ucas"))
            {
                for ext in COMPANION_EXTENSIONS {
                    let comp_primary = link_path.with_extension(ext);
                    let comp_target = target_path.with_extension(ext);
                    let comp_backup =
                        PathBuf::from(format!("{}.backup", comp_primary.to_string_lossy()));

                    if comp_backup.exists() {
                        let _ = fs::remove_file(&comp_primary);
                        if fs::rename(&comp_backup, &comp_primary).is_ok() {
                            let _ = fs::remove_file(&comp_target);
                        }
                    } else if comp_target.exists() {
                        let _ = fs::remove_file(&comp_primary);
                        if copy_file_with_progress(&comp_target, &comp_primary, |_, _| true).is_ok()
                        {
                            let _ = fs::remove_file(&comp_target);
                        }
                    }
                }
            }

            if restored {
                reverted += 1;
            }

            if manifest.files.is_empty() {
                delete_manifest(src_path);
            } else {
                save_manifest(src_path, &manifest);
            }

            let current_idx = reverted;
            let pct = if total > 0 {
                current_idx as f32 / total as f32
            } else {
                1.0
            };
            on_progress(current_idx, total, pct);
        }
    }

    // Clean up any legacy junction subfolders created by previous versions
    for entry in WalkDir::new(src_path).into_iter().filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy();
        if name.contains("_Drive") {
            let _ = fs::remove_dir(entry.path());
        }
    }

    reverted
}

pub fn execute_consolidate(src_path: &Path, chosen_target_dir: &Path) -> Result<usize, String> {
    let mut manifest =
        load_manifest(src_path).ok_or("No active manifest found for consolidation.")?;
    let chosen_str = chosen_target_dir.to_string_lossy().to_string();

    let mut remaining_entries = Vec::new();
    let mut consolidated_count = 0;

    for entry in manifest.files {
        if entry.target_file_path.starts_with(&chosen_str) {
            let link_path = src_path.join(&entry.rel_path);
            let target_path = PathBuf::from(&entry.target_file_path);
            let backup_path = PathBuf::from(format!("{}.backup", link_path.to_string_lossy()));

            if entry.has_local_backup && backup_path.exists() {
                let _ = fs::remove_file(&link_path);
                if fs::rename(&backup_path, &link_path).is_ok() {
                    let _ = fs::remove_file(&target_path);
                    clean_empty_parents(&target_path, chosen_target_dir);
                    consolidated_count += 1;
                }
            } else if target_path.exists() {
                let _ = fs::remove_file(&link_path);
                if copy_file_with_progress(&target_path, &link_path, |_, _| true).is_ok() {
                    let _ = fs::remove_file(&target_path);
                    clean_empty_parents(&target_path, chosen_target_dir);
                    consolidated_count += 1;
                }
            }

            if link_path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ucas"))
            {
                for ext in COMPANION_EXTENSIONS {
                    let comp_primary = link_path.with_extension(ext);
                    let comp_target = target_path.with_extension(ext);
                    let comp_backup =
                        PathBuf::from(format!("{}.backup", comp_primary.to_string_lossy()));

                    if comp_backup.exists() {
                        let _ = fs::remove_file(&comp_primary);
                        if fs::rename(&comp_backup, &comp_primary).is_ok() {
                            let _ = fs::remove_file(&comp_target);
                        }
                    } else if comp_target.exists() {
                        let _ = fs::remove_file(&comp_primary);
                        if copy_file_with_progress(&comp_target, &comp_primary, |_, _| true).is_ok()
                        {
                            let _ = fs::remove_file(&comp_target);
                        }
                    }
                }
            }
        } else {
            remaining_entries.push(entry);
        }
    }

    if remaining_entries.is_empty() {
        delete_manifest(src_path);
    } else {
        manifest.files = remaining_entries;
        save_manifest(src_path, &manifest);
    }

    let _ = fs::remove_dir(chosen_target_dir);
    Ok(consolidated_count)
}
