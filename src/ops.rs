//! Long-running operations (run on a worker thread): installs, updates, backups, reports.

use crate::config::{self, InstallState, Settings};
use crate::game::Game;
use crate::remote::{self, GhRelease, NcRelease};
use crate::{steam, util};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

pub enum Event {
    Log(String),
    Progress(Option<f32>, String),
}

#[derive(Clone)]
pub struct Reporter {
    pub tx: Sender<Event>,
    pub ctx: eframe::egui::Context,
}

impl Reporter {
    pub fn log(&self, msg: impl Into<String>) {
        let _ = self.tx.send(Event::Log(msg.into()));
        self.ctx.request_repaint();
    }
    pub fn progress(&self, fraction: Option<f32>, label: impl Into<String>) {
        let _ = self.tx.send(Event::Progress(fraction, label.into()));
        self.ctx.request_repaint();
    }
    pub fn download(&self, url: &str, dest: &Path, sha256: Option<&str>, what: &str) -> Result<()> {
        self.log(format!("Downloading {what}…"));
        let mut last = 0u64;
        util::download(url, dest, sha256, &mut |done, total| {
            if done - last > 128 * 1024 || Some(done) == total {
                last = done;
                let frac = total.map(|t| done as f32 / t.max(1) as f32);
                self.progress(frac, format!("Downloading {what}: {}", util::human_size(done)));
            }
        })?;
        if sha256.is_some() {
            self.log("Checksum verified.");
        }
        Ok(())
    }
}

fn ensure_game_closed() -> Result<()> {
    if steam::game_running() {
        bail!("Cyberpunk 2077 is running. Quit the game first.");
    }
    Ok(())
}

fn staging(name: &str) -> PathBuf {
    config::sub("staging").join(name)
}

/// Keeps the download cache small: only the newest few archives stay.
fn prune_downloads(keep: usize) {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(config::sub("downloads"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in files.into_iter().skip(keep) {
        let _ = fs::remove_file(p);
    }
}

// ---------- mods off / on ----------

/// What "mods off" moves out of the game: RED4ext's loader and its whole folder (NightCoop included).
fn parkable(game: &Game) -> [(PathBuf, PathBuf); 2] {
    let park = game.park_dir();
    [
        (game.dir.join("bin").join("x64").join("winmm.dll"), park.join("bin").join("x64").join("winmm.dll")),
        (game.red4ext_dir(), park.join("red4ext")),
    ]
}

/// Moves a file or folder, falling back to copy + delete across filesystems.
fn move_path(from: &Path, to: &Path) -> Result<()> {
    if let Some(p) = to.parent() {
        fs::create_dir_all(p)?;
    }
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if from.is_dir() {
        util::copy_merge(from, to)?;
        fs::remove_dir_all(from)?;
    } else {
        fs::copy(from, to)?;
        fs::remove_file(from)?;
    }
    Ok(())
}

/// Turns every RED4ext mod off, leaving a vanilla game. Undo with [`mods_on`].
pub fn mods_off(game: &Game, rep: &Reporter) -> Result<()> {
    ensure_game_closed()?;
    let mut moved = 0;
    for (live, parked) in parkable(game) {
        if live.exists() {
            if parked.exists() {
                if parked.is_dir() { fs::remove_dir_all(&parked)? } else { fs::remove_file(&parked)? }
            }
            move_path(&live, &parked).with_context(|| format!("cannot move {}", live.display()))?;
            moved += 1;
        }
    }
    if moved == 0 {
        bail!("no mods are installed");
    }
    rep.log("Mods are OFF — Cyberpunk now starts unmodded. Turn them back on from Home any time.");
    Ok(())
}

pub fn mods_on(game: &Game, rep: &Reporter) -> Result<()> {
    ensure_game_closed()?;
    for (live, parked) in parkable(game) {
        if !parked.exists() {
            continue;
        }
        if live.exists() {
            // Something was reinstalled meanwhile; the newer live copy wins.
            rep.log(format!("{} already exists; keeping it.", live.display()));
            if parked.is_dir() { fs::remove_dir_all(&parked)? } else { fs::remove_file(&parked)? }
        } else {
            move_path(&parked, &live).with_context(|| format!("cannot restore {}", live.display()))?;
        }
    }
    let _ = fs::remove_dir_all(game.park_dir());
    rep.log("Mods are back ON.");
    Ok(())
}

/// Installs and uninstalls work on the live folders, so bring parked mods back first.
fn ensure_mods_on(game: &Game, rep: &Reporter) -> Result<()> {
    if game.park_dir().exists() {
        rep.log("Mods were switched off; turning them back on first.");
        mods_on(game, rep)?;
    }
    Ok(())
}

// ---------- saves ----------

pub fn backup_saves(game: &Game, settings: &Settings, state: &mut InstallState, rep: &Reporter, label: &str) -> Result<Option<PathBuf>> {
    let Some(saves) = game.saves_dir() else {
        rep.log("Saves folder not known for this install; skipping backup.");
        return Ok(None);
    };
    if !saves.is_dir() {
        rep.log("No saves folder yet (game never saved); nothing to back up.");
        return Ok(None);
    }
    rep.progress(None, "Backing up saves…");
    let dir = config::sub("backups").join("saves");
    fs::create_dir_all(&dir)?;
    let file = dir.join(format!("saves-{}{}.zip", util::timestamp(), if label.is_empty() { String::new() } else { format!("-{label}") }));
    let mut zip = zip::ZipWriter::new(fs::File::create(&file)?);
    let n = util::zip_dir(&mut zip, &saves, "saves")?;
    zip.finish()?;
    rep.log(format!("Backed up {n} save files → {}", file.display()));
    state.last_save_backup = Some(util::timestamp());
    prune_backups(settings.keep_backups);
    mirror_backup(&file, settings, rep);
    Ok(Some(file))
}

/// Copies a backup to the user's extra folder (SD card, USB stick, a synced cloud folder…).
fn mirror_backup(file: &Path, settings: &Settings, rep: &Reporter) {
    let dir = settings.backup_mirror.trim();
    if dir.is_empty() {
        return;
    }
    let dir = Path::new(dir);
    if !dir.is_dir() {
        rep.log(format!("Extra backup folder {} is not available (SD card or drive not connected?); skipped.", dir.display()));
        return;
    }
    match fs::copy(file, dir.join(file.file_name().unwrap_or_default())) {
        Ok(_) => rep.log(format!("Also copied to {}", dir.display())),
        Err(e) => rep.log(format!("Could not copy the backup to {}: {e}", dir.display())),
    }
}

pub fn list_backups() -> Vec<(PathBuf, u64)> {
    let dir = config::sub("backups").join("saves");
    let mut v: Vec<(PathBuf, u64)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "zip").unwrap_or(false))
        .map(|e| (e.path(), e.metadata().map(|m| m.len()).unwrap_or(0)))
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v
}

fn prune_backups(keep: usize) {
    if keep == 0 {
        return;
    }
    for (p, _) in list_backups().into_iter().skip(keep) {
        let _ = fs::remove_file(p);
    }
}

pub fn restore_saves(game: &Game, settings: &Settings, state: &mut InstallState, zip_path: &Path, rep: &Reporter) -> Result<()> {
    ensure_game_closed()?;
    let saves = game.saves_dir().ok_or_else(|| anyhow!("saves folder unknown"))?;
    // Unpack first: the safety backup below may prune old backups, including this one.
    let stage = staging("restore");
    util::extract_archive(zip_path, &stage)?;
    let src = stage.join("saves");
    if !src.is_dir() {
        bail!("this backup does not look like a NightCoop Helper save backup");
    }
    if saves.is_dir() {
        rep.log("Backing up current saves before restoring…");
        backup_saves(game, settings, state, rep, "before-restore")?;
    }
    // Swap folders so a failed copy never leaves the player without saves.
    let old = saves.with_file_name("Cyberpunk 2077.nchelper-old");
    if old.exists() {
        fs::remove_dir_all(&old)?;
    }
    if saves.is_dir() {
        fs::rename(&saves, &old)?;
    }
    let n = match util::copy_merge(&src, &saves) {
        Ok(files) => files.len(),
        Err(e) => {
            let _ = fs::remove_dir_all(&saves);
            let _ = fs::rename(&old, &saves);
            return Err(e.context("restore failed; your previous saves were put back"));
        }
    };
    let _ = fs::remove_dir_all(&old);
    let _ = fs::remove_dir_all(&stage);
    rep.log(format!("Restored {n} files into {}", saves.display()));
    Ok(())
}

// ---------- RED4ext ----------

/// Version of the RED4ext currently in the game, if both its files are present.
pub fn installed_red4ext(game: &Game) -> Option<String> {
    let loader = game.dir.join("bin").join("x64").join("winmm.dll");
    let dll = game.red4ext_dir().join("RED4ext.dll");
    (loader.is_file() && dll.is_file()).then(|| util::pe_version(&dll).unwrap_or_else(|| "unknown".into()))
}

pub fn install_red4ext_release(game: &Game, rel: &GhRelease, state: &mut InstallState, rep: &Reporter) -> Result<()> {
    let asset = remote::red4ext_asset(rel).ok_or_else(|| anyhow!("RED4ext {} has no zip download", rel.tag_name))?;
    let file = config::sub("downloads").join(&asset.name);
    rep.download(&asset.browser_download_url, &file, asset.sha256().as_deref(), &format!("RED4ext {}", rel.version()))?;
    install_red4ext_archive(game, &file, &rel.version(), state, rep)
}

pub fn install_red4ext_archive(game: &Game, archive: &Path, version: &str, state: &mut InstallState, rep: &Reporter) -> Result<()> {
    ensure_game_closed()?;
    ensure_mods_on(game, rep)?;
    rep.progress(None, "Installing RED4ext…");
    let stage = staging("red4ext");
    util::extract_archive(archive, &stage)?;
    if !stage.join("red4ext").join("RED4ext.dll").is_file() || !stage.join("bin").join("x64").join("winmm.dll").is_file() {
        bail!("this archive doesn't look like RED4ext (expected bin/x64/winmm.dll and red4ext/RED4ext.dll)");
    }
    let mut files = Vec::new();
    for top in ["bin", "red4ext"] {
        for rel in util::copy_merge(&stage.join(top), &game.dir.join(top))? {
            files.push(Path::new(top).join(rel).to_string_lossy().replace('\\', "/"));
        }
    }
    let _ = fs::remove_dir_all(&stage);
    state.game_dir = game.dir.display().to_string();
    state.red4ext_version = Some(version.to_string());
    state.red4ext_files = files;
    state.installed_at = Some(util::timestamp());
    state.game_build_id = game.steam.as_ref().and_then(|g| g.build_id.clone());
    prune_downloads(6);
    rep.log(format!("RED4ext {version} installed."));
    Ok(())
}

pub fn uninstall_red4ext(game: &Game, state: &mut InstallState, rep: &Reporter) -> Result<()> {
    ensure_game_closed()?;
    ensure_mods_on(game, rep)?;
    let mut files: Vec<String> = state.red4ext_files.clone();
    if files.is_empty() {
        files = ["bin/x64/winmm.dll", "red4ext/RED4ext.dll", "red4ext/LICENSE.txt", "red4ext/THIRD_PARTY_LICENSES.txt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
    }
    for f in &files {
        let p = game.dir.join(f);
        if p.is_file() {
            fs::remove_file(&p).with_context(|| format!("cannot remove {}", p.display()))?;
        }
    }
    // Remove the red4ext folder entirely only when no other plugins are left in it.
    let plugins = game.red4ext_dir().join("plugins");
    let has_plugins = fs::read_dir(&plugins).map(|mut rd| rd.next().is_some()).unwrap_or(false);
    if !has_plugins && !game.disabled_plugin_dir().exists() {
        let _ = fs::remove_dir_all(game.red4ext_dir());
    }
    state.red4ext_version = None;
    state.red4ext_files.clear();
    state.red4ext_custom = false;
    rep.log("RED4ext removed.");
    Ok(())
}

// ---------- NightCoop ----------

pub fn install_nightcoop_release(game: &Game, rel: &NcRelease, manifest_url: &str, state: &mut InstallState, rep: &Reporter) -> Result<()> {
    let file = rel.mod_file().ok_or_else(|| anyhow!("NightCoop {} has no download", rel.version))?;
    let dest = config::sub("downloads").join(format!("nightcoop-{}-{}", rel.version, file.file_name()));
    let mut last_err = None;
    for url in file.urls(manifest_url) {
        match rep.download(&url, &dest, file.sha256.as_deref(), &format!("NightCoop {}", rel.version)) {
            Ok(()) => {
                last_err = None;
                break;
            }
            Err(e) => {
                rep.log(format!("Download from {url} failed: {e:#}"));
                last_err = Some(e);
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e);
    }
    install_nightcoop_archive(game, &dest, Some(rel.version.clone()), file.sha256.clone(), state, rep)
}

/// Finds the `red4ext` folder inside an extracted archive, tolerating an extra wrapper folder.
fn find_red4ext_root(dir: &Path, depth: usize) -> Option<PathBuf> {
    let rd = fs::read_dir(dir).ok()?;
    let entries: Vec<_> = rd.flatten().collect();
    for e in &entries {
        if e.path().is_dir() && e.file_name().to_string_lossy().eq_ignore_ascii_case("red4ext") {
            return Some(dir.to_path_buf());
        }
    }
    if depth == 0 {
        return None;
    }
    entries.iter().filter(|e| e.path().is_dir()).find_map(|e| find_red4ext_root(&e.path(), depth - 1))
}

fn find_file(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    for e in fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_file() && e.file_name().to_string_lossy().eq_ignore_ascii_case(name) {
            return Some(p);
        }
    }
    if depth == 0 {
        return None;
    }
    fs::read_dir(dir).ok()?.flatten().filter(|e| e.path().is_dir()).find_map(|e| find_file(&e.path(), name, depth - 1))
}

/// Files inside the plugin folder that belong to the player, not the build.
const KEEP_FILES: [&str; 2] = ["servers.txt", "hub.txt"];

pub fn install_nightcoop_archive(
    game: &Game,
    archive: &Path,
    version: Option<String>,
    sha256: Option<String>,
    state: &mut InstallState,
    rep: &Reporter,
) -> Result<()> {
    ensure_game_closed()?;
    ensure_mods_on(game, rep)?;
    rep.progress(None, "Installing NightCoop…");
    let stage = staging("nightcoop");
    util::extract_archive(archive, &stage)?;

    // Work out where the plugin lives in the archive.
    let plugin_src: PathBuf;
    let mut extra_roots: Vec<PathBuf> = Vec::new();
    let mut bundle_root: Option<PathBuf> = None;
    if let Some(root) = find_red4ext_root(&stage, 3) {
        let dll = find_file(&root.join("red4ext"), "NightCoop.dll", 3).ok_or_else(|| anyhow!("NightCoop.dll not found in the archive's red4ext folder"))?;
        plugin_src = dll.parent().unwrap().to_path_buf();
        // Builds ship the RED4ext they were tested with (bin/x64/winmm.dll + red4ext/RED4ext.dll).
        if root.join("red4ext").join("RED4ext.dll").is_file() && root.join("bin").join("x64").join("winmm.dll").is_file() {
            bundle_root = Some(root.clone());
        }
        // Other game-root folders shipped next to red4ext (archive mods, scripts…).
        // `bin` is left out: the loader in it is handled with the bundled RED4ext.
        for name in ["archive", "r6", "engine", "mods"] {
            let p = root.join(name);
            if p.is_dir() {
                extra_roots.push(p);
            }
        }
    } else if let Some(dll) = find_file(&stage, "NightCoop.dll", 4) {
        plugin_src = dll.parent().unwrap().to_path_buf();
    } else {
        bail!("NightCoop.dll was not found in this archive");
    }

    let dest = game.plugin_dir();
    // Keep the player's server list across updates.
    let mut kept: Vec<(String, Vec<u8>)> = KEEP_FILES
        .iter()
        .filter_map(|n| fs::read(dest.join(n)).ok().map(|b| (n.to_string(), b)))
        .collect();
    // A disabled copy would be stale after this install.
    if game.disabled_plugin_dir().exists() {
        for n in KEEP_FILES {
            if let Ok(b) = fs::read(game.disabled_plugin_dir().join(n)) {
                if !kept.iter().any(|(k, _)| k == n) {
                    kept.push((n.to_string(), b));
                }
            }
        }
        fs::remove_dir_all(game.disabled_plugin_dir())?;
    }
    if dest.exists() {
        // Snapshot the old build so it can be rolled back.
        let old_ver = state.nightcoop_version.clone().unwrap_or_else(|| "unknown".into());
        let snap = config::sub("rollback").join(format!("{}__{}", util::timestamp(), sanitize(&old_ver)));
        util::copy_merge(&dest, &snap)?;
        prune_rollbacks(3);
        rep.log(format!("Previous build saved for rollback ({old_ver})."));
        // The docs say: replace the folder as a whole, leave no old files.
        fs::remove_dir_all(&dest).with_context(|| format!("cannot remove old {}", dest.display()))?;
    }
    let n = util::copy_merge(&plugin_src, &dest)?.len();
    for root in &extra_roots {
        let name = root.file_name().unwrap();
        util::copy_merge(root, &game.dir.join(name))?;
        rep.log(format!("Also installed the archive's {} folder.", name.to_string_lossy()));
    }
    for (name, bytes) in &kept {
        fs::write(dest.join(name), bytes)?;
    }
    if let Some(root) = &bundle_root {
        install_bundled_red4ext(game, root, state, rep)?;
    }
    let _ = fs::remove_dir_all(&stage);

    let dll = dest.join("NightCoop.dll");
    if !dll.is_file() {
        bail!("install finished but {} is missing", dll.display());
    }
    prune_downloads(6);
    let version = version.or_else(|| util::pe_version(&dll)).unwrap_or_else(|| "unknown".into());
    state.game_dir = game.dir.display().to_string();
    state.nightcoop_version = Some(version.clone());
    state.nightcoop_sha256 = sha256;
    state.nightcoop_dll_sha256 = util::sha256_file(&dll).ok();
    state.nightcoop_from_file = Some(archive.display().to_string());
    state.installed_at = Some(util::timestamp());
    state.game_build_id = game.steam.as_ref().and_then(|g| g.build_id.clone());
    rep.log(format!("NightCoop {version} installed ({n} files){}.", if kept.is_empty() { "" } else { ", your servers.txt was kept" }));
    Ok(())
}

/// Keeps RED4ext matched to the one shipped inside the NightCoop build (the tested pair),
/// unless the player chose their own version.
fn install_bundled_red4ext(game: &Game, root: &Path, state: &mut InstallState, rep: &Reporter) -> Result<()> {
    let bundled = util::pe_version(&root.join("red4ext").join("RED4ext.dll")).unwrap_or_else(|| "0".into());
    state.red4ext_bundled = Some(bundled.clone());
    let loader = game.dir.join("bin").join("x64").join("winmm.dll");
    let installed = installed_red4ext(game);
    if let Some(cur) = &installed {
        if state.red4ext_custom {
            rep.log(format!("Keeping your chosen RED4ext {cur} (the build ships {bundled})."));
            return Ok(());
        }
        if util::same_version(&bundled, cur) {
            rep.log(format!("RED4ext {cur} matches the build."));
            return Ok(());
        }
    }
    let mut files = Vec::new();
    fs::create_dir_all(loader.parent().unwrap())?;
    fs::copy(root.join("bin").join("x64").join("winmm.dll"), &loader).with_context(|| format!("cannot write {}", loader.display()))?;
    files.push("bin/x64/winmm.dll".to_string());
    for e in fs::read_dir(root.join("red4ext"))?.flatten() {
        if e.path().is_file() {
            let name = e.file_name();
            fs::copy(e.path(), game.red4ext_dir().join(&name))?;
            files.push(format!("red4ext/{}", name.to_string_lossy()));
        }
    }
    state.red4ext_version = Some(bundled.clone());
    state.red4ext_files = files;
    rep.log(format!("RED4ext {bundled} installed from the NightCoop build."));
    Ok(())
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect()
}

pub fn list_rollbacks() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(config::sub("rollback")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    v.sort();
    v.reverse();
    v
}

fn prune_rollbacks(keep: usize) {
    for p in list_rollbacks().into_iter().skip(keep) {
        let _ = fs::remove_dir_all(p);
    }
}

pub fn restore_rollback(game: &Game, snap: &Path, state: &mut InstallState, rep: &Reporter) -> Result<()> {
    ensure_game_closed()?;
    ensure_mods_on(game, rep)?;
    let dest = game.plugin_dir();
    let kept: Vec<(String, Vec<u8>)> = KEEP_FILES.iter().filter_map(|n| fs::read(dest.join(n)).ok().map(|b| (n.to_string(), b))).collect();
    if dest.exists() {
        fs::remove_dir_all(&dest)?;
    }
    util::copy_merge(snap, &dest)?;
    for (name, bytes) in &kept {
        fs::write(dest.join(name), bytes)?;
    }
    let ver = snap.file_name().map(|n| n.to_string_lossy().split_once("__").map(|(_, v)| v.to_string()).unwrap_or_default());
    state.nightcoop_version = ver.filter(|v| v != "unknown");
    state.nightcoop_dll_sha256 = util::sha256_file(&dest.join("NightCoop.dll")).ok();
    let _ = fs::remove_dir_all(snap);
    rep.log("Previous NightCoop build restored.");
    Ok(())
}

pub fn uninstall_nightcoop(game: &Game, state: &mut InstallState, rep: &Reporter) -> Result<()> {
    ensure_game_closed()?;
    ensure_mods_on(game, rep)?;
    for dir in [game.plugin_dir(), game.disabled_plugin_dir()] {
        if dir.exists() {
            if let Ok(b) = fs::read(dir.join("servers.txt")) {
                let keep = config::sub("backups").join("servers.txt");
                fs::write(&keep, b)?;
                rep.log(format!("Your servers.txt was saved to {}", keep.display()));
            }
            fs::remove_dir_all(&dir)?;
        }
    }
    state.nightcoop_version = None;
    state.nightcoop_dll_sha256 = None;
    rep.log("NightCoop removed. Your saves were not touched.");
    Ok(())
}

/// Moves the plugin out of RED4ext's plugin folder (solo play) or back.
pub fn set_nightcoop_enabled(game: &Game, enabled: bool, rep: &Reporter) -> Result<()> {
    ensure_game_closed()?;
    ensure_mods_on(game, rep)?;
    let (from, to) = if enabled { (game.disabled_plugin_dir(), game.plugin_dir()) } else { (game.plugin_dir(), game.disabled_plugin_dir()) };
    if !from.exists() {
        bail!("nothing to {}", if enabled { "enable" } else { "disable" });
    }
    if to.exists() {
        fs::remove_dir_all(&to)?;
    }
    fs::rename(&from, &to)?;
    rep.log(if enabled { "NightCoop enabled." } else { "NightCoop disabled — the game will start without co-op." });
    Ok(())
}

// ---------- Steam launch options ----------

pub fn apply_launch_option(game: &Game, enable: bool, restart_steam: bool, rep: &Reporter) -> Result<()> {
    let sg = game.steam.as_ref().ok_or_else(|| anyhow!("launch options only apply to the Steam version"))?;
    let was_running = steam::steam_running();
    if was_running {
        if !restart_steam {
            bail!("Steam is running; it has to be closed to change launch options");
        }
        rep.progress(None, "Closing Steam…");
        rep.log("Closing Steam (it would overwrite the change otherwise)…");
        steam::shutdown_steam(&sg.steam_root)?;
    }
    let n = if enable {
        steam::set_launch_options(&sg.steam_root, steam::merge_launch_options)?
    } else {
        steam::set_launch_options(&sg.steam_root, steam::strip_launch_options)?
    };
    rep.log(if n == 0 { "Launch options were already correct.".to_string() } else { format!("Launch options updated for {n} Steam account(s).") });
    if was_running {
        rep.log("Starting Steam again…");
        steam::start_steam(&sg.steam_root)?;
    }
    Ok(())
}

// ---------- bug report ----------

pub fn bug_report(game: Option<&Game>, diagnostics: &str, rep: &Reporter) -> Result<PathBuf> {
    rep.progress(None, "Collecting logs…");
    let dir = dirs::desktop_dir().filter(|d| d.is_dir()).unwrap_or_else(|| config::sub("reports"));
    let file = dir.join(format!("nightcoop-report-{}.zip", util::timestamp()));
    let mut zip = zip::ZipWriter::new(fs::File::create(&file)?);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("diagnostics.txt", opts)?;
    std::io::Write::write_all(&mut zip, diagnostics.as_bytes())?;
    if let Some(g) = game {
        for (dir, name) in [(g.red4ext_dir().join("logs"), "red4ext-logs"), (g.plugin_dir().join("logs"), "nightcoop-logs")] {
            if dir.is_dir() {
                let n = util::zip_dir(&mut zip, &dir, name)?;
                rep.log(format!("Added {n} files from {}", dir.display()));
            }
        }
    }
    zip.finish()?;
    rep.log(format!("Report saved: {}", file.display()));
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::Store;
    use std::io::Write as _;

    fn reporter() -> Reporter {
        let (tx, _rx) = std::sync::mpsc::channel();
        // Keep the receiver alive for the whole test so sends don't fail.
        std::mem::forget(_rx);
        Reporter { tx, ctx: eframe::egui::Context::default() }
    }

    fn make_zip(path: &Path, files: &[(&str, &[u8])]) {
        let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
        for (name, data) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn full_install_cycle() {
        let root = std::env::temp_dir().join(format!("nchelper-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let game_dir = root.join("Cyberpunk 2077");
        fs::create_dir_all(game_dir.join("bin/x64")).unwrap();
        fs::write(game_dir.join("bin/x64/Cyberpunk2077.exe"), b"MZ").unwrap();
        let game = Game { dir: game_dir.clone(), store: Store::Manual, steam: None };
        let rep = reporter();
        let mut state = InstallState::default();

        // RED4ext, using the same layout as the official zip.
        let r4 = root.join("red4ext-1.30.0.zip");
        make_zip(&r4, &[("bin/x64/winmm.dll", b"loader"), ("red4ext/RED4ext.dll", b"r4"), ("red4ext/LICENSE.txt", b"l")]);
        install_red4ext_archive(&game, &r4, "1.30.0", &mut state, &rep).unwrap();
        assert!(game_dir.join("bin/x64/winmm.dll").is_file());
        assert_eq!(state.red4ext_files.len(), 3);

        // NightCoop build wrapped in an extra folder, as people often zip things.
        let nc1 = root.join("nc1.zip");
        make_zip(&nc1, &[("NightCoop-0.1/red4ext/plugins/NightCoop/NightCoop.dll", b"v1"), ("NightCoop-0.1/red4ext/plugins/NightCoop/old.txt", b"x"), ("NightCoop-0.1/README.md", b"r")]);
        install_nightcoop_archive(&game, &nc1, Some("0.1.0".into()), None, &mut state, &rep).unwrap();
        assert_eq!(fs::read(game.plugin_dir().join("NightCoop.dll")).unwrap(), b"v1");
        assert!(!game_dir.join("README.md").exists());
        fs::write(game.servers_file(), b"hub.example.org#ABCD").unwrap();

        // Update: old files are gone, servers.txt survives, a rollback snapshot exists.
        let nc2 = root.join("nc2.7z.zip");
        make_zip(&nc2, &[("red4ext/plugins/NightCoop/NightCoop.dll", b"v2")]);
        install_nightcoop_archive(&game, &nc2, Some("0.2.0".into()), None, &mut state, &rep).unwrap();
        assert_eq!(fs::read(game.plugin_dir().join("NightCoop.dll")).unwrap(), b"v2");
        assert!(!game.plugin_dir().join("old.txt").exists());
        assert_eq!(fs::read(game.servers_file()).unwrap(), b"hub.example.org#ABCD");
        assert!(list_rollbacks().iter().any(|p| p.to_string_lossy().ends_with("0.1.0")), "rollback snapshot");

        // Status scan sees our version (DLL hash matches the record).
        let settings = Settings { game_path: game_dir.display().to_string(), ..Settings::default() };
        let st = crate::game::Status::scan(&settings, &state);
        assert!(st.nightcoop_installed);
        assert_eq!(st.nightcoop_version.as_deref(), Some("0.2.0"));
        assert!(st.loader_present);

        // Disable / enable.
        set_nightcoop_enabled(&game, false, &rep).unwrap();
        assert!(!game.plugin_dir().exists() && game.disabled_plugin_dir().join("NightCoop.dll").is_file());
        set_nightcoop_enabled(&game, true, &rep).unwrap();
        assert!(game.plugin_dir().join("NightCoop.dll").is_file());

        // Mods off: a vanilla game, versions still visible, and back on again.
        mods_off(&game, &rep).unwrap();
        assert!(!game_dir.join("bin/x64/winmm.dll").exists() && !game.red4ext_dir().exists());
        let st = crate::game::Status::scan(&settings, &state);
        assert!(st.mods_off && st.nightcoop_installed && st.loader_present && !st.ready());
        assert_eq!(st.nightcoop_version.as_deref(), Some("0.2.0"));
        mods_on(&game, &rep).unwrap();
        assert!(game_dir.join("bin/x64/winmm.dll").is_file() && game.plugin_dir().join("NightCoop.dll").is_file());
        assert!(!game.park_dir().exists());
        // Installing while mods are off brings them back first.
        mods_off(&game, &rep).unwrap();
        install_nightcoop_archive(&game, &nc2, Some("0.2.0".into()), None, &mut state, &rep).unwrap();
        assert!(!game.park_dir().exists() && game_dir.join("bin/x64/winmm.dll").is_file());
        assert_eq!(fs::read(game.servers_file()).unwrap(), b"hub.example.org#ABCD");
        let snap = list_rollbacks().into_iter().find(|p| p.to_string_lossy().ends_with("0.1.0")).expect("rollback snapshot");

        // Roll back to 0.1.0, keeping servers.txt.
        restore_rollback(&game, &snap, &mut state, &rep).unwrap();
        assert_eq!(fs::read(game.plugin_dir().join("NightCoop.dll")).unwrap(), b"v1");
        assert_eq!(state.nightcoop_version.as_deref(), Some("0.1.0"));
        assert_eq!(fs::read(game.servers_file()).unwrap(), b"hub.example.org#ABCD");

        // Uninstall everything.
        uninstall_nightcoop(&game, &mut state, &rep).unwrap();
        assert!(!game.plugin_dir().exists());
        uninstall_red4ext(&game, &mut state, &rep).unwrap();
        assert!(!game_dir.join("bin/x64/winmm.dll").exists());
        assert!(!game.red4ext_dir().exists());
        assert!(game_dir.join("bin/x64/Cyberpunk2077.exe").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[ignore = "downloads from GitHub"]
    fn real_red4ext_download() {
        let root = std::env::temp_dir().join(format!("nchelper-net-{}", std::process::id()));
        let game_dir = root.join("Cyberpunk 2077");
        fs::create_dir_all(game_dir.join("bin/x64")).unwrap();
        let game = Game { dir: game_dir.clone(), store: Store::Manual, steam: None };
        let releases = remote::github_releases(remote::RED4EXT_REPO).unwrap();
        let rel = releases.iter().find(|r| !r.prerelease && remote::red4ext_asset(r).is_some()).unwrap();
        assert!(remote::red4ext_asset(rel).unwrap().sha256().is_some(), "GitHub should publish a digest");
        let mut state = InstallState::default();
        install_red4ext_release(&game, rel, &mut state, &reporter()).unwrap();
        assert!(game_dir.join("red4ext/RED4ext.dll").is_file());
        assert_eq!(util::pe_version(&game_dir.join("red4ext/RED4ext.dll")).map(|v| util::same_version(&v, &rel.version())), Some(true));
        let m = remote::nightcoop_manifest(remote::NIGHTCOOP_MANIFEST).unwrap();
        println!("nightcoop releases: {}", m.releases.len());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[ignore = "downloads the real NightCoop build"]
    fn real_nightcoop_install() {
        let root = std::env::temp_dir().join(format!("nchelper-nc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let game_dir = root.join("Cyberpunk 2077");
        fs::create_dir_all(game_dir.join("bin/x64")).unwrap();
        fs::write(game_dir.join("bin/x64/Cyberpunk2077.exe"), b"MZ").unwrap();
        let game = Game { dir: game_dir.clone(), store: Store::Manual, steam: None };
        let m = remote::nightcoop_manifest(remote::NIGHTCOOP_MANIFEST).unwrap();
        // Only betas exist so far: the stable channel must still find the site's latest.
        let rel = m.latest_for(crate::config::Channel::Stable).expect("a build").clone();
        assert_eq!(m.latest_for(crate::config::Channel::Beta).unwrap().version, rel.version);
        println!("latest: {} game {:?} protocol {:?}", rel.version, rel.game, rel.protocol_str());
        let mut state = InstallState::default();
        install_nightcoop_release(&game, &rel, remote::NIGHTCOOP_MANIFEST, &mut state, &reporter()).unwrap();
        assert!(game.plugin_dir().join("NightCoop.dll").is_file());
        // The build bundles RED4ext; with none installed it must be put in place.
        assert!(game_dir.join("bin/x64/winmm.dll").is_file() && game.red4ext_dir().join("RED4ext.dll").is_file());
        assert!(state.red4ext_version.as_deref().map(|v| util::cmp_versions(v, "1.30").is_ge()).unwrap_or(false));
        let settings = Settings { game_path: game_dir.display().to_string(), ..Settings::default() };
        let st = crate::game::Status::scan(&settings, &state);
        assert_eq!(st.nightcoop_version.as_deref(), Some(rel.version.as_str()));
        assert!(st.loader_present && st.red4ext_version.is_some());
        assert_eq!(state.red4ext_bundled, state.red4ext_version);

        // "My own version": NightCoop updates must leave a chosen RED4ext alone…
        fs::write(game.red4ext_dir().join("RED4ext.dll"), b"custom").unwrap();
        state.red4ext_custom = true;
        install_nightcoop_release(&game, &rel, remote::NIGHTCOOP_MANIFEST, &mut state, &reporter()).unwrap();
        assert_eq!(fs::read(game.red4ext_dir().join("RED4ext.dll")).unwrap(), b"custom");
        // …and "Bundled" puts the build's own RED4ext back (from the cached, checksum-matched download).
        state.red4ext_custom = false;
        install_nightcoop_release(&game, &rel, remote::NIGHTCOOP_MANIFEST, &mut state, &reporter()).unwrap();
        assert_ne!(fs::read(game.red4ext_dir().join("RED4ext.dll")).unwrap(), b"custom");
        let _ = fs::remove_dir_all(&root);
    }
}
