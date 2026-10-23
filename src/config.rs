//! Portable data folder, user settings and the record of what we installed.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const DATA_DIR_NAME: &str = "NightCoopHelper-data";

/// Everything lives next to the executable when that folder is writable (true portable
/// mode); otherwise it falls back to the user's config directory.
pub fn data_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join(DATA_DIR_NAME))) {
            if fs::create_dir_all(&dir).is_ok() && is_writable(&dir) {
                return dir;
            }
        }
        let dir = dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("NightCoopHelper");
        let _ = fs::create_dir_all(&dir);
        dir
    })
    .clone()
}

fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(".write-test");
    let ok = fs::write(&probe, b"ok").is_ok();
    let _ = fs::remove_file(probe);
    ok
}

pub fn sub(name: &str) -> PathBuf {
    let d = data_dir().join(name);
    let _ = fs::create_dir_all(&d);
    d
}

fn load<T: for<'de> Deserialize<'de> + Default>(file: &str) -> T {
    fs::read_to_string(data_dir().join(file)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save<T: Serialize>(file: &str, value: &T) {
    if let Ok(s) = serde_json::to_string_pretty(value) {
        let _ = fs::write(data_dir().join(file), s);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Channel {
    #[default]
    Stable,
    Beta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Manual game folder; empty means auto-detect.
    pub game_path: String,
    pub channel: Channel,
    pub ui_scale: f32,
    pub check_updates_on_start: bool,
    pub backup_saves_before_install: bool,
    /// Keep this many save backups (oldest are deleted). 0 = keep all.
    pub keep_backups: usize,
    /// GitHub "owner/repo" where releases of this helper are published, for self-update.
    pub helper_repo: String,
    pub manifest_url: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            game_path: String::new(),
            channel: Channel::Stable,
            ui_scale: if crate::steam::is_steam_deck() { 1.5 } else { 1.15 },
            check_updates_on_start: true,
            backup_saves_before_install: true,
            keep_backups: 10,
            helper_repo: crate::selfupdate::HOME_REPO.to_string(),
            manifest_url: crate::remote::NIGHTCOOP_MANIFEST.to_string(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        load("settings.json")
    }
    pub fn save(&self) {
        save("settings.json", self)
    }
}

/// What this helper installed, so updates and uninstalls are exact.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct InstallState {
    pub game_dir: String,
    pub red4ext_version: Option<String>,
    pub red4ext_files: Vec<String>,
    pub nightcoop_version: Option<String>,
    pub nightcoop_sha256: Option<String>,
    pub nightcoop_dll_sha256: Option<String>,
    pub nightcoop_from_file: Option<String>,
    pub installed_at: Option<String>,
    pub last_save_backup: Option<String>,
}

impl InstallState {
    pub fn load() -> Self {
        load("state.json")
    }
    pub fn save(&self) {
        save("state.json", self)
    }
}
