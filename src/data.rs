//! Servers (from the subscription), apps (running processes and ones with rules), and the rule types.
//! Entries are interned for the life of the process, so views can keep `&'static` references to them.
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, RwLock};
use std::time::Instant;

use crate::theme::{AMBER, CYAN, EMERALD, INDIGO, RED, SKY, VIOLET};

/// Leak a string once and hand out the same reference afterwards.
pub fn intern(s: &str) -> &'static str {
    static ALL: LazyLock<Mutex<HashSet<&'static str>>> = LazyLock::new(Default::default);
    let mut all = ALL.lock().unwrap();
    if let Some(found) = all.get(s) {
        return found;
    }
    let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
    all.insert(leaked);
    leaked
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Protocol {
    Tls,
    Grpc,
    Hy2,
}

impl Protocol {
    pub const ALL: [Protocol; 3] = [Protocol::Tls, Protocol::Grpc, Protocol::Hy2];
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Tls => "TLS",
            Protocol::Grpc => "gRPC",
            Protocol::Hy2 => "Hysteria2",
        }
    }
}

pub struct Server {
    pub id: &'static str,
    pub country: &'static str,
    pub code: &'static str,
    /// Exit city once seen through the tunnel, else the subscription's name for the node ("Server #2").
    pub city: &'static str,
    /// Exit address once seen through the tunnel.
    pub ip: &'static str,
    pub lat: f64,
    pub lon: f64,
    /// Core outbound per transport, as the subscription published it.
    pub variants: Vec<(Protocol, serde_json::Value)>,
}

impl Server {
    /// The chosen transport, else the first one this node offers in TLS, gRPC, Hysteria2 order.
    pub fn variant(&self, want: Protocol) -> Option<(Protocol, &serde_json::Value)> {
        let pick = |p: Protocol| self.variants.iter().find(|(q, _)| *q == p).map(|(q, v)| (*q, v));
        pick(want).or_else(|| Protocol::ALL.iter().find_map(|p| pick(*p)))
    }

    /// Transports that can carry another hop (no UDP-only ones).
    pub fn chainable(&self, want: Protocol) -> Option<(Protocol, &serde_json::Value)> {
        match self.variant(want) {
            Some((Protocol::Hy2, _)) | None => self.variant(Protocol::Tls).filter(|(p, _)| *p != Protocol::Hy2),
            some => some,
        }
    }
}

static NONE: LazyLock<Server> =
    LazyLock::new(|| Server { id: "none", country: "No servers", code: "", city: "Sign in to connect", ip: "", lat: 52., lon: 30., variants: vec![] });
static SERVERS: RwLock<Vec<&'static Server>> = RwLock::new(Vec::new());
static PINGS: LazyLock<RwLock<HashMap<&'static str, u32>>> = LazyLock::new(Default::default);

pub fn servers() -> Vec<&'static Server> {
    SERVERS.read().unwrap().clone()
}

pub fn set_servers(list: Vec<Server>) {
    *SERVERS.write().unwrap() = list.into_iter().map(|s| &*Box::leak(Box::new(s))).collect();
}

pub fn server(id: &str) -> &'static Server {
    let all = SERVERS.read().unwrap();
    all.iter().find(|s| s.id == id).copied().or_else(|| all.first().copied()).unwrap_or(&NONE)
}

pub fn has_server(id: &str) -> bool {
    SERVERS.read().unwrap().iter().any(|s| s.id == id)
}

/// Replace one server's exit details after seeing them through the tunnel.
pub fn learn_exit(id: &str, ip: &str, city: Option<&str>, lat: f64, lon: f64) {
    let mut all = SERVERS.write().unwrap();
    if let Some(slot) = all.iter_mut().find(|s| s.id == id) {
        let s = *slot;
        let city = city.map(intern).unwrap_or(s.city);
        if s.ip == ip && s.city == city && s.lat == lat && s.lon == lon {
            return;
        }
        *slot = Box::leak(Box::new(Server { ip: intern(ip), city, lat, lon, variants: s.variants.clone(), ..*s }));
    }
}

/// Measured round trip through the server (the core's HTTP probe), if any.
pub fn ping(id: &str) -> Option<u32> {
    PINGS.read().unwrap().get(id).copied()
}

pub fn set_pings(pings: impl IntoIterator<Item = (&'static str, Option<u32>)>) {
    let mut all = PINGS.write().unwrap();
    for (id, ms) in pings {
        match ms {
            Some(ms) => all.insert(id, ms),
            None => all.remove(id),
        };
    }
}

/// Lowest measured ping; the first server until something is measured.
pub fn fastest() -> &'static Server {
    let all = servers();
    all.iter().filter_map(|s| ping(s.id).map(|p| (p, *s))).min_by_key(|(p, _)| *p).map(|(_, s)| s).or_else(|| all.first().copied()).unwrap_or(&NONE)
}

/// English name and capital of the countries nodes are likely to be in, for the list and the map before an exit is seen.
pub fn country(code: &str) -> Option<(&'static str, f64, f64)> {
    Some(match code {
        "AE" => ("United Arab Emirates", 24.45, 54.38),
        "AM" => ("Armenia", 40.18, 44.51),
        "AT" => ("Austria", 48.21, 16.37),
        "AU" => ("Australia", -33.87, 151.21),
        "BE" => ("Belgium", 50.85, 4.35),
        "BG" => ("Bulgaria", 42.70, 23.32),
        "BR" => ("Brazil", -23.55, -46.63),
        "BY" => ("Belarus", 53.90, 27.57),
        "CA" => ("Canada", 43.65, -79.38),
        "CH" => ("Switzerland", 47.37, 8.54),
        "CZ" => ("Czechia", 50.08, 14.44),
        "DE" => ("Germany", 50.11, 8.68),
        "DK" => ("Denmark", 55.68, 12.57),
        "EE" => ("Estonia", 59.44, 24.75),
        "ES" => ("Spain", 40.42, -3.70),
        "FI" => ("Finland", 60.17, 24.94),
        "FR" => ("France", 48.86, 2.35),
        "GB" => ("United Kingdom", 51.51, -0.13),
        "GE" => ("Georgia", 41.72, 44.79),
        "GR" => ("Greece", 37.98, 23.73),
        "HK" => ("Hong Kong", 22.32, 114.17),
        "HU" => ("Hungary", 47.50, 19.04),
        "IE" => ("Ireland", 53.35, -6.26),
        "IL" => ("Israel", 32.09, 34.78),
        "IN" => ("India", 19.08, 72.88),
        "IS" => ("Iceland", 64.15, -21.94),
        "IT" => ("Italy", 45.46, 9.19),
        "JP" => ("Japan", 35.68, 139.69),
        "KR" => ("South Korea", 37.57, 126.98),
        "KZ" => ("Kazakhstan", 43.24, 76.89),
        "LT" => ("Lithuania", 54.69, 25.28),
        "LU" => ("Luxembourg", 49.61, 6.13),
        "LV" => ("Latvia", 56.95, 24.11),
        "MD" => ("Moldova", 47.01, 28.86),
        "NL" => ("Netherlands", 52.37, 4.90),
        "NO" => ("Norway", 59.91, 10.75),
        "PL" => ("Poland", 52.23, 21.01),
        "PT" => ("Portugal", 38.72, -9.14),
        "RO" => ("Romania", 44.43, 26.10),
        "RS" => ("Serbia", 44.79, 20.45),
        "RU" => ("Russia", 55.76, 37.62),
        "SE" => ("Sweden", 59.33, 18.07),
        "SG" => ("Singapore", 1.35, 103.82),
        "TR" => ("Turkey", 41.01, 28.98),
        "UA" => ("Ukraine", 50.45, 30.52),
        "US" => ("United States", 40.71, -74.01),
        _ => return None,
    })
}

// ---------- apps ----------

pub struct AppInfo {
    /// Normalised executable name; rules are keyed by it.
    pub id: &'static str,
    pub name: &'static str,
    /// Executable as the core sees it (`/proc/<pid>/exe` basename).
    pub process: &'static str,
    pub initials: &'static str,
    /// Badge: background colour (shown at 20%) and text colour.
    pub badge: (u32, u32),
}

/// Apps that run under several executables, and nicer names for common ones.
const KNOWN: &[(&str, &str, &[&str], (u32, u32))] = &[
    ("helium", "Helium", &["helium"], (SKY, 0x00bcff)),
    ("chromium", "Chromium", &["chromium", "chrome"], (SKY, 0x00bcff)),
    ("firefox", "Firefox", &["firefox", "firefox-bin"], (AMBER, 0xffd230)),
    ("telegram", "Telegram", &["telegram-desktop", "Telegram"], (CYAN, 0x53eafd)),
    ("materialgram", "Materialgram", &["materialgram"], (CYAN, 0x53eafd)),
    ("steam", "Steam", &["steam", "steamwebhelper"], (INDIGO, 0xa3b3ff)),
    ("discord", "Discord", &["Discord", "discord", "DiscordCanary", "vesktop"], (VIOLET, 0xc4b4ff)),
    ("sunshine", "Sunshine", &["sunshine"], (AMBER, 0xffd230)),
    ("qbittorrent", "qBittorrent", &["qbittorrent", "qbittorrent-nox"], (RED, 0xff6467)),
    ("spotify", "Spotify", &["spotify"], (EMERALD, 0x5ee9b5)),
];

const BADGES: [(u32, u32); 7] = [(SKY, 0x00bcff), (CYAN, 0x53eafd), (INDIGO, 0xa3b3ff), (VIOLET, 0xc4b4ff), (AMBER, 0xffd230), (RED, 0xff6467), (EMERALD, 0x5ee9b5)];

/// Background services that talk to the network but are not apps anyone routes.
const HIDDEN: &[&str] = &["xray", "hitori", "bash", "sh", "coreutils", "ssh", "pipewire", "wireplumber", "node", "python3", "srt-logger", "p11-kit-server", "Xwayland"];

/// `.Telegram-wrapped` (NixOS wrappers) → `telegram`; known aliases → their app.
pub fn app_id(exe: &str) -> String {
    let base = exe.trim_start_matches('.').trim_end_matches("-wrapped").trim_end_matches("-bin");
    KNOWN
        .iter()
        .find(|(_, _, names, _)| names.contains(&base) || names.contains(&exe))
        .map(|(id, ..)| (*id).to_owned())
        .unwrap_or_else(|| base.to_lowercase())
}

/// Every executable name an app's traffic can come from.
pub fn processes(id: &str) -> Vec<String> {
    let mut names: Vec<String> = KNOWN.iter().find(|k| k.0 == id).map(|k| k.2.iter().map(|s| (*s).to_owned()).collect()).unwrap_or_default();
    names.extend(SEEN.read().unwrap().iter().filter(|(exe, _)| app_id(exe) == id).map(|(exe, _)| (*exe).to_owned()));
    for base in names.clone().iter().chain(std::iter::once(&id.to_owned())) {
        names.push(base.clone());
        names.push(format!(".{base}-wrapped"));
    }
    names.sort();
    names.dedup();
    names
}

static APPS: LazyLock<RwLock<HashMap<&'static str, &'static AppInfo>>> = LazyLock::new(Default::default);
/// Executable → when it last had an internet socket open.
static SEEN: LazyLock<RwLock<HashMap<&'static str, Instant>>> = LazyLock::new(Default::default);

pub fn app(id: &str) -> &'static AppInfo {
    if let Some(a) = APPS.read().unwrap().get(id) {
        return a;
    }
    let known = KNOWN.iter().find(|k| k.0 == id);
    let name = known.map(|k| k.1.to_owned()).unwrap_or_else(|| {
        let mut c = id.chars();
        c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
    });
    let letters: String = name.chars().filter(|c| c.is_alphanumeric()).take(2).collect::<String>().to_uppercase();
    let badge = known.map(|k| k.3).unwrap_or_else(|| BADGES[id.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize)) % BADGES.len()]);
    let process = SEEN.read().unwrap().keys().find(|exe| app_id(exe) == id).copied().or(known.map(|k| k.2[0])).unwrap_or(intern(id));
    let info: &'static AppInfo = Box::leak(Box::new(AppInfo { id: intern(id), name: intern(&name), process, initials: intern(&letters), badge }));
    APPS.write().unwrap().insert(info.id, info);
    info
}

/// Record executables that have internet sockets now (see `apps::scan`).
pub fn saw(exes: &[String]) {
    let now = Instant::now();
    let mut seen = SEEN.write().unwrap();
    for exe in exes {
        if !HIDDEN.contains(&app_id(exe).as_str()) {
            seen.insert(intern(exe), now);
        }
    }
}

/// How long an app counts as in use after the last scan that saw a connection, so a quiet moment does not hide it.
const ACTIVE_FOR: std::time::Duration = std::time::Duration::from_secs(60);

/// The app had a connection open within the last minute.
pub fn active(id: &str) -> bool {
    SEEN.read().unwrap().iter().any(|(exe, t)| t.elapsed() < ACTIVE_FOR && app_id(exe) == id)
}

/// Minutes since any of the app's executables had a connection open; `None` if not seen this session.
pub fn seen_min(id: &str) -> Option<u32> {
    let seen = SEEN.read().unwrap();
    seen.iter().filter(|(exe, _)| app_id(exe) == id).map(|(_, t)| (t.elapsed().as_secs() / 60) as u32).min()
}

/// Apps seen this session, most recent first.
pub fn running() -> Vec<&'static AppInfo> {
    let mut ids: Vec<(String, Instant)> = vec![];
    for (exe, t) in SEEN.read().unwrap().iter() {
        let id = app_id(exe);
        match ids.iter_mut().find(|(i, _)| *i == id) {
            Some(slot) => slot.1 = slot.1.max(*t),
            None => ids.push((id, *t)),
        }
    }
    ids.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ids.into_iter().map(|(id, _)| app(&id)).collect()
}

// ---------- rules ----------

/// Auto: direct first, VPN when it fails · Vpn: always tunnelled · Direct: never tunnelled · Block: no network.
/// Also the default route: where traffic without a rule goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RuleMode {
    Auto,
    Vpn,
    Direct,
    Block,
}

impl RuleMode {
    pub const ALL: [RuleMode; 4] = [RuleMode::Auto, RuleMode::Vpn, RuleMode::Direct, RuleMode::Block];
    /// A website has no "direct first" of its own: it goes through the VPN, direct, or nowhere.
    pub const SITE: [RuleMode; 3] = [RuleMode::Vpn, RuleMode::Direct, RuleMode::Block];

    pub fn key(self) -> &'static str {
        match self {
            RuleMode::Auto => "auto",
            RuleMode::Vpn => "vpn",
            RuleMode::Direct => "direct",
            RuleMode::Block => "block",
        }
    }

    pub fn parse(key: &str) -> Self {
        RuleMode::ALL.into_iter().find(|m| m.key() == key).unwrap_or(RuleMode::Auto)
    }

    pub fn label(self) -> &'static str {
        match self {
            RuleMode::Auto => "Auto",
            RuleMode::Vpn => "VPN",
            RuleMode::Direct => "Direct",
            RuleMode::Block => "Block",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            RuleMode::Auto => "wand-sparkles",
            RuleMode::Vpn => "shield-check",
            RuleMode::Direct => "globe",
            RuleMode::Block => "ban",
        }
    }

    /// Unruled traffic goes out directly first, so failures there can become suggestions.
    pub fn tries_direct(self) -> bool {
        matches!(self, RuleMode::Auto | RuleMode::Direct)
    }

    /// As the default route.
    pub fn default_hint(self) -> &'static str {
        match self {
            RuleMode::Auto => "Direct first, VPN when it fails",
            RuleMode::Vpn => "Everything through the VPN",
            RuleMode::Direct => "Only your rules use the VPN",
            RuleMode::Block => "Only apps and sites you allow",
        }
    }
}

/// One app's or website's route. `None` fields follow the main connection; without a rule the default route applies.
#[derive(Clone, PartialEq, Debug)]
pub struct AppRule {
    pub mode: RuleMode,
    /// Own exit server instead of the main connection's.
    pub exit: Option<&'static str>,
    /// Server traffic passes through before the exit (per-app cascade).
    pub via: Option<&'static str>,
    pub protocol: Option<Protocol>,
    /// Block this app while the tunnel is down.
    pub kill: bool,
}

impl AppRule {
    pub const AUTO: AppRule = AppRule { mode: RuleMode::Auto, exit: None, via: None, protocol: None, kill: false };
    pub fn mode(mode: RuleMode) -> Self {
        Self { mode, ..Self::AUTO }
    }
    /// Changes nothing: the app just follows the default route.
    pub fn is_default(&self, default: RuleMode) -> bool {
        *self == Self::mode(default)
    }
}

/// `https://www.Example.com/path` → `example.com`; `None` unless it looks like a domain.
pub fn domain(input: &str) -> Option<String> {
    let s = input.trim().to_lowercase();
    let s = s.split_once("://").map_or(s.as_str(), |(_, r)| r);
    let host = s.split(['/', '?', '#', ':']).next()?.trim_start_matches("www.").trim_end_matches('.');
    let ok = host.contains('.')
        && host.len() <= 253
        && host.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-'))
        && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    ok.then(|| host.to_owned())
}

#[derive(Clone, Copy, PartialEq)]
pub enum Scope {
    App,
    Site,
}

/// A site that failed directly and answered through the tunnel.
#[derive(Clone, PartialEq)]
pub struct Suggestion {
    pub id: &'static str,
    /// App the traffic came from; empty when unknown.
    pub app_id: &'static str,
    pub target: &'static str,
    pub scope: Scope,
    pub reason: &'static str,
    pub via_id: &'static str,
    pub hits: u32,
    pub at: Instant,
    pub speed: Option<(f64, f64)>,
}

// ---------- account ----------

#[derive(Clone, PartialEq, Default)]
pub struct Account {
    /// Panel username (the Telegram id for bot-created accounts).
    pub name: String,
    pub title: String,
    pub active: bool,
    pub status: String,
    pub days_left: Option<u32>,
    pub expires: Option<String>,
    pub used: u64,
    pub limit: u64,
    pub support: Option<String>,
}

impl Account {
    pub fn plan(&self) -> String {
        if !self.active {
            return title_case(&self.status);
        }
        if self.title.is_empty() { "Active".into() } else { self.title.clone() }
    }

    pub fn summary(&self) -> String {
        match self.days_left {
            Some(d) => format!("{} · {d} days left", self.plan()),
            None => self.plan(),
        }
    }
}

fn title_case(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut c = lower.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// `2029-02-04T13:30:14Z` → `4 Feb 2029`.
pub fn date(iso: &str) -> Option<String> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mut parts = iso.get(..10)?.split('-');
    let (y, m, d): (u32, usize, u32) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    Some(format!("{d} {} {y}", MONTHS.get(m.checked_sub(1)?)?))
}

pub fn bytes(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024. && i < units.len() - 1 {
        v /= 1024.;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", units[i]) }
}

pub fn ago(min: u32) -> String {
    if min == 0 { "now".into() } else if min < 60 { format!("{min}m") } else { format!("{}h", min / 60) }
}

/// JS-style `String(number)` for one-decimal values: `3` rather than `3.0`.
pub fn fixed1(v: f64) -> String {
    format!("{v:.1}")
}

/// JS number to string (no trailing `.0`).
pub fn js_num(v: f64) -> String {
    if v.fract() == 0. { format!("{}", v as i64) } else { format!("{v}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_from_what_people_paste() {
        assert_eq!(domain("https://www.YouTube.com/watch?v=1").as_deref(), Some("youtube.com"));
        assert_eq!(domain("chatgpt.com").as_deref(), Some("chatgpt.com"));
        assert_eq!(domain("sub.example.org:8443/x").as_deref(), Some("sub.example.org"));
        for bad in ["", "localhost", "exa mple.com", "a..b", "-x.com", "x\".com", "домен.рф"] {
            assert_eq!(domain(bad), None, "{bad}");
        }
    }

    #[test]
    fn app_ids_fold_wrappers_and_aliases() {
        assert_eq!(app_id(".Telegram-wrapped"), "telegram");
        assert_eq!(app_id("steamwebhelper"), "steam");
        assert_eq!(app_id("helium"), "helium");
        assert_eq!(app_id(".foot-wrapped"), "foot");
        let steam = processes("steam");
        assert!(steam.contains(&"steamwebhelper".to_owned()) && steam.contains(&".steam-wrapped".to_owned()));
    }

    #[test]
    fn dates_and_sizes() {
        assert_eq!(date("2029-02-04T13:30:14.755Z").as_deref(), Some("4 Feb 2029"));
        assert_eq!(date("bad"), None);
        assert_eq!(bytes(240390509781), "223.9 GB");
        assert_eq!(bytes(12), "12 B");
    }
}
