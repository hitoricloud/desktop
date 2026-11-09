//! Sites that fail when sent direct, read from the core's log (autorouting: direct first, VPN when it fails).
//! Xray logs these only at Info level; a reset line lacks the target, so it comes from the session's `connection opened to` line.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

const FREEDOM_FAILED: &str = "failed to process outbound traffic > proxy/freedom: ";
const OPENED: &str = "proxy/freedom: connection opened to ";
const WINDOW: Duration = Duration::from_secs(10 * 60);
const MIN_HITS: usize = 2;
const MAX_HOSTS: usize = 256;
const MAX_SESSIONS: usize = 512;
const MAX_PENDING: usize = 4096;

pub const TIMED_OUT: &str = "Timed out directly";
pub const RESET: &str = "Reset directly";
pub const REFUSED: &str = "Refused directly";
pub const BLOCKED: &str = "Blocked directly";

#[derive(Debug, PartialEq)]
pub struct Failure {
    pub host: String,
    pub reason: &'static str,
}

/// A direct (freedom) dial to a domain that failed, from one core log line.
pub fn parse(line: &str) -> Option<Failure> {
    let rest = line.split_once(FREEDOM_FAILED)?.1.strip_prefix("failed to open connection to ")?;
    let (target, cause) = rest.split_once(' ').unwrap_or((rest, ""));
    let host = domain(target.strip_prefix("tcp:")?)?;
    Some(Failure { host, reason: reason(cause)? })
}

fn reason(cause: &str) -> Option<&'static str> {
    const CAUSES: &[(&str, &str)] = &[
        ("i/o timeout", TIMED_OUT),
        ("connection timed out", TIMED_OUT),
        ("connection reset by peer", RESET),
        ("connection refused", REFUSED),
        ("no such host", BLOCKED),
        ("server misbehaving", BLOCKED),
        ("no route to host", BLOCKED),
        ("network is unreachable", BLOCKED),
        ("permission denied", BLOCKED),
        ("operation not permitted", BLOCKED),
    ];
    CAUSES.iter().find(|(text, _)| cause.contains(text)).map(|(_, r)| *r)
}

/// `host:port` → normalised host, or `None` for IP literals and anything that is not a domain.
fn domain(host_port: &str) -> Option<String> {
    let host = host_port.rsplit_once(':')?.0;
    if host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_ok() {
        return None;
    }
    let host = normalise(host);
    let valid = host.contains('.') && host.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b));
    valid.then_some(host)
}

fn normalise(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    match host.strip_prefix("www.") {
        Some(bare) if bare.contains('.') => bare.to_owned(),
        _ => host,
    }
}

/// `2026/11/10 01:04:20.970122 [Info] [1570030338] ...` → `1570030338`.
fn session(line: &str) -> Option<&str> {
    let sid = line.split_once("] [")?.1.split_once(']')?.0;
    (!sid.is_empty() && sid.bytes().all(|b| b.is_ascii_digit())).then_some(sid)
}

struct Host {
    reason: &'static str,
    failures: VecDeque<Instant>,
    reported: bool,
}

/// Counts direct failures per host and hands out each failing host once per new failure.
#[derive(Default)]
pub struct Tracker {
    hosts: HashMap<String, Host>,
    /// Recent direct sessions to domains: (session id, host), oldest first.
    opened: VecDeque<(String, String)>,
    /// Failures read since the last `candidates` call, which timestamps them.
    pending: Vec<Failure>,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, lines: &[String]) {
        for line in lines {
            if let Some(failure) = parse(line) {
                self.push(failure);
            } else if let Some(rest) = line.split_once(OPENED).map(|(_, r)| r) {
                let target = rest.split([',', ' ']).next().unwrap_or(rest);
                if let (Some(sid), Some(host)) = (session(line), target.strip_prefix("tcp:").and_then(domain)) {
                    if self.opened.len() >= MAX_SESSIONS {
                        self.opened.pop_front();
                    }
                    self.opened.push_back((sid.to_owned(), host));
                }
            } else if line.split_once(FREEDOM_FAILED).is_some_and(|(_, r)| r.starts_with("connection ends > ") && r.contains("connection reset by peer")) {
                let Some(sid) = session(line) else { continue };
                if let Some(i) = self.opened.iter().rposition(|(s, _)| s == sid) {
                    let (_, host) = self.opened.remove(i).unwrap();
                    self.push(Failure { host, reason: RESET });
                }
            }
        }
    }

    fn push(&mut self, failure: Failure) {
        if self.pending.len() < MAX_PENDING {
            self.pending.push(failure);
        }
    }

    /// `(host, reason, failures in the last 10 min)` for hosts that failed at least twice and
    /// failed again since they were last returned.
    pub fn candidates(&mut self, now: Instant) -> Vec<(String, &'static str, u32)> {
        for Failure { host, reason } in std::mem::take(&mut self.pending) {
            if !self.hosts.contains_key(&host) && self.hosts.len() >= MAX_HOSTS {
                let stalest = self.hosts.iter().min_by_key(|(_, h)| h.failures.back().copied()).map(|(k, _)| k.clone());
                stalest.map(|k| self.hosts.remove(&k));
            }
            let entry = self.hosts.entry(host).or_insert_with(|| Host { reason, failures: VecDeque::new(), reported: false });
            entry.reason = reason;
            entry.failures.push_back(now);
            entry.reported = false;
        }
        self.hosts.retain(|_, h| {
            while h.failures.front().is_some_and(|&t| now.saturating_duration_since(t) > WINDOW) {
                h.failures.pop_front();
            }
            !h.failures.is_empty()
        });
        let mut out: Vec<_> = self
            .hosts
            .iter_mut()
            .filter(|(_, h)| !h.reported && h.failures.len() >= MIN_HITS)
            .map(|(host, h)| {
                h.reported = true;
                (host.clone(), h.reason, h.failures.len() as u32)
            })
            .collect();
        out.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        out
    }

    pub fn forget(&mut self, host: &str) {
        let host = normalise(host);
        self.hosts.remove(&host);
        self.pending.retain(|f| f.host != host);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from bin/xray 26.6.1 (freedom, socks inbound, curl --socks5-hostname).
    const TIMEOUT: &str = "2026/11/10 01:05:44.130951 [Info] [1863072742] app/proxyman/outbound: app/proxyman/outbound: failed to process outbound traffic > proxy/freedom: failed to open connection to tcp:timeout.test.example:443 > common/retry: [dial tcp 10.255.255.1:443: i/o timeout] > common/retry: all retry attempts failed";
    const TIMEOUT_IP: &str = "2026/11/10 01:05:44.130974 [Info] [871355105] app/proxyman/outbound: app/proxyman/outbound: failed to process outbound traffic > proxy/freedom: failed to open connection to tcp:10.255.255.1:80 > common/retry: [dial tcp 10.255.255.1:80: i/o timeout] > common/retry: all retry attempts failed";
    const REFUSED_LINE: &str = "2026/11/10 01:04:20.963456 [Info] [805032887] app/proxyman/outbound: app/proxyman/outbound: failed to process outbound traffic > proxy/freedom: failed to open connection to tcp:refused.test.example:42973 > common/retry: [dial tcp 127.0.0.1:42973: connect: connection refused] > common/retry: all retry attempts failed";
    const REFUSED_ASIS: &str = "2026/11/10 01:06:37.631098 [Info] [3093743476] app/proxyman/outbound: app/proxyman/outbound: failed to process outbound traffic > proxy/freedom: failed to open connection to tcp:127.0.0.1.nip.io:42973 > common/retry: [dial tcp 127.0.0.1:42973: connect: connection refused] > common/retry: all retry attempts failed";
    const DNS: &str = "2026/11/10 01:04:22.006751 [Info] [3765279367] app/proxyman/outbound: app/proxyman/outbound: failed to process outbound traffic > proxy/freedom: failed to open connection to tcp:nonexistent-hitori-zz9.invalid:443 > common/retry: [dial tcp: lookup nonexistent-hitori-zz9.invalid on 192.168.1.1:53: no such host] > common/retry: all retry attempts failed";
    const RESET_OPENED: &str = "2026/11/10 01:04:23.061827 [Info] [2016473908] proxy/freedom: connection opened to tcp:www.Reset2.example:56835, local endpoint 127.0.0.1:56818, remote endpoint 127.0.0.1:56835";
    const RESET_ENDS: &str = "2026/11/10 01:04:23.062561 [Info] [2016473908] app/proxyman/outbound: app/proxyman/outbound: failed to process outbound traffic > proxy/freedom: connection ends > readfrom tcp 127.0.0.1:54335->127.0.0.1:36638: splice: connection reset by peer";
    const PROXIED: &str = "2026/11/10 01:06:39.732479 [Info] [4181146653] app/proxyman/outbound: app/proxyman/outbound: failed to process outbound traffic > proxy/vless/outbound: failed to find an available destination > common/retry: [dial tcp 127.0.0.1:42973: connect: connection refused] > common/retry: all retry attempts failed";
    const NOISE: &[&str] = &[
        "2026/11/10 01:04:18.966048 [Warning] core: Xray 26.6.1 started",
        "2026/11/10 01:04:20.997854 [Info] [3765279367] proxy/freedom: failed to get IP address for domain nonexistent-hitori-zz9.invalid > app/dns: returning nil for domain nonexistent-hitori-zz9.invalid > lookup nonexistent-hitori-zz9.invalid on 192.168.1.1:53: no such host",
        "2026/11/10 01:04:20.997825 [Info] app/dns: failed to lookup ip for domain nonexistent-hitori-zz9.invalid at server localhost in serial query mode > lookup nonexistent-hitori-zz9.invalid on 192.168.1.1:53: no such host",
        "2026/11/10 01:04:19.961790 from tcp:127.0.0.1:36594 accepted tcp:refused.test.example:42973 [socks >> direct]",
        "2026/11/10 01:04:19.961753 [Info] [805032887] proxy/freedom: dialing to tcp:127.0.0.1:42973",
        "",
    ];

    fn lines(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_owned()).collect()
    }

    fn failure(host: &str, reason: &'static str) -> Option<Failure> {
        Some(Failure { host: host.to_owned(), reason })
    }

    #[test]
    fn parse_real_direct_failures() {
        assert_eq!(parse(TIMEOUT), failure("timeout.test.example", TIMED_OUT));
        assert_eq!(parse(REFUSED_LINE), failure("refused.test.example", REFUSED));
        assert_eq!(parse(REFUSED_ASIS), failure("127.0.0.1.nip.io", REFUSED));
        assert_eq!(parse(DNS), failure("nonexistent-hitori-zz9.invalid", BLOCKED));
        assert_eq!(parse(&TIMEOUT.replace("[Info]", "[Warning]")), failure("timeout.test.example", TIMED_OUT));
    }

    #[test]
    fn parse_ignores_ip_targets_proxied_failures_and_noise() {
        assert_eq!(parse(TIMEOUT_IP), None);
        assert_eq!(parse(&TIMEOUT_IP.replace("10.255.255.1:80 >", "[2001:db8::1]:443 >")), None);
        assert_eq!(parse(PROXIED), None);
        assert_eq!(parse(RESET_ENDS), None);
        assert_eq!(parse(&TIMEOUT.replace("i/o timeout", "context canceled")), None);
        for line in NOISE {
            assert_eq!(parse(line), None, "{line}");
        }
    }

    #[test]
    fn parse_normalises_hosts() {
        let line = TIMEOUT.replace("timeout.test.example", "WWW.Example.COM");
        assert_eq!(parse(&line), failure("example.com", TIMED_OUT));
        assert_eq!(normalise("www.com"), "www.com");
    }

    #[test]
    fn tracker_needs_two_failures() {
        let mut t = Tracker::new();
        let now = Instant::now();
        t.feed(&lines(&[TIMEOUT]));
        assert!(t.candidates(now).is_empty());
        t.feed(&lines(&[TIMEOUT]));
        assert_eq!(t.candidates(now + Duration::from_secs(5)), vec![("timeout.test.example".to_owned(), TIMED_OUT, 2)]);
    }

    #[test]
    fn tracker_reports_once_until_new_failure() {
        let mut t = Tracker::new();
        let now = Instant::now();
        t.feed(&lines(&[REFUSED_LINE, REFUSED_LINE]));
        assert_eq!(t.candidates(now).len(), 1);
        assert!(t.candidates(now + Duration::from_secs(10)).is_empty());
        t.feed(&lines(&[REFUSED_LINE]));
        assert_eq!(t.candidates(now + Duration::from_secs(20)), vec![("refused.test.example".to_owned(), REFUSED, 3)]);
    }

    #[test]
    fn tracker_window_is_ten_minutes() {
        let mut t = Tracker::new();
        let now = Instant::now();
        t.feed(&lines(&[TIMEOUT]));
        t.candidates(now);
        t.feed(&lines(&[TIMEOUT]));
        assert!(t.candidates(now + WINDOW + Duration::from_secs(1)).is_empty());
        t.feed(&lines(&[TIMEOUT]));
        assert_eq!(t.candidates(now + WINDOW + Duration::from_secs(2))[0].2, 2);
    }

    #[test]
    fn tracker_correlates_resets_by_session() {
        let mut t = Tracker::new();
        let now = Instant::now();
        let other = RESET_ENDS.replace("[2016473908]", "[1]");
        t.feed(&lines(&[RESET_OPENED, RESET_ENDS, &other, RESET_OPENED, RESET_ENDS]));
        assert_eq!(t.candidates(now), vec![("reset2.example".to_owned(), RESET, 2)]);
    }

    #[test]
    fn tracker_ignores_noise_and_ip_targets() {
        let mut t = Tracker::new();
        t.feed(&lines(&[TIMEOUT_IP, TIMEOUT_IP, PROXIED, PROXIED]));
        t.feed(&lines(NOISE));
        t.feed(&lines(NOISE));
        assert!(t.candidates(Instant::now()).is_empty());
    }

    #[test]
    fn tracker_forget() {
        let mut t = Tracker::new();
        let now = Instant::now();
        t.feed(&lines(&[TIMEOUT, TIMEOUT]));
        t.forget("WWW.timeout.test.example");
        assert!(t.candidates(now).is_empty());
        t.feed(&lines(&[TIMEOUT]));
        assert!(t.candidates(now).is_empty());
    }

    #[test]
    fn tracker_is_capped() {
        let mut t = Tracker::new();
        let now = Instant::now();
        for i in 0..MAX_HOSTS + 50 {
            let line = TIMEOUT.replace("timeout.test.example", &format!("h{i}.example"));
            t.feed(&lines(&[&line, &line]));
        }
        assert_eq!(t.candidates(now).len(), MAX_HOSTS);
        for i in 0..MAX_SESSIONS + 10 {
            t.feed(&[RESET_OPENED.replace("2016473908", &i.to_string())]);
        }
        assert_eq!(t.opened.len(), MAX_SESSIONS);
    }
}
