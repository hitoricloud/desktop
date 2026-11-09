//! The prebuilt Xray core: finding it, the config the app generates for it, and running it.
//! Proxy mode runs it as the user with SOCKS/HTTP listeners; system-wide mode hands it to the root helper (`tun.rs`).
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::data::{AppRule, Protocol, RuleMode, Server, SiteRule, processes};

pub const SOCKS_PORT: u16 = 10808;
pub const HTTP_PORT: u16 = 10809;
pub const TUN_NAME: &str = "hitori0";
/// Firewall mark on the core's own sockets, so they skip the TUN routing table.
pub const MARK: u32 = 0x6869;
const PROBE_URL: &str = "https://www.gstatic.com/generate_204";

/// `HITORI_XRAY`, next to the executable, `../libexec/hitori/xray`, then the checkout's `bin/xray` (scripts/fetch-xray.sh).
pub fn xray_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok();
    let dir = exe.as_ref().and_then(|e| e.parent().map(PathBuf::from));
    let candidates = [
        std::env::var_os("HITORI_XRAY").map(PathBuf::from),
        dir.as_ref().map(|d| d.join("xray")),
        dir.as_ref().map(|d| d.join("../libexec/hitori/xray")),
        Some(PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/bin/xray"))),
    ];
    candidates.into_iter().flatten().find(|p| p.is_file()).and_then(|p| p.canonicalize().ok())
}

/// What to connect to and how to route, as chosen in the app.
#[derive(Clone)]
pub struct Plan {
    pub exit: &'static Server,
    pub entry: Option<&'static Server>,
    pub protocol: Protocol,
    pub autoroute: bool,
    pub tun: bool,
    pub kill_switch: bool,
    pub apps: Vec<(&'static str, AppRule)>,
    pub sites: Vec<(&'static str, SiteRule)>,
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
/// then the autorouting lists and the catch-all.
pub fn config(plan: &Plan, ports: Ports) -> Result<Value, String> {
    let exit = plan.exit;
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
    if plan.tun {
        rules.push(json!({"type": "field", "inboundTag": ["tun"], "port": "53", "outboundTag": "dns-out"}));
    }
    rules.push(field(json!({"inboundTag": ["dns-internal", "probe-in"]}), "proxy"));
    rules.push(field(json!({"ip": PRIVATE}), "direct"));
    rules.push(field(json!({"domain": ["domain:localhost"]}), "direct"));
    rules.extend(plan.base_rules.iter().cloned());

    let names = |app: &str| processes(app);
    for (app, _) in plan.apps.iter().filter(|(_, r)| r.mode == RuleMode::Block) {
        rules.push(field(json!({"process": names(app)}), "block"));
    }
    for (site, rule) in &plan.sites {
        let domain = json!({"domain": [format!("domain:{site}")]});
        rules.push(field(domain, if *rule == SiteRule::Vpn { "proxy" } else { "direct" }));
    }
    for (app, r) in &plan.apps {
        let custom = r.exit.is_some() || r.via.is_some() || r.protocol.is_some();
        let tag = |out: &mut Outbounds| -> Option<String> {
            if !custom {
                return Some("proxy".into());
            }
            let exit = r.exit.map(crate::data::server).unwrap_or(exit);
            let via = match (r.via, r.exit) {
                (Some(v), _) => Some(crate::data::server(v)),
                (None, None) => entry,
                (None, Some(_)) => None,
            };
            out.route(exit, via, r.protocol.unwrap_or(plan.protocol))
        };
        match r.mode {
            RuleMode::Block => {}
            RuleMode::Direct => rules.push(field(json!({"process": names(app)}), "direct")),
            RuleMode::Vpn => {
                if let Some(t) = tag(&mut out) {
                    rules.push(field(json!({"process": names(app)}), &t));
                }
            }
            // Auto with its own exit: what needs the tunnel goes there, the rest stays direct like everything else.
            RuleMode::Auto if custom => {
                if let Some(t) = tag(&mut out) {
                    for list in plan.tunnel_rules.iter() {
                        let mut rule = list.clone();
                        rule["process"] = json!(names(app));
                        rule["outboundTag"] = json!(t);
                        rules.push(rule);
                    }
                }
            }
            RuleMode::Auto => {}
        }
    }
    if plan.autoroute {
        rules.extend(plan.tunnel_rules.iter().cloned());
    }
    rules.push(field(json!({"network": "tcp,udp"}), if plan.autoroute { "direct" } else { "proxy" }));

    out.list.push(json!({"tag": "direct", "protocol": "freedom"}));
    out.list.push(json!({"tag": "block", "protocol": "blackhole"}));
    if plan.tun {
        out.list.push(json!({"tag": "dns-out", "protocol": "dns"}));
        out.list = out.list.into_iter().map(|o| if o["tag"] == "block" { o } else { with_sockopt(o, "mark", json!(MARK)) }).collect();
    }

    let sniff = json!({"enabled": true, "destOverride": ["http", "tls", "quic"], "routeOnly": false});
    let mut inbounds = vec![json!({"tag": "probe-in", "listen": "127.0.0.1", "port": ports.probe, "protocol": "http"})];
    if plan.tun {
        inbounds.push(json!({"tag": "tun", "port": 0, "protocol": "tun", "settings": {"name": TUN_NAME, "mtu": 1500}, "sniffing": sniff}));
    } else {
        inbounds.push(json!({"tag": "socks", "listen": "127.0.0.1", "port": SOCKS_PORT, "protocol": "socks", "settings": {"udp": true}, "sniffing": sniff}));
        inbounds.push(json!({"tag": "http", "listen": "127.0.0.1", "port": HTTP_PORT, "protocol": "http", "sniffing": sniff}));
    }
    let mut dns = (*plan.dns).clone();
    dns["tag"] = json!("dns-internal");
    Ok(json!({
        "log": {"loglevel": "warning"},
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

fn write_config(path: &PathBuf, config: &Value) -> Result<(), String> {
    crate::store::write(path, &serde_json::to_vec(config).unwrap(), true).map_err(|e| format!("Cannot write the core config: {e}"))
}

/// Kill the core with the app, whatever way the app ends.
fn die_with_parent(cmd: &mut Command) {
    // SAFETY: prctl is async-signal-safe; this runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
}

/// Latest core log lines, kept for error messages, and lines handed to whoever watches for failures.
#[derive(Clone, Default)]
pub struct Log {
    tail: Arc<Mutex<Vec<String>>>,
    fresh: Arc<Mutex<Vec<String>>>,
}

impl Log {
    fn follow(&self, stream: impl std::io::Read + Send + 'static) {
        let log = self.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let mut tail = log.tail.lock().unwrap();
                tail.push(line.clone());
                if tail.len() > 40 {
                    tail.remove(0);
                }
                drop(tail);
                let mut fresh = log.fresh.lock().unwrap();
                if fresh.len() < 2000 {
                    fresh.push(line);
                }
            }
        });
    }

    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.fresh.lock().unwrap())
    }

    /// The most telling recent error, without the timestamp.
    pub fn last_error(&self) -> Option<String> {
        let tail = self.tail.lock().unwrap();
        let line = tail.iter().rev().find(|l| l.contains("Failed") || l.contains("failed") || l.contains("[Error]") || l.contains("error"))?;
        let text = line.split_once("] ").map(|(_, t)| t).unwrap_or(line);
        Some(text.chars().take(160).collect())
    }
}

pub struct Core {
    child: Child,
    /// System-wide mode: commands to the root helper; closing it tears the routing down.
    helper: Option<ChildStdin>,
    helper_out: Option<std::sync::mpsc::Receiver<String>>,
    pub ports: Ports,
    pub tun: bool,
    pub log: Log,
    path: PathBuf,
}

impl Core {
    /// Start and wait until it serves; system-wide mode may wait for the administrator's password.
    pub fn start(plan: &Plan) -> Result<Core, String> {
        let xray = xray_path().ok_or("The Xray core is missing — run scripts/fetch-xray.sh")?;
        let ports = Ports { probe: free_port(), metrics: free_port() };
        let path = crate::store::runtime_dir().join("core.json");
        write_config(&path, &config(plan, ports)?)?;
        let log = Log::default();
        if !plan.tun {
            let mut cmd = Command::new(&xray);
            cmd.args(["run", "-c"]).arg(&path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
            die_with_parent(&mut cmd);
            let mut child = cmd.spawn().map_err(|e| format!("Cannot start the core: {e}"))?;
            log.follow(child.stderr.take().unwrap());
            let mut core = Core { child, helper: None, helper_out: None, ports, tun: false, log, path };
            core.wait_serving(Duration::from_secs(10))?;
            return Ok(core);
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        // SAFETY: getuid/getgid cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut cmd = Command::new("pkexec");
        cmd.arg(exe)
            .arg("--tun-helper")
            .arg(&xray)
            .arg(&path)
            .args([uid.to_string(), gid.to_string(), (plan.kill_switch as u8).to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("Cannot ask for administrator rights (pkexec): {e}"))?;
        log.follow(child.stderr.take().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let helper = child.stdin.take();
        let mut core = Core { child, helper, helper_out: Some(rx), ports, tun: true, log, path };
        core.wait_ready(Duration::from_secs(180))?;
        Ok(core)
    }

    fn wait_serving(&mut self, limit: Duration) -> Result<(), String> {
        let start = Instant::now();
        let addr = SocketAddr::from(([127, 0, 0, 1], self.ports.metrics));
        while start.elapsed() < limit {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(self.log.last_error().unwrap_or_else(|| format!("The core stopped ({status})")));
            }
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("The core did not start in time".into())
    }

    /// Helper lines until `ready`; `down`/exit means it failed.
    fn wait_ready(&mut self, limit: Duration) -> Result<(), String> {
        let start = Instant::now();
        let rx = self.helper_out.as_ref().unwrap();
        while start.elapsed() < limit {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(line) if line == "ready" => return Ok(()),
                Ok(line) if line.starts_with("error ") => return Err(line[6..].to_owned()),
                Ok(line) if line.starts_with("down") => return Err(self.log.last_error().unwrap_or_else(|| "The core stopped".into())),
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let status = self.child.wait().ok().and_then(|s| s.code());
                    return Err(match status {
                        Some(126) | Some(127) => "Administrator rights were not granted".into(),
                        _ => self.log.last_error().unwrap_or_else(|| "System-wide mode could not start".into()),
                    });
                }
            }
        }
        Err("Timed out waiting for administrator rights".into())
    }

    /// Apply a new plan: the helper restarts the core under the same routing (no leak in between); proxy mode restarts it.
    pub fn reload(&mut self, plan: &Plan) -> Result<(), String> {
        if plan.tun != self.tun {
            return Err("mode changed".into());
        }
        write_config(&self.path, &config(plan, self.ports)?)?;
        if self.tun {
            self.send("restart")?;
            return self.wait_ready(Duration::from_secs(30));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut fresh = Core::start(plan)?;
        std::mem::swap(self, &mut fresh);
        Ok(())
    }

    fn send(&mut self, line: &str) -> Result<(), String> {
        let helper = self.helper.as_mut().ok_or("no helper")?;
        writeln!(helper, "{line}").and_then(|_| helper.flush()).map_err(|_| "The system-wide helper stopped".to_owned())
    }

    pub fn set_kill_switch(&mut self, on: bool) {
        if self.tun {
            let _ = self.send(if on { "kill 1" } else { "kill 0" });
        }
    }

    /// Still running? In system-wide mode the helper outlives a crashed core and reports it.
    pub fn alive(&mut self) -> bool {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return false;
        }
        match &self.helper_out {
            Some(rx) => !rx.try_iter().any(|l| l.starts_with("down")),
            None => true,
        }
    }

    /// Exit address through the tunnel itself.
    pub fn exit_ip(&self) -> Option<String> {
        let agent = ureq::AgentBuilder::new().proxy(ureq::Proxy::new(format!("http://127.0.0.1:{}", self.ports.probe)).ok()?).timeout(Duration::from_secs(15)).build();
        let v: Value = agent.get("https://stat.ripe.net/data/whats-my-ip/data.json").call().ok()?.into_json().ok()?;
        v["data"]["ip"].as_str().map(str::to_owned)
    }

    /// Through the tunnel: can `host:443` be reached? (Confirms a suggestion before showing it.)
    pub fn reachable(&self, host: &str) -> bool {
        let Ok(mut s) = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], self.ports.probe)), Duration::from_secs(2)) else { return false };
        let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
        if write!(s, "CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\r\n").is_err() {
            return false;
        }
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).is_ok() && line.split_whitespace().nth(1) == Some("200")
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if self.helper.take().is_some() {
            // EOF: the helper stops the core and removes its routes.
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(5) {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.shutdown();
        }
    }
}

/// Measure every server once with a throwaway core; `None` for servers that did not answer.
pub fn probe(servers: &[&'static Server], protocol: Protocol) -> Result<Vec<(&'static str, Option<u32>)>, String> {
    let xray = xray_path().ok_or("The Xray core is missing")?;
    let port = free_port();
    let path = crate::store::runtime_dir().join("probe.json");
    write_config(&path, &probe_config(servers, protocol, port))?;
    let mut cmd = Command::new(xray);
    cmd.args(["run", "-c"]).arg(&path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    die_with_parent(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
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
            autoroute: true,
            tun: false,
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
        let mut p = plan();
        p.autoroute = false;
        let c = config(&p, Ports { probe: 1, metrics: 2 }).unwrap();
        assert_eq!(c["routing"]["rules"].as_array().unwrap().last().unwrap()["outboundTag"], "proxy");
        assert!(rule_for(&c, "domain", "domain:blocked.example").is_none());
    }

    #[test]
    fn cascade_and_app_rules() {
        let mut p = plan();
        let b = p.servers[1];
        p.entry = Some(b);
        p.sites = vec![("bank.example", SiteRule::Direct)];
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
        assert_eq!(rule_for(&c, "process", "helium").unwrap()["outboundTag"], "p-bb-2-Grpc");
        assert_eq!(rule_for(&c, "domain", "domain:bank.example").unwrap()["outboundTag"], "direct");
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
}
