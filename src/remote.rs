//! Release feeds: RED4ext from GitHub, NightCoop from nightcoop.net/releases.json.

use crate::config::Channel;
use crate::util;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

pub const NIGHTCOOP_SITE: &str = "https://nightcoop.net/";
pub const NIGHTCOOP_MANIFEST: &str = "https://nightcoop.net/releases.json";
pub const NIGHTCOOP_DISCORD: &str = "https://discord.gg/6UvSdSmaTM";
pub const RED4EXT_REPO: &str = "WopsS/RED4ext";

// ---------- GitHub ----------

#[derive(Debug, Clone, Deserialize)]
pub struct GhAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub digest: Option<String>,
}

impl GhAsset {
    pub fn sha256(&self) -> Option<String> {
        self.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).map(str::to_string)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GhRelease {
    pub tag_name: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assets: Vec<GhAsset>,
}

impl GhRelease {
    pub fn version(&self) -> String {
        self.tag_name.trim_start_matches('v').to_string()
    }
    pub fn date(&self) -> String {
        self.published_at.as_deref().map(|d| d.chars().take(10).collect()).unwrap_or_default()
    }
}

pub fn github_releases(repo: &str) -> Result<Vec<GhRelease>> {
    let text = util::get_text(&format!("https://api.github.com/repos/{repo}/releases?per_page=20"))?;
    let mut list: Vec<GhRelease> = serde_json::from_str(&text).context("unexpected reply from GitHub")?;
    list.retain(|r| !r.draft);
    Ok(list)
}

/// The main RED4ext zip of a release (not the debug symbols).
pub fn red4ext_asset(rel: &GhRelease) -> Option<&GhAsset> {
    rel.assets.iter().find(|a| {
        let n = a.name.to_ascii_lowercase();
        n.ends_with(".zip") && n.starts_with("red4ext") && !n.contains("symbol") && !n.contains("pdb")
    })
}

// ---------- NightCoop ----------

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct NcManifest {
    pub schema: Option<u32>,
    pub updated: Option<String>,
    pub latest: Option<String>,
    pub releases: Vec<NcRelease>,
    pub demo: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct NcRelease {
    pub version: String,
    pub channel: Option<String>,
    pub yanked: Value,
    pub date: Option<String>,
    pub title: Value,
    pub notes: Value,
    pub game: Vec<String>,
    pub protocol: Value,
    /// Source revision of the build, e.g. "a7c537d".
    pub build: Option<String>,
    pub files: Vec<NcFile>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct NcFile {
    pub kind: Option<String>,
    pub name: Option<String>,
    pub url: String,
    pub size: Option<u64>,
    pub sha256: Option<String>,
    pub mirrors: Vec<String>,
}

/// Site text is either a plain string or {"en": ..., "ru": ...}.
pub fn tr(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => m.get("en").or_else(|| m.get("ru")).and_then(|x| x.as_str()).map(str::to_string),
        _ => None,
    }
}

impl NcRelease {
    pub fn is_yanked(&self) -> bool {
        !matches!(self.yanked, Value::Null | Value::Bool(false))
    }
    pub fn yank_reason(&self) -> Option<String> {
        match &self.yanked {
            Value::Object(m) => m.get("reason").and_then(tr),
            _ => None,
        }
    }
    pub fn is_stable(&self) -> bool {
        self.channel.as_deref() == Some("stable")
    }
    pub fn mod_file(&self) -> Option<&NcFile> {
        self.files.iter().find(|f| f.kind.as_deref() == Some("mod")).or_else(|| self.files.first())
    }
    pub fn protocol_str(&self) -> Option<String> {
        match &self.protocol {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    }
    pub fn title_text(&self) -> Option<String> {
        tr(&self.title)
    }
    pub fn notes_text(&self) -> Option<String> {
        match &self.notes {
            Value::Array(items) => {
                let lines: Vec<String> = items.iter().filter_map(tr).map(|l| format!("• {l}")).collect();
                (!lines.is_empty()).then(|| lines.join("\n"))
            }
            Value::Object(m) => {
                let v = m.get("en").or_else(|| m.get("ru"))?;
                match v {
                    Value::Array(items) => Some(items.iter().filter_map(|x| x.as_str()).map(|l| format!("• {l}")).collect::<Vec<_>>().join("\n")),
                    other => other.as_str().map(str::to_string),
                }
            }
            other => tr(other),
        }
    }
}

impl NcFile {
    /// Absolute URLs for this file: the main one, then mirrors.
    pub fn urls(&self, base: &str) -> Vec<String> {
        std::iter::once(&self.url).chain(self.mirrors.iter()).filter(|u| !u.is_empty()).map(|u| absolute(base, u)).collect()
    }
    pub fn file_name(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        let last = self.url.rsplit('/').next().unwrap_or("nightcoop.zip");
        let last = last.split(['?', '#']).next().unwrap_or(last);
        if last.is_empty() { "nightcoop.zip".into() } else { last.to_string() }
    }
}

fn absolute(base: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.to_string();
    }
    let origin_end = base.find("://").map(|i| i + 3).and_then(|i| base[i..].find('/').map(|j| i + j)).unwrap_or(base.len());
    if let Some(stripped) = url.strip_prefix('/') {
        format!("{}/{}", &base[..origin_end], stripped)
    } else {
        let dir_end = base.rfind('/').filter(|&i| i >= origin_end).map(|i| i + 1).unwrap_or(base.len());
        let dir = &base[..dir_end];
        if dir.ends_with('/') { format!("{dir}{url}") } else { format!("{dir}/{url}") }
    }
}

impl NcManifest {
    /// The newest non-withdrawn release for the chosen channel.
    pub fn latest_for(&self, channel: Channel) -> Option<&NcRelease> {
        let usable = |r: &&NcRelease| !r.is_yanked() && r.mod_file().is_some();
        if channel == Channel::Beta {
            if let Some(l) = &self.latest {
                if let Some(r) = self.releases.iter().filter(usable).find(|r| &r.version == l) {
                    if let Some(newest) = self.releases.iter().filter(usable).max_by(|a, b| util::cmp_versions(&a.version, &b.version)) {
                        if util::cmp_versions(&newest.version, &r.version).is_gt() {
                            return Some(newest);
                        }
                    }
                    return Some(r);
                }
            }
            return self.releases.iter().filter(usable).max_by(|a, b| util::cmp_versions(&a.version, &b.version));
        }
        // Stable channel: newest stable, or — while everything is still beta — the site's "latest".
        if let Some(r) = self.releases.iter().filter(usable).filter(|r| r.is_stable()).max_by(|a, b| util::cmp_versions(&a.version, &b.version)) {
            return Some(r);
        }
        if let Some(l) = &self.latest {
            if let Some(r) = self.releases.iter().filter(usable).find(|r| &r.version == l) {
                return Some(r);
            }
        }
        self.releases.iter().filter(usable).max_by(|a, b| util::cmp_versions(&a.version, &b.version))
    }

    pub fn find(&self, version: &str) -> Option<&NcRelease> {
        self.releases.iter().find(|r| r.version == version)
    }
}

pub fn nightcoop_manifest(url: &str) -> Result<NcManifest> {
    let text = util::get_text(url)?;
    serde_json::from_str(&text).context("the NightCoop release list has an unexpected format")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_channels() {
        assert_eq!(absolute("https://nightcoop.net/releases.json", "files/a.zip"), "https://nightcoop.net/files/a.zip");
        assert_eq!(absolute("https://nightcoop.net/releases.json", "/files/a.zip"), "https://nightcoop.net/files/a.zip");
        assert_eq!(absolute("https://nightcoop.net/x/releases.json", "https://cdn.x/a.zip"), "https://cdn.x/a.zip");

        let m: NcManifest = serde_json::from_str(
            r#"{"schema":1,"latest":"0.2.0","releases":[
            {"version":"0.3.0","channel":"beta","yanked":{"reason":{"en":"crash"}},"files":[{"kind":"mod","url":"a.zip"}]},
            {"version":"0.2.0","channel":"beta","yanked":false,"title":{"ru":"т","en":"t"},"notes":{"en":["one","two"]},"files":[{"kind":"mod","url":"b.zip","sha256":"ab"}]},
            {"version":"0.1.0","channel":"stable","files":[{"kind":"mod","url":"c.zip"}]}]}"#,
        )
        .unwrap();
        assert_eq!(m.latest_for(Channel::Beta).unwrap().version, "0.2.0");
        assert_eq!(m.latest_for(Channel::Stable).unwrap().version, "0.1.0");
        assert_eq!(m.releases[0].yank_reason().unwrap(), "crash");
        assert_eq!(m.releases[1].notes_text().unwrap(), "• one\n• two");

        let empty: NcManifest = serde_json::from_str(r#"{"schema":1,"updated":"x","latest":null,"releases":[]}"#).unwrap();
        assert!(empty.latest_for(Channel::Stable).is_none());
    }
}
