//! Updating the helper itself from a GitHub repository's releases.

use crate::ops::Reporter;
use crate::remote::{self, GhAsset};
use crate::util;
use anyhow::{anyhow, Result};
use std::fs;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Where releases of this helper are published.
pub const HOME_REPO: &str = "clawdroppin/Cyberpunk2077-NightCOOP-AutoInstaller-windows-steamdeck-";

#[derive(Debug, Clone)]
pub struct HelperUpdate {
    pub version: String,
    pub asset: GhAsset,
    pub notes: Option<String>,
}

fn platform_asset(assets: &[GhAsset]) -> Option<&GhAsset> {
    assets.iter().find(|a| {
        let n = a.name.to_ascii_lowercase();
        if cfg!(windows) {
            n.ends_with(".exe") && n.contains("windows")
        } else {
            n.contains("linux") && ![".zip", ".gz", ".txt", ".sha256"].iter().any(|ext| n.ends_with(ext))
        }
    })
}

pub fn check(repo: &str) -> Result<Option<HelperUpdate>> {
    let releases = remote::github_releases(repo)?;
    let Some(rel) = releases.into_iter().find(|r| !r.prerelease) else { return Ok(None) };
    if !util::cmp_versions(&rel.version(), VERSION).is_gt() {
        return Ok(None);
    }
    let asset = platform_asset(&rel.assets).cloned().ok_or_else(|| anyhow!("release {} has no build for this platform", rel.tag_name))?;
    Ok(Some(HelperUpdate { version: rel.version(), asset, notes: rel.body.clone() }))
}

/// Replaces the running executable with the new build. Takes effect on the next start.
pub fn apply(update: &HelperUpdate, rep: &Reporter) -> Result<()> {
    let exe = std::env::current_exe()?;
    let new = exe.with_extension("new");
    rep.download(&update.asset.browser_download_url, &new, update.asset.sha256().as_deref(), &format!("helper {}", update.version))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&new, fs::Permissions::from_mode(0o755))?;
        fs::rename(&new, &exe)?;
    }
    #[cfg(windows)]
    {
        // A running .exe can be renamed but not overwritten.
        let old = exe.with_extension("old.exe");
        let _ = fs::remove_file(&old);
        fs::rename(&exe, &old)?;
        fs::rename(&new, &exe)?;
    }
    rep.log(format!("Helper updated to {}. Restart it to use the new version.", update.version));
    Ok(())
}

/// Removes the leftover from a previous Windows self-update.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = fs::remove_file(exe.with_extension("old.exe"));
    }
}

pub fn restart() {
    if let Ok(exe) = std::env::current_exe() {
        if std::process::Command::new(exe).spawn().is_ok() {
            std::process::exit(0);
        }
    }
}
