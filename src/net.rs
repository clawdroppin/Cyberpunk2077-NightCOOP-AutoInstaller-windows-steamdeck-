//! Connection check: the same public STUN servers NightCoop uses tell us whether UDP gets out
//! and whether this router keeps one public port per socket (direct connections likely) or
//! changes it per destination (the mod then relays through its hub — still works).

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

/// NightCoop's official hub (found in the plugin); only resolved, never contacted.
pub const OFFICIAL_HUB: &str = "hub.nightcoop.net";
const STUN_SERVERS: [&str; 2] = ["stun.l.google.com:19302", "stun.cloudflare.com:3478"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nat {
    /// Same public port towards different servers: peers can usually connect directly.
    Friendly,
    /// The public port changes per destination (symmetric NAT): expect the relay.
    Strict,
    /// Only one STUN server answered.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub hub_resolves: bool,
    pub udp_works: bool,
    pub nat: Nat,
}

impl CheckResult {
    pub fn summary(&self) -> String {
        if !self.hub_resolves && !self.udp_works {
            return "No internet connection reached the outside — check Wi-Fi.".into();
        }
        if !self.hub_resolves {
            return "Can't look up NightCoop's server name. Your network or DNS may block it; try another network.".into();
        }
        if !self.udp_works {
            return "UDP traffic seems blocked (common on school, work or hotel Wi-Fi). NightCoop needs UDP — try a home network or phone hotspot.".into();
        }
        match self.nat {
            Nat::Friendly => "Looks good: UDP works and your router allows direct connections.".into(),
            Nat::Strict => "UDP works, but your router is strict, so the mod will connect through its relay. That still works, with a bit more delay.".into(),
            Nat::Unknown => "UDP works. Couldn't tell how strict your router is; the mod falls back to its relay if needed.".into(),
        }
    }
}

fn stun_request(id: &[u8; 12]) -> [u8; 20] {
    let mut req = [0u8; 20];
    req[0..2].copy_from_slice(&0x0001u16.to_be_bytes()); // Binding Request
    req[4..8].copy_from_slice(&0x2112_A442u32.to_be_bytes()); // magic cookie
    req[8..20].copy_from_slice(id);
    req
}

/// Extracts the mapped (public) address from a STUN Binding Response.
fn parse_mapped(resp: &[u8], id: &[u8; 12]) -> Option<SocketAddr> {
    if resp.len() < 20 || resp[0..2] != [0x01, 0x01] || &resp[8..20] != id {
        return None;
    }
    let len = u16::from_be_bytes([resp[2], resp[3]]) as usize;
    let body = resp.get(20..20 + len)?;
    let mut i = 0;
    let mut plain = None;
    while i + 4 <= body.len() {
        let kind = u16::from_be_bytes([body[i], body[i + 1]]);
        let alen = u16::from_be_bytes([body[i + 2], body[i + 3]]) as usize;
        let val = body.get(i + 4..i + 4 + alen)?;
        if (kind == 0x0020 || kind == 0x0001) && alen >= 8 && val[1] == 0x01 {
            let mut port = u16::from_be_bytes([val[2], val[3]]);
            let mut ip = [val[4], val[5], val[6], val[7]];
            if kind == 0x0020 {
                port ^= 0x2112;
                for (b, m) in ip.iter_mut().zip(0x2112_A442u32.to_be_bytes()) {
                    *b ^= m;
                }
                return Some(SocketAddr::from((ip, port)));
            }
            plain = Some(SocketAddr::from((ip, port)));
        }
        i += 4 + ((alen + 3) & !3);
    }
    plain
}

fn stun_query(sock: &UdpSocket, server: &str, seed: u8) -> Option<SocketAddr> {
    let target = server.to_socket_addrs().ok()?.find(|a| a.is_ipv4())?;
    let mut id = [0u8; 12];
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    for (i, b) in id.iter_mut().enumerate() {
        *b = (t >> (i * 5)) as u8 ^ seed.wrapping_mul(31).wrapping_add(i as u8);
    }
    let req = stun_request(&id);
    let mut buf = [0u8; 512];
    for _ in 0..3 {
        sock.send_to(&req, target).ok()?;
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if from == target {
                if let Some(addr) = parse_mapped(&buf[..n], &id) {
                    return Some(addr);
                }
            }
        }
    }
    None
}

/// Runs the check (a few seconds at most). Sends only standard STUN requests.
pub fn connection_check() -> CheckResult {
    let hub_resolves = (OFFICIAL_HUB, 47800).to_socket_addrs().map(|mut a| a.next().is_some()).unwrap_or(false);
    let mapped: Vec<SocketAddr> = UdpSocket::bind("0.0.0.0:0")
        .ok()
        .and_then(|s| s.set_read_timeout(Some(Duration::from_millis(900))).ok().map(|_| s))
        .map(|sock| STUN_SERVERS.iter().enumerate().filter_map(|(i, srv)| stun_query(&sock, srv, i as u8)).collect())
        .unwrap_or_default();
    let nat = match mapped.as_slice() {
        [a, b, ..] if a == b => Nat::Friendly,
        [_, _, ..] => Nat::Strict,
        _ => Nat::Unknown,
    };
    CheckResult { hub_resolves, udp_works: !mapped.is_empty(), nat }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xor_mapped_address() {
        let id = [7u8; 12];
        // Binding success with XOR-MAPPED-ADDRESS for 203.0.113.5:54321.
        let mut resp = vec![0x01, 0x01, 0x00, 0x0c, 0x21, 0x12, 0xA4, 0x42];
        resp.extend_from_slice(&id);
        let port = 54321u16 ^ 0x2112;
        let ip = [203u8 ^ 0x21, 0 ^ 0x12, 113 ^ 0xA4, 5 ^ 0x42];
        resp.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
        resp.extend_from_slice(&port.to_be_bytes());
        resp.extend_from_slice(&ip);
        assert_eq!(parse_mapped(&resp, &id), Some("203.0.113.5:54321".parse().unwrap()));
        assert_eq!(parse_mapped(&resp, &[8u8; 12]), None);
    }

    #[test]
    #[ignore = "talks to public STUN servers"]
    fn live_check() {
        let r = connection_check();
        println!("{r:?}\n{}", r.summary());
        assert!(r.udp_works);
    }
}
