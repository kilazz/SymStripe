use crate::manifest::load_manifest;
use crate::{AppWindow, StripeBlockUI};
use slint::{Color, ModelRc, SharedString, StandardListViewItem, VecModel};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Centralized color palette for multi-spindle drives visualization.
pub fn get_drive_color(index: usize) -> Color {
    let palette = [
        Color::from_rgb_u8(96, 165, 250),  // Drive 1: Blue
        Color::from_rgb_u8(192, 132, 252), // Drive 2: Purple
        Color::from_rgb_u8(245, 158, 11),  // Drive 3: Amber
        Color::from_rgb_u8(244, 114, 182), // Drive 4: Pink
        Color::from_rgb_u8(52, 211, 153),  // Drive 5: Emerald
    ];
    palette[index % palette.len()]
}

pub struct PoolChunkItem {
    pub part_idx: u64,
    pub member_idx: usize,
    pub part_size: u64,
    pub ext: String,
}

#[derive(Default)]
pub struct PoolFileInfo {
    pub total_size: u64,
    pub chunks: Vec<PoolChunkItem>,
}

/// Inspects the game root in Paradigm 1 (In-Place Symlinks) and updates the UI.
pub fn inspect_game_folder(src_path: &Path, ui: &slint::Weak<AppWindow>) {
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

        let mut visualizer_items = Vec::new();

        for (idx, f) in manifest.files.iter().enumerate() {
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

            let drive_idx = f.target_drive_idx;
            let col = get_drive_color(drive_idx);
            visualizer_items.push(StripeBlockUI {
                id: idx as i32,
                label: format!("Chunk #{}", idx + 1).into(),
                file_name: name.to_string().into(),
                offset_str: format!("0x{:08X}", idx * 16 * 1024 * 1024).into(),
                size_str: format!("{:.1} MB", mb).into(),
                drive_name: format!("Drive {} ({})", drive_idx + 1, f.target_file_path).into(),
                status_str: if f.has_local_backup {
                    "Protected (.backup)".into()
                } else {
                    "Active Storage".into()
                },
                color_drive: col,
                is_cached: idx % 3 == 0,
            });
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
            app_ui.set_visualizer_blocks(ModelRc::new(VecModel::from(visualizer_items)));
            app_ui.set_status_msg("Manifest detected: Directory is currently STRIPED.".into());
        });
    } else {
        let _ = ui.upgrade_in_event_loop(|app_ui| {
            app_ui.set_is_already_striped(false);
            app_ui.set_summary_text("Ready for analysis.".into());
            app_ui.set_file_list(ModelRc::new(VecModel::from(Vec::new())));
            app_ui.set_visualizer_blocks(ModelRc::new(VecModel::from(Vec::new())));
        });
    }
}

/// Scans physical storage member folders in Paradigm 2 (Virtual Pool) and builds the block map.
pub fn inspect_pool_storage(members: &[PathBuf], stripe_mb: u32, ui: &slint::Weak<AppWindow>) {
    if members.is_empty() {
        return;
    }

    let mut visualizer_items = Vec::new();
    let mut tree_rows = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut total_chunks = 0;

    tree_rows.push("▼ 💽 [VIRTUAL STORAGE POOL] — Live Member Topology".to_string());

    let mut file_chunks: HashMap<String, PoolFileInfo> = HashMap::new();

    for (m_idx, member_root) in members.iter().enumerate() {
        for entry in walkdir::WalkDir::new(member_root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if let Ok(meta) = entry.metadata()
                && meta.is_file()
            {
                let path = entry.path();
                let file_name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();

                if file_name.contains(".part") {
                    if let Some(part_pos) = file_name.find(".part") {
                        let base_name = &file_name[..part_pos];
                        let rest = &file_name[part_pos + 5..];
                        let parts: Vec<&str> = rest.split('.').collect();
                        let part_idx = parts
                            .first()
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(0);
                        let ext = parts.get(1).unwrap_or(&"raw").to_string();

                        let rel = path
                            .strip_prefix(member_root)
                            .unwrap_or(path)
                            .parent()
                            .unwrap_or(Path::new(""))
                            .join(base_name)
                            .to_string_lossy()
                            .to_string();

                        let entry_chunk = file_chunks.entry(rel).or_default();
                        entry_chunk.total_size += meta.len();
                        entry_chunk.chunks.push(PoolChunkItem {
                            part_idx,
                            member_idx: m_idx,
                            part_size: meta.len(),
                            ext,
                        });
                        total_bytes += meta.len();
                        total_chunks += 1;
                    }
                } else if !file_name.ends_with(".symstripe_meta")
                    && !file_name.starts_with(".symstripe_jbod")
                {
                    let rel = path
                        .strip_prefix(member_root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .to_string();

                    let entry_chunk = file_chunks.entry(rel).or_default();
                    entry_chunk.total_size += meta.len();
                    entry_chunk.chunks.push(PoolChunkItem {
                        part_idx: 0,
                        member_idx: m_idx,
                        part_size: meta.len(),
                        ext: "direct".to_string(),
                    });
                    total_bytes += meta.len();
                    total_chunks += 1;
                }
            }
        }
    }

    let mut sorted_files: Vec<(String, PoolFileInfo)> = file_chunks.into_iter().collect();
    sorted_files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut global_chunk_id = 0;
    for (file_rel, mut file_info) in sorted_files {
        file_info.chunks.sort_by_key(|c| c.part_idx);
        let mb = file_info.total_size as f64 / (1024.0 * 1024.0);

        if file_info.chunks.len() > 1 {
            tree_rows.push(format!(
                "   ├─ [STRIPED] {} ({:.1} MB across {} drives)",
                file_rel,
                mb,
                file_info.chunks.len()
            ));
        } else if let Some(first) = file_info.chunks.first() {
            tree_rows.push(format!(
                "   ├─ [Drive {}] {} ({:.1} MB)",
                first.member_idx + 1,
                file_rel,
                mb
            ));
        }

        for chunk in file_info.chunks {
            let part_mb = chunk.part_size as f64 / (1024.0 * 1024.0);
            let drive_color = get_drive_color(chunk.member_idx);
            let member_name = format!(
                "Drive {} ({})",
                chunk.member_idx + 1,
                members[chunk.member_idx].display()
            );

            let status_tag = match chunk.ext.as_str() {
                "lz4" => "LZ4 Fast Compressed",
                "zst" => "Zstd ATG DirectStorage",
                "raw" => "Direct Raw Block",
                _ => "Active Storage",
            };

            let offset_bytes = chunk.part_idx * (stripe_mb as u64) * 1024 * 1024;

            visualizer_items.push(StripeBlockUI {
                id: global_chunk_id,
                label: format!("Stripe #{}", global_chunk_id + 1).into(),
                file_name: file_rel.clone().into(),
                offset_str: format!("0x{:08X}", offset_bytes).into(),
                size_str: format!("{:.1} MB", part_mb).into(),
                drive_name: member_name.into(),
                status_str: status_tag.into(),
                color_drive: drive_color,
                is_cached: global_chunk_id % 3 == 0,
            });
            global_chunk_id += 1;
        }
    }

    let summary = format!(
        "Pool Topology: {} chunks ({:.2} GB across {} active members)",
        total_chunks,
        total_bytes as f64 / 1e9,
        members.len()
    );

    let _ = ui.upgrade_in_event_loop(move |app_ui| {
        app_ui.set_summary_text(summary.into());
        let items: Vec<StandardListViewItem> = tree_rows
            .into_iter()
            .map(|s| StandardListViewItem::from(SharedString::from(s)))
            .collect();
        app_ui.set_file_list(ModelRc::new(VecModel::from(items)));
        app_ui.set_visualizer_blocks(ModelRc::new(VecModel::from(visualizer_items)));
    });
}
