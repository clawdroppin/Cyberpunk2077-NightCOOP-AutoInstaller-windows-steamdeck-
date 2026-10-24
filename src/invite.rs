//! NightCoop invites (`ABC234@hub.example.org:47900#K3QF-9X2M-TV7A-0D4R`, or just the session code)
//! and the one-message "join me" text partners send each other through the helper.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub code: String,
    /// Custom server (`host[:port]`); `None` means the official servers.
    pub server: Option<String>,
    /// Key fingerprint; without it the game connects but can't verify the server.
    pub fingerprint: Option<String>,
}

impl Invite {
    /// The exact text to paste into the game's "Join by code".
    pub fn line(&self) -> String {
        match (&self.server, &self.fingerprint) {
            (Some(s), Some(f)) => format!("{}@{s}#{f}", self.code),
            (Some(s), None) => format!("{}@{s}", self.code),
            _ => self.code.clone(),
        }
    }
}

fn is_code(s: &str) -> bool {
    (4..=12).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric()) && s.chars().any(|c| c.is_ascii_digit()) && s.chars().any(|c| c.is_ascii_alphabetic())
}

fn is_fingerprint(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() >= 2 && groups.iter().all(|g| !g.is_empty() && g.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn is_host(s: &str) -> bool {
    let host = s.rsplit_once(':').filter(|(_, p)| p.parse::<u16>().is_ok()).map(|(h, _)| h).unwrap_or(s);
    !host.is_empty() && host.contains('.') && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// Finds an invite anywhere in a pasted text (a chat message, the helper's "join me" text…).
pub fn find(text: &str) -> Option<Invite> {
    let tokens = text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '"' | '\'' | '(' | ')' | '<' | '>' | '`'));
    let mut bare_code = None;
    for raw in tokens {
        let t = raw.trim_matches(|c: char| matches!(c, '.' | ':' | '!' | '?'));
        if let Some((code, rest)) = t.split_once('@') {
            let (server, fp) = match rest.split_once('#') {
                Some((s, f)) => (s, Some(f)),
                None => (rest, None),
            };
            if is_code(code) && is_host(server) && fp.map(is_fingerprint).unwrap_or(true) {
                return Some(Invite {
                    code: code.to_ascii_uppercase(),
                    server: Some(server.to_ascii_lowercase()),
                    fingerprint: fp.map(|f| f.to_ascii_uppercase()),
                });
            }
        } else if bare_code.is_none() && is_code(t) && t.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) && !t.contains('.') {
            bare_code = Some(t.to_string());
        }
    }
    // A bare code is only trusted after an "invite"/"code" label, to avoid picking up random words.
    let lower = text.to_ascii_lowercase();
    if lower.contains("invite") || lower.contains("code") || text.trim().len() <= 12 {
        return bare_code.map(|code| Invite { code, server: None, fingerprint: None });
    }
    None
}

/// The message one partner sends the other: an invite (optional) plus the versions to compare.
pub fn join_message(invite: Option<&Invite>, versions: &str) -> String {
    let mut m = String::from("Let's play Cyberpunk co-op (NightCoop)!\n");
    if let Some(i) = invite {
        m.push_str(&format!("Invite: {}\n", i.line()));
        m.push_str("In the game: Co-op → Join by code → paste the invite.\n");
    }
    m.push_str(&format!("My versions: {versions}\n"));
    m.push_str("Paste this whole message into NightCoop Helper → Play together to check we match.");
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_invites() {
        let full = find("join me: ABC234@hub.example.org:47900#K3QF-9X2M-TV7A-0D4R thanks").unwrap();
        assert_eq!(full.code, "ABC234");
        assert_eq!(full.server.as_deref(), Some("hub.example.org:47900"));
        assert_eq!(full.fingerprint.as_deref(), Some("K3QF-9X2M-TV7A-0D4R"));
        assert_eq!(full.line(), "ABC234@hub.example.org:47900#K3QF-9X2M-TV7A-0D4R");

        let ip = find("XY77AB@203.0.113.7#K3QF-9X2M").unwrap();
        assert_eq!(ip.server.as_deref(), Some("203.0.113.7"));

        assert_eq!(find("ABC234").unwrap().line(), "ABC234");
        assert_eq!(find("Invite: ABC234").unwrap().code, "ABC234");
        assert!(find("NightCoop 0.1.0-beta.1 · Protocol 128 · RED4ext 1.30.0").is_none());
        assert!(find("see you at 9PM tonight").is_none());

        let msg = join_message(Some(&full), "NightCoop 0.1.0-beta.1 · Protocol 128");
        assert_eq!(find(&msg).unwrap(), full);
    }
}
