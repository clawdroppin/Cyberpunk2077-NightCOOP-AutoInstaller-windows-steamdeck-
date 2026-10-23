//! Locating Cyberpunk 2077 and reading the current state of the install.

use crate::config::{InstallState, Settings};
use crate::steam::{self, SteamGame, UserConfig};
use crate::util;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))] // GOG/Epic are only detected on Windows
pub enum Store {
    Steam,
    Gog,
    Epic,
    Manual,
}

impl Store {
    pub fn label(self) -> &'static str {
        match self {
            Store::Steam => "Steam",
            Store::Gog => "GOG",
            Store::Epic => "Epic",
            Store::Manual => "Manual",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Game {
    pub dir: PathBuf,
    pub store: Store,
    pub steam: Option<SteamGame>,
}

pub fn is_game_dir(p: &Path) -> bool {
    p.join("bin").join("x64").join("Cyberpunk2077.exe").is_file()
}

pub fn detect(settings: &Settings) -> Option<Game> {
    let manual = settings.game_path.trim();
    if !manual.is_empty() {
        let dir = PathBuf::from(manual);
        if is_game_dir(&dir) {
            let steam = steam::steam_game_for_dir(&dir);
            let store = if steam.is_some() { Store::Steam } else { Store::Manual };
            return Some(Game { dir, store, steam });
        }
    }
    if let Some(sg) = steam::find_cyberpunk() {
        if is_game_dir(&sg.game_dir) {
            return Some(Game { dir: sg.game_dir.clone(), store: Store::Steam, steam: Some(sg) });
        }
    }
    detect_other_stores()
}

#[cfg(windows)]
fn detect_other_stores() -> Option<Game> {
    use winreg::enums::*;
    use winreg::RegKey;
    // GOG Galaxy
    for key in ["SOFTWARE\\WOW6432Node\\GOG.com\\Games\\1423049311", "SOFTWARE\\GOG.com\\Games\\1423049311"] {
        if let Ok(k) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(key) {
            if let Ok(p) = k.get_value::<String, _>("path") {
                let dir = PathBuf::from(p);
                if is_game_dir(&dir) {
                    return Some(Game { dir, store: Store::Gog, steam: None });
                }
            }
        }
    }
    // Epic Games Launcher manifests
    let manifests = PathBuf::from("C:\\ProgramData\\Epic\\EpicGamesLauncher\\Data\\Manifests");
    if let Ok(rd) = fs::read_dir(manifests) {
        for e in rd.flatten() {
            let Ok(text) = fs::read_to_string(e.path()) else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            if v["DisplayName"].as_str().map(|n| n.contains("Cyberpunk 2077")).unwrap_or(false) {
                if let Some(loc) = v["InstallLocation"].as_str() {
                    let dir = PathBuf::from(loc);
                    if is_game_dir(&dir) {
                        return Some(Game { dir, store: Store::Epic, steam: None });
                    }
                }
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn detect_other_stores() -> Option<Game> {
    None
}

impl Game {
    pub fn exe(&self) -> PathBuf {
        self.dir.join("bin").join("x64").join("Cyberpunk2077.exe")
    }
    pub fn red4ext_dir(&self) -> PathBuf {
        self.dir.join("red4ext")
    }
    pub fn plugin_dir(&self) -> PathBuf {
        self.red4ext_dir().join("plugins").join("NightCoop")
    }
    pub fn disabled_plugin_dir(&self) -> PathBuf {
        self.red4ext_dir().join("NightCoop.disabled")
    }
    /// Where "mods off" keeps RED4ext and NightCoop while the game runs unmodded.
    pub fn park_dir(&self) -> PathBuf {
        self.dir.join("_NightCoopHelper_mods_off")
    }
    pub fn servers_file(&self) -> PathBuf {
        self.plugin_dir().join("servers.txt")
    }
    /// Running through Proton (Linux) rather than natively on Windows.
    pub fn uses_proton(&self) -> bool {
        cfg!(not(windows)) && self.steam.is_some()
    }

    pub fn saves_dir(&self) -> Option<PathBuf> {
        let rel = ["Saved Games", "CD Projekt Red", "Cyberpunk 2077"];
        if cfg!(windows) {
            let mut p = dirs::home_dir()?;
            rel.iter().for_each(|r| p.push(r));
            return Some(p);
        }
        let sg = self.steam.as_ref()?;
        let mut p = sg.compatdata().join("pfx").join("drive_c").join("users").join("steamuser");
        rel.iter().for_each(|r| p.push(r));
        Some(p)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub game: Option<Game>,
    pub game_version: Option<String>,
    pub phantom_liberty: bool,
    pub loader_present: bool,
    pub red4ext_version: Option<String>,
    pub nightcoop_installed: bool,
    pub nightcoop_disabled: bool,
    pub nightcoop_version: Option<String>,
    pub saves_dir: Option<PathBuf>,
    pub save_count: usize,
    pub saves_size: u64,
    pub proton: Option<String>,
    pub users: Vec<UserConfig>,
    pub steam_running: bool,
    pub game_running: bool,
    pub log_lines: Vec<String>,
    pub log_file: Option<PathBuf>,
    /// RED4ext and NightCoop are parked (mods switched off).
    pub mods_off: bool,
    pub steam_build: Option<String>,
    /// The game's Steam build changed since mods were installed.
    pub game_updated: bool,
    pub cloud: Option<steam::CloudInfo>,
}

impl Status {
    pub fn scan(settings: &Settings, state: &InstallState) -> Status {
        let mut s = Status { steam_running: steam::steam_running(), game_running: steam::game_running(), ..Default::default() };
        let Some(game) = detect(settings) else { return s };
        let same_dir = Path::new(&state.game_dir) == game.dir;

        s.game_version = util::pe_version(&game.exe());
        s.phantom_liberty = game.dir.join("archive").join("pc").join("ep1").is_dir();
        s.mods_off = game.park_dir().exists();
        // While mods are off, report what is parked so versions stay visible.
        let (bin_dir, r4_dir) = if s.mods_off {
            (game.park_dir().join("bin").join("x64"), game.park_dir().join("red4ext"))
        } else {
            (game.dir.join("bin").join("x64"), game.red4ext_dir())
        };
        s.loader_present = bin_dir.join("winmm.dll").is_file();
        let r4 = r4_dir.join("RED4ext.dll");
        if r4.is_file() {
            s.red4ext_version = util::pe_version(&r4)
                .or_else(|| same_dir.then(|| state.red4ext_version.clone()).flatten())
                .or_else(|| Some("unknown version".into()));
        }
        let dll = r4_dir.join("plugins").join("NightCoop").join("NightCoop.dll");
        let legacy_disabled = r4_dir.join("NightCoop.disabled").join("NightCoop.dll");
        s.nightcoop_installed = dll.is_file();
        s.nightcoop_disabled = !s.nightcoop_installed && legacy_disabled.is_file();
        if s.nightcoop_installed || s.nightcoop_disabled {
            let dll = if s.nightcoop_installed { dll } else { legacy_disabled };
            // Trust our record only while the DLL is still the one we installed.
            let ours = same_dir
                && state.nightcoop_dll_sha256.is_some()
                && util::sha256_file(&dll).ok() == state.nightcoop_dll_sha256;
            s.nightcoop_version = ours
                .then(|| state.nightcoop_version.clone())
                .flatten()
                .or_else(|| util::pe_version(&dll))
                .or_else(|| Some("unknown version".into()));
        }
        s.saves_dir = game.saves_dir();
        if let Some(dir) = &s.saves_dir {
            if let Ok(rd) = fs::read_dir(dir) {
                s.save_count = rd.flatten().filter(|e| e.path().join("sav.dat").is_file()).count();
            }
            s.saves_size = util::dir_size(dir);
        }
        if let Some(sg) = &game.steam {
            s.proton = steam::proton_version(sg);
            s.users = steam::user_configs(&sg.steam_root);
            s.cloud = steam::cloud_info(&sg.steam_root);
            s.steam_build = sg.build_id.clone();
            s.game_updated = same_dir
                && (s.nightcoop_installed || s.red4ext_version.is_some())
                && matches!((&state.game_build_id, &sg.build_id), (Some(a), Some(b)) if a != b);
        }
        (s.log_file, s.log_lines) = red4ext_log_summary(&game);
        s.game = Some(game);
        s
    }

    pub fn launch_option_ok(&self) -> bool {
        !self.users.is_empty() && self.users.iter().all(|u| u.launch_options.as_deref().map(steam::has_override).unwrap_or(false))
    }

    /// At least one Steam account on this device still has the override (what removal must clean up).
    pub fn launch_option_any(&self) -> bool {
        self.users.iter().any(|u| u.launch_options.as_deref().map(steam::has_override).unwrap_or(false))
    }

    pub fn needs_launch_option(&self) -> bool {
        self.game.as_ref().map(|g| g.uses_proton()).unwrap_or(false)
    }

    pub fn ready(&self) -> bool {
        self.game.is_some()
            && !self.mods_off
            && self.loader_present
            && self.red4ext_version.is_some()
            && self.nightcoop_installed
            && (!self.needs_launch_option() || self.launch_option_ok())
    }

    pub fn diagnostics(&self, state: &InstallState) -> String {
        let mut d = String::new();
        let mut line = |k: &str, v: String| d.push_str(&format!("{k:<22} {v}\n"));
        line("Helper", env!("CARGO_PKG_VERSION").to_string());
        line("OS", format!("{} {}", std::env::consts::OS, std::env::consts::ARCH));
        line("Steam Deck", steam::is_steam_deck().to_string());
        line("SteamOS", steam::is_steamos().to_string());
        line("Gaming Mode", steam::in_gaming_mode().to_string());
        match &self.game {
            Some(g) => {
                line("Game folder", g.dir.display().to_string());
                line("Store", g.store.label().to_string());
                if let Some(sg) = &g.steam {
                    line("Steam root", sg.steam_root.display().to_string());
                    line("Steam build", sg.build_id.clone().unwrap_or_else(|| "?".into()));
                }
            }
            None => line("Game folder", "NOT FOUND".into()),
        }
        let opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "-".into());
        line("Game exe version", opt(&self.game_version));
        line("Phantom Liberty", self.phantom_liberty.to_string());
        line("Proton", opt(&self.proton));
        line("RED4ext loader", self.loader_present.to_string());
        line("RED4ext", opt(&self.red4ext_version));
        line("NightCoop", format!("{} (installed {}, disabled {})", opt(&self.nightcoop_version), self.nightcoop_installed, self.nightcoop_disabled));
        line("Installed by helper", opt(&state.installed_at));
        line("Mods switched off", self.mods_off.to_string());
        line("Game updated since install", self.game_updated.to_string());
        if let Some(c) = &self.cloud {
            line("Steam Cloud", format!("{} files, last upload {}", c.files, c.newest_text()));
        }
        line("Saves", format!("{} saves, {}", self.save_count, util::human_size(self.saves_size)));
        line("Saves folder", self.saves_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "-".into()));
        for u in &self.users {
            line(&format!("Launch opts [{}]", u.persona.clone().unwrap_or_else(|| u.user_id.clone())), opt(&u.launch_options));
        }
        line("Steam running", self.steam_running.to_string());
        line("Game running", self.game_running.to_string());
        if !self.log_lines.is_empty() {
            d.push_str("\nRED4ext log (relevant lines):\n");
            for l in &self.log_lines {
                d.push_str(l);
                d.push('\n');
            }
        }
        d
    }
}

/// Lines from the newest RED4ext log that mention NightCoop, errors or warnings.
fn red4ext_log_summary(game: &Game) -> (Option<PathBuf>, Vec<String>) {
    let logs = game.red4ext_dir().join("logs");
    let newest = fs::read_dir(&logs)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().to_ascii_lowercase().starts_with("red4ext"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    let Some(entry) = newest else { return (None, Vec::new()) };
    let text = fs::read(entry.path()).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let lines: Vec<String> = text
        .lines()
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("nightcoop") || l.contains("error") || l.contains("warn") || l.contains("version")
        })
        .map(|l| l.chars().take(240).collect())
        .collect();
    let start = lines.len().saturating_sub(25);
    (Some(entry.path()), lines[start..].to_vec())
}
