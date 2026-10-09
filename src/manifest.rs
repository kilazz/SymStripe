use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const MANIFEST_FILENAME: &str = ".striping_manifest.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ManifestFileEntry {
    pub rel_path: String,
    pub target_drive_idx: usize,
    pub target_file_path: String,
    pub size_bytes: u64,
    pub has_local_backup: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StripingManifest {
    pub game_path: String,
    pub total_drives: usize,
    pub files: Vec<ManifestFileEntry>,
}

pub fn load_manifest(game_root: &Path) -> Option<StripingManifest> {
    let manifest_path = game_root.join(MANIFEST_FILENAME);
    fs::read_to_string(manifest_path)
        .ok()
        .and_then(|c| serde_json::from_str::<StripingManifest>(&c).ok())
}

pub fn save_manifest(game_root: &Path, manifest: &StripingManifest) {
    let manifest_path = game_root.join(MANIFEST_FILENAME);
    if let Ok(json) = serde_json::to_string_pretty(manifest) {
        let _ = fs::write(manifest_path, json);
    }
}

pub fn delete_manifest(game_root: &Path) {
    let manifest_path = game_root.join(MANIFEST_FILENAME);
    let _ = fs::remove_file(manifest_path);
}
