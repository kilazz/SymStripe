use crate::manifest::{
    ManifestFileEntry, StripingManifest, delete_manifest, load_manifest, save_manifest,
};
use crate::win32::get_free_disk_space_bytes;
use std::fs;
use std::os::windows::fs::symlink_file;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub fn is_excluded(path: &Path, exclusions: &[String]) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_lowercase();
        return exclusions.iter().any(|ex| ex == &ext_lower);
    }
    false
}

pub fn is_media_file(path: &Path) -> bool {
    const MEDIA_EXTS: &[&str] = &[
        "bik", "bk2", "mp4", "avi", "mkv", "wmv", "fsb", "pck", "bnk", "wav", "ogg", "wem",
    ];
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_lowercase();
        return MEDIA_EXTS.contains(&ext_lower.as_str());
    }
    false
}

pub fn parse_exclusions(exclusions_str: &str) -> Vec<String> {
    exclusions_str
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Recursively removes empty parent directories up the tree
fn clean_empty_parents(file_path: &Path) {
    let mut current = file_path.to_path_buf();
    while let Some(parent) = current.parent() {
        // fs::remove_dir only succeeds if the directory is completely empty
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
        if is_excluded(p, exclusions) {
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

    let target_size = (total_size as f64 * 0.8) as u64; // 80% coverage
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

pub fn calculate_allocation_plan(
    src_path: &Path,
    targets: &[PathBuf],
    min_size_bytes: u64,
    exclusions: &[String],
    aggressive_media: bool,
) -> Option<SimulationPlan> {
    let mut files = Vec::new();

    for entry in WalkDir::new(src_path).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if is_excluded(p, exclusions) {
            continue;
        }
        if let Some(meta) = fs::symlink_metadata(p).ok().filter(|m| m.is_file()) {
            let is_media = aggressive_media && is_media_file(p);
            if is_media || meta.len() >= min_size_bytes {
                files.push((p.to_path_buf(), meta.len(), is_media));
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

pub fn execute_striping(
    src_path: &Path,
    targets: &[PathBuf],
    plan: SimulationPlan,
    keep_backup: bool,
    mut on_progress: impl FnMut(usize, usize, &str, f32),
) -> Result<usize, String> {
    for (idx, target) in targets.iter().enumerate() {
        let required = plan.drive_bytes[idx + 1];
        if let Some(free) = get_free_disk_space_bytes(target).filter(|&free| free < required) {
            return Err(format!(
                "ABORTED: Insufficient space on Drive {} ({})! Need {:.2} GB, Available: {:.2} GB",
                idx + 2,
                target.display(),
                required as f64 / 1e9,
                free as f64 / 1e9
            ));
        }
    }

    let files_to_move: usize = plan.drive_buckets.iter().skip(1).map(|b| b.len()).sum();
    let mut manifest_entries = Vec::new();
    let mut moved_count = 0;

    for (target_idx, bucket) in plan.drive_buckets.into_iter().enumerate().skip(1) {
        let target_root = &targets[target_idx - 1];

        for (f_path, size, _) in bucket {
            if let Ok(rel) = f_path.strip_prefix(src_path) {
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

                if fs::copy(&f_path, &target_file).is_ok() {
                    let backup_created = if keep_backup {
                        let backup_path =
                            PathBuf::from(format!("{}.backup", f_path.to_string_lossy()));
                        fs::rename(&f_path, &backup_path).is_ok()
                    } else {
                        let _ = fs::remove_file(&f_path);
                        false
                    };

                    let _ = symlink_file(&target_file, &f_path);

                    manifest_entries.push(ManifestFileEntry {
                        rel_path: rel.to_string_lossy().to_string(),
                        target_drive_idx: target_idx,
                        target_file_path: target_file.to_string_lossy().to_string(),
                        size_bytes: size,
                        has_local_backup: backup_created,
                    });

                    moved_count += 1;
                }
            }
        }
    }

    let manifest = StripingManifest {
        game_path: src_path.to_string_lossy().to_string(),
        total_drives: plan.total_drives,
        files: manifest_entries,
    };
    save_manifest(src_path, &manifest);

    Ok(moved_count)
}

pub fn execute_revert(src_path: &Path, mut on_progress: impl FnMut(usize, usize, f32)) -> usize {
    let mut reverted = 0;

    if let Some(manifest) = load_manifest(src_path) {
        let total = manifest.files.len();

        for (idx, entry) in manifest.files.into_iter().enumerate() {
            let link_path = src_path.join(&entry.rel_path);
            let target_path = PathBuf::from(&entry.target_file_path);
            let backup_path = PathBuf::from(format!("{}.backup", link_path.to_string_lossy()));

            if entry.has_local_backup && backup_path.exists() {
                let _ = fs::remove_file(&link_path);
                if fs::rename(&backup_path, &link_path).is_ok() {
                    let _ = fs::remove_file(&target_path);
                    clean_empty_parents(&target_path);
                    reverted += 1;
                }
            } else if target_path.exists() {
                let _ = fs::remove_file(&link_path);
                if fs::copy(&target_path, &link_path).is_ok() {
                    let _ = fs::remove_file(&target_path);
                    clean_empty_parents(&target_path);
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
        delete_manifest(src_path);
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

pub fn execute_consolidate(src_path: &Path, chosen_target_dir: &Path) -> Result<usize, String> {
    let mut manifest =
        load_manifest(src_path).ok_or("No active manifest found for consolidation.")?;
    let chosen_str = chosen_target_dir.to_string_lossy().to_string();

    let mut files_to_keep = Vec::new();
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
            files_to_keep.push(entry);
        }
    }

    if files_to_keep.is_empty() {
        delete_manifest(src_path);
    } else {
        manifest.files = files_to_keep;
        save_manifest(src_path, &manifest);
    }

    // Safety: we use remove_dir instead of remove_dir_all to ensure we don't wipe custom user files
    let _ = fs::remove_dir(chosen_target_dir);
    Ok(consolidated_count)
}
