// The WSL half only exists on Windows; its helpers are unused elsewhere.
#![cfg_attr(not(windows), allow(dead_code))]
//! Hosting your own NightCoop hub: the setup script (VPS / Linux PC / WSL), the server address,
//! pointing the game at it, and — on Windows — running it inside WSL2.

use crate::game::Game;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;
#[cfg(windows)]
use std::process::Stdio;

/// The one-command setup (also published in this repo for `curl … | sudo bash` on a VPS).
pub const SETUP_SCRIPT: &str = include_str!("../scripts/nightcoop-hub-setup.sh");
pub const SETUP_URL: &str =
    "https://raw.githubusercontent.com/clawdroppin/Cyberpunk2077-NightCOOP-AutoInstaller-windows-steamdeck-/main/scripts/nightcoop-hub-setup.sh";
pub const OFFICIAL_GUIDE: &str = "https://github.com/Twenfys/nightcoop-hub/blob/HEAD/tools/hub/docker/README.md";
pub const PORT: u16 = 47800;
/// Threads for a home/WSL hub: 2 is plenty for friends and keeps the port list short (47810–47811).
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
    let dir = game.plugin_dir();
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

// ---------- WSL2 (Windows) ----------

#[cfg(windows)]
pub mod wsl {
    use super::*;
    use std::io::Write;
    use std::os::windows::process::CommandExt;

    pub const DISTRO: &str = "Ubuntu-24.04";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const KEY: &str = "/var/lib/nightcoop/hub-sign.key";
    const BIN: &str = "/usr/local/bin/nightcoop_hub";
    const FW_RULE: &str = "NightCoop hub (UDP)";

    fn hidden(program: &str) -> Command {
        let mut c = Command::new(program);
        c.creation_flags(CREATE_NO_WINDOW);
        c
    }

    /// `wsl.exe` prints UTF-16 for its own messages.
    fn decode(bytes: &[u8]) -> String {
        if bytes.len() >= 2 && bytes.iter().skip(1).step_by(2).filter(|b| **b == 0).count() * 2 >= bytes.len() / 2 {
            let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units).replace('\u{feff}', "")
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        }
    }

    /// Registered Linux distributions, or `None` when WSL itself isn't installed.
    pub fn distros() -> Option<Vec<String>> {
        let out = hidden("wsl.exe").args(["-l", "-q"]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(decode(&out.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
    }

    pub fn distro_ready() -> bool {
        distros().map(|d| d.iter().any(|x| x.eq_ignore_ascii_case(DISTRO))).unwrap_or(false)
            && root(&["true"]).map(|(code, _)| code == 0).unwrap_or(false)
    }

    /// Runs a command in the distro as root (no password needed), returning (exit code, output).
    pub fn root(args: &[&str]) -> Result<(i32, String)> {
        let out = hidden("wsl.exe").args(["-d", DISTRO, "-u", "root", "--"]).args(args).output().context("couldn't run wsl.exe")?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), decode(&out.stderr));
        Ok((out.status.code().unwrap_or(-1), text))
    }

    /// Runs a PowerShell script elevated (one UAC prompt) and waits. Returns false if it was declined.
    fn elevated(script: &str) -> Result<bool> {
        let dir = crate::config::sub("staging");
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

    /// Installs WSL with Ubuntu 24.04 (admin; Windows may want a restart afterwards).
    pub fn install() -> Result<()> {
        let log = crate::config::sub("staging").join("wsl-install.log");
        let script = format!("wsl.exe --install -d {DISTRO} --no-launch *> '{}'", log.display());
        if !elevated(&script)? {
            bail!("the administrator prompt was declined");
        }
        Ok(())
    }

    /// Opens Ubuntu once in a window, for the first-start questions (a user name and password).
    pub fn open_first_start() -> Result<()> {
        Command::new("cmd").args(["/c", "start", "Ubuntu first start", "wsl.exe", "-d", DISTRO]).spawn()?;
        Ok(())
    }

    fn wslconfig() -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|h| h.join(".wslconfig"))
    }

    /// Mirrored networking: Linux shares Windows' network, so the hub sees players' real addresses
    /// and the router's forwarded ports reach it (needs Windows 11 22H2+).
    pub fn mirrored() -> bool {
        let Some(path) = wslconfig() else { return false };
        let text = fs::read_to_string(path).unwrap_or_default();
        let mut in_wsl2 = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_wsl2 = line.eq_ignore_ascii_case("[wsl2]");
            } else if in_wsl2 {
                if let Some((k, v)) = line.split_once('=') {
                    if k.trim().eq_ignore_ascii_case("networkingMode") {
                        return v.trim().eq_ignore_ascii_case("mirrored");
                    }
                }
            }
        }
        false
    }

    /// Sets `networkingMode=mirrored` under `[wsl2]`, keeping everything else, then restarts WSL.
    pub fn set_mirrored() -> Result<()> {
        let path = wslconfig().context("no home folder")?;
        let text = fs::read_to_string(&path).unwrap_or_default();
        if path.is_file() {
            let _ = fs::copy(&path, crate::config::sub("backups").join(format!("wslconfig-{}.bak", crate::util::timestamp())));
        }
        fs::write(&path, set_ini_key(&text, "wsl2", "networkingMode", "mirrored"))?;
        let _ = hidden("wsl.exe").arg("--shutdown").status();
        Ok(())
    }

    pub fn firewall_ok() -> bool {
        hidden("netsh").args(["advfirewall", "firewall", "show", "rule", &format!("name={FW_RULE}")]).stdout(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
    }

    /// Windows Firewall + WSL's own (Hyper-V) firewall: let the hub's UDP ports in. One admin prompt.
    pub fn open_firewall(threads: u16) -> Result<()> {
        let ports = ports(threads).iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
        let script = format!(
            "Remove-NetFirewallRule -DisplayName '{FW_RULE}' -ErrorAction SilentlyContinue\n\
             New-NetFirewallRule -DisplayName '{FW_RULE}' -Direction Inbound -Protocol UDP -LocalPort {ports} -Action Allow -Profile Any | Out-Null\n\
             try {{\n\
               Remove-NetFirewallHyperVRule -Name 'NightCoopHub' -ErrorAction SilentlyContinue\n\
               New-NetFirewallHyperVRule -Name 'NightCoopHub' -DisplayName 'NightCoop hub (WSL)' -Direction Inbound -VMCreatorId '{{40E0AC32-46A5-438A-A0B2-2B479E8F2E90}}' -Protocol UDP -LocalPorts {ports} -Action Allow | Out-Null\n\
             }} catch {{ }}"
        );
        if !elevated(&script)? {
            bail!("the administrator prompt was declined");
        }
        Ok(())
    }

    pub fn hub_built() -> bool {
        root(&["test", "-x", BIN]).map(|(c, _)| c == 0).unwrap_or(false)
    }

    /// Builds and installs the official hub inside Ubuntu with the setup script (a few minutes).
    pub fn build_hub(computer_name: &str) -> Result<String> {
        let mut child = hidden("wsl.exe")
            .args(["-d", DISTRO, "-u", "root", "--", "bash", "-s", "--", "--no-service", "--shards", &HOME_THREADS.to_string(), "--name", computer_name, "--region", "home"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("couldn't run wsl.exe")?;
        child.stdin.take().unwrap().write_all(SETUP_SCRIPT.replace("\r\n", "\n").as_bytes())?;
        let out = child.wait_with_output()?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            let tail: Vec<&str> = text.lines().rev().take(12).collect();
            bail!("building the hub failed:\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
        }
        Ok(text)
    }

    pub fn running() -> bool {
        root(&["pgrep", "-x", "nightcoop_hub"]).map(|(c, _)| c == 0).unwrap_or(false)
    }

    /// Starts the hub in the background; it keeps running after the helper closes (until Windows shuts down
    /// or "Stop" is pressed). Output goes to the helper's data folder.
    pub fn start(computer_name: &str) -> Result<()> {
        if running() {
            return Ok(());
        }
        restore_key()?;
        let log = fs::File::create(crate::config::sub("logs").join("hub.log"))?;
        let threads = HOME_THREADS.to_string();
        let port = PORT.to_string();
        Command::new("wsl.exe")
            .args(["-d", DISTRO, "-u", "nightcoop", "--", BIN, &port, "--shards", &threads, "--max-clients", "512", "--sign-key-auto", KEY, "--hub-name", computer_name, "--region", "home"])
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("couldn't start the hub")?;
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if running() {
                let _ = backup_key();
                return Ok(());
            }
        }
        bail!("the hub didn't start — see {}", crate::config::sub("logs").join("hub.log").display())
    }

    pub fn stop() -> Result<()> {
        root(&["pkill", "-x", "nightcoop_hub"])?;
        Ok(())
    }

    pub fn fingerprint() -> Option<String> {
        let (_, out) = root(&["sudo", "-u", "nightcoop", BIN, "--hub-public", KEY]).ok()?;
        find_fingerprint(&out)
    }

    fn key_backup() -> std::path::PathBuf {
        crate::config::sub("backups").join("hub-sign.key")
    }

    /// Copies the hub's key into the helper's backups (it is the server's identity).
    pub fn backup_key() -> Result<()> {
        let out = hidden("wsl.exe").args(["-d", DISTRO, "-u", "root", "--", "cat", KEY]).output()?;
        if out.status.success() && !out.stdout.is_empty() {
            fs::write(key_backup(), &out.stdout)?;
        }
        Ok(())
    }

    /// After a reinstall, puts the backed-up key back so the fingerprint stays the same.
    fn restore_key() -> Result<()> {
        let backup = key_backup();
        if !backup.is_file() || root(&["test", "-s", KEY]).map(|(c, _)| c == 0).unwrap_or(false) {
            return Ok(());
        }
        let mut child = hidden("wsl.exe")
            .args(["-d", DISTRO, "-u", "root", "--", "sh", "-c", &format!("cat > {KEY} && chown nightcoop: {KEY} && chmod 600 {KEY}")])
            .stdin(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(&fs::read(&backup)?)?;
        child.wait()?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn decodes_wsl_output() {
            let utf16: Vec<u8> = "Ubuntu-24.04\r\n".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            assert_eq!(decode(&utf16).trim(), "Ubuntu-24.04");
            assert_eq!(decode(b"plain"), "plain");
        }
    }
}

/// Sets `key=value` in `[section]` of an INI text, adding the section or key when missing.
pub fn set_ini_key(text: &str, section: &str, key: &str, value: &str) -> String {
    let header = format!("[{section}]");
    let mut out: Vec<String> = Vec::new();
    let (mut in_section, mut seen_section, mut done) = (false, false, false);
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if in_section && !done {
                out.push(format!("{key}={value}"));
                done = true;
            }
            in_section = t.eq_ignore_ascii_case(&header);
            seen_section |= in_section;
        } else if in_section && t.split_once('=').map(|(k, _)| k.trim().eq_ignore_ascii_case(key)).unwrap_or(false) {
            if !done {
                out.push(format!("{key}={value}"));
                done = true;
            }
            continue;
        }
        out.push(line.to_string());
    }
    if in_section && !done {
        out.push(format!("{key}={value}"));
        done = true;
    }
    if !seen_section && !done {
        if out.last().map(|l| !l.trim().is_empty()).unwrap_or(false) {
            out.push(String::new());
        }
        out.push(header);
        out.push(format!("{key}={value}"));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// The hub's log as seen from Windows (WSL) — the last lines, for the UI.
pub fn log_tail(path: &Path, lines: usize) -> Vec<String> {
    let text = fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let all: Vec<String> = text.lines().map(|l| l.chars().take(200).collect()).collect();
    all[all.len().saturating_sub(lines)..].to_vec()
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

    #[test]
    fn ini_editing() {
        assert_eq!(set_ini_key("", "wsl2", "networkingMode", "mirrored"), "[wsl2]\nnetworkingMode=mirrored\n");
        assert_eq!(
            set_ini_key("[wsl2]\nmemory=4GB\nnetworkingMode=nat\n[experimental]\nx=1\n", "wsl2", "networkingMode", "mirrored"),
            "[wsl2]\nmemory=4GB\nnetworkingMode=mirrored\n[experimental]\nx=1\n"
        );
        assert_eq!(set_ini_key("[wsl2]\nmemory=4GB\n", "wsl2", "networkingMode", "mirrored"), "[wsl2]\nmemory=4GB\nnetworkingMode=mirrored\n");
        assert_eq!(set_ini_key("[boot]\nsystemd=true\n", "wsl2", "networkingMode", "mirrored"), "[boot]\nsystemd=true\n\n[wsl2]\nnetworkingMode=mirrored\n");
    }
}
