//! Asking the home router (UPnP IGD) for its public address and to forward ports — what consoles and
//! games do. Plain SSDP over UDP plus SOAP over HTTP, so no extra libraries.

use anyhow::{anyhow, bail, Context, Result};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Router {
    /// Where SOAP requests go (the WANIPConnection / WANPPPConnection control URL).
    control_url: String,
    service: String,
    /// This computer's address on the home network (the target of forwarded ports).
    pub local_ip: Ipv4Addr,
    pub name: Option<String>,
}

const SEARCH_TARGETS: [&str; 3] = [
    "urn:schemas-upnp-org:service:WANIPConnection:2",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
];

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))? + start;
    Some(xml[start..end].trim())
}

/// Finds a UPnP router on the local network (about 3 seconds at most).
pub fn discover() -> Result<Router> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_millis(400)))?;
    let multicast: SocketAddr = "239.255.255.250:1900".parse().unwrap();
    for st in SEARCH_TARGETS {
        let msg = format!("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\r\n");
        let _ = sock.send_to(msg.as_bytes(), multicast);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        let reply = String::from_utf8_lossy(&buf[..n]);
        let Some(location) = reply.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("location").then(|| v.trim().to_string())
        }) else {
            continue;
        };
        if let Ok(router) = describe(&location, from) {
            return Ok(router);
        }
    }
    bail!("no router answered UPnP (it may be switched off in the router's settings)")
}

/// Reads the router's device description and picks its WAN connection service.
fn describe(location: &str, from: SocketAddr) -> Result<Router> {
    let xml = crate::util::get_text(location)?;
    let base = {
        let after_scheme = location.find("://").map(|i| i + 3).unwrap_or(0);
        let end = location[after_scheme..].find('/').map(|i| i + after_scheme).unwrap_or(location.len());
        location[..end].to_string()
    };
    for chunk in xml.split("<service>").skip(1) {
        let Some(service) = tag(chunk, "serviceType") else { continue };
        if !(service.contains("WANIPConnection") || service.contains("WANPPPConnection")) {
            continue;
        }
        let control = tag(chunk, "controlURL").ok_or_else(|| anyhow!("router description has no control URL"))?;
        let control_url = if control.starts_with("http") { control.to_string() } else { format!("{base}/{}", control.trim_start_matches('/')) };
        // The local address used to reach the router is the one ports get forwarded to.
        let probe = UdpSocket::bind("0.0.0.0:0")?;
        probe.connect(from)?;
        let IpAddr::V4(local_ip) = probe.local_addr()?.ip() else { bail!("IPv6-only network") };
        return Ok(Router { control_url, service: service.to_string(), local_ip, name: tag(&xml, "friendlyName").map(str::to_string) });
    }
    bail!("this UPnP device is not an internet router")
}

impl Router {
    fn soap(&self, action: &str, args: &[(&str, String)]) -> Result<String> {
        let body: String = args.iter().map(|(k, v)| format!("<{k}>{v}</{k}>")).collect();
        let envelope = format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{}\">{body}</u:{action}></s:Body></s:Envelope>",
            self.service
        );
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(6)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut resp = agent
            .post(&self.control_url)
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPAction", &format!("\"{}#{action}\"", self.service))
            .send(envelope)
            .with_context(|| format!("router didn't answer {action}"))?;
        let ok = resp.status().is_success();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if !ok {
            let why = tag(&text, "errorDescription").unwrap_or("refused");
            bail!("router refused {action}: {why}");
        }
        Ok(text)
    }

    pub fn external_ip(&self) -> Result<Ipv4Addr> {
        let reply = self.soap("GetExternalIPAddress", &[])?;
        tag(&reply, "NewExternalIPAddress").and_then(|s| s.parse().ok()).ok_or_else(|| anyhow!("router didn't say its public address"))
    }

    pub fn add_udp_port(&self, port: u16, description: &str) -> Result<()> {
        self.soap(
            "AddPortMapping",
            &[
                ("NewRemoteHost", String::new()),
                ("NewExternalPort", port.to_string()),
                ("NewProtocol", "UDP".into()),
                ("NewInternalPort", port.to_string()),
                ("NewInternalClient", self.local_ip.to_string()),
                ("NewEnabled", "1".into()),
                ("NewPortMappingDescription", description.into()),
                ("NewLeaseDuration", "0".into()),
            ],
        )
        .map(|_| ())
    }

    pub fn remove_udp_port(&self, port: u16) -> Result<()> {
        self.soap("DeletePortMapping", &[("NewRemoteHost", String::new()), ("NewExternalPort", port.to_string()), ("NewProtocol", "UDP".into())]).map(|_| ())
    }
}

/// Addresses that are not on the public internet: the router itself is behind another NAT (CGNAT), so
/// nobody outside can reach a server at home whatever ports are opened.
pub fn is_carrier_or_private(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || (o[0] == 100 && (64..128).contains(&o[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgnat_ranges() {
        assert!(is_carrier_or_private("100.64.1.2".parse().unwrap()));
        assert!(is_carrier_or_private("100.127.255.1".parse().unwrap()));
        assert!(is_carrier_or_private("192.168.1.10".parse().unwrap()));
        assert!(is_carrier_or_private("10.0.0.1".parse().unwrap()));
        assert!(!is_carrier_or_private("100.128.0.1".parse().unwrap()));
        assert!(!is_carrier_or_private("203.0.113.7".parse().unwrap()));
    }

    #[test]
    fn reads_device_tags() {
        let xml = "<root><friendlyName>Home Router</friendlyName><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/ctl/IPConn</controlURL></service></root>";
        assert_eq!(tag(xml, "friendlyName"), Some("Home Router"));
        let svc = xml.split("<service>").nth(1).unwrap();
        assert_eq!(tag(svc, "controlURL"), Some("/ctl/IPConn"));
    }

    #[test]
    #[ignore = "talks to the local router"]
    fn live_router() {
        match discover() {
            Ok(r) => println!("router {:?} local {} external {:?}", r.name, r.local_ip, r.external_ip()),
            Err(e) => println!("no UPnP router: {e:#}"),
        }
    }
}
