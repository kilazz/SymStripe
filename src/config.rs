use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub primary_path: String,
    pub secondary_targets: Vec<String>,
    pub min_size_mb: String,
    pub exclusions: String,
    #[serde(default = "default_true")]
    pub keep_backup: bool,
    #[serde(default = "default_false")]
    pub aggressive_media: bool,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: "Default Profile".to_string(),
            primary_path: String::new(),
            secondary_targets: Vec::new(),
            min_size_mb: "50".to_string(),
            exclusions: "exe, dll, pdb".to_string(),
            keep_backup: true,
            aggressive_media: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppConfig {
    pub active_profile_index: usize,
    pub profiles: Vec<Profile>,
    pub drive_letter: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            active_profile_index: 0,
            profiles: vec![Profile::default()],
            drive_letter: "Z:".to_string(),
        }
    }
}

pub fn get_config_file_path() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join("config.json")))
        .unwrap_or_else(|| PathBuf::from("config.json"))
}

pub fn load_config() -> AppConfig {
    let path = get_config_file_path();
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<AppConfig>(&content).ok())
        .filter(|cfg| !cfg.profiles.is_empty())
        .unwrap_or_default()
}

pub fn save_config(cfg: &AppConfig) {
    let path = get_config_file_path();
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(path, json);
    }
}
