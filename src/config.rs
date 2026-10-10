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

fn default_mode() -> i32 {
    0 // 0 = In-Place Optimizer, 1 = Virtual Storage Pool
}

fn default_pool_policy() -> i32 {
    1 // 0 = Capacity (JBOD), 1 = Speed (RAID-0)
}

fn default_stripe_mb() -> u32 {
    16
}

fn default_mount_point() -> String {
    "Z:".to_string()
}

fn default_custom_size_gb() -> String {
    "0".to_string() // 0 = Auto detect from physical drives
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Profile {
    pub name: String,

    // Paradigm 1: In-Place Game Optimizer
    pub primary_path: String,
    pub secondary_targets: Vec<String>,
    pub min_size_mb: String,
    pub exclusions: String,
    pub media_extensions: String,
    #[serde(default = "default_true")]
    pub keep_backup: bool,
    #[serde(default = "default_false")]
    pub aggressive_media: bool,
    #[serde(default = "default_true")]
    pub ue5_safe_mode: bool,

    #[serde(default = "default_mode")]
    pub mode_idx: i32,

    // Paradigm 2: Virtual Storage Pool
    #[serde(default = "Vec::new")]
    pub pool_members: Vec<String>,
    #[serde(default = "default_pool_policy")]
    pub pool_policy: i32,
    #[serde(default = "default_stripe_mb")]
    pub pool_stripe_mb: u32,
    #[serde(default = "default_mount_point")]
    pub pool_mount_point: String,
    #[serde(default = "default_custom_size_gb")]
    pub pool_custom_size_gb: String, // Added customizable size quota in GB
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: "Default Profile".to_string(),
            primary_path: String::new(),
            secondary_targets: Vec::new(),
            min_size_mb: "50".to_string(),
            exclusions: "exe, dll, pdb, utoc, sig".to_string(),
            media_extensions: "bik, bk2, mp4, fsb, pck, wem".to_string(),
            keep_backup: true,
            aggressive_media: false,
            ue5_safe_mode: true,
            mode_idx: 0,
            pool_members: Vec::new(),
            pool_policy: 1,
            pool_stripe_mb: 16,
            pool_mount_point: "Z:".to_string(),
            pool_custom_size_gb: "0".to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppConfig {
    pub active_profile_index: usize,
    pub profiles: Vec<Profile>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            active_profile_index: 0,
            profiles: vec![Profile::default()],
        }
    }
}

pub fn get_config_dir() -> PathBuf {
    if let Some(appdata) = env::var_os("APPDATA") {
        PathBuf::from(appdata).join("SymStripe")
    } else if let Ok(exe) = env::current_exe() {
        exe.parent().unwrap_or(&PathBuf::from(".")).to_path_buf()
    } else {
        PathBuf::from(".")
    }
}

pub fn get_config_file_path() -> PathBuf {
    get_config_dir().join("config.json")
}

pub fn load_config() -> AppConfig {
    let main_path = get_config_file_path();

    if let Ok(content) = fs::read_to_string(&main_path)
        && let Ok(cfg) = serde_json::from_str::<AppConfig>(&content)
        && !cfg.profiles.is_empty()
    {
        return cfg;
    }

    if let Ok(exe_path) = env::current_exe()
        && let Some(parent) = exe_path.parent()
    {
        let legacy_path = parent.join("config.json");
        if legacy_path.exists()
            && let Ok(content) = fs::read_to_string(&legacy_path)
            && let Ok(cfg) = serde_json::from_str::<AppConfig>(&content)
            && !cfg.profiles.is_empty()
        {
            save_config(&cfg);
            return cfg;
        }
    }

    AppConfig::default()
}

pub fn save_config(cfg: &AppConfig) {
    let dir = get_config_dir();
    let _ = fs::create_dir_all(&dir);

    let path = get_config_file_path();
    let tmp_path = dir.join("config.json.tmp");

    if let Ok(json) = serde_json::to_string_pretty(cfg)
        && fs::write(&tmp_path, json).is_ok()
    {
        let _ = fs::rename(&tmp_path, &path);
    }
}
