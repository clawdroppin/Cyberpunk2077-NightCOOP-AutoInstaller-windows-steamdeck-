//! Shared helpers: HTTP, downloads, archives, file copying, version comparison.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const USER_AGENT: &str = concat!("NightCoopHelper/", env!("CARGO_PKG_VERSION"));

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .user_agent(USER_AGENT)
        .build()
        .into()
}

pub fn get_text(url: &str) -> Result<String> {
    let mut resp = agent()
        .get(url)
        .header("Accept", "application/json")
        .call()
        .with_context(|| format!("could not reach {url}"))?;
    Ok(resp.body_mut().with_config().limit(16 * 1024 * 1024).read_to_string()?)
}

/// Downloads `url` to `dest`, reporting (done, total) bytes. Verifies SHA-256 when given.
pub fn download(url: &str, dest: &Path, sha256: Option<&str>, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    // Reuse an earlier download when its checksum already matches.
    if let Some(want) = sha256 {
        if dest.is_file() && sha256_file(dest).map(|h| h.eq_ignore_ascii_case(want)).unwrap_or(false) {
            return Ok(());
        }
    }
    let resp = agent().get(url).call().with_context(|| format!("download failed: {url}"))?;
    let total = resp.body().content_length();
    let mut reader = resp.into_body().into_reader();
    let part = dest.with_extension("part");
    let mut file = fs::File::create(&part).with_context(|| format!("cannot write {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done, total);
    }
    file.flush()?;
    drop(file);
    if let Some(want) = sha256 {
        let got = hex::encode(hasher.finalize());
        if !got.eq_ignore_ascii_case(want) {
            let _ = fs::remove_file(&part);
            bail!("checksum mismatch for {url}\nexpected {want}\ngot      {got}");
        }
    }
    fs::rename(&part, dest)?;
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Extracts a .zip or .7z archive into `dest` (which is emptied first).
pub fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    fs::create_dir_all(dest)?;
    let mut magic = [0u8; 6];
    fs::File::open(archive)?.read_exact(&mut magic).context("archive is too small")?;
    if magic.starts_with(b"PK") {
        let mut zip = zip::ZipArchive::new(fs::File::open(archive)?).context("not a valid zip")?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            let Some(rel) = entry.enclosed_name() else { continue };
            let out = dest.join(rel);
            if entry.is_dir() {
                fs::create_dir_all(&out)?;
            } else {
                if let Some(p) = out.parent() {
                    fs::create_dir_all(p)?;
                }
                let mut f = fs::File::create(&out)?;
                std::io::copy(&mut entry, &mut f)?;
            }
        }
        Ok(())
    } else if magic == [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C] {
        sevenz_rust::decompress_file(archive, dest).map_err(|e| anyhow!("7z extraction failed: {e}"))
    } else {
        bail!("{} is not a .zip or .7z archive", archive.display())
    }
}

/// Recursively copies `src` into `dst`, merging and overwriting. Returns copied files (relative to `dst`).
pub fn copy_merge(src: &Path, dst: &Path) -> Result<Vec<PathBuf>> {
    let mut copied = Vec::new();
    copy_merge_inner(src, dst, Path::new(""), &mut copied)?;
    Ok(copied)
}

fn copy_merge_inner(src: &Path, dst: &Path, rel: &Path, copied: &mut Vec<PathBuf>) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let from = entry.path();
        let to = dst.join(&name);
        let rel = rel.join(&name);
        if entry.file_type()?.is_dir() {
            copy_merge_inner(&from, &to, &rel, copied)?;
        } else {
            fs::copy(&from, &to).with_context(|| format!("cannot write {} (is the game running?)", to.display()))?;
            copied.push(rel);
        }
    }
    Ok(())
}

/// Adds every file under `dir` to `zip`, stored under `prefix/`.
pub fn zip_dir<W: Write + std::io::Seek>(zip: &mut zip::ZipWriter<W>, dir: &Path, prefix: &str) -> Result<usize> {
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
                zip.start_file(format!("{prefix}/{rel}"), opts)?;
                let mut f = fs::File::open(&path)?;
                std::io::copy(&mut f, zip)?;
                count += 1;
            }
        }
    }
    Ok(count)
}

pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(_) => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => {}
            }
        }
    }
    total
}

pub fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", b / 1024.0 / 1024.0 / 1024.0)
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / 1024.0 / 1024.0)
    } else {
        format!("{} KB", (bytes / 1024).max(1))
    }
}

/// Compares versions like "1.30.0", "v1.2.3-beta.4" by their numeric parts.
pub fn cmp_versions(a: &str, b: &str) -> Ordering {
    let nums = |s: &str| -> Vec<u64> {
        s.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect()
    };
    let (na, nb) = (nums(a), nums(b));
    for i in 0..na.len().max(nb.len()) {
        let (x, y) = (na.get(i).copied().unwrap_or(0), nb.get(i).copied().unwrap_or(0));
        match x.cmp(&y) {
            Ordering::Equal => continue,
            o => return o,
        }
    }
    Ordering::Equal
}

pub fn same_version(a: &str, b: &str) -> bool {
    cmp_versions(a, b) == Ordering::Equal
}

/// Reads the FileVersion / ProductVersion of a Windows PE file (works on any OS).
pub fn pe_version(path: &Path) -> Option<String> {
    use pelite::pe64::{Pe, PeFile};
    let map = pelite::FileMap::open(path).ok()?;
    let pe = PeFile::from_bytes(&map).ok()?;
    let info = pe.resources().ok()?.version_info().ok()?;
    if let Some(lang) = info.translation().first() {
        for key in ["ProductVersion", "FileVersion"] {
            if let Some(v) = info.value(*lang, key) {
                let v = v.trim().trim_end_matches('\0').to_string();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    let f = info.fixed()?;
    let v = f.dwFileVersion;
    Some(format!("{}.{}.{}.{}", v.Major, v.Minor, v.Patch, v.Build))
}

pub fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string()
}

/// Opens a folder or URL with the desktop's default handler.
pub fn open_path(target: impl AsRef<std::ffi::OsStr>) {
    let _ = open::that_detached(target);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(cmp_versions("v1.30.0", "1.29.9"), Ordering::Greater);
        assert_eq!(cmp_versions("1.2", "1.2.0"), Ordering::Equal);
        assert_eq!(cmp_versions("0.9.10", "0.9.9"), Ordering::Greater);
    }
}
