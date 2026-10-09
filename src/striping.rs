use crate::manifest::{
    ManifestFileEntry, StripingManifest, delete_manifest, load_manifest, save_manifest,
};
use crate::win32::get_free_disk_space_bytes;
use std::fs;
use std::os::windows::fs::symlink_file;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Checks if a file's extension matches any extension in the provided list.
pub fn is_ext_match(path: &Path, exts: &[String]) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_lowercase();
        return exts.iter().any(|e| e == &ext_lower);
    }
    false
}

/// Parses a comma-delimited string of extensions into a normalized vector.
pub fn parse_ext_list(ext_str: &str) -> Vec<String> {
    ext_str
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Detects memory-mapped engine metadata, TOCs, and IoStore header stubs.
/// These files MUST remain physically on the primary drive to avoid `IoDispatcher` crashes in UE4/5.
pub fn is_iostore_metadata_companion(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_lowercase();

        // 1. Never relocate table of contents, signatures, or index files
        if matches!(ext_lower.as_str(), "utoc" | "sig" | "toc" | "idx") {
            return true;
        }

        // 2. If it is a .pak file, check if it is a companion stub to an IoStore container (.ucas/.utoc)
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

/// Traverses up the directory hierarchy and deletes empty parent directories.
fn clean_empty_parents(file_path: &Path) {
    let mut current = file_path.to_path_buf();
    while let Some(parent) = current.parent() {
        if fs::remove_dir(parent).is_ok() {
            current = parent.to_path_buf();
        } else {
            break;
        }
    }
}

/// Pre-flight capability probe: verifies that the process can create NTFS symbolic links in the source directory.
pub fn verify_symlink_privilege(test_dir: &Path) -> Result<(), String> {
    let test_src = test_dir.join(".symstripe_perm_test.tmp");
    let test_link = test_dir.join(".symstripe_perm_link.tmp");

    let _ = fs::remove_file(&test_src);
    let _ = fs::remove_file(&test_link);

    if let Err(e) = fs::write(&test_src, b"permission_probe") {
        return Err(format!(
            "Access denied writing to directory '{}': {}",
            test_dir.display(),
            e
        ));
    }

    let link_result = symlink_file(&test_src, &test_link);

    let _ = fs::remove_file(&test_src);
    let _ = fs::remove_file(&test_link);

    match link_result {
        Ok(_) => Ok(()),
        Err(e) => Err(format!(
            "Missing privilege to create symbolic links ({e}).\n\
            Windows requires Administrator privileges OR Developer Mode enabled.\n\
            Enable Developer Mode in Windows Settings (System -> For developers) or run SymStripe as Administrator."
        )),
    }
}

/// Automatically computes an optimal threshold size (in MB) covering ~80% of eligible data.
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

    let target_size = (total_size as f64 * 0.8) as u64; // ~80% target volume coverage
    let mut accumulated = 0;
    let mut threshold = files[0];

    for size in files {
        accumulated += size;
        threshold = size;
        if accumulated >= target_size {
            break;
        }
    }

    let mb = (threshold / 1_048_576).max(1);
    Some(mb)
}

/// Verifies all manifest symlinks against physical secondary targets, auto-restoring missing links from .backup.
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
            clean_empty_parents(&target_path);
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
    pub files: Vec<(PathBuf, u64, bool)>, // (Path, Size, Is_Media)
    pub drive_bytes: Vec<u64>,
    pub drive_buckets: Vec<Vec<(PathBuf, u64, bool)>>,
    pub total_drives: usize,
}

/// Calculates an optimal multi-drive distribution plan using LPT bin-packing.
/// Automatically retains memory-mapped metadata (.utoc/.pak stubs) on the primary drive.
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

    for (f_path, size, is_media) in files.clone() {
        let best_drive = if is_media && total_drives > 1 {
            (1..total_drives)
                .min_by_key(|&idx| drive_bytes[idx])
                .unwrap_or(1)
        } else {
            (0..total_drives)
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

/// Executes striping relocation with pre-flight permission checks, staged rollback, and incremental manifest saves.
pub fn execute_striping(
    src_path: &Path,
    targets: &[PathBuf],
    plan: SimulationPlan,
    keep_backup: bool,
    mut on_progress: impl FnMut(usize, usize, &str, f32),
) -> Result<usize, String> {
    verify_symlink_privilege(src_path)?;

    for (idx, target) in targets.iter().enumerate() {
        let required = plan.drive_bytes[idx + 1];
        if let Some(free) = get_free_disk_space_bytes(target).filter(|&free| free < required) {
            return Err(format!(
                "ABORTED: Insufficient space on Drive {} ({})! Required: {:.2} GB, Available: {:.2} GB",
                idx + 2,
                target.display(),
                required as f64 / 1e9,
                free as f64 / 1e9
            ));
        }
    }

    let files_to_move: usize = plan.drive_buckets.iter().skip(1).map(|b| b.len()).sum();

    let mut manifest = load_manifest(src_path).unwrap_or_else(|| StripingManifest {
        game_path: src_path.to_string_lossy().to_string(),
        total_drives: plan.total_drives,
        files: Vec::new(),
    });

    let mut moved_count = 0;

    for (target_idx, bucket) in plan.drive_buckets.into_iter().enumerate().skip(1) {
        let target_root = &targets[target_idx - 1];

        for (f_path, size, _) in bucket {
            let rel = match f_path.strip_prefix(src_path) {
                Ok(r) => r,
                Err(_) => continue,
            };

            let target_file = target_root.join(rel);
            if let Some(parent) = target_file.parent() {
                let _ = fs::create_dir_all(parent);
            }

            let filename = rel
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            let current_idx = moved_count + 1;
            let pct = if files_to_move > 0 {
                current_idx as f32 / files_to_move as f32
            } else {
                1.0
            };
            on_progress(current_idx, files_to_move, &filename, pct);

            // Step 1: Copy data to target drive
            if let Err(e) = fs::copy(&f_path, &target_file) {
                return Err(format!(
                    "Failed to copy file '{}' to target drive: {e}",
                    f_path.display()
                ));
            }

            // Step 2: Staged rename of original source file
            let stage_path = PathBuf::from(format!("{}.symstripe_stage", f_path.to_string_lossy()));
            let backup_path = PathBuf::from(format!("{}.backup", f_path.to_string_lossy()));

            let stage_destination = if keep_backup {
                &backup_path
            } else {
                &stage_path
            };

            if let Err(e) = fs::rename(&f_path, stage_destination) {
                let _ = fs::remove_file(&target_file);
                clean_empty_parents(&target_file);
                return Err(format!(
                    "Failed to rename original file '{}': {e}",
                    f_path.display()
                ));
            }

            // Step 3: Create NTFS symlink
            if let Err(sym_err) = symlink_file(&target_file, &f_path) {
                let _ = fs::rename(stage_destination, &f_path);
                let _ = fs::remove_file(&target_file);
                clean_empty_parents(&target_file);

                return Err(format!(
                    "Symlink creation failed for '{}': {sym_err}. Source file was safely restored.",
                    f_path.display()
                ));
            }

            // Step 4: Clean stage file
            if !keep_backup {
                let _ = fs::remove_file(&stage_path);
            }

            // Step 5: Save manifest incrementally
            manifest.files.push(ManifestFileEntry {
                rel_path: rel.to_string_lossy().to_string(),
                target_drive_idx: target_idx,
                target_file_path: target_file.to_string_lossy().to_string(),
                size_bytes: size,
                has_local_backup: keep_backup,
            });
            save_manifest(src_path, &manifest);

            moved_count += 1;
        }
    }

    Ok(moved_count)
}

/// Reverts all striped files back to the primary volume incrementally.
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
                    clean_empty_parents(&target_path);
                    restored = true;
                }
            } else if target_path.exists() {
                let _ = fs::remove_file(&link_path);
                if fs::copy(&target_path, &link_path).is_ok() {
                    let _ = fs::remove_file(&target_path);
                    clean_empty_parents(&target_path);
                    restored = true;
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
    } else {
        let mut all_files = Vec::new();
        for entry in WalkDir::new(src_path).into_iter().filter_map(|e| e.ok()) {
            all_files.push(entry.path().to_path_buf());
        }

        let total = all_files.len();
        for (idx, p) in all_files.into_iter().enumerate() {
            let backup_p = PathBuf::from(format!("{}.backup", p.to_string_lossy()));

            if backup_p.exists() {
                let _ = fs::remove_file(&p);
                if fs::rename(&backup_p, &p).is_ok() {
                    reverted += 1;
                }
            } else if let Some(target) = fs::symlink_metadata(&p)
                .ok()
                .filter(|m| m.file_type().is_symlink())
                .and_then(|_| fs::read_link(&p).ok())
                .filter(|t| t.exists())
            {
                let _ = fs::remove_file(&p);
                if fs::copy(&target, &p).is_ok() {
                    let _ = fs::remove_file(&target);
                    clean_empty_parents(&target);
                    reverted += 1;
                }
            }

            let pct = if total > 0 {
                (idx + 1) as f32 / total as f32
            } else {
                1.0
            };
            on_progress(idx + 1, total, pct);
        }
    }

    reverted
}

/// Safely consolidates files from a specific secondary folder back into the primary volume.
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
                    clean_empty_parents(&target_path);
                    consolidated_count += 1;
                }
            } else if target_path.exists() {
                let _ = fs::remove_file(&link_path);
                if fs::copy(&target_path, &link_path).is_ok() {
                    let _ = fs::remove_file(&target_path);
                    clean_empty_parents(&target_path);
                    consolidated_count += 1;
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
