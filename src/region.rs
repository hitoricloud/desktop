//! Where the user really is (public IP looked up outside the tunnel, cached by IP) and the ready-made lists of what is
//! blocked there: compact text lists from maintained community projects, refreshed daily and kept for offline use.
use std::collections::HashSet;
use std::net::{IpAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::net::{self, Geo};

/// The public IP is looked up again after this long even when the local network looks the same.
pub const RECHECK: Duration = Duration::from_secs(30 * 60);
/// Downloaded lists are refreshed after a day; older copies keep working offline.
const TTL: Duration = Duration::from_secs(24 * 3600);
/// Keeps the core config small: the rank-ordered lists come last and are cut here.
const MAX_ENTRIES: usize = 20_000;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Domains,
    Subnets,
}

/// A file in a GitHub repository, fetched raw or through jsDelivr where GitHub itself is blocked.
#[derive(Clone, Debug)]
pub struct Source {
    repo: &'static str,
    branch: &'static str,
    path: String,
    kind: Kind,
}

impl Source {
    fn new(repo: &'static str, branch: &'static str, path: impl Into<String>, kind: Kind) -> Self {
        Self { repo, branch, path: path.into(), kind }
    }

    fn urls(&self) -> [String; 2] {
        let Self { repo, branch, path, .. } = self;
        [format!("https://raw.githubusercontent.com/{repo}/{branch}/{path}"), format!("https://cdn.jsdelivr.net/gh/{repo}@{branch}/{path}")]
    }

    fn file(&self) -> String {
        format!("{}-{}", self.repo.replace('/', "_"), self.path.replace('/', "_"))
    }
}

/// Services whose own subnets itdoginfo lists for Russia (blocked, or throttled hosting).
const RU_SUBNETS: [&str; 9] = ["telegram", "meta", "twitter", "discord", "cloudflare", "cloudfront", "hetzner", "ovh", "digitalocean"];

/// What is known to be blocked in a country (ISO code); empty where there is no maintained list.
pub fn sources(country: &str) -> Vec<Source> {
    use Kind::*;
    match country {
        "RU" => {
            let mut v = vec![Source::new("itdoginfo/allow-domains", "main", "Russia/inside-raw.lst", Domains)];
            for family in ["IPv4", "IPv6"] {
                v.extend(RU_SUBNETS.map(|s| Source::new("itdoginfo/allow-domains", "main", format!("Subnets/{family}/{s}.lst"), Subnets)));
            }
            v.push(Source::new("runetfreedom/russia-blocked-geoip", "release", "text/ru-blocked-community.txt", Subnets));
            v
        }
        // Sites that refuse Iranian users, then what the filter blocks, most visited first.
        "IR" => {
            let mut v = vec![
                Source::new("Chocolate4U/Iran-v2ray-rules", "release", "sanctioned.txt", Domains),
                Source::new("armanlife/iran-blocked-domains", "main", "data/ir-blocked", Domains),
            ];
            v.extend(["telegram", "twitter", "facebook"].map(|s| Source::new("Chocolate4U/Iran-v2ray-rules", "release", format!("text/{s}.txt"), Subnets)));
            v
        }
        "CN" => {
            let mut v = vec![
                Source::new("Loyalsoldier/v2ray-rules-dat", "release", "gfw.txt", Domains),
                Source::new("Loyalsoldier/v2ray-rules-dat", "release", "greatfire.txt", Domains),
            ];
            v.extend(["telegram", "twitter", "facebook", "google"].map(|s| Source::new("Loyalsoldier/geoip", "release", format!("text/{s}.txt"), Subnets)));
            v
        }
        _ => vec![],
    }
}

/// `example.com`, `.example.com`, `+.example.com` and `domain:`/`full:`/`keyword:` lines as Xray matchers. Regexps are
/// left out: one the core cannot compile would keep it from starting.
pub fn domain(line: &str) -> Option<String> {
    let line = line.split('#').next()?.trim();
    if line.is_empty() || line.starts_with("//") || line.starts_with('!') {
        return None;
    }
    let (kind, value) = match line.split_once(':') {
        Some((k @ ("domain" | "full" | "keyword"), v)) => (k, v),
        Some(_) => return None,
        None => ("domain", line),
    };
    // v2fly exports may append attributes: `domain:example.com:@ads`.
    let value = value.split(':').next()?.trim().trim_start_matches("+.").trim_matches('.').to_ascii_lowercase();
    let ok = !value.is_empty() && value.len() <= 253 && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'));
    ok.then(|| format!("{kind}:{value}"))
}

/// `1.2.3.0/24`, `2001:db8::/32` or a bare address, normalised.
pub fn subnet(line: &str) -> Option<String> {
    let line = line.split('#').next()?.trim();
    let (addr, prefix) = line.split_once('/').map_or((line, None), |(a, p)| (a, Some(p)));
    let ip: IpAddr = addr.trim().parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let prefix = match prefix {
        Some(p) => p.trim().parse::<u8>().ok().filter(|p| *p <= max)?,
        None => max,
    };
    Some(format!("{ip}/{prefix}"))
}

fn parse(kind: Kind, text: &str) -> Vec<String> {
    let f = if kind == Kind::Domains { domain } else { subnet };
    text.lines().filter_map(f).collect()
}

#[derive(Clone, Default, PartialEq, Debug)]
pub struct Lists {
    pub domains: Vec<String>,
    pub subnets: Vec<String>,
}

impl Lists {
    /// Routing rules that send the lists through the tunnel.
    pub fn rules(&self) -> Vec<Value> {
        let mut rules = vec![];
        if !self.domains.is_empty() {
            rules.push(json!({"type": "field", "domain": self.domains, "outboundTag": "proxy"}));
        }
        if !self.subnets.is_empty() {
            rules.push(json!({"type": "field", "ip": self.subnets, "outboundTag": "proxy"}));
        }
        rules
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fetch {
    /// Only what is cached (startup, before the network answers).
    Never,
    /// Lists older than the TTL or missing.
    Stale,
    Always,
}

fn stale(path: &Path, now: SystemTime) -> bool {
    std::fs::metadata(path).and_then(|m| m.modified()).map_or(true, |t| now.duration_since(t).unwrap_or_default() >= TTL)
}

/// Every source's entries from the cache in `dir`, downloading as `mode` says; a failed or unusable download (a captive
/// portal page) keeps the cached copy. Returns the merged lists and how many sources had nothing at all.
fn gather(dir: &Path, sources: &[Source], mode: Fetch, now: SystemTime, fetch: &(dyn Fn(&str) -> Option<Vec<u8>> + Sync)) -> (Lists, usize) {
    let parsed: Vec<Option<Vec<String>>> = std::thread::scope(|scope| {
        let jobs: Vec<_> = sources
            .iter()
            .map(|s| {
                scope.spawn(move || {
                    let path = dir.join(s.file());
                    let want = match mode {
                        Fetch::Never => false,
                        Fetch::Stale => stale(&path, now),
                        Fetch::Always => true,
                    };
                    let downloaded = want.then(|| {
                        s.urls().iter().find_map(|url| {
                            let bytes = fetch(url)?;
                            let entries = parse(s.kind, &String::from_utf8_lossy(&bytes));
                            (!entries.is_empty()).then_some((bytes, entries))
                        })
                    });
                    match downloaded.flatten() {
                        Some((bytes, entries)) => {
                            if let Err(e) = crate::store::write(&path, &bytes, false) {
                                eprintln!("hitori: cannot cache {}: {e}", path.display());
                            }
                            Some(entries)
                        }
                        None => std::fs::read_to_string(&path).ok().map(|t| parse(s.kind, &t)).filter(|e| !e.is_empty()),
                    }
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().ok().flatten()).collect()
    });
    let mut lists = Lists::default();
    let mut seen = HashSet::new();
    for (s, entries) in sources.iter().zip(&parsed) {
        let out = if s.kind == Kind::Domains { &mut lists.domains } else { &mut lists.subnets };
        for e in entries.iter().flatten() {
            if out.len() < MAX_ENTRIES && seen.insert(e.clone()) {
                out.push(e.clone());
            }
        }
    }
    (lists, parsed.iter().filter(|p| p.is_none()).count())
}

fn lists_dir() -> PathBuf {
    crate::store::cache_dir().join("region")
}

/// The country's lists from the cache, downloading as `mode` says. The second value: no source had anything.
pub fn load(country: &str, mode: Fetch) -> (Lists, bool) {
    let sources = sources(country);
    let (lists, missing) = gather(&lists_dir(), &sources, mode, SystemTime::now(), &|url| net::download(url, 16 << 20));
    (lists, !sources.is_empty() && missing == sources.len())
}

/// The last location, cached in cache_dir/region.json.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Location {
    pub ip: String,
    /// ISO 3166-1 alpha-2.
    pub country: String,
    pub name: Option<String>,
    pub city: Option<String>,
    pub lat: f64,
    pub lon: f64,
    /// Unix seconds of the last public IP check.
    pub checked_at: u64,
}

impl Location {
    pub fn path() -> PathBuf {
        crate::store::cache_dir().join("region.json")
    }

    fn new(ip: String, geo: &Geo, now: u64) -> Option<Self> {
        Some(Self { ip, country: geo.code.clone()?, name: geo.country.clone(), city: geo.city.clone(), lat: geo.lat, lon: geo.lon, checked_at: now })
    }

    pub fn geo(&self) -> Geo {
        Geo { lat: self.lat, lon: self.lon, city: self.city.clone(), country: self.name.clone(), code: Some(self.country.clone()) }
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Time to look at the public IP again: never checked, the local network changed, or the last check is old.
pub fn due(cached: Option<&Location>, now: u64, network_changed: bool) -> bool {
    network_changed || cached.is_none_or(|c| now.saturating_sub(c.checked_at) >= RECHECK.as_secs())
}

/// The cached location still holds for this public IP.
pub fn reuse<'a>(cached: Option<&'a Location>, ip: &str) -> Option<&'a Location> {
    cached.filter(|c| c.ip == ip)
}

/// The address this computer reaches the internet from; changes with the network. Sends nothing.
pub fn local_addr() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("1.1.1.1:53").ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

pub struct Found {
    pub location: Location,
    pub lists: Lists,
    /// No list could be downloaded or read from the cache.
    pub missing: bool,
}

/// With `lookup`, the public IP (located again only when it changed or `force`); otherwise the cached location. Then the
/// country's lists. `None` when the IP turns out to be one of the tunnel's exits: that lookup went through the VPN.
pub fn check(cached: Option<Location>, lookup: bool, force: bool, exits: &[String]) -> Result<Option<Found>, String> {
    let location = if lookup {
        let ip = net::my_ip().ok_or(t!("region.error.offline"))?;
        if exits.contains(&ip) {
            return Ok(None);
        }
        match reuse(cached.as_ref(), &ip).filter(|_| !force) {
            Some(c) => Location { checked_at: now(), ..c.clone() },
            None => net::locate(&ip).and_then(|g| Location::new(ip, &g, now())).ok_or(t!("region.error.cannot_tell_country"))?,
        }
    } else {
        cached.ok_or(t!("region.error.checked_while_off"))?
    };
    let (lists, missing) = load(&location.country, if force { Fetch::Always } else { Fetch::Stale });
    Ok(Some(Found { location, lists, missing }))
}

#[derive(Clone, PartialEq, Default, Debug)]
pub enum Status {
    #[default]
    Computing,
    Ready,
    Failed(String),
}

/// What the UI shows about the region: the onboarding screen and Settings.
#[derive(Clone, PartialEq, Default, Debug)]
pub struct Summary {
    /// ISO code.
    pub country: Option<String>,
    pub name: Option<String>,
    pub domains: usize,
    pub subnets: usize,
    pub status: Status,
}

fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl Summary {
    pub fn place(&self) -> String {
        self.name.clone().or_else(|| self.country.clone()).unwrap_or_else(|| t!("region.place.unknown").into())
    }

    /// e.g. "1,183 blocked sites and 2,040 subnets go through the VPN."
    pub fn line(&self) -> String {
        if let Status::Failed(e) = &self.status
            && self.country.is_none()
        {
            return e.clone();
        }
        match (&self.country, self.domains + self.subnets) {
            (None, _) => t!("region.summary.checking").into(),
            (Some(c), 0) if sources(c).is_empty() => t!("region.summary.no_list", place = self.place()),
            (Some(_), 0) if self.status == Status::Computing => t!("region.summary.loading").into(),
            (Some(_), 0) => t!("region.summary.cannot_download", place = self.place()),
            _ => t!("region.summary.line", sites = t!("region.summary.sites", n = self.domains, count = thousands(self.domains)), subnets = t!("region.summary.subnets", n = self.subnets, count = thousands(self.subnets))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn parses_domain_lists_of_every_project() {
        let got: Vec<_> = [
            ".ua",
            "4pda.to",
            "domain:Zona.Media",
            "full:api.x.com",
            "keyword:telegram",
            "+.youtube.com",
            "domain:ads.example:@ads",
            "regexp:^a.*$",
            "# comment",
            "",
            "bad domain",
            "ip-cidr:1.2.3.0/24",
            "x.org # note",
        ]
        .iter()
        .filter_map(|l| domain(l))
        .collect();
        assert_eq!(
            got,
            [
                "domain:ua",
                "domain:4pda.to",
                "domain:zona.media",
                "full:api.x.com",
                "keyword:telegram",
                "domain:youtube.com",
                "domain:ads.example",
                "domain:x.org"
            ]
        );
    }

    #[test]
    fn parses_subnets() {
        let got: Vec<_> = ["91.108.4.0/22", "2001:67c:4e8::/48", "1.1.1.1", " 10.0.0.0/8 ", "1.2.3.0/33", "nonsense", "# x", "::1/129"]
            .iter()
            .filter_map(|l| subnet(l))
            .collect();
        assert_eq!(got, ["91.108.4.0/22", "2001:67c:4e8::/48", "1.1.1.1/32", "10.0.0.0/8"]);
    }

    #[test]
    fn lists_become_tunnel_rules() {
        let lists = Lists { domains: vec!["domain:a.example".into()], subnets: vec!["1.2.3.0/24".into()] };
        assert_eq!(
            lists.rules(),
            [
                json!({"type": "field", "domain": ["domain:a.example"], "outboundTag": "proxy"}),
                json!({"type": "field", "ip": ["1.2.3.0/24"], "outboundTag": "proxy"})
            ]
        );
        assert!(Lists::default().rules().is_empty());
    }

    #[test]
    fn every_country_has_domains_and_subnets() {
        for c in ["RU", "IR", "CN"] {
            let s = sources(c);
            assert!(s.iter().any(|s| s.kind == Kind::Domains) && s.iter().any(|s| s.kind == Kind::Subnets), "{c}");
            assert!(s.iter().all(|s| s.urls()[0].starts_with("https://raw.githubusercontent.com/") && s.urls()[1].starts_with("https://cdn.jsdelivr.net/gh/")));
        }
        assert!(sources("DE").is_empty());
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hitori-region-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn src() -> Vec<Source> {
        vec![
            Source::new("o/r", "main", "a.lst", Kind::Domains),
            Source::new("o/r", "main", "b.lst", Kind::Domains),
            Source::new("o/r", "main", "net.lst", Kind::Subnets),
        ]
    }

    #[test]
    fn cache_is_used_until_it_expires_and_kept_when_offline() {
        let dir = temp("ttl");
        let calls = AtomicUsize::new(0);
        let online = |url: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(if url.ends_with("net.lst") {
                b"1.2.3.0/24\n".to_vec()
            } else {
                format!("{}.example\nshared.example\n", &url[url.len() - 5..url.len() - 4]).into_bytes()
            })
        };
        let now = SystemTime::now();
        let (lists, missing) = gather(&dir, &src(), Fetch::Stale, now, &online);
        assert_eq!(
            (lists.domains, lists.subnets, missing),
            (vec!["domain:a.example".into(), "domain:shared.example".into(), "domain:b.example".into()], vec!["1.2.3.0/24".into()], 0)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        // Fresh: nothing downloaded.
        gather(&dir, &src(), Fetch::Stale, now + Duration::from_secs(3600), &online);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        // Expired and offline: raw and mirror tried, the stale copy still works.
        let offline = |_: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        };
        let (lists, missing) = gather(&dir, &src(), Fetch::Stale, now + TTL + Duration::from_secs(1), &offline);
        assert_eq!((lists.domains.len(), lists.subnets.len(), missing), (3, 1, 0));
        assert_eq!(calls.load(Ordering::SeqCst), 3 + 6);
        let (_, missing) = gather(&dir, &src(), Fetch::Never, now, &offline);
        assert_eq!((missing, calls.load(Ordering::SeqCst)), (0, 9), "cache only");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn falls_back_to_the_mirror_and_ignores_junk() {
        let dir = temp("junk");
        let portal = |_: &str| Some(b"<html><body>Sign in to Wi-Fi</body></html>".to_vec());
        let (lists, missing) = gather(&dir, &src(), Fetch::Always, SystemTime::now(), &portal);
        assert_eq!(lists, Lists::default());
        assert_eq!(missing, 3, "a captive portal page is not a list");
        let mirror = |url: &str| url.contains("jsdelivr").then(|| b"m.example\n".to_vec());
        let (lists, _) = gather(&dir, &src()[..1], Fetch::Always, SystemTime::now(), &mirror);
        assert_eq!(lists.domains, ["domain:m.example"]);
        let (lists, _) = gather(&dir, &src()[..1], Fetch::Always, SystemTime::now(), &portal);
        assert_eq!(lists.domains, ["domain:m.example"], "junk does not overwrite the cache");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn caps_long_lists() {
        let dir = temp("cap");
        let big = |_: &str| Some((0..MAX_ENTRIES + 50).map(|i| format!("d{i}.example\n")).collect::<String>().into_bytes());
        let (lists, _) = gather(&dir, &src()[..1], Fetch::Always, SystemTime::now(), &big);
        assert_eq!(lists.domains.len(), MAX_ENTRIES);
        assert_eq!(lists.domains[0], "domain:d0.example", "rank order kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn at(ip: &str, country: &str, checked_at: u64) -> Location {
        Location { ip: ip.into(), country: country.into(), name: None, city: None, lat: 0., lon: 0., checked_at }
    }

    #[test]
    fn rechecks_on_network_change_or_age() {
        let c = at("1.1.1.1", "RU", 1000);
        assert!(due(None, 1000, false));
        assert!(!due(Some(&c), 1000 + 60, false));
        assert!(due(Some(&c), 1000 + 60, true));
        assert!(due(Some(&c), 1000 + RECHECK.as_secs(), false));
    }

    #[test]
    fn location_is_cached_by_ip() {
        let c = at("1.1.1.1", "RU", 0);
        assert!(reuse(Some(&c), "1.1.1.1").is_some());
        assert!(reuse(Some(&c), "2.2.2.2").is_none(), "a new IP is located again");
        assert!(reuse(None, "1.1.1.1").is_none());
        let geo = Geo { lat: 55.7, lon: 37.6, city: Some("Moscow".into()), country: Some("Russia".into()), code: Some("RU".into()) };
        let l = Location::new("5.5.5.5".into(), &geo, 9).unwrap();
        assert_eq!((l.country.as_str(), l.geo()), ("RU", geo.clone()));
        assert!(Location::new("5.5.5.5".into(), &Geo { code: None, ..geo }, 9).is_none());
    }

    /// Downloads every country's lists and has the real core load them: cargo test live_region -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_region_lists_load_in_the_core() {
        let xray = crate::xray::xray_path().expect("bin/xray");
        let dir = temp("live");
        for c in ["RU", "IR", "CN"] {
            let start = std::time::Instant::now();
            let (lists, missing) = gather(&dir, &sources(c), Fetch::Always, SystemTime::now(), &|url| net::download(url, 16 << 20));
            eprintln!("{c}: {} domains, {} subnets, {missing} missing, {:?}", lists.domains.len(), lists.subnets.len(), start.elapsed());
            assert_eq!(missing, 0, "{c}");
            let config = json!({"outbounds": [{"tag": "proxy", "protocol": "freedom"}, {"tag": "direct", "protocol": "freedom"}],
                "routing": {"domainStrategy": "IPIfNonMatch", "domainMatcher": "hybrid", "rules": lists.rules()}});
            let path = dir.join(format!("{c}.json"));
            crate::store::write(&path, &serde_json::to_vec(&config).unwrap(), false).unwrap();
            let out = std::process::Command::new(&xray).args(["run", "-test", "-c"]).arg(&path).output().unwrap();
            assert!(out.status.success(), "{c}: {}", String::from_utf8_lossy(&out.stdout));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summary_lines() {
        let s = Summary { country: Some("RU".into()), name: Some("Russia".into()), domains: 1183, subnets: 20345, status: Status::Ready };
        assert_eq!(s.line(), "1,183 blocked sites and 20,345 subnets go through the VPN.");
        assert_eq!(
            Summary { country: Some("DE".into()), name: Some("Germany".into()), ..Default::default() }.line(),
            "No ready-made list for Germany; your provider's rules apply."
        );
        assert_eq!(Summary::default().line(), "Checking where you are…");
        assert_eq!(Summary { status: Status::Failed("offline".into()), ..Default::default() }.line(), "offline");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
