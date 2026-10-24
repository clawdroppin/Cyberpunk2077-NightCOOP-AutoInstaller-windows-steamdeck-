//! Steam discovery, launch options, process control and Steam Deck detection.

use crate::vdf;
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const CYBERPUNK_APPID: &str = "1091500";
/// Makes Proton load RED4ext's winmm.dll (and CET's version.dll, harmless if absent).
pub const DLL_OVERRIDE: &str = "WINEDLLOVERRIDES=\"winmm,version=n,b\"";

pub fn is_steam_deck() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let read = |p: &str| fs::read_to_string(p).unwrap_or_default().trim().to_string();
    let vendor = read("/sys/devices/virtual/dmi/id/board_vendor");
    let product = read("/sys/devices/virtual/dmi/id/product_name");
    vendor == "Valve" || matches!(product.as_str(), "Jupiter" | "Galileo")
}

pub fn is_steamos() -> bool {
    fs::read_to_string("/etc/os-release").map(|s| s.lines().any(|l| l.trim() == "ID=steamos")).unwrap_or(false)
}

/// True when running inside Steam's Gaming Mode (gamescope session).
pub fn in_gaming_mode() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").map(|v| v.eq_ignore_ascii_case("gamescope")).unwrap_or(false)
        || std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some()
}

/// All Steam installation roots found on this machine.
pub fn steam_roots() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        use winreg::enums::*;
        use winreg::RegKey;
        if let Ok(k) = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Software\\Valve\\Steam") {
            if let Ok(p) = k.get_value::<String, _>("SteamPath") {
                candidates.push(PathBuf::from(p));
            }
        }
        for key in ["SOFTWARE\\WOW6432Node\\Valve\\Steam", "SOFTWARE\\Valve\\Steam"] {
            if let Ok(k) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(key) {
                if let Ok(p) = k.get_value::<String, _>("InstallPath") {
                    candidates.push(PathBuf::from(p));
                }
            }
        }
        candidates.push(PathBuf::from("C:\\Program Files (x86)\\Steam"));
        candidates.push(PathBuf::from("C:\\Program Files\\Steam"));
    }
    #[cfg(not(windows))]
    if let Some(home) = dirs::home_dir() {
        for rel in [
            ".local/share/Steam",
            ".steam/steam",
            ".steam/root",
            ".steam/debian-installation",
            ".var/app/com.valvesoftware.Steam/.local/share/Steam",
            "snap/steam/common/.local/share/Steam",
        ] {
            candidates.push(home.join(rel));
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    for c in candidates {
        if !c.join("steamapps").is_dir() {
            continue;
        }
        let c = canon(&c);
        if !roots.contains(&c) {
            roots.push(c);
        }
    }
    roots
}

/// Canonical path without Windows' `\\?\` verbatim prefix (keeps paths readable and Steam-compatible).
pub fn canon(p: &Path) -> PathBuf {
    let c = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let stripped = c.to_string_lossy().strip_prefix(r"\\?\").filter(|rest| !rest.starts_with("UNC")).map(PathBuf::from);
    stripped.unwrap_or(c)
}

pub fn is_flatpak_root(root: &Path) -> bool {
    root.to_string_lossy().contains("com.valvesoftware.Steam")
}

/// Every library folder (including the root itself) listed by a Steam install.
pub fn library_folders(root: &Path) -> Vec<PathBuf> {
    let mut libs = vec![canon(root)];
    let file = root.join("steamapps").join("libraryfolders.vdf");
    if let Ok(text) = fs::read_to_string(&file) {
        if let Ok(doc) = vdf::parse(&text) {
            if let Some(lf) = doc.get("libraryfolders") {
                for (_, entry) in lf.children() {
                    let path = entry.get_str("path").or_else(|| entry.str());
                    if let Some(p) = path {
                        let p = canon(Path::new(&p));
                        if !libs.contains(&p) {
                            libs.push(p);
                        }
                    }
                }
            }
        }
    }
    libs
}

#[derive(Debug, Clone)]
pub struct SteamGame {
    pub steam_root: PathBuf,
    pub library: PathBuf,
    pub game_dir: PathBuf,
    pub build_id: Option<String>,
}

impl SteamGame {
    pub fn compatdata(&self) -> PathBuf {
        self.library.join("steamapps").join("compatdata").join(CYBERPUNK_APPID)
    }
}

pub fn find_cyberpunk() -> Option<SteamGame> {
    for root in steam_roots() {
        for lib in library_folders(&root) {
            let manifest = lib.join("steamapps").join(format!("appmanifest_{CYBERPUNK_APPID}.acf"));
            let Ok(text) = fs::read_to_string(&manifest) else { continue };
            let Ok(doc) = vdf::parse(&text) else { continue };
            let Some(state) = doc.get("AppState") else { continue };
            let installdir = state.get_str("installdir").unwrap_or_else(|| "Cyberpunk 2077".into());
            let game_dir = lib.join("steamapps").join("common").join(installdir);
            if game_dir.is_dir() {
                return Some(SteamGame {
                    steam_root: root.clone(),
                    library: lib.clone(),
                    game_dir,
                    build_id: state.get_str("buildid"),
                });
            }
        }
    }
    None
}

/// If `game_dir` sits inside a Steam library, rebuilds the SteamGame for it.
pub fn steam_game_for_dir(game_dir: &Path) -> Option<SteamGame> {
    let common = game_dir.parent()?;
    let steamapps = common.parent()?;
    if !common.file_name()?.eq_ignore_ascii_case("common") || !steamapps.file_name()?.eq_ignore_ascii_case("steamapps") {
        return None;
    }
    let library = steamapps.parent()?.to_path_buf();
    let found = find_cyberpunk();
    let steam_root = found
        .as_ref()
        .map(|g| g.steam_root.clone())
        .or_else(|| steam_roots().into_iter().next())
        .unwrap_or_else(|| library.clone());
    let build_id = fs::read_to_string(steamapps.join(format!("appmanifest_{CYBERPUNK_APPID}.acf")))
        .ok()
        .and_then(|t| vdf::parse(&t).ok())
        .and_then(|d| d.get("AppState").and_then(|s| s.get_str("buildid")));
    Some(SteamGame { steam_root, library, game_dir: game_dir.to_path_buf(), build_id })
}

/// Proton version used for the game's prefix, if it has been launched at least once.
pub fn proton_version(game: &SteamGame) -> Option<String> {
    let v = fs::read_to_string(game.compatdata().join("version")).ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

// ---------- Visual C++ runtime inside the Proton prefix ----------

/// RED4ext (and its loader) link against Microsoft's VC++ 2015-2022 runtime. Wine's built-in
/// stand-ins are too old for current builds: loading RED4ext.dll then fails with error 998.
pub const VC_REDIST_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";
const VC_MIN_VERSION: &str = "14.40";
const VC_DLLS: [&str; 3] = ["msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll"];

/// DLLs Wine must load from Microsoft's runtime instead of its own stand-ins — the same list
/// `winetricks vcrun2022` marks "native,builtin".
const VC_OVERRIDES: [&str; 11] = [
    "concrt140", "msvcp140", "msvcp140_1", "msvcp140_2", "msvcp140_atomic_wait", "msvcp140_codecvt_ids",
    "vcamp140", "vccorlib140", "vcomp140", "vcruntime140", "vcruntime140_1",
];

/// `Some(true)` when the prefix has Microsoft's runtime ≥ 14.40 and Wine is told to use it,
/// `Some(false)` when it doesn't, `None` when the prefix doesn't exist yet (game never started).
pub fn vc_runtime_ok(game: &SteamGame) -> Option<bool> {
    let dlls = vc_dlls_ok(game)?;
    Some(dlls && vc_overrides_set(game))
}

/// Microsoft's runtime DLLs (not Wine's stand-ins, ≥ 14.40) are in the prefix's system32.
pub fn vc_dlls_ok(game: &SteamGame) -> Option<bool> {
    let sys32 = game.compatdata().join("pfx").join("drive_c").join("windows").join("system32");
    if !sys32.is_dir() {
        return None;
    }
    Some(VC_DLLS.iter().all(|name| {
        let path = sys32.join(name);
        let mut head = vec![0u8; 512];
        let is_wine = fs::File::open(&path)
            .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
            .map(|n| head[..n].windows(12).any(|w| w == b"Wine builtin" || w == b"Wine placeho"))
            .unwrap_or(true);
        !is_wine && crate::util::pe_version(&path).map(|v| crate::util::cmp_versions(&v, VC_MIN_VERSION).is_ge()).unwrap_or(false)
    }))
}

/// The prefix's registry (`pfx/user.reg`) tells Wine to prefer the native runtime DLLs.
pub fn vc_overrides_set(game: &SteamGame) -> bool {
    let Ok(reg) = fs::read_to_string(game.compatdata().join("pfx").join("user.reg")) else { return false };
    let Some(start) = reg.find("[Software\\\\Wine\\\\DllOverrides]") else { return false };
    let section = &reg[start..];
    let section = &section[..section[1..].find("\n[").map(|i| i + 1).unwrap_or(section.len())];
    ["msvcp140", "vcruntime140", "vcruntime140_1"].iter().all(|d| section.contains(&format!("\"{d}\"=\"native")))
}

/// Marks the runtime DLLs "native,builtin" in the prefix's registry (one silent `regedit` run).
pub fn set_vc_overrides(game: &SteamGame) -> Result<()> {
    let mut reg = String::from("Windows Registry Editor Version 5.00\r\n\r\n[HKEY_CURRENT_USER\\Software\\Wine\\DllOverrides]\r\n");
    for d in VC_OVERRIDES {
        reg.push_str(&format!("\"{d}\"=\"native,builtin\"\r\n"));
    }
    let file = crate::config::sub("downloads").join("nightcoop-vc-overrides.reg");
    fs::write(&file, reg)?;
    // Wine maps the Linux root to drive Z:.
    let wine_path = format!("Z:{}", file.display().to_string().replace('/', "\\"));
    let code = run_in_prefix(game, "regedit", &["/S", &wine_path])?;
    if code != 0 {
        bail!("regedit ended with code {code}");
    }
    Ok(())
}

/// The Proton build the game runs with: `compatdata/1091500/config_info` lists paths inside it.
/// Falls back to the newest Proton installed in any Steam library.
pub fn proton_dir(game: &SteamGame) -> Option<PathBuf> {
    if let Ok(text) = fs::read_to_string(game.compatdata().join("config_info")) {
        for line in text.lines().skip(1) {
            if let Some(dir) = Path::new(line.trim()).ancestors().find(|a| a.join("proton").is_file()) {
                return Some(dir.to_path_buf());
            }
        }
    }
    let mut found: Vec<PathBuf> = library_folders(&game.steam_root)
        .into_iter()
        .flat_map(|lib| fs::read_dir(lib.join("steamapps").join("common")).into_iter().flatten().flatten().map(|e| e.path()))
        .filter(|p| p.join("proton").is_file() && p.file_name().map(|n| n.to_string_lossy().starts_with("Proton")).unwrap_or(false))
        .collect();
    // "Proton - Experimental" first, then the highest numbered version.
    found.sort_by(|a, b| {
        let key = |p: &PathBuf| {
            let n = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            (n.contains("Experimental"), n)
        };
        let (ea, na) = key(a);
        let (eb, nb) = key(b);
        eb.cmp(&ea).then_with(|| crate::util::cmp_versions(&nb, &na))
    });
    found.into_iter().next()
}

/// Runs a Windows program inside the game's Proton prefix (what Protontricks does) and waits for it.
pub fn run_in_prefix(game: &SteamGame, exe: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> Result<i32> {
    let proton = proton_dir(game).ok_or_else(|| anyhow!("couldn't find the Proton version Cyberpunk uses — start the game once, then try again"))?;
    let status = Command::new(proton.join("proton"))
        .arg("run")
        .arg(exe.as_ref())
        .args(args)
        .env("STEAM_COMPAT_DATA_PATH", game.compatdata())
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", &game.steam_root)
        .env("SteamAppId", CYBERPUNK_APPID)
        .env("SteamGameId", CYBERPUNK_APPID)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("could not start Proton ({})", proton.display()))?;
    Ok(status.code().unwrap_or(-1))
}

// ---------- Steam Cloud ----------

#[derive(Debug, Clone)]
pub struct CloudInfo {
    pub files: usize,
    /// Unix time of the newest file Steam has uploaded.
    pub newest: Option<i64>,
}

impl CloudInfo {
    pub fn newest_text(&self) -> String {
        self.newest
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "unknown".into())
    }
}

/// What Steam Cloud holds for Cyberpunk, read from Steam's own cache (`remotecache.vdf`).
pub fn cloud_info(root: &Path) -> Option<CloudInfo> {
    let rd = fs::read_dir(root.join("userdata")).ok()?;
    let mut best: Option<CloudInfo> = None;
    for e in rd.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join(CYBERPUNK_APPID).join("remotecache.vdf")) else { continue };
        let Ok(doc) = vdf::parse(&text) else { continue };
        let Some(app) = doc.get(CYBERPUNK_APPID) else { continue };
        let files: Vec<&vdf::Value> = app.children().iter().map(|(_, v)| v).filter(|v| matches!(v, vdf::Value::Obj(_))).collect();
        let newest = files.iter().filter_map(|f| f.get_str("remotetime")?.parse::<i64>().ok()).filter(|&t| t > 0).max();
        let info = CloudInfo { files: files.len(), newest };
        if best.as_ref().map(|b| info.files > b.files).unwrap_or(true) {
            best = Some(info);
        }
    }
    best
}

// ---------- launch options ----------

#[derive(Debug, Clone)]
pub struct UserConfig {
    pub user_id: String,
    pub persona: Option<String>,
    pub path: PathBuf,
    pub launch_options: Option<String>,
}

fn persona_names(root: &Path) -> Vec<(String, String)> {
    // loginusers.vdf keys are 64-bit IDs; userdata folders use the low 32 bits.
    let mut out = Vec::new();
    let Ok(text) = fs::read_to_string(root.join("config").join("loginusers.vdf")) else { return out };
    let Ok(doc) = vdf::parse(&text) else { return out };
    if let Some(users) = doc.get("users") {
        for (id64, u) in users.children() {
            if let (Ok(id), Some(name)) = (vdf::unescape(id64).parse::<u64>(), u.get_str("PersonaName")) {
                out.push(((id & 0xFFFF_FFFF).to_string(), name));
            }
        }
    }
    out
}

pub fn user_configs(root: &Path) -> Vec<UserConfig> {
    let names = persona_names(root);
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(root.join("userdata")) else { return out };
    for e in rd.flatten() {
        let user_id = e.file_name().to_string_lossy().to_string();
        if user_id == "0" || !user_id.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let path = e.path().join("config").join("localconfig.vdf");
        if !path.is_file() {
            continue;
        }
        let launch_options = fs::read_to_string(&path)
            .ok()
            .and_then(|t| vdf::parse(&t).ok())
            .and_then(|d| {
                d.path(&["UserLocalConfigStore", "Software", "Valve", "Steam", "apps", CYBERPUNK_APPID, "LaunchOptions"])
                    .and_then(|v| v.str())
            });
        let persona = names.iter().find(|(id, _)| *id == user_id).map(|(_, n)| n.clone());
        out.push(UserConfig { user_id, persona, path, launch_options });
    }
    out
}

pub fn has_override(opts: &str) -> bool {
    opts.contains("WINEDLLOVERRIDES") && opts.contains("winmm")
}

/// Adds the DLL override to existing launch options without losing anything the user had.
pub fn merge_launch_options(existing: &str) -> String {
    let existing = existing.trim();
    if has_override(existing) {
        return existing.to_string();
    }
    if existing.is_empty() {
        return format!("{DLL_OVERRIDE} %command%");
    }
    if let Some(pos) = existing.find("WINEDLLOVERRIDES=\"") {
        // Already overriding other DLLs: add ours to the list.
        let insert_at = pos + "WINEDLLOVERRIDES=\"".len();
        let mut s = existing.to_string();
        s.insert_str(insert_at, "winmm,version=n,b;");
        return s;
    }
    if existing.contains("%command%") {
        format!("{DLL_OVERRIDE} {existing}")
    } else {
        // Plain arguments are appended to the game command by Steam.
        format!("{DLL_OVERRIDE} %command% {existing}")
    }
}

/// Removes our override, leaving any other options in place.
pub fn strip_launch_options(existing: &str) -> String {
    let s = existing
        .replace(&format!("{DLL_OVERRIDE} %command%"), "%command%")
        .replace(DLL_OVERRIDE, "")
        .replace("winmm,version=n,b;", "");
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s == "%command%" {
        String::new()
    } else {
        s
    }
}

/// Writes new launch options for every Steam user. Steam must be closed, or it overwrites the file on exit.
pub fn set_launch_options(root: &Path, transform: impl Fn(&str) -> String) -> Result<usize> {
    if steam_running() {
        bail!("Steam is still running; close it first");
    }
    let backups = crate::config::sub("backups").join("steam-localconfig");
    fs::create_dir_all(&backups)?;
    let mut changed = 0;
    for uc in user_configs(root) {
        let text = fs::read_to_string(&uc.path)?;
        let mut doc = vdf::parse(&text).with_context(|| format!("could not parse {}", uc.path.display()))?;
        let app = doc
            .obj_mut("UserLocalConfigStore")
            .obj_mut("Software")
            .obj_mut("Valve")
            .obj_mut("Steam")
            .obj_mut("apps")
            .obj_mut(CYBERPUNK_APPID);
        let current = app.get_str("LaunchOptions").unwrap_or_default();
        let new = transform(&current);
        if new == current {
            continue;
        }
        app.set_str("LaunchOptions", &new);
        fs::copy(&uc.path, backups.join(format!("localconfig-{}-{}.vdf", uc.user_id, crate::util::timestamp())))?;
        let tmp = uc.path.with_extension("vdf.nchelper");
        fs::write(&tmp, vdf::write(&doc))?;
        fs::rename(&tmp, &uc.path)?;
        changed += 1;
    }
    Ok(changed)
}

// ---------- processes ----------

fn processes_matching(pred: impl Fn(&str, &[String]) -> bool) -> bool {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::OnlyIfNotSet));
    sys.processes().values().any(|p| {
        let name = p.name().to_string_lossy().to_ascii_lowercase();
        let cmd: Vec<String> = p.cmd().iter().map(|c| c.to_string_lossy().to_ascii_lowercase()).collect();
        pred(&name, &cmd)
    })
}

pub fn steam_running() -> bool {
    processes_matching(|name, _| name == "steam" || name == "steam.exe")
}

pub fn game_running() -> bool {
    processes_matching(|name, cmd| {
        name.starts_with("cyberpunk2077") || cmd.first().map(|c| c.ends_with("cyberpunk2077.exe")).unwrap_or(false)
    })
}

fn steam_command(root: &Path) -> Command {
    if cfg!(windows) {
        Command::new(root.join("steam.exe"))
    } else if is_flatpak_root(root) {
        let mut c = Command::new("flatpak");
        c.args(["run", "com.valvesoftware.Steam"]);
        c
    } else {
        Command::new("steam")
    }
}

pub fn shutdown_steam(root: &Path) -> Result<()> {
    if !steam_running() {
        return Ok(());
    }
    if in_gaming_mode() {
        bail!("Steam can't be restarted from Gaming Mode. Switch to Desktop Mode first.");
    }
    steam_command(root)
        .arg("-shutdown")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("could not ask Steam to close: {e}"))?;
    let start = Instant::now();
    while steam_running() {
        if start.elapsed() > Duration::from_secs(60) {
            bail!("Steam did not close within a minute. Close it manually (Steam menu → Exit) and try again.");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // Give Steam a moment to finish writing its config files.
    std::thread::sleep(Duration::from_secs(2));
    Ok(())
}

pub fn start_steam(root: &Path) -> Result<()> {
    let mut cmd = steam_command(root);
    // Own process group, so closing the helper never takes Steam down with it.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("could not start Steam: {e}"))
}

pub fn launch_game() {
    crate::util::open_path(format!("steam://rungameid/{CYBERPUNK_APPID}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vc_runtime_detection() {
        let root = std::env::temp_dir().join(format!("nchelper-vc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let game = SteamGame { steam_root: root.clone(), library: root.clone(), game_dir: root.join("game"), build_id: None };
        assert_eq!(vc_runtime_ok(&game), None, "no prefix yet");
        let sys32 = game.compatdata().join("pfx/drive_c/windows/system32");
        fs::create_dir_all(&sys32).unwrap();
        // Wine's stand-ins carry this marker in their DOS stub.
        let mut wine = vec![0u8; 0x40];
        wine.extend_from_slice(b"Wine builtin DLL\0");
        for n in VC_DLLS {
            fs::write(sys32.join(n), &wine).unwrap();
        }
        assert_eq!(vc_runtime_ok(&game), Some(false));
        // Microsoft's own runtime (copied from this Windows machine when available).
        let real = std::path::Path::new("C:/Windows/System32");
        if VC_DLLS.iter().all(|n| real.join(n).is_file()) {
            for n in VC_DLLS {
                fs::copy(real.join(n), sys32.join(n)).unwrap();
            }
            assert_eq!(vc_dlls_ok(&game), Some(true));
            assert_eq!(vc_runtime_ok(&game), Some(false), "overrides not set yet");
            fs::write(
                game.compatdata().join("pfx/user.reg"),
                "WINE REGISTRY Version 2\n\n[Software\\\\Wine\\\\DllOverrides] 1700000000\n#time=1\n\"msvcp140\"=\"native,builtin\"\n\"vcruntime140\"=\"native,builtin\"\n\"vcruntime140_1\"=\"native,builtin\"\n\n[Software\\\\Wine\\\\Other] 1\n",
            )
            .unwrap();
            assert_eq!(vc_runtime_ok(&game), Some(true));
        }
        // The Proton build comes from config_info's paths (a folder holding the `proton` script).
        let proton = root.join("lib/steamapps/common/Proton 9.0 (Beta)");
        fs::create_dir_all(proton.join("files/share/fonts")).unwrap();
        fs::write(proton.join("proton"), b"#!/usr/bin/env python3").unwrap();
        fs::write(game.compatdata().join("config_info"), format!("9.0-302
{}/files/share/fonts/
", proton.display())).unwrap();
        assert_eq!(proton_dir(&game).as_deref(), Some(proton.as_path()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn launch_option_merging() {
        assert_eq!(merge_launch_options(""), "WINEDLLOVERRIDES=\"winmm,version=n,b\" %command%");
        assert_eq!(merge_launch_options("-skipStartScreen"), "WINEDLLOVERRIDES=\"winmm,version=n,b\" %command% -skipStartScreen");
        assert_eq!(merge_launch_options("gamemoderun %command%"), "WINEDLLOVERRIDES=\"winmm,version=n,b\" gamemoderun %command%");
        assert_eq!(merge_launch_options("WINEDLLOVERRIDES=\"dxgi=n,b\" %command%"), "WINEDLLOVERRIDES=\"winmm,version=n,b;dxgi=n,b\" %command%");
        let merged = merge_launch_options("-skipStartScreen");
        assert_eq!(merge_launch_options(&merged), merged);
        assert_eq!(strip_launch_options(&merge_launch_options("")), "");
        assert_eq!(strip_launch_options(&merge_launch_options("gamemoderun %command%")), "gamemoderun %command%");
        assert_eq!(strip_launch_options(&merge_launch_options("WINEDLLOVERRIDES=\"dxgi=n,b\" %command%")), "WINEDLLOVERRIDES=\"dxgi=n,b\" %command%");
    }
}
