//! The prebuilt Xray core: finding it, the config the app generates for it, and running it.
//! Proxy and system-proxy modes run it as the user with SOCKS/HTTP listeners; TUN mode hands it to the privileged
//! helper (`tun.rs` on Linux, `elevated.rs` on Windows and macOS) through `helper.rs`.
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::data::{AppRule, Protocol, RuleMode, Server, processes};

pub const SOCKS_PORT: u16 = 10808;
pub const HTTP_PORT: u16 = 10809;
/// The TUN device (Linux), the wintun adapter (Windows); macOS names utun devices itself.
pub const TUN_NAME: &str = if cfg!(windows) { "Hitori" } else { "hitori0" };
/// Firewall mark on the core's own sockets, so they skip the TUN routing table (Linux).
pub const MARK: u32 = 0x6869;
/// Windows and macOS: the TUN's addresses, the routes into it (halves, so they win over any default route) and the
/// resolver given to the system. DNS to any address through the TUN is answered by the core.
pub const TUN_ADDRS: [&str; 2] = ["172.19.0.1/30", "fdfe:dcba:9876::1/126"];
pub const TUN_ROUTES: [&str; 4] = ["0.0.0.0/1", "128.0.0.0/1", "::/1", "8000::/1"];
pub const TUN_DNS: &str = "1.1.1.1";
const PROBE_URL: &str = "https://www.gstatic.com/generate_204";

/// `HITORI_XRAY`, next to the executable (also inside a macOS bundle), `../libexec/hitori/xray`, then the checkout's
/// `bin/xray` (scripts/fetch-xray.sh); `xray.exe` on Windows.
pub fn xray_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok();
    let dir = exe.as_ref().and_then(|e| e.parent().map(PathBuf::from));
    let name = format!("xray{}", std::env::consts::EXE_SUFFIX);
    let candidates = [
        std::env::var_os("HITORI_XRAY").map(PathBuf::from),
        dir.as_ref().map(|d| d.join(&name)),
        dir.as_ref().map(|d| d.join("../libexec/hitori").join(&name)),
        Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin").join(&name)),
    ];
    candidates.into_iter().flatten().find(|p| p.is_file()).and_then(|p| p.canonicalize().ok())
}

/// What to connect to and how to route, as chosen in the app.
#[derive(Clone)]
pub struct Plan {
    pub exit: &'static Server,
    pub entry: Option<&'static Server>,
    pub protocol: Protocol,
    pub default_route: RuleMode,
    /// Every app through a TUN run by the privileged helper.
    pub tun: bool,
    /// The system proxy at the core (Windows, macOS).
    pub system_proxy: bool,
    pub kill_switch: bool,
    pub apps: Vec<(&'static str, AppRule)>,
    pub sites: Vec<(&'static str, AppRule)>,
    pub servers: Vec<&'static Server>,
    pub tunnel_rules: Arc<Vec<Value>>,
    pub base_rules: Arc<Vec<Value>>,
    pub dns: Arc<Value>,
}

#[derive(Clone, Copy, Debug)]
pub struct Ports {
    pub probe: u16,
    pub metrics: u16,
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).unwrap_or(0)
}

const PRIVATE: &[&str] =
    &["10.0.0.0/8", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16", "172.16.0.0/12", "192.168.0.0/16", "224.0.0.0/4", "255.255.255.255/32", "::1/128", "fc00::/7", "fe80::/10", "ff00::/8"];

fn with_sockopt(mut outbound: Value, key: &str, value: Value) -> Value {
    let stream = outbound.as_object_mut().unwrap().entry("streamSettings").or_insert_with(|| json!({}));
    stream.as_object_mut().unwrap().entry("sockopt").or_insert_with(|| json!({}))[key] = value;
    outbound
}

fn tagged(mut outbound: Value, tag: &str) -> Value {
    outbound["tag"] = json!(tag);
    outbound
}

/// Outbounds by tag, built once each: `proxy` (+ `entry`), `p-<id>` per server, and per-app routes.
struct Outbounds {
    list: Vec<Value>,
    protocol: Protocol,
}

impl Outbounds {
    fn has(&self, tag: &str) -> bool {
        self.list.iter().any(|o| o["tag"] == tag)
    }

    /// `exit` reached directly or through `via`, over `protocol`; returns its tag.
    fn route(&mut self, exit: &'static Server, via: Option<&'static Server>, protocol: Protocol) -> Option<String> {
        let Some(via) = via.filter(|v| v.id != exit.id) else {
            let (p, outbound) = exit.variant(protocol)?;
            let tag = if p == self.protocol { format!("p-{}", exit.id) } else { format!("p-{}-{p:?}", exit.id) };
            if !self.has(&tag) {
                self.list.push(tagged(outbound.clone(), &tag));
            }
            return Some(tag);
        };
        let hop = format!("h-{}", via.id);
        if !self.has(&hop) {
            self.list.push(tagged(via.chainable(protocol)?.1.clone(), &hop));
        }
        let tag = format!("c-{}-{}", via.id, exit.id);
        if !self.has(&tag) {
            let outbound = exit.chainable(protocol)?.1.clone();
            self.list.push(with_sockopt(tagged(outbound, &tag), "dialerProxy", json!(hop)));
        }
        Some(tag)
    }
}

fn field(extra: Value, outbound: &str) -> Value {
    let mut rule = json!({"type": "field", "outboundTag": outbound});
    rule.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
    rule
}

/// The complete core config. Rule order: DNS and app probes, LAN, provider rules, blocked apps, sites, other apps,
/// then the default route (the autorouting lists and their direct catch-all for Auto).
pub fn config(plan: &Plan, ports: Ports) -> Result<Value, String> {
    let exit = plan.exit;
    // App rules only where the core can tell apps apart.
    let tun = plan.tun;
    let apps: &[(&'static str, AppRule)] = if crate::os::PER_APP { &plan.apps } else { &[] };
    let entry = plan.entry.filter(|e| e.id != exit.id);
    let mut out = Outbounds { list: vec![], protocol: plan.protocol };
    let main = match entry {
        Some(e) => {
            let hop = e.chainable(plan.protocol).ok_or("The first server has no usable transport")?.1.clone();
            out.list.push(tagged(hop, "entry"));
            let last = exit.chainable(plan.protocol).ok_or("The exit server has no usable transport")?.1.clone();
            with_sockopt(tagged(last, "proxy"), "dialerProxy", json!("entry"))
        }
        None => tagged(exit.variant(plan.protocol).ok_or("This server has no usable transport")?.1.clone(), "proxy"),
    };
    out.list.insert(0, main);
    for s in &plan.servers {
        out.route(s, None, plan.protocol);
    }

    let mut rules = vec![];
    if tun {
        rules.push(json!({"type": "field", "inboundTag": ["tun"], "port": "53", "outboundTag": "dns-out"}));
    }
    rules.push(field(json!({"inboundTag": ["dns-internal", "probe-in"]}), "proxy"));
    rules.push(field(json!({"ip": PRIVATE}), "direct"));
    rules.push(field(json!({"domain": ["domain:localhost"]}), "direct"));
    // The provider's rules send some traffic direct (torrents), which an allowlist must not do.
    if plan.default_route != RuleMode::Block {
        rules.extend(plan.base_rules.iter().cloned());
    }

    let names = |app: &str| processes(app);
    for (app, _) in apps.iter().filter(|(_, r)| r.mode == RuleMode::Block) {
        rules.push(field(json!({"process": names(app)}), "block"));
    }
    // Where a rule's tunnelled traffic goes: the main route, or the rule's own exit, cascade and protocol.
    let tag = |out: &mut Outbounds, r: &AppRule| -> Option<String> {
        if r.exit.is_none() && r.via.is_none() && r.protocol.is_none() {
            return Some("proxy".into());
        }
        let find = |id: &str| plan.servers.iter().copied().find(|s| s.id == id);
        let exit = match r.exit {
            Some(id) => find(id)?,
            None => exit,
        };
        let via = match (r.via, r.exit) {
            (Some(v), _) => Some(find(v)?),
            (None, None) => entry,
            (None, Some(_)) => None,
        };
        out.route(exit, via, r.protocol.unwrap_or(plan.protocol))
    };
    for (site, r) in &plan.sites {
        let domain = json!({"domain": [format!("domain:{site}")]});
        match r.mode {
            RuleMode::Block => rules.push(field(domain, "block")),
            RuleMode::Direct => rules.push(field(domain, "direct")),
            RuleMode::Vpn => {
                if let Some(t) = tag(&mut out, r) {
                    rules.push(field(domain, &t));
                }
            }
            RuleMode::Auto => {}
        }
    }
    for (app, r) in apps {
        let custom = r.exit.is_some() || r.via.is_some() || r.protocol.is_some();
        match r.mode {
            RuleMode::Block => {}
            RuleMode::Direct => rules.push(field(json!({"process": names(app)}), "direct")),
            RuleMode::Vpn => {
                if let Some(t) = tag(&mut out, r) {
                    rules.push(field(json!({"process": names(app)}), &t));
                }
            }
            // Under the Auto default a plain Auto app needs no rules of its own.
            RuleMode::Auto if !custom && plan.default_route == RuleMode::Auto => {}
            // What needs the tunnel goes to the app's exit, the rest of the app direct.
            RuleMode::Auto => {
                if let Some(t) = tag(&mut out, r) {
                    for list in plan.tunnel_rules.iter() {
                        let mut rule = list.clone();
                        rule["process"] = json!(names(app));
                        rule["outboundTag"] = json!(t);
                        rules.push(rule);
                    }
                    if plan.default_route != RuleMode::Auto {
                        rules.push(field(json!({"process": names(app)}), "direct"));
                    }
                }
            }
        }
    }
    if plan.default_route == RuleMode::Auto {
        rules.extend(plan.tunnel_rules.iter().cloned());
    }
    let last = match plan.default_route {
        RuleMode::Auto | RuleMode::Direct => "direct",
        RuleMode::Vpn => "proxy",
        RuleMode::Block => "block",
    };
    rules.push(field(json!({"network": "tcp,udp"}), last));

    out.list.push(json!({"tag": "direct", "protocol": "freedom"}));
    out.list.push(json!({"tag": "block", "protocol": "blackhole"}));
    if tun {
        out.list.push(json!({"tag": "dns-out", "protocol": "dns"}));
    }
    if tun && cfg!(target_os = "linux") {
        out.list = out.list.into_iter().map(|o| if o["tag"] == "block" { o } else { with_sockopt(o, "mark", json!(MARK)) }).collect();
    }

    let sniff = json!({"enabled": true, "destOverride": ["http", "tls", "quic"], "routeOnly": false});
    let mut inbounds = vec![json!({"tag": "probe-in", "listen": "127.0.0.1", "port": ports.probe, "protocol": "http"})];
    if tun {
        inbounds.push(json!({"tag": "tun", "port": 0, "protocol": "tun", "settings": tun_settings(), "sniffing": sniff}));
    } else {
        inbounds.push(json!({"tag": "socks", "listen": "127.0.0.1", "port": SOCKS_PORT, "protocol": "socks", "settings": {"udp": true}, "sniffing": sniff}));
        inbounds.push(json!({"tag": "http", "listen": "127.0.0.1", "port": HTTP_PORT, "protocol": "http", "sniffing": sniff}));
    }
    let mut dns = (*plan.dns).clone();
    dns["tag"] = json!("dns-internal");
    Ok(json!({
        // Info carries the failed direct dials that autorouting suggestions come from.
        "log": {"loglevel": if plan.default_route.tries_direct() { "info" } else { "warning" }},
        "dns": dns,
        "inbounds": inbounds,
        "outbounds": out.list,
        "routing": {"domainStrategy": "IPIfNonMatch", "domainMatcher": "hybrid", "rules": rules},
        "observatory": {"subjectSelector": ["p-", "proxy"], "probeUrl": PROBE_URL, "probeInterval": "30s", "enableConcurrency": true},
        "stats": {},
        "policy": {"system": {"statsOutboundUplink": true, "statsOutboundDownlink": true}},
        "metrics": {"tag": "metrics", "listen": format!("127.0.0.1:{}", ports.metrics)},
    }))
}

/// The TUN inbound. Windows: the core sets the adapter's addresses, routes and DNS itself; macOS: the helper opens the
/// utun and sets them. Both bind the core's own sockets to the uplink (`autoOutboundsInterface`, filled in by the
/// helper), so its traffic to the servers never loops back into the TUN.
fn tun_settings() -> Value {
    if cfg!(windows) {
        json!({"name": TUN_NAME, "mtu": 1500, "gateway": TUN_ADDRS, "dns": [TUN_DNS], "autoSystemRoutingTable": TUN_ROUTES})
    } else {
        json!({"name": TUN_NAME, "mtu": 1500})
    }
}

/// `--tun-selftest`: a TUN that sends everything direct, to check routing, DNS and teardown on a real machine (CI).
#[cfg(not(target_os = "linux"))]
pub fn selftest_config(metrics: u16) -> Value {
    json!({
        "log": {"loglevel": "info"},
        "dns": {"servers": [TUN_DNS], "tag": "dns-internal"},
        "inbounds": [{"tag": "tun", "port": 0, "protocol": "tun", "settings": tun_settings(), "sniffing": {"enabled": true, "destOverride": ["http", "tls"]}}],
        "outbounds": [{"tag": "direct", "protocol": "freedom"}, {"tag": "dns-out", "protocol": "dns"}],
        "routing": {"rules": [{"type": "field", "inboundTag": ["tun"], "port": "53", "outboundTag": "dns-out"}]},
        "stats": {},
        "policy": {"system": {"statsOutboundUplink": true, "statsOutboundDownlink": true}},
        "metrics": {"tag": "metrics", "listen": format!("127.0.0.1:{metrics}")},
    })
}

/// One-off core that only measures every server (used while disconnected).
pub fn probe_config(servers: &[&'static Server], protocol: Protocol, metrics: u16) -> Value {
    let outbounds: Vec<Value> = servers.iter().filter_map(|s| Some(tagged(s.variant(protocol)?.1.clone(), &format!("p-{}", s.id)))).collect();
    json!({
        "log": {"loglevel": "error"},
        "outbounds": outbounds,
        "observatory": {"subjectSelector": ["p-"], "probeUrl": PROBE_URL, "probeInterval": "60s", "enableConcurrency": true},
        "metrics": {"tag": "metrics", "listen": format!("127.0.0.1:{metrics}")},
    })
}

#[derive(Default, Debug)]
pub struct Metrics {
    /// Bytes through the tunnel (every outbound except direct/block/dns).
    pub down: u64,
    pub up: u64,
    /// Probe round trip by outbound tag; `None` when the last probe failed.
    pub delays: HashMap<String, Option<u32>>,
}

pub fn metrics(port: u16) -> Option<Metrics> {
    let v: Value = ureq::get(&format!("http://127.0.0.1:{port}/debug/vars")).timeout(Duration::from_secs(2)).call().ok()?.into_json().ok()?;
    let mut m = Metrics::default();
    if let Some(out) = v["stats"]["outbound"].as_object() {
        for (tag, c) in out.iter().filter(|(t, _)| !matches!(t.as_str(), "direct" | "block" | "dns-out")) {
            // Chained hops count the same bytes again.
            if tag == "entry" || tag.starts_with("h-") {
                continue;
            }
            m.down += c["downlink"].as_u64().unwrap_or(0);
            m.up += c["uplink"].as_u64().unwrap_or(0);
        }
    }
    if let Some(obs) = v["observatory"].as_object() {
        for (tag, s) in obs {
            let alive = s["alive"].as_bool().unwrap_or(false);
            m.delays.insert(tag.clone(), alive.then(|| s["delay"].as_u64().unwrap_or(0) as u32));
        }
    }
    Some(m)
}

pub fn write_config(path: &PathBuf, config: &Value) -> Result<(), String> {
    crate::store::write(path, &serde_json::to_vec(config).unwrap(), true).map_err(|e| format!("Cannot write the core config: {e}"))
}

/// Latest core log lines, kept for error messages, and lines handed to whoever watches for failures.
#[derive(Clone, Default)]
pub struct Log {
    tail: Arc<Mutex<Vec<String>>>,
    fresh: Arc<Mutex<Vec<String>>>,
}

impl Log {
    pub fn follow(&self, stream: impl std::io::Read + Send + 'static) {
        let log = self.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                log.push(line);
            }
        });
    }

    pub fn push(&self, line: String) {
        let mut tail = self.tail.lock().unwrap();
        tail.push(line.clone());
        if tail.len() > 200 {
            tail.remove(0);
        }
        drop(tail);
        let mut fresh = self.fresh.lock().unwrap();
        if fresh.len() < 2000 {
            fresh.push(line);
        }
    }

    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.fresh.lock().unwrap())
    }

    /// The most telling recent error, without the timestamp.
    pub fn last_error(&self) -> Option<String> {
        let tail = self.tail.lock().unwrap();
        let line = tail
            .iter()
            .rev()
            .find(|l| l.contains("[Error]") || l.contains("Failed to start") || l.contains("panic:"))
            .or_else(|| tail.iter().rev().find(|l| l.contains("[Warning]")))?;
        let text = line.split_once("] ").map(|(_, t)| t).unwrap_or(line);
        Some(text.chars().take(160).collect())
    }
}

pub struct Core {
    /// Proxy and system-proxy modes: the core itself.
    child: Option<Child>,
    /// TUN mode: the privileged helper running the core; dropping it tears the routing down.
    helper: Option<crate::helper::Link>,
    pub ports: Ports,
    /// Started in TUN mode.
    pub tun: bool,
    /// The system proxy points at this core until this is dropped.
    system_proxy: Option<crate::sysproxy::Active>,
    pub log: Log,
    path: PathBuf,
}

impl Core {
    /// Start and wait until it serves; TUN mode may wait for the administrator's password.
    pub fn start(plan: &Plan) -> Result<Core, String> {
        let xray = xray_path().ok_or("The Xray core is missing — run scripts/fetch-xray.sh")?;
        let ports = Ports { probe: free_port(), metrics: free_port() };
        let path = crate::store::runtime_dir().join("core.json");
        let config = config(plan, ports)?;
        let log = Log::default();
        if plan.tun {
            let helper = crate::helper::Link::start(&xray, &path, &config, plan.kill_switch, &log)?;
            return Ok(Core { child: None, helper: Some(helper), ports, tun: true, system_proxy: None, log, path });
        }
        write_config(&path, &config)?;
        let mut cmd = Command::new(&xray);
        cmd.args(["run", "-c"]).arg(&path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = crate::os::spawn_core(&mut cmd).map_err(|e| format!("Cannot start the core: {e}"))?;
        // The core logs to stdout; stderr carries Go panics.
        log.follow(child.stdout.take().unwrap());
        log.follow(child.stderr.take().unwrap());
        let mut core = Core { child: Some(child), helper: None, ports, tun: false, system_proxy: None, log, path };
        core.wait_serving(Duration::from_secs(10))?;
        if plan.system_proxy {
            core.system_proxy = Some(crate::sysproxy::set(HTTP_PORT, SOCKS_PORT).map_err(|e| format!("Cannot set the system proxy: {e}"))?);
        }
        Ok(core)
    }

    fn wait_serving(&mut self, limit: Duration) -> Result<(), String> {
        let start = Instant::now();
        let addr = SocketAddr::from(([127, 0, 0, 1], self.ports.metrics));
        while start.elapsed() < limit {
            if let Some(Ok(Some(status))) = self.child.as_mut().map(|c| c.try_wait()) {
                return Err(self.log.last_error().unwrap_or_else(|| format!("The core stopped ({status})")));
            }
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("The core did not start in time".into())
    }

    /// Apply a new plan: the helper restarts the core under the same routing; otherwise the core is restarted.
    pub fn reload(&mut self, plan: &Plan) -> Result<(), String> {
        if plan.tun != self.tun || plan.system_proxy != self.system_proxy.is_some() {
            return Err("mode changed".into());
        }
        let config = config(plan, self.ports)?;
        if let Some(helper) = self.helper.as_mut() {
            return helper.reload(&self.path, &config);
        }
        self.stop_child();
        // The system proxy stays in place for the core that replaces this one.
        let proxy = self.system_proxy.take();
        let mut fresh = Core::start(&Plan { system_proxy: false, ..plan.clone() })?;
        fresh.system_proxy = proxy;
        std::mem::swap(self, &mut fresh);
        Ok(())
    }

    pub fn set_kill_switch(&mut self, on: bool) {
        if let Some(helper) = self.helper.as_mut() {
            let _ = helper.send(if on { "kill 1" } else { "kill 0" });
        }
    }

    /// Still running? In TUN mode the helper outlives a crashed core and reports it.
    pub fn alive(&mut self) -> bool {
        match (self.helper.as_mut(), self.child.as_mut()) {
            (Some(helper), _) => helper.alive(),
            (None, Some(child)) => !matches!(child.try_wait(), Ok(Some(_))),
            (None, None) => false,
        }
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn stop_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            crate::os::reaped(&child);
        }
    }

    fn shutdown(&mut self) {
        // The system proxy first, so apps never point at a core that is gone.
        self.system_proxy = None;
        self.helper = None;
        self.stop_child();
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Core {
    /// Also when the core died on its own: the config holds server credentials.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Exit address through the tunnel itself (the `probe-in` listener always goes through `proxy`).
pub fn exit_ip(probe: u16) -> Option<String> {
    // Socket timeouts also bound the proxy's CONNECT handshake, which the overall timeout does not.
    let agent = ureq::AgentBuilder::new()
        .proxy(ureq::Proxy::new(format!("http://127.0.0.1:{probe}")).ok()?)
        .timeout(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(10))
        .timeout_write(Duration::from_secs(10))
        .build();
    let v: Value = agent.get("https://stat.ripe.net/data/whats-my-ip/data.json").call().ok()?.into_json().ok()?;
    v["data"]["ip"].as_str().map(str::to_owned)
}

/// Through the tunnel: can `host:443` be reached? (Confirms a suggestion before showing it.)
pub fn reachable(probe: u16, host: &str) -> bool {
    let Ok(mut s) = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], probe)), Duration::from_secs(2)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    if write!(s, "CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\r\n").is_err() {
        return false;
    }
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).is_ok() && line.split_whitespace().nth(1) == Some("200")
}

/// Measure every server once with a throwaway core; `None` for servers that did not answer.
pub fn probe(servers: &[&'static Server], protocol: Protocol) -> Result<Vec<(&'static str, Option<u32>)>, String> {
    let xray = xray_path().ok_or("The Xray core is missing")?;
    let port = free_port();
    let path = crate::store::runtime_dir().join("probe.json");
    write_config(&path, &probe_config(servers, protocol, port))?;
    let mut cmd = Command::new(xray);
    cmd.args(["run", "-c"]).arg(&path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = crate::os::spawn_core(&mut cmd).map_err(|e| e.to_string())?;
    let start = Instant::now();
    let mut result = HashMap::new();
    while start.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(500));
        if let Some(m) = metrics(port) {
            result = m.delays;
            if servers.iter().all(|s| result.contains_key(&format!("p-{}", s.id))) {
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    crate::os::reaped(&child);
    let _ = std::fs::remove_file(&path);
    Ok(servers.iter().map(|s| (s.id, result.get(&format!("p-{}", s.id)).copied().flatten())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &'static str, protos: &[Protocol]) -> &'static Server {
        let variants = protos
            .iter()
            .map(|p| {
                let o = match p {
                    Protocol::Hy2 => json!({"protocol": "hysteria", "streamSettings": {"network": "hysteria"}}),
                    Protocol::Grpc => json!({"protocol": "vless", "settings": {"n": id}, "streamSettings": {"network": "grpc"}}),
                    Protocol::Tls => json!({"protocol": "vless", "settings": {"n": id}, "streamSettings": {"network": "tcp"}}),
                };
                (*p, o)
            })
            .collect();
        Box::leak(Box::new(Server { id, country: "X", code: "XX", city: "", ip: "", lat: 0., lon: 0., variants }))
    }

    fn plan() -> Plan {
        let a = node("aa-1", &[Protocol::Tls, Protocol::Hy2]);
        let b = node("bb-2", &[Protocol::Grpc]);
        Plan {
            exit: a,
            entry: None,
            protocol: Protocol::Tls,
            default_route: RuleMode::Auto,
            tun: false,
            system_proxy: false,
            kill_switch: true,
            apps: vec![],
            sites: vec![],
            servers: vec![a, b],
            tunnel_rules: Arc::new(vec![json!({"type": "field", "domain": ["domain:blocked.example"], "outboundTag": "proxy"})]),
            base_rules: Arc::new(vec![json!({"type": "field", "protocol": ["bittorrent"], "outboundTag": "direct"})]),
            dns: Arc::new(json!({"servers": ["1.1.1.1"]})),
        }
    }

    fn position(c: &Value, key: &str, value: &str) -> Option<usize> {
        c["routing"]["rules"].as_array().unwrap().iter().position(|r| r[key].as_array().is_some_and(|v| v.iter().any(|x| x == value)))
    }

    fn rule_for<'a>(c: &'a Value, key: &str, value: &str) -> Option<&'a Value> {
        position(c, key, value).map(|i| &c["routing"]["rules"][i])
    }

    #[test]
    fn autoroute_sends_lists_through_the_tunnel_and_the_rest_direct() {
        let c = config(&plan(), Ports { probe: 1, metrics: 2 }).unwrap();
        let rules = c["routing"]["rules"].as_array().unwrap();
        assert_eq!(rules.last().unwrap()["outboundTag"], "direct");
        assert_eq!(rule_for(&c, "domain", "domain:blocked.example").unwrap()["outboundTag"], "proxy");
        assert_eq!(rule_for(&c, "protocol", "bittorrent").unwrap()["outboundTag"], "direct");
        let tags: Vec<_> = c["outbounds"].as_array().unwrap().iter().map(|o| o["tag"].as_str().unwrap().to_owned()).collect();
        assert_eq!(tags, ["proxy", "p-aa-1", "p-bb-2-Grpc", "direct", "block"]);
        assert!(c["inbounds"].as_array().unwrap().iter().any(|i| i["port"] == SOCKS_PORT));
    }

    #[test]
    fn default_route_decides_the_catch_all() {
        let last = |p: &Plan| config(p, Ports { probe: 1, metrics: 2 }).unwrap()["routing"]["rules"].as_array().unwrap().last().unwrap()["outboundTag"].clone();
        for (mode, out) in [(RuleMode::Vpn, "proxy"), (RuleMode::Direct, "direct"), (RuleMode::Block, "block")] {
            let mut p = plan();
            p.default_route = mode;
            let c = config(&p, Ports { probe: 1, metrics: 2 }).unwrap();
            assert_eq!(last(&p), out, "{mode:?}");
            assert!(rule_for(&c, "domain", "domain:blocked.example").is_none(), "{mode:?}: lists only for Auto");
            assert_eq!(rule_for(&c, "protocol", "bittorrent").is_some(), mode != RuleMode::Block, "{mode:?}: provider rules");
            assert_eq!(rule_for(&c, "inboundTag", "probe-in").unwrap()["outboundTag"], "proxy", "{mode:?}: probes always tunnelled");
        }
    }

    #[test]
    fn auto_app_under_another_default_gets_its_own_lists() {
        let mut p = plan();
        p.default_route = RuleMode::Vpn;
        p.apps = vec![("steam", AppRule::mode(RuleMode::Auto))];
        let c = config(&p, Ports { probe: 1, metrics: 2 }).unwrap();
        let rules = c["routing"]["rules"].as_array().unwrap();
        let listed = rules.iter().position(|r| r["process"].as_array().is_some_and(|v| v.iter().any(|x| x == "steam")) && r["domain"].is_array()).unwrap();
        assert_eq!(rules[listed]["outboundTag"], "proxy");
        let rest = rules.iter().position(|r| r["process"].as_array().is_some_and(|v| v.iter().any(|x| x == "steam")) && r["domain"].is_null()).unwrap();
        assert_eq!(rules[rest]["outboundTag"], "direct");
        assert!(listed < rest);
        p.default_route = RuleMode::Auto;
        let c = config(&p, Ports { probe: 1, metrics: 2 }).unwrap();
        assert!(rule_for(&c, "process", "steam").is_none(), "follows the Auto default without rules");
    }

    #[test]
    fn cascade_and_app_rules() {
        let mut p = plan();
        let b = p.servers[1];
        p.entry = Some(b);
        p.sites = vec![("bank.example", AppRule::mode(RuleMode::Direct)), ("video.example", AppRule { exit: Some("bb-2"), ..AppRule::mode(RuleMode::Vpn) }), ("ads.example", AppRule::mode(RuleMode::Block))];
        p.apps = vec![
            ("steam", AppRule::mode(RuleMode::Direct)),
            ("qbittorrent", AppRule::mode(RuleMode::Block)),
            ("helium", AppRule { exit: Some("bb-2"), ..AppRule::mode(RuleMode::Vpn) }),
        ];
        let c = config(&p, Ports { probe: 1, metrics: 2 }).unwrap();
        let outs = c["outbounds"].as_array().unwrap();
        let proxy = outs.iter().find(|o| o["tag"] == "proxy").unwrap();
        assert_eq!(proxy["streamSettings"]["sockopt"]["dialerProxy"], "entry");
        assert_eq!(outs.iter().find(|o| o["tag"] == "entry").unwrap()["streamSettings"]["network"], "grpc");
        assert_eq!(rule_for(&c, "process", "steamwebhelper").unwrap()["outboundTag"], "direct");
        assert_eq!(rule_for(&c, "process", "qbittorrent").unwrap()["outboundTag"], "block");
        assert_eq!(rule_for(&c, "process", "helium").map(|r| r["outboundTag"].clone()), Some(json!("p-bb-2-Grpc")), "{:#}", c["routing"]["rules"]);
        assert_eq!(rule_for(&c, "domain", "domain:bank.example").unwrap()["outboundTag"], "direct");
        assert_eq!(rule_for(&c, "domain", "domain:video.example").unwrap()["outboundTag"], "p-bb-2-Grpc", "a website's own exit");
        assert_eq!(rule_for(&c, "domain", "domain:ads.example").unwrap()["outboundTag"], "block");
        let (block, site, direct) = (position(&c, "process", "qbittorrent").unwrap(), position(&c, "domain", "domain:bank.example").unwrap(), position(&c, "process", "steam").unwrap());
        assert!(block < site && site < direct, "blocked apps, then sites, then other apps");
    }

    #[test]
    fn system_wide_marks_every_socket_and_hijacks_dns() {
        let mut p = plan();
        p.tun = true;
        let c = config(&p, Ports { probe: 1, metrics: 2 }).unwrap();
        for o in c["outbounds"].as_array().unwrap() {
            let mark = &o["streamSettings"]["sockopt"]["mark"];
            if o["tag"] == "block" { assert!(mark.is_null()) } else { assert_eq!(mark, MARK) }
        }
        assert_eq!(c["routing"]["rules"][0]["outboundTag"], "dns-out");
        assert!(c["inbounds"].as_array().unwrap().iter().any(|i| i["protocol"] == "tun" && i["settings"]["name"] == TUN_NAME));
        assert!(!c["inbounds"].as_array().unwrap().iter().any(|i| i["port"] == SOCKS_PORT));
    }

    /// Against the real subscription and core: HITORI_TEST_SUB=<subscription JSON> cargo test live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_subscription_through_the_real_core() {
        let Ok(path) = std::env::var("HITORI_TEST_SUB") else { return };
        let sub = crate::sub::parse(&std::fs::read(path).unwrap()).unwrap();
        crate::data::set_servers(sub.servers);
        let servers = crate::data::servers();
        let xray = xray_path().expect("bin/xray");
        let pings = probe(&servers, Protocol::Tls).unwrap();
        eprintln!("probe: {pings:?}");
        assert!(pings.iter().filter(|(_, p)| p.is_some()).count() >= servers.len() / 2);
        let mut up: Vec<_> = pings.iter().filter_map(|(id, p)| Some((*p.as_ref()?, *id))).collect();
        up.sort();
        let pick = |i: usize| servers.iter().copied().find(|s| s.id == up[i].1).unwrap();
        // Two answering servers in different countries.
        let ee = pick(0);
        let ch = (1..up.len()).map(pick).find(|s| s.code != ee.code).unwrap();
        let (main_loc, other_loc) = (format!("loc={}", ee.code), format!("loc={}", ch.code));
        eprintln!("exit {} other {}", ee.id, ch.id);
        let mut plan = Plan {
            exit: ee,
            entry: None,
            protocol: Protocol::Tls,
            default_route: RuleMode::Vpn,
            tun: false,
            system_proxy: false,
            kill_switch: true,
            apps: vec![("curl", AppRule { exit: Some(ch.id), ..AppRule::mode(RuleMode::Vpn) })],
            sites: vec![("example.org", AppRule::mode(RuleMode::Direct))],
            servers: servers.clone(),
            tunnel_rules: Arc::new(sub.tunnel_rules),
            base_rules: Arc::new(sub.base_rules),
            dns: Arc::new(sub.dns),
        };
        // Every generated variant must pass the core's own validation.
        for (tun, entry, route, proto) in [(false, None, RuleMode::Vpn, Protocol::Tls), (true, None, RuleMode::Auto, Protocol::Hy2), (false, Some(ch), RuleMode::Block, Protocol::Grpc)] {
            let mut p = plan.clone();
            (p.tun, p.entry, p.default_route, p.protocol) = (tun, entry, route, proto);
            let path = std::env::temp_dir().join(format!("hitori-test-{}.json", std::process::id()));
            crate::store::write(&path, &serde_json::to_vec(&config(&p, Ports { probe: free_port(), metrics: free_port() }).unwrap()).unwrap(), true).unwrap();
            let out = Command::new(&xray).args(["run", "-test", "-c"]).arg(&path).output().unwrap();
            std::fs::remove_file(&path).unwrap();
            assert!(out.status.success(), "tun={tun} entry={} route={route:?}: {}", entry.is_some(), String::from_utf8_lossy(&out.stdout));
        }
        plan.apps.clear();
        let core = Core::start(&plan).unwrap();
        let ip = exit_ip(core.ports.probe).expect("exit ip through the tunnel");
        eprintln!("exit ip via {}: {ip}", ee.id);
        let trace = |args: &[&str]| {
            let out = Command::new("curl").args(args).args(["-sS", "-m", "15", "https://1.1.1.1/cdn-cgi/trace"]).output().unwrap();
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
        };
        let got = trace(&["--socks5-hostname", &format!("127.0.0.1:{SOCKS_PORT}")]);
        assert!(got.contains(&main_loc), "{main_loc}: {got:?} / {:?}", core.log.last_error());
        std::thread::sleep(Duration::from_secs(4));
        let m = metrics(core.ports.metrics).unwrap();
        eprintln!("metrics: down {} up {} proxy delay {:?}", m.down, m.up, m.delays.get("proxy"));
        assert!(m.down > 0 && m.up > 0);
        assert!(reachable(core.ports.probe, "www.gstatic.com"));
        // Per-app exit: curl's traffic goes to Switzerland while the main exit stays Estonia.
        let mut per_app = plan.clone();
        per_app.apps = vec![("curl", AppRule { exit: Some(ch.id), ..AppRule::mode(RuleMode::Vpn) })];
        let mut core = core;
        core.reload(&per_app).unwrap();
        assert!(trace(&["--socks5-hostname", &format!("127.0.0.1:{SOCKS_PORT}")]).contains(&other_loc));
        // Cascade: Switzerland exit through Estonia.
        let mut cascade = plan.clone();
        (cascade.exit, cascade.entry) = (ch, Some(ee));
        core.reload(&cascade).unwrap();
        assert!(trace(&["--proxy", &format!("http://127.0.0.1:{HTTP_PORT}")]).contains(&other_loc));
        let config = core.path.clone();
        core.stop();
        assert!(!config.exists(), "config with credentials removed");
    }

    /// System-wide mode on this machine through pkexec (asks for the password; build target/debug/hitori first):
    /// HITORI_TEST_SUB=<subscription JSON> cargo test live_system_wide -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_system_wide() {
        let Ok(path) = std::env::var("HITORI_TEST_SUB") else { return };
        let sub = crate::sub::parse(&std::fs::read(path).unwrap()).unwrap();
        crate::data::set_servers(sub.servers);
        let servers = crate::data::servers();
        let sh = |cmd: &str| String::from_utf8_lossy(&Command::new("sh").args(["-c", cmd]).output().unwrap().stdout).into_owned();
        let loc = |tries: u32| {
            let mut got = String::new();
            for _ in 0..tries {
                got = sh("curl -s -m 10 https://1.1.1.1/cdn-cgi/trace | grep ^loc=");
                if !got.is_empty() {
                    break;
                }
            }
            got.trim().to_owned()
        };
        let country = || {
            let mut got = String::new();
            for _ in 0..3 {
                let start = Instant::now();
                got = sh("curl -4 -sS -m 15 https://ipinfo.io/country 2>&1").trim().to_owned();
                eprintln!("ipinfo.io: {got:?} in {:?}", start.elapsed());
                if got.len() == 2 {
                    break;
                }
            }
            got
        };
        let rules = || sh("ip rule; ip -6 rule").lines().filter(|l| l.starts_with("733")).count();
        let before = loc(3);
        assert_eq!(rules(), 0, "leftover rules from an earlier run");
        let mut up: Vec<_> = probe(&servers, Protocol::Tls).unwrap().into_iter().filter_map(|(id, p)| Some((p?, id))).collect();
        up.sort();
        // The fastest server in another country, so the exit is visible.
        let exit = up.iter().filter_map(|(_, id)| servers.iter().copied().find(|s| s.id == *id)).find(|s| format!("loc={}", s.code) != before).unwrap();
        let want = format!("loc={}", exit.code);
        eprintln!("direct {before}, exit {} ({})", exit.id, exit.code);

        let plan = Plan {
            exit,
            entry: None,
            protocol: Protocol::Tls,
            default_route: RuleMode::Vpn,
            tun: true,
            system_proxy: false,
            kill_switch: true,
            apps: vec![],
            sites: vec![],
            servers: servers.clone(),
            tunnel_rules: Arc::new(sub.tunnel_rules),
            base_rules: Arc::new(sub.base_rules),
            dns: Arc::new(sub.dns),
        };
        let mut core = Core::start(&plan).unwrap();
        assert!(rules() >= 5, "policy rules installed");
        assert_eq!(loc(3), want, "plain curl goes through the tunnel");
        assert_eq!(format!("loc={}", country()), want, "names resolve and connect through the tunnel");
        assert!(sh("ip route show table 26869").contains("unreachable default"), "kill switch on");
        core.set_kill_switch(false);
        std::thread::sleep(Duration::from_millis(500));
        assert!(!sh("ip route show table 26869").contains("unreachable"), "kill switch off");
        core.set_kill_switch(true);
        core.reload(&plan).unwrap();
        assert!(core.alive());
        assert_eq!(loc(3), want, "still tunneled after a restart");
        eprintln!("system-wide: {want}, restart and kill switch ok");

        let config = core.path.clone();
        core.stop();
        assert!(!config.exists(), "config with credentials removed");
        assert_eq!(rules(), 0, "rules removed");
        assert!(sh("ip route show table 26869").is_empty(), "table flushed");
        assert!(sh("ip -br link show dev hitori0 2>&1").contains("does not exist"), "tun gone");
        assert_eq!(loc(3), before, "direct again");
    }
}

