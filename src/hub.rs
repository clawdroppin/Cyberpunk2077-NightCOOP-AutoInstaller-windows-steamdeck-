//! Hosting your own NightCoop hub: built from the official source right on this computer (Windows or
//! Linux) with a portable compiler, the setup script for a VPS, the server address, and pointing the
//! game at it.

use crate::game::Game;
use crate::ops::Reporter;
use crate::{config, util};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The one-command setup (published in this repo for `curl … | sudo bash` on a VPS).
#[cfg(test)]
const SETUP_SCRIPT: &str = include_str!("../scripts/nightcoop-hub-setup.sh");
pub const SETUP_URL: &str =
    "https://raw.githubusercontent.com/clawdroppin/Cyberpunk2077-NightCOOP-AutoInstaller-windows-steamdeck-/main/scripts/nightcoop-hub-setup.sh";
pub const OFFICIAL_GUIDE: &str = "https://github.com/Twenfys/nightcoop-hub/blob/HEAD/tools/hub/docker/README.md";
pub const PORT: u16 = 47800;
/// Threads for a home hub: 2 is plenty for friends and keeps the port list short (47810–47811).
pub const HOME_THREADS: u16 = 2;

/// Every UDP port the hub listens on: the entry port plus one per thread (port+10, port+11, …).
pub fn ports(threads: u16) -> Vec<u16> {
    std::iter::once(PORT).chain((0..threads).map(|i| PORT + 10 + i)).collect()
}

pub fn ports_text(threads: u16) -> String {
    let last = PORT + 10 + threads.saturating_sub(1);
    if threads <= 1 { format!("UDP {PORT} and {}", PORT + 10) } else { format!("UDP {PORT} and {}–{last}", PORT + 10) }
}

/// The command to paste on a server. `root` users have no `sudo` on some minimal images.
pub fn setup_command(as_root: bool) -> String {
    if as_root { format!("curl -fsSL {SETUP_URL} | bash") } else { format!("curl -fsSL {SETUP_URL} | sudo bash") }
}

/// A server address the game accepts: `host[:port]#FINGERPRINT`, the fingerprint required (it is what
/// stops an impostor server). Returns it tidied up.
pub fn parse_address(text: &str) -> Option<String> {
    let t = text.trim().trim_start_matches("NIGHTCOOP_HUB_ADDRESS=");
    let (host, fp) = t.split_once('#')?;
    (crate::invite::is_host(host) && crate::invite::is_fingerprint(fp)).then(|| format!("{}#{}", host.to_ascii_lowercase(), fp.to_ascii_uppercase()))
}

/// Finds the fingerprint (`XXXX-XXXX-XXXX-XXXX`) in the hub's output.
pub fn find_fingerprint(text: &str) -> Option<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).find_map(|w| {
        let groups: Vec<&str> = w.split('-').collect();
        (groups.len() == 4 && groups.iter().all(|g| g.len() == 4 && g.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))).then(|| w.to_string())
    })
}

/// Points this player's game at the server: NightCoop reads `hub.txt` next to NightCoop.dll on its next
/// start, adds the address to its own servers and selects it (documented in the official hub guide).
pub fn use_in_game(game: &Game, address: &str) -> Result<()> {
    let dir = game.active_plugin_dir();
    if !dir.join("NightCoop.dll").is_file() {
        bail!("install NightCoop first");
    }
    fs::write(dir.join("hub.txt"), format!("{address}\n")).context("couldn't write hub.txt")?;
    Ok(())
}

/// Opens a terminal that logs in to the server over SSH and runs the setup script there.
/// The password (or key passphrase) is typed in that window — the helper never sees it.
pub fn open_ssh_setup(user: &str, host: &str) -> Result<()> {
    let user = user.trim();
    let host = host.trim();
    if user.is_empty() || host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c)) || !user.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        bail!("enter the server's address (e.g. 203.0.113.7) and user name (usually root)");
    }
    let remote = setup_command(user == "root");
    let target = format!("{user}@{host}");
    #[cfg(windows)]
    {
        Command::new("cmd")
            .args(["/c", "start", "NightCoop server setup", "cmd", "/k", "ssh", "-t", "-o", "StrictHostKeyChecking=accept-new", &target, &remote])
            .spawn()
            .context("couldn't open a terminal")?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let ssh = format!("ssh -t -o StrictHostKeyChecking=accept-new {target} '{remote}'; echo; read -p 'Press Enter to close' _");
        for (term, args) in [("konsole", vec!["--hold", "-e", "bash", "-c"]), ("gnome-terminal", vec!["--", "bash", "-c"]), ("xterm", vec!["-hold", "-e", "bash", "-c"]), ("x-terminal-emulator", vec!["-e", "bash", "-c"])] {
            let mut cmd = Command::new(term);
            cmd.args(&args).arg(&ssh);
            if cmd.spawn().is_ok() {
                return Ok(());
            }
        }
        bail!("couldn't open a terminal; run this yourself: ssh {target} '{remote}'")
    }
}

/// A command that never flashes a console window on Windows.
fn hidden(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// The last lines of a log file, for the UI.
pub fn log_tail(path: &Path, lines: usize) -> Vec<String> {
    let text = fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let all: Vec<String> = text.lines().map(|l| l.chars().take(200).collect()).collect();
    all[all.len().saturating_sub(lines)..].to_vec()
}

// ---------- the hub on this computer ----------

/// Zig: a whole C/C++ compiler in one download, no installer. Taken from its PyPI wheels (plain zip
/// files, pinned by checksum) — the same build this project uses for its own Linux release.
#[cfg(windows)]
const ZIG: (&str, &str) = (
    "https://files.pythonhosted.org/packages/a6/3c/baff40b3fc8ab4e83530246a52e8f3a5186c3606562712dfb58483b04f79/ziglang-0.16.0-py3-none-win_amd64.whl",
    "089a16a4eb5a2f45151993342f8fabad24ff1a0723dc146642895c29208a3939",
);
#[cfg(not(windows))]
const ZIG: (&str, &str) = (
    "https://files.pythonhosted.org/packages/3e/ed/7b79023aa27ceb5d461ecf761181e7c33c57bbc1a6256a39535d1c7083d2/ziglang-0.16.0-py3-none-manylinux_2_12_x86_64.manylinux2010_x86_64.musllinux_1_1_x86_64.whl",
    "9fcda73f62b851dd72a54b710ad40a209896db14cfb13649e62191243556342b",
);
const ZIG_VERSION: &str = "0.16.0";
/// The official hub source (its main branch: running Build again updates it).
const HUB_SOURCE: &str = "https://codeload.github.com/Twenfys/nightcoop-hub/zip/refs/heads/main";
/// ENet commit the mod is built with; the hub's Dockerfile names the current one.
const ENET_FALLBACK: &str = "5a9c537fd464b3c6d3c55e1d3bd47588faf71b42";

pub fn hub_dir() -> PathBuf {
    config::sub("hub")
}

pub fn exe() -> PathBuf {
    hub_dir().join(if cfg!(windows) { "nightcoop_hub.exe" } else { "nightcoop_hub" })
}

/// The hub's secret key: its identity, which the fingerprint comes from.
pub fn key_file() -> PathBuf {
    hub_dir().join("hub-sign.key")
}

pub fn log_file() -> PathBuf {
    config::sub("logs").join("hub.log")
}

pub fn built() -> bool {
    exe().is_file()
}

/// "source commit · build date" of the installed hub.
pub fn build_info() -> Option<String> {
    fs::read_to_string(hub_dir().join("build.txt")).ok().map(|s| s.trim().to_string())
}

fn zig(rep: &Reporter) -> Result<PathBuf> {
    let root = config::sub("tools").join(format!("zig-{ZIG_VERSION}"));
    let bin = root.join("ziglang").join(if cfg!(windows) { "zig.exe" } else { "zig" });
    if bin.is_file() {
        return Ok(bin);
    }
    let wheel = config::sub("downloads").join(format!("zig-{ZIG_VERSION}.zip"));
    rep.download(ZIG.0, &wheel, Some(ZIG.1), "the compiler (zig, about 100 MB, once)")?;
    rep.progress(None, "Unpacking the compiler…");
    let tmp = root.with_extension("part");
    util::extract_archive(&wheel, &tmp)?;
    if root.exists() {
        fs::remove_dir_all(&root)?;
    }
    fs::rename(&tmp, &root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))?;
    }
    let _ = fs::remove_file(&wheel);
    Ok(bin)
}

/// Downloads a GitHub zip and unpacks it; returns the single top folder inside and the commit
/// GitHub writes into the zip's comment.
fn fetch_source(rep: &Reporter, url: &str, name: &str, what: &str) -> Result<(PathBuf, Option<String>)> {
    let staging = config::sub("staging").join(format!("hub-{name}"));
    let zip_path = config::sub("downloads").join(format!("{name}.zip"));
    let _ = fs::remove_file(&zip_path);
    rep.download(url, &zip_path, None, what)?;
    let commit = zip::ZipArchive::new(fs::File::open(&zip_path)?)
        .ok()
        .map(|z| String::from_utf8_lossy(z.comment()).trim().to_string())
        .filter(|c| c.len() >= 7 && c.chars().all(|ch| ch.is_ascii_hexdigit()));
    util::extract_archive(&zip_path, &staging)?;
    let top = fs::read_dir(&staging)?.flatten().map(|e| e.path()).find(|p| p.is_dir()).context("the source download was empty")?;
    Ok((top, commit))
}

fn compile(zig: &Path, cache: &Path, args: &[&str], what: &str) -> Result<()> {
    let out = hidden(zig)
        .args(args)
        .env("ZIG_GLOBAL_CACHE_DIR", cache)
        .env("ZIG_LOCAL_CACHE_DIR", cache)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("couldn't run the compiler for {what}"))?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stderr);
        let errors: Vec<&str> = text.lines().filter(|l| l.contains("error")).take(8).collect();
        bail!("compiling {what} failed:\n{}", if errors.is_empty() { text.lines().rev().take(8).collect::<Vec<_>>().join("\n") } else { errors.join("\n") });
    }
    Ok(())
}

/// Downloads the official hub source, ENet and the compiler, and builds the hub into the helper's
/// data folder. Running it again updates to the newest source; the key (and fingerprint) stay.
pub fn build(rep: &Reporter) -> Result<String> {
    if running() {
        bail!("stop the server first");
    }
    let zig = zig(rep)?;
    let (src, commit) = fetch_source(rep, HUB_SOURCE, "nightcoop-hub", "the official hub source")?;
    let dockerfile = fs::read_to_string(src.join("tools/hub/docker/Dockerfile")).unwrap_or_default();
    let enet_commit = dockerfile
        .lines()
        .find_map(|l| l.trim().strip_prefix("ARG ENET_COMMIT="))
        .map(|c| c.trim().to_string())
        .filter(|c| c.len() == 40 && c.chars().all(|ch| ch.is_ascii_hexdigit()))
        .unwrap_or_else(|| ENET_FALLBACK.to_string());
    let (enet, _) = fetch_source(rep, &format!("https://codeload.github.com/lsalzman/enet/zip/{enet_commit}"), "enet", "ENet (the network library)")?;

    let cache = config::sub("tools").join("zig-cache");
    let obj = config::sub("staging").join("hub-obj");
    let _ = fs::remove_dir_all(&obj);
    fs::create_dir_all(&obj)?;
    let enet_inc = format!("-I{}", enet.join("include").display());
    let target: &[&str] = if cfg!(windows) { &["-target", "x86_64-windows-gnu"] } else { &[] };
    let enet_files: &[&str] = if cfg!(windows) {
        &["callbacks", "compress", "host", "list", "packet", "peer", "protocol", "win32"]
    } else {
        &["callbacks", "compress", "host", "list", "packet", "peer", "protocol", "unix"]
    };
    // The same flags as the official Dockerfile.
    let unix_defines = [
        "-DHAS_FCNTL=1", "-DHAS_POLL=1", "-DHAS_SOCKLEN_T=1", "-DHAS_MSGHDR_FLAGS=1", "-DHAS_GETADDRINFO=1", "-DHAS_GETNAMEINFO=1",
        "-DHAS_INET_PTON=1", "-DHAS_INET_NTOP=1", "-DHAS_GETHOSTBYNAME_R=1", "-DHAS_GETHOSTBYADDR_R=1",
    ];
    let mut objects = Vec::new();
    let total = enet_files.len() + 5;
    let mut step = 0;
    let mut tick = |what: &str| {
        step += 1;
        rep.progress(Some(step as f32 / total as f32), format!("Building the hub: {what} (the first build takes a few minutes)…"));
    };
    for f in enet_files {
        tick(&format!("ENet {f}"));
        let o = obj.join(format!("enet_{f}.o")).display().to_string();
        let c = enet.join(format!("{f}.c")).display().to_string();
        let mut args = vec!["cc"];
        args.extend_from_slice(target);
        args.extend(["-c", "-O2", &enet_inc]);
        if !cfg!(windows) {
            args.extend(unix_defines);
        }
        args.extend([c.as_str(), "-o", o.as_str()]);
        compile(&zig, &cache, &args, &format!("ENet ({f})"))?;
        objects.push(o);
    }
    tick("Monocypher");
    let mono_o = obj.join("monocypher.o").display().to_string();
    let mono_c = src.join("vendor/monocypher/monocypher.c").display().to_string();
    let mut args = vec!["cc"];
    args.extend_from_slice(target);
    args.extend(["-c", "-O2", mono_c.as_str(), "-o", mono_o.as_str()]);
    compile(&zig, &cache, &args, "Monocypher")?;
    objects.push(mono_o);
    for f in ["src/Log.cpp", "src/NetCrypto.cpp", "src/HubServer.cpp", "tools/hub/main.cpp"] {
        let name = Path::new(f).file_stem().unwrap_or_default().to_string_lossy().to_string();
        tick(&name);
        let o = obj.join(format!("{name}.o")).display().to_string();
        let c = src.join(f).display().to_string();
        let mut args = vec!["c++"];
        args.extend_from_slice(target);
        args.extend(["-c", "-std=c++20", "-O2", "-w", &enet_inc, c.as_str(), "-o", o.as_str()]);
        compile(&zig, &cache, &args, &name)?;
        objects.push(o);
    }
    tick("linking");
    let out = hub_dir().join(if cfg!(windows) { "nightcoop_hub.new.exe" } else { "nightcoop_hub.new" });
    let out_s = out.display().to_string();
    let mut args = vec!["c++"];
    args.extend_from_slice(target);
    args.extend(["-o", out_s.as_str()]);
    args.extend(objects.iter().map(String::as_str));
    if cfg!(windows) {
        args.extend(["-lws2_32", "-lwinmm", "-lbcrypt"]);
    } else {
        args.push("-pthread");
    }
    compile(&zig, &cache, &args, "the hub")?;

    // The official smoke test: make a throwaway key and read its public half back.
    let probe = config::sub("staging").join("hub-smoke.key");
    let _ = fs::remove_file(&probe);
    let ok = hidden(&out).arg("--gen-sign-key").arg(&probe).output().map(|o| o.status.success()).unwrap_or(false)
        && hidden(&out).arg("--hub-public").arg(&probe).output().map(|o| String::from_utf8_lossy(&o.stdout).contains("nightcoop-hub-public 1 ")).unwrap_or(false);
    let _ = fs::remove_file(&probe);
    if !ok {
        let _ = fs::remove_file(&out);
        bail!("the hub was built but doesn't run on this computer");
    }
    fs::rename(&out, exe())?;
    if !key_file().is_file() {
        let backup = key_backup();
        if backup.is_file() {
            fs::copy(&backup, key_file())?;
            rep.log("Restored the server's key from the backup — the fingerprint stays the same.");
        } else {
            hidden(exe()).arg("--gen-sign-key").arg(key_file()).output().context("couldn't create the server's key")?;
            let _ = fs::copy(key_file(), &backup);
        }
    }
    let info = format!(
        "source {} · built {}",
        commit.as_deref().map(|c| &c[..c.len().min(7)]).unwrap_or("main"),
        chrono::Local::now().format("%Y-%m-%d")
    );
    let _ = fs::write(hub_dir().join("build.txt"), &info);
    if let Some(c) = &commit {
        let _ = fs::write(hub_dir().join("commit.txt"), c);
    }
    let _ = fs::remove_dir_all(config::sub("staging").join("hub-nightcoop-hub"));
    let _ = fs::remove_dir_all(config::sub("staging").join("hub-enet"));
    let _ = fs::remove_dir_all(&obj);
    Ok(info)
}

/// The newest commit of the official hub source.
pub fn latest_commit() -> Result<String> {
    let mut resp = util::agent()
        .get("https://api.github.com/repos/Twenfys/nightcoop-hub/commits/main")
        .header("Accept", "application/vnd.github.sha")
        .call()
        .context("couldn't reach GitHub")?;
    let sha = resp.body_mut().read_to_string()?.trim().to_string();
    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("unexpected reply from GitHub");
    }
    Ok(sha)
}

/// The source commit the installed hub was built from.
pub fn installed_commit() -> Option<String> {
    fs::read_to_string(hub_dir().join("commit.txt")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The official source has changed since this hub was built.
pub fn update_available(latest: &str) -> bool {
    built() && installed_commit().is_none_or(|c| !c.eq_ignore_ascii_case(latest))
}

/// Rebuilds from the newest source; a running hub is restarted (sessions resume within 90 s).
pub fn update(rep: &Reporter, name: &str) -> Result<String> {
    let was_running = running();
    if was_running {
        rep.log("Stopping the server for the update…");
        stop()?;
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let info = build(rep)?;
    if was_running {
        start(name)?;
        rep.log("Server running again.");
    }
    Ok(info)
}

fn key_backup() -> PathBuf {
    config::sub("backups").join("hub-sign.key")
}

/// Our hub processes (matched by their executable, so another copy elsewhere is left alone).
fn hub_pids() -> Vec<sysinfo::Pid> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let want = exe();
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet));
    sys.processes()
        .iter()
        .filter(|(_, p)| p.name().to_string_lossy().to_ascii_lowercase().starts_with("nightcoop_hub") && p.exe().map(|e| same_file(e, &want)).unwrap_or(false))
        .map(|(pid, _)| *pid)
        .collect()
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

pub fn running() -> bool {
    !hub_pids().is_empty()
}

/// Starts the hub in the background. It keeps running after the helper closes, until "Stop" or shutdown.
pub fn start(name: &str) -> Result<()> {
    if running() {
        return Ok(());
    }
    if !built() {
        bail!("build the server first");
    }
    let log = fs::File::create(log_file())?;
    let mut cmd = Command::new(exe());
    cmd.args([&PORT.to_string(), "--shards", &HOME_THREADS.to_string(), "--max-clients", "512", "--hub-name", name, "--region", "home", "--sign-key-auto"])
        .arg(key_file())
        .current_dir(hub_dir())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000 | 0x0000_0200); // no window, own process group
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().context("couldn't start the hub")?;
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(300));
        if let Some(status) = child.try_wait()? {
            bail!("the hub stopped right away ({status}). Its log: {}", log_tail(&log_file(), 3).join(" / "));
        }
    }
    let _ = fs::copy(key_file(), key_backup());
    Ok(())
}

pub fn stop() -> Result<()> {
    use sysinfo::{ProcessesToUpdate, System};
    let pids = hub_pids();
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&pids), true);
    for pid in &pids {
        if let Some(p) = sys.process(*pid) {
            p.kill();
        }
    }
    Ok(())
}

pub fn fingerprint() -> Option<String> {
    if !built() || !key_file().is_file() {
        return None;
    }
    let out = hidden(exe()).arg("--hub-public").arg(key_file()).output().ok()?;
    find_fingerprint(&String::from_utf8_lossy(&out.stdout))
}

// ---------- start with the computer ----------

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const AUTOSTART_NAME: &str = "NightCoopHub";

/// The helper starts the hub (and nothing else) when the computer starts.
pub fn autostart() -> bool {
    #[cfg(windows)]
    {
        winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .open_subkey(RUN_KEY)
            .and_then(|k| k.get_value::<String, _>(AUTOSTART_NAME))
            .is_ok()
    }
    #[cfg(not(windows))]
    {
        autostart_file().is_some_and(|f| f.is_file())
    }
}

#[cfg(not(windows))]
fn autostart_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("autostart").join("nightcoop-hub.desktop"))
}

pub fn set_autostart(on: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    #[cfg(windows)]
    {
        let (key, _) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER).create_subkey(RUN_KEY)?;
        if on {
            key.set_value(AUTOSTART_NAME, &format!("\"{}\" --start-hub", exe.display()))?;
        } else {
            let _ = key.delete_value(AUTOSTART_NAME);
        }
    }
    #[cfg(not(windows))]
    {
        let file = autostart_file().context("no config folder")?;
        if on {
            fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))?;
            fs::write(&file, format!("[Desktop Entry]\nType=Application\nName=NightCoop hub\nExec=\"{}\" --start-hub\nX-GNOME-Autostart-enabled=true\n", exe.display()))?;
        } else {
            let _ = fs::remove_file(&file);
        }
    }
    Ok(())
}

/// The computer's name, tidied up for the hub's name.
pub fn computer_name() -> String {
    let n = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "home".into());
    let n: String = n.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(30).collect::<String>().to_ascii_lowercase();
    if n.is_empty() { "home".into() } else { n }
}

// ---------- Windows Firewall ----------

#[cfg(windows)]
pub mod firewall {
    use super::*;

    const RULE: &str = "NightCoop hub (UDP)";

    pub fn ok() -> bool {
        hidden("netsh").args(["advfirewall", "firewall", "show", "rule", &format!("name={RULE}")]).stdout(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
    }

    /// Lets the hub's UDP ports in. One administrator prompt.
    pub fn open(threads: u16) -> Result<()> {
        let ports = ports(threads).iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
        let script = format!(
            "Remove-NetFirewallRule -DisplayName '{RULE}' -ErrorAction SilentlyContinue\n\
             New-NetFirewallRule -DisplayName '{RULE}' -Direction Inbound -Protocol UDP -LocalPort {ports} -Action Allow -Profile Any | Out-Null"
        );
        if !elevated(&script)? {
            bail!("the administrator prompt was declined");
        }
        Ok(())
    }

    /// Runs a PowerShell script elevated (one UAC prompt) and waits. Returns false if it was declined.
    fn elevated(script: &str) -> Result<bool> {
        let dir = config::sub("staging");
        let file = dir.join("nightcoop-admin.ps1");
        let done = dir.join("nightcoop-admin.done");
        let _ = fs::remove_file(&done);
        fs::write(&file, format!("{script}\nSet-Content -LiteralPath '{}' -Value ok\n", done.display()))?;
        let launcher = format!(
            "try {{ Start-Process -FilePath powershell.exe -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','\"{}\"' -Verb RunAs -Wait -WindowStyle Hidden }} catch {{ exit 1 }}",
            file.display()
        );
        let status = hidden("powershell.exe").args(["-NoProfile", "-Command", &launcher]).status()?;
        Ok(status.success() && done.is_file())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_and_ports() {
        assert_eq!(parse_address(" NIGHTCOOP_HUB_ADDRESS=203.0.113.7#k3qf-9x2m-tv7a-0d4r "), Some("203.0.113.7#K3QF-9X2M-TV7A-0D4R".into()));
        assert_eq!(parse_address("hub.example.org:47900#K3QF-9X2M-TV7A-0D4R"), Some("hub.example.org:47900#K3QF-9X2M-TV7A-0D4R".into()));
        assert_eq!(parse_address("hub.example.org"), None, "a fingerprint is required");
        assert_eq!(ports(2), vec![47800, 47810, 47811]);
        assert_eq!(ports_text(2), "UDP 47800 and 47810–47811");
        assert_eq!(
            find_fingerprint("hub: the fingerprint of this hub's key is K3QF-9X2M-TV7A-0D4R — players …").as_deref(),
            Some("K3QF-9X2M-TV7A-0D4R")
        );
        assert!(SETUP_SCRIPT.contains("NIGHTCOOP_HUB_ADDRESS="));
    }

    /// Downloads zig and the official source, builds the hub, runs it, reads its fingerprint, stops it.
    #[test]
    #[ignore]
    fn real_native_hub() {
        let (tx, rx) = std::sync::mpsc::channel();
        let rep = Reporter { tx, ctx: eframe::egui::Context::default() };
        let printer = std::thread::spawn(move || {
            for ev in rx {
                if let crate::ops::Event::Log(l) = ev {
                    println!("{l}");
                }
            }
        });
        let latest = latest_commit().unwrap();
        let info = build(&rep).unwrap();
        println!("{info}");
        assert!(built() && !update_available(&latest), "built from the newest source");
        start("helper-test").unwrap();
        assert!(running());
        let fp = fingerprint().unwrap();
        println!("fingerprint {fp}");
        assert!(log_tail(&log_file(), 40).iter().any(|l| l.contains(&fp)));
        stop().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(800));
        assert!(!running());
        drop(rep);
        printer.join().unwrap();
    }
}
