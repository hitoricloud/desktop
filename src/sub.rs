//! The subscription: Remnawave's Xray-JSON format (one complete config per host) plus the public `/info` endpoint.
use std::io::Read;
use std::time::Duration;

use serde_json::{Value, json};

use crate::data::{self, Account, Protocol, Server};

pub struct Subscription {
    pub servers: Vec<Server>,
    /// Lists that need the tunnel (the Auto profile's balancer rules), retargeted at `proxy`.
    pub tunnel_rules: Vec<Value>,
    /// Provider rules every profile carries (e.g. BitTorrent direct).
    pub base_rules: Vec<Value>,
    pub dns: Value,
}

pub struct Fetched {
    pub body: Vec<u8>,
    pub title: Option<String>,
    pub support: Option<String>,
}

const FORMATS: &[&str] = &["json", "v2ray-json", "xray-json", "singbox", "sing-box", "mihomo", "stash", "clash", "v2ray", "info"];

/// The subscription link without a format suffix or query.
pub fn base(url: &str) -> Result<String, String> {
    let url = url.trim();
    let url = url.split(['?', '#']).next().unwrap_or(url).trim_end_matches('/');
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(t!("sub.error.paste_full_link").into());
    }
    match url.rsplit_once('/') {
        Some((head, last)) if FORMATS.contains(&last) && head.matches('/').count() > 3 => Ok(head.to_owned()),
        _ => Ok(url.to_owned()),
    }
}

/// The system as the panel (`x-device-os`) and the device card name it.
pub const OS: &str = if cfg!(target_os = "macos") {
    "macOS"
} else if cfg!(windows) {
    "Windows"
} else {
    "Linux"
};

/// Trimmed output of a system tool; no console window flashes up on Windows.
#[cfg(not(target_os = "linux"))]
fn tool(cmd: &str, args: &[&str]) -> Option<String> {
    let mut c = std::process::Command::new(cmd);
    c.args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000);
    let out = c.output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|s| !s.is_empty())
}

/// `reg query <key> /v <name>`: the value after the type column.
#[cfg(windows)]
fn registry(key: &str, name: &str) -> Option<String> {
    let out = tool("reg", &["query", key, "/v", name])?;
    out.lines().find(|l| l.trim_start().starts_with(name))?.split_whitespace().nth(2).map(str::to_owned)
}

fn machine_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    let id = std::fs::read_to_string("/etc/machine-id").or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id")).ok();
    #[cfg(target_os = "macos")]
    let id = tool("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"])
        .and_then(|o| o.lines().find(|l| l.contains("\"IOPlatformUUID\""))?.rsplit('"').nth(1).map(str::to_owned));
    #[cfg(windows)]
    let id = registry(r"HKLM\SOFTWARE\Microsoft\Cryptography", "MachineGuid");
    id.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// Stable per-machine id for the panel's device limit (Remnawave `x-hwid`).
pub fn hwid() -> String {
    use std::hash::{Hash, Hasher};
    static HWID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HWID.get_or_init(|| {
        let seed = machine_id().unwrap_or_else(hostname);
        let half = |salt: &str| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (salt, seed.as_str()).hash(&mut h);
            h.finish()
        };
        format!("{:016x}{:016x}", half("hitori-hwid-a"), half("hitori-hwid-b"))
    })
    .clone()
}

pub fn hostname() -> String {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        #[cfg(target_os = "linux")]
        let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok();
        #[cfg(target_os = "macos")]
        let name = tool("scutil", &["--get", "ComputerName"]);
        #[cfg(windows)]
        let name = std::env::var("COMPUTERNAME").ok();
        name.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| OS.to_lowercase())
    })
    .clone()
}

fn os_version() -> String {
    #[cfg(target_os = "linux")]
    let v = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    #[cfg(target_os = "macos")]
    let v = tool("sw_vers", &["-productVersion"]);
    #[cfg(windows)]
    let v = registry(r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion", "CurrentBuild");
    v.map(|s| s.trim().to_owned()).unwrap_or_default()
}

/// Header values must be visible ASCII (ureq rejects the rest with `BadHeader`): percent-encode UTF-8, controls and `%`.
fn header_value(s: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b'%');
    percent_encoding::utf8_percent_encode(s, SET).to_string()
}

fn request(url: &str) -> ureq::Request {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(25))
        .build()
        .get(url)
        .set("User-Agent", &format!("Hitori/{} ({OS})", env!("CARGO_PKG_VERSION")))
        .set("x-hwid", &hwid())
        .set("x-device-os", OS)
        .set("x-ver-os", &header_value(&os_version()))
        .set("x-device-model", &header_value(&hostname()))
}

fn decode_title(v: &str) -> String {
    use base64::Engine;
    match v.strip_prefix("base64:") {
        Some(b) => base64::engine::general_purpose::STANDARD.decode(b.trim()).ok().and_then(|b| String::from_utf8(b).ok()).unwrap_or_default(),
        None => v.to_owned(),
    }
}

fn status_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(404, _) => t!("sub.error.not_found").into(),
        ureq::Error::Status(403, _) => t!("sub.error.refused_device").into(),
        ureq::Error::Status(code, _) => t!("sub.error.server_answered", code = code),
        ureq::Error::Transport(t) => t!("sub.error.cannot_reach", kind = t.kind()),
    }
}

pub fn fetch(base: &str) -> Result<Fetched, String> {
    let resp = request(&format!("{base}/json")).call().map_err(status_error)?;
    let title = resp.header("profile-title").map(decode_title).filter(|t| !t.is_empty());
    let support = resp.header("support-url").map(str::to_owned);
    let mut body = Vec::new();
    resp.into_reader().take(16 << 20).read_to_end(&mut body).map_err(|e| e.to_string())?;
    Ok(Fetched { body, title, support })
}

/// Account details from the panel's public `/info` (no credentials beyond the link itself).
pub fn info(base: &str, title: Option<String>, support: Option<String>) -> Option<Account> {
    let v: Value = request(&format!("{base}/info")).call().ok()?.into_json().ok()?;
    let u = &v["response"]["user"];
    let num = |v: &Value| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
    Some(Account {
        name: u["username"].as_str().unwrap_or_default().to_owned(),
        title: title.unwrap_or_default(),
        active: u["isActive"].as_bool().unwrap_or(false),
        status: u["userStatus"].as_str().unwrap_or_default().to_owned(),
        days_left: u["daysLeft"].as_u64().map(|d| d as u32),
        expires: u["expiresAt"].as_str().and_then(data::date),
        used: num(&u["trafficUsedBytes"]),
        limit: num(&u["trafficLimitBytes"]),
        support,
    })
}

/// 🇪🇪 → "EE".
fn flag_code(s: &str) -> Option<(String, &str)> {
    let mut chars = s.char_indices();
    let mut code = String::new();
    for _ in 0..2 {
        let (_, c) = chars.next()?;
        let n = c as u32;
        if !(0x1F1E6..=0x1F1FF).contains(&n) {
            return None;
        }
        code.push(char::from_u32(n - 0x1F1E6 + 'A' as u32)?);
    }
    let rest = chars.next().map(|(i, _)| &s[i..]).unwrap_or("");
    Some((code, rest.trim()))
}

fn protocol(outbound: &Value) -> Option<Protocol> {
    let stream = &outbound["streamSettings"];
    match (outbound["protocol"].as_str()?, stream["network"].as_str().unwrap_or("tcp")) {
        ("hysteria", _) => Some(Protocol::Hy2),
        ("vless" | "trojan" | "vmess", "grpc") => Some(Protocol::Grpc),
        ("vless" | "trojan" | "vmess", "tcp" | "raw") => Some(Protocol::Tls),
        _ => None,
    }
}

pub fn parse(body: &[u8]) -> Result<Subscription, String> {
    let configs: Value = serde_json::from_slice(body).map_err(|_| t!("sub.error.no_profiles").to_owned())?;
    let configs = configs.as_array().ok_or(t!("sub.error.no_profiles"))?;
    let mut servers: Vec<Server> = vec![];
    let mut tunnel_rules = vec![];
    let mut base_rules = None;
    let mut dns = None;
    for c in configs {
        let remarks = c["remarks"].as_str().unwrap_or_default();
        let rules = c["routing"]["rules"].as_array().cloned().unwrap_or_default();
        if c["routing"]["balancers"].as_array().is_some_and(|b| !b.is_empty()) {
            if tunnel_rules.is_empty() {
                tunnel_rules = rules
                    .into_iter()
                    .filter_map(|mut r| {
                        let obj = r.as_object_mut()?;
                        obj.remove("balancerTag")?;
                        obj.insert("outboundTag".into(), json!("proxy"));
                        Some(r)
                    })
                    .collect();
            }
            continue;
        }
        // Hosts without a country flag are dividers or special-purpose (e.g. mobile-only) profiles.
        let Some((code, rest)) = flag_code(remarks) else { continue };
        let Some(outbound) = c["outbounds"].as_array().and_then(|o| o.iter().find(|o| o["tag"] == "proxy")) else { continue };
        let Some(proto) = protocol(outbound) else { continue };
        base_rules.get_or_insert(rules);
        dns.get_or_insert(c["dns"].clone());
        let name = rest.split(" - ").next().unwrap_or(rest).trim();
        let number = name.rsplit_once('#').and_then(|(_, n)| n.trim().parse::<u32>().ok());
        let id = data::intern(&match number {
            Some(n) => format!("{}-{n}", code.to_lowercase()),
            None => code.to_lowercase(),
        });
        let mut outbound = outbound.clone();
        outbound.as_object_mut().map(|o| o.remove("tag"));
        if let Some(s) = servers.iter_mut().find(|s| s.id == id) {
            if s.variants.iter().all(|(p, _)| *p != proto) {
                s.variants.push((proto, outbound));
            }
            continue;
        }
        let (country, lat, lon) = data::country(&code).unwrap_or(("", 50., 10.));
        let country = if country.is_empty() { data::intern(name.split('#').next().unwrap_or(name).trim()) } else { country };
        let city = data::intern(&number.map(|n| t!("sub.label.server_number", n = n)).unwrap_or_else(|| t!("sub.label.server").into()));
        servers.push(Server { id, country, code: data::intern(&code), city, ip: "", lat, lon, variants: vec![(proto, outbound)] });
    }
    if servers.is_empty() {
        return Err(t!("sub.error.no_servers").into());
    }
    for s in &mut servers {
        s.variants.sort_by_key(|(p, _)| Protocol::ALL.iter().position(|q| q == p));
    }
    Ok(Subscription { servers, tunnel_rules, base_rules: base_rules.unwrap_or_default(), dns: dns.unwrap_or_else(|| json!({"servers": ["1.1.1.1", "1.0.0.1"]})) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(remarks: &str, outbound: Value) -> Value {
        json!({"remarks": remarks, "dns": {"servers": ["1.1.1.1"]}, "routing": {"rules": [{"type": "field", "protocol": ["bittorrent"], "outboundTag": "direct"}]},
               "outbounds": [outbound, {"tag": "direct", "protocol": "freedom"}]})
    }

    #[test]
    fn groups_hosts_into_servers() {
        let tcp = json!({"tag": "proxy", "protocol": "vless", "streamSettings": {"network": "tcp", "security": "reality"}});
        let grpc = json!({"tag": "proxy", "protocol": "vless", "streamSettings": {"network": "grpc", "security": "reality"}});
        let hy = json!({"tag": "proxy", "protocol": "hysteria", "streamSettings": {"network": "hysteria"}});
        let xhttp = json!({"tag": "proxy", "protocol": "vless", "streamSettings": {"network": "xhttp"}});
        let auto = json!({"remarks": "Автоматический", "routing": {"balancers": [{"tag": "auto-proxy"}],
            "rules": [{"type": "field", "domain": ["domain:ua"], "balancerTag": "auto-proxy"}, {"type": "field", "network": "tcp,udp", "outboundTag": "direct"}]}, "outbounds": []});
        let body = serde_json::to_vec(&json!([
            auto,
            host("Обход белых списков ", xhttp.clone()),
            host("🇪🇪 Эстония #1", tcp.clone()),
            host("🇪🇪 Эстония #1 - Hysteria2", hy),
            host("🇪🇪 Эстония #1 - GRPC", grpc.clone()),
            host("🇱🇻 Latvia #2 - GRPC", grpc),
            host("⬇ Для доверенных ⬇", tcp.clone()),
            host("🇩🇪 Германия #2", xhttp),
        ]))
        .unwrap();
        let sub = parse(&body).unwrap();
        let ids: Vec<_> = sub.servers.iter().map(|s| s.id).collect();
        assert_eq!(ids, ["ee-1", "lv-2"]);
        let ee = &sub.servers[0];
        assert_eq!((ee.country, ee.code, ee.city), ("Estonia", "EE", "Server #1"));
        assert_eq!(ee.variants.iter().map(|v| v.0).collect::<Vec<_>>(), [Protocol::Tls, Protocol::Grpc, Protocol::Hy2]);
        assert!(ee.variants[0].1.get("tag").is_none());
        let lv = &sub.servers[1];
        assert_eq!(lv.variant(Protocol::Hy2).unwrap().0, Protocol::Grpc);
        assert_eq!(sub.tunnel_rules, vec![json!({"type": "field", "domain": ["domain:ua"], "outboundTag": "proxy"})]);
        assert_eq!(sub.base_rules[0]["protocol"][0], "bittorrent");
    }

    #[test]
    fn link_normalisation() {
        assert_eq!(base("https://sub.example/api/sub/AbC/json?x=1").unwrap(), "https://sub.example/api/sub/AbC");
        assert_eq!(base(" https://sub.example/sub/AbC/ ").unwrap(), "https://sub.example/sub/AbC");
        assert_eq!(base("https://sub.example/json").unwrap(), "https://sub.example/json");
        assert!(base("sub.example/sub/x").is_err());
        assert_eq!(flag_code("🇨🇭 Швейцария #7").unwrap(), ("CH".to_owned(), "Швейцария #7"));
        assert!(flag_code("Автоматический").is_none());
        assert_eq!(decode_title("base64:aGl0b3JpdnBu"), "hitorivpn");
    }

    #[test]
    fn header_values_are_ascii() {
        assert_eq!(header_value("MacBook Air — oki"), "MacBook Air %E2%80%94 oki");
        assert_eq!(header_value("ПК"), "%D0%9F%D0%9A");
        assert_eq!(header_value("100% Mac\t1"), "100%25 Mac%091");
        assert_eq!(header_value("MacBook-Pro"), "MacBook-Pro");
    }
}
