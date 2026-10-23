//! Adding this helper to Steam as a non-Steam game (binary shortcuts.vdf),
//! so it can be opened from the Steam Deck's Gaming Mode.

use anyhow::{bail, Result};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
enum Node {
    Map(Vec<(String, Node)>),
    Str(String),
    Int(i32),
}

fn read_cstr(b: &[u8], pos: &mut usize) -> Result<String> {
    let start = *pos;
    while *pos < b.len() && b[*pos] != 0 {
        *pos += 1;
    }
    if *pos >= b.len() {
        bail!("truncated shortcuts.vdf");
    }
    let s = String::from_utf8_lossy(&b[start..*pos]).into_owned();
    *pos += 1;
    Ok(s)
}

fn read_map(b: &[u8], pos: &mut usize) -> Result<Vec<(String, Node)>> {
    let mut items = Vec::new();
    loop {
        let Some(&t) = b.get(*pos) else { bail!("truncated shortcuts.vdf") };
        *pos += 1;
        match t {
            0x08 => return Ok(items),
            0x00 => {
                let k = read_cstr(b, pos)?;
                items.push((k, Node::Map(read_map(b, pos)?)));
            }
            0x01 => {
                let k = read_cstr(b, pos)?;
                items.push((k, Node::Str(read_cstr(b, pos)?)));
            }
            0x02 => {
                let k = read_cstr(b, pos)?;
                let Some(bytes) = b.get(*pos..*pos + 4) else { bail!("truncated shortcuts.vdf") };
                *pos += 4;
                items.push((k, Node::Int(i32::from_le_bytes(bytes.try_into().unwrap()))));
            }
            other => bail!("unsupported value type {other:#x} in shortcuts.vdf"),
        }
    }
}

fn write_map(out: &mut Vec<u8>, items: &[(String, Node)]) {
    for (k, v) in items {
        match v {
            Node::Map(m) => {
                out.push(0x00);
                out.extend_from_slice(k.as_bytes());
                out.push(0);
                write_map(out, m);
            }
            Node::Str(s) => {
                out.push(0x01);
                out.extend_from_slice(k.as_bytes());
                out.push(0);
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
            Node::Int(i) => {
                out.push(0x02);
                out.extend_from_slice(k.as_bytes());
                out.push(0);
                out.extend_from_slice(&i.to_le_bytes());
            }
        }
    }
    out.push(0x08);
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Adds (or refreshes) a shortcut for `exe` in every Steam user's shortcuts.vdf.
/// Steam must be closed. Returns how many users were updated.
pub fn add_to_steam(steam_root: &Path, exe: &Path, name: &str) -> Result<usize> {
    if crate::steam::steam_running() {
        bail!("Steam is still running; close it first");
    }
    let exe_q = format!("\"{}\"", exe.display());
    let dir_q = format!("\"{}\"", exe.parent().unwrap_or(exe).display());
    let appid = (crc32(format!("{exe_q}{name}").as_bytes()) | 0x8000_0000) as i32;
    let mut count = 0;
    for e in fs::read_dir(steam_root.join("userdata"))?.flatten() {
        let uid = e.file_name().to_string_lossy().to_string();
        if uid == "0" || !uid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let cfg = e.path().join("config");
        if !cfg.is_dir() {
            continue;
        }
        let file = cfg.join("shortcuts.vdf");
        let mut root: Vec<(String, Node)> = match fs::read(&file) {
            Ok(bytes) if !bytes.is_empty() => {
                let mut pos = 0;
                read_map(&bytes, &mut pos)?
            }
            _ => vec![("shortcuts".into(), Node::Map(Vec::new()))],
        };
        if !root.iter().any(|(k, _)| k.eq_ignore_ascii_case("shortcuts")) {
            root.push(("shortcuts".into(), Node::Map(Vec::new())));
        }
        let Some((_, Node::Map(list))) = root.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case("shortcuts")) else { continue };
        // Drop an older entry for the same executable, then renumber.
        list.retain(|(_, n)| match n {
            Node::Map(m) => !m.iter().any(|(k, v)| k.eq_ignore_ascii_case("exe") && matches!(v, Node::Str(s) if *s == exe_q)),
            _ => true,
        });
        let entry = vec![
            ("appid".into(), Node::Int(appid)),
            ("AppName".into(), Node::Str(name.into())),
            ("Exe".into(), Node::Str(exe_q.clone())),
            ("StartDir".into(), Node::Str(dir_q.clone())),
            ("icon".into(), Node::Str(String::new())),
            ("ShortcutPath".into(), Node::Str(String::new())),
            ("LaunchOptions".into(), Node::Str(String::new())),
            ("IsHidden".into(), Node::Int(0)),
            ("AllowDesktopConfig".into(), Node::Int(1)),
            ("AllowOverlay".into(), Node::Int(1)),
            ("OpenVR".into(), Node::Int(0)),
            ("Devkit".into(), Node::Int(0)),
            ("DevkitGameID".into(), Node::Str(String::new())),
            ("DevkitOverrideAppID".into(), Node::Int(0)),
            ("LastPlayTime".into(), Node::Int(0)),
            ("FlatpakAppID".into(), Node::Str(String::new())),
            ("tags".into(), Node::Map(Vec::new())),
        ];
        list.push((String::new(), Node::Map(entry)));
        for (i, (k, _)) in list.iter_mut().enumerate() {
            *k = i.to_string();
        }
        if file.is_file() {
            let _ = fs::copy(&file, file.with_extension("vdf.nchelper-backup"));
        }
        let mut out = Vec::new();
        // The file is one map whose end marker is followed by a final 0x08.
        write_map(&mut out, &root);
        fs::write(&file, out)?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_round_trip() {
        let root = vec![(
            "shortcuts".to_string(),
            Node::Map(vec![("0".into(), Node::Map(vec![("appid".into(), Node::Int(-5)), ("AppName".into(), Node::Str("X".into()))]))]),
        )];
        let mut out = Vec::new();
        write_map(&mut out, &root);
        let mut pos = 0;
        let back = read_map(&out, &mut pos).unwrap();
        let mut again = Vec::new();
        write_map(&mut again, &back);
        assert_eq!(out, again);
        assert_eq!(pos, out.len());
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
