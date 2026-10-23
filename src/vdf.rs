//! Minimal reader/writer for Valve's text KeyValues format (.vdf / .acf).
//!
//! String values are kept exactly as written in the file (still escaped), so a
//! file we read and write back only changes where we deliberately set a value.

use anyhow::{bail, Result};

#[derive(Debug, Clone)]
pub enum Value {
    /// Raw, still-escaped text between the quotes.
    Str(String),
    Obj(Vec<(String, Value)>),
}

impl Value {
    pub fn new_obj() -> Value {
        Value::Obj(Vec::new())
    }

    /// Case-insensitive child lookup (Steam is inconsistent: "apps" vs "Apps").
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(items) => items
                .iter()
                .find(|(k, _)| unescape(k).eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            Value::Str(_) => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        match self {
            Value::Obj(items) => items
                .iter_mut()
                .find(|(k, _)| unescape(k).eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            Value::Str(_) => None,
        }
    }

    pub fn path(&self, keys: &[&str]) -> Option<&Value> {
        keys.iter().try_fold(self, |v, k| v.get(k))
    }

    /// Unescaped string value.
    pub fn str(&self) -> Option<String> {
        match self {
            Value::Str(s) => Some(unescape(s)),
            Value::Obj(_) => None,
        }
    }

    pub fn get_str(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|v| v.str())
    }

    pub fn children(&self) -> &[(String, Value)] {
        match self {
            Value::Obj(items) => items,
            Value::Str(_) => &[],
        }
    }

    /// Returns the child object `key`, creating it (or replacing a string) if needed.
    pub fn obj_mut(&mut self, key: &str) -> &mut Value {
        if !matches!(self, Value::Obj(_)) {
            *self = Value::new_obj();
        }
        let exists = matches!(self.get(key), Some(Value::Obj(_)));
        if !exists {
            self.remove(key);
            if let Value::Obj(items) = self {
                items.push((escape(key), Value::new_obj()));
            }
        }
        self.get_mut(key).expect("just inserted")
    }

    /// Sets an (unescaped) string value, replacing any existing one in place.
    pub fn set_str(&mut self, key: &str, value: &str) {
        if !matches!(self, Value::Obj(_)) {
            *self = Value::new_obj();
        }
        if let Some(v) = self.get_mut(key) {
            *v = Value::Str(escape(value));
        } else if let Value::Obj(items) = self {
            items.push((escape(key), Value::Str(escape(value))));
        }
    }

    pub fn remove(&mut self, key: &str) {
        if let Value::Obj(items) = self {
            items.retain(|(k, _)| !unescape(k).eq_ignore_ascii_case(key));
        }
    }
}

pub fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[derive(Debug, PartialEq)]
enum Tok {
    Str(String),
    Open,
    Close,
}

fn tokenize(src: &str) -> Result<Vec<Tok>> {
    let mut toks = Vec::new();
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            0xEF if b[i..].starts_with(&[0xEF, 0xBB, 0xBF]) => i += 3,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' => {
                toks.push(Tok::Open);
                i += 1;
            }
            b'}' => {
                toks.push(Tok::Close);
                i += 1;
            }
            b'[' => {
                // Platform conditional like [$WIN32]; ignored.
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                let start = i;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i > b.len() {
                    bail!("unterminated string in VDF");
                }
                toks.push(Tok::Str(src[start..i.min(b.len())].to_string()));
                i += 1;
            }
            _ => {
                let start = i;
                while i < b.len() && !matches!(b[i], b' ' | b'\t' | b'\r' | b'\n' | b'{' | b'}' | b'"') {
                    i += 1;
                }
                toks.push(Tok::Str(src[start..i].to_string()));
            }
        }
    }
    Ok(toks)
}

/// Parses a document into a root object holding its top-level keys.
pub fn parse(src: &str) -> Result<Value> {
    let toks = tokenize(src)?;
    let mut pos = 0;
    let root = parse_items(&toks, &mut pos, true)?;
    Ok(Value::Obj(root))
}

fn parse_items(toks: &[Tok], pos: &mut usize, top: bool) -> Result<Vec<(String, Value)>> {
    let mut items = Vec::new();
    loop {
        match toks.get(*pos) {
            None if top => return Ok(items),
            None => bail!("unexpected end of VDF"),
            Some(Tok::Close) if !top => {
                *pos += 1;
                return Ok(items);
            }
            Some(Tok::Close) => bail!("unexpected '}}' in VDF"),
            Some(Tok::Open) => bail!("unexpected '{{' in VDF"),
            Some(Tok::Str(key)) => {
                *pos += 1;
                match toks.get(*pos) {
                    Some(Tok::Str(v)) => {
                        *pos += 1;
                        items.push((key.clone(), Value::Str(v.clone())));
                    }
                    Some(Tok::Open) => {
                        *pos += 1;
                        let child = parse_items(toks, pos, false)?;
                        items.push((key.clone(), Value::Obj(child)));
                    }
                    _ => bail!("key '{key}' has no value"),
                }
            }
        }
    }
}

/// Serializes a root object (as returned by [`parse`]) in Steam's layout.
pub fn write(root: &Value) -> String {
    let mut out = String::new();
    for (k, v) in root.children() {
        write_item(&mut out, k, v, 0);
    }
    out
}

fn write_item(out: &mut String, key: &str, v: &Value, depth: usize) {
    let indent = "\t".repeat(depth);
    match v {
        Value::Str(s) => out.push_str(&format!("{indent}\"{key}\"\t\t\"{s}\"\n")),
        Value::Obj(items) => {
            out.push_str(&format!("{indent}\"{key}\"\n{indent}{{\n"));
            for (k, c) in items {
                write_item(out, k, c, depth + 1);
            }
            out.push_str(&format!("{indent}}}\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_edit() {
        let src = r#""UserLocalConfigStore"
{
	"Software"
	{
		"Valve"
		{
			"Steam"
			{
				"Apps"
				{
					"1091500"
					{
						"LaunchOptions"		"-skipStartScreen \"x\""
						"path"		"C:\\Games"
					}
				}
			}
		}
	}
}
"#;
        let mut v = parse(src).unwrap();
        let app = v.path(&["UserLocalConfigStore", "Software", "Valve", "Steam", "apps", "1091500"]).unwrap();
        assert_eq!(app.get_str("LaunchOptions").unwrap(), "-skipStartScreen \"x\"");
        assert_eq!(app.get_str("path").unwrap(), "C:\\Games");
        assert_eq!(write(&v), src);

        let app = v
            .obj_mut("UserLocalConfigStore")
            .obj_mut("Software")
            .obj_mut("Valve")
            .obj_mut("Steam")
            .obj_mut("apps")
            .obj_mut("1091500");
        app.set_str("LaunchOptions", "WINEDLLOVERRIDES=\"winmm=n,b\" %command%");
        let again = parse(&write(&v)).unwrap();
        let lo = again
            .path(&["UserLocalConfigStore", "Software", "Valve", "Steam", "Apps", "1091500", "LaunchOptions"])
            .unwrap()
            .str()
            .unwrap();
        assert_eq!(lo, "WINEDLLOVERRIDES=\"winmm=n,b\" %command%");
    }
}
