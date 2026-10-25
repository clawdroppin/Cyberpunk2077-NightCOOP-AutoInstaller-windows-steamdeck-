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

/// The release file names `scripts/package.py` produces (they never change).
const PLATFORM_ASSET: &str = if cfg!(windows) { "NightCoopHelper-windows-x64.exe" } else { "NightCoopHelper-linux-x86_64" };

pub fn check(repo: &str) -> Result<Option<HelperUpdate>> {
    let releases = match remote::github_releases(repo) {
        Ok(r) => r,
        // GitHub's API allows 60 lookups an hour per address; the plain website has no such limit.
        Err(api_err) => return check_via_website(repo).map_err(|_| api_err),
    };
    let Some(rel) = releases.into_iter().find(|r| !r.prerelease) else { return Ok(None) };
    if !util::cmp_versions(&rel.version(), VERSION).is_gt() {
        return Ok(None);
    }
    let asset = platform_asset(&rel.assets).cloned().ok_or_else(|| anyhow!("release {} has no build for this platform", rel.tag_name))?;
    Ok(Some(HelperUpdate { version: rel.version(), asset, notes: rel.body.clone() }))
}

/// Fallback without the API: `/releases/latest` redirects to the newest tag, the file names are fixed,
/// and the release's SHA256SUMS.txt still lets the download be verified.
fn check_via_website(repo: &str) -> Result<Option<HelperUpdate>> {
    let tag = remote::latest_tag_via_redirect(repo)?;
    let version = tag.trim_start_matches('v').to_string();
    if !util::cmp_versions(&version, VERSION).is_gt() {
        return Ok(None);
    }
    let base = format!("https://github.com/{repo}/releases/download/{tag}");
    let sums = util::get_text(&format!("{base}/SHA256SUMS.txt")).unwrap_or_default();
    let sha = sums.lines().find(|l| l.trim_end().ends_with(PLATFORM_ASSET)).and_then(|l| l.split_whitespace().next()).map(str::to_string);
    let asset = GhAsset {
        name: PLATFORM_ASSET.to_string(),
        browser_download_url: format!("{base}/{PLATFORM_ASSET}"),
        digest: sha.map(|s| format!("sha256:{s}")),
    };
    Ok(Some(HelperUpdate { version, asset, notes: None }))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "talks to github.com"]
    fn website_fallback_finds_latest_release() {
        let tag = remote::latest_tag_via_redirect(HOME_REPO).unwrap();
        assert!(tag.starts_with('v'), "{tag}");
        let sums = util::get_text(&format!("https://github.com/{HOME_REPO}/releases/download/{tag}/SHA256SUMS.txt")).unwrap();
        assert!(sums.lines().any(|l| l.ends_with(PLATFORM_ASSET)), "{sums}");
        println!("latest helper release {tag}");
    }
}
