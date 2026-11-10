//! The prebuilt Xray core: finding it, the config the app generates for it, and running it.
//! Proxy mode runs it as the user with SOCKS/HTTP listeners; system-wide mode hands it to the root helper (`tun.rs`).
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::data::{AppRule, Protocol, RuleMode, Server, processes};

pub const SOCKS_PORT: u16 = 10808;
pub const HTTP_PORT: u16 = 10809;
pub const TUN_NAME: &str = "hitori0";
/// Firewall mark on the core's own sockets, so they skip the TUN routing table.
pub const MARK: u32 = 0x6869;
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
    pub tun: bool,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
    // A TUN only on Linux (the system proxy elsewhere); app rules only where the core can tell apps apart.
    let tun = plan.tun && crate::os::TUN;
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
        out.list = out.list.into_iter().map(|o| if o["tag"] == "block" { o } else { with_sockopt(o, "mark", json!(MARK)) }).collect();
    }

    let sniff = json!({"enabled": true, "destOverride": ["http", "tls", "quic"], "routeOnly": false});
    let mut inbounds = vec![json!({"tag": "probe-in", "listen": "127.0.0.1", "port": ports.probe, "protocol": "http"})];
    if tun {
        inbounds.push(json!({"tag": "tun", "port": 0, "protocol": "tun", "settings": {"name": TUN_NAME, "mtu": 1500}, "sniffing": sniff}));
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
                if tail.len() > 200 {
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
    child: Child,
    /// System-wide mode: commands to the root helper; closing it tears the routing down.
    helper: Option<ChildStdin>,
    helper_out: Option<std::sync::mpsc::Receiver<String>>,
    pub ports: Ports,
    /// Started in system-wide mode (a TUN through the helper on Linux, the system proxy elsewhere).
    pub tun: bool,
    /// The system proxy points at this core and is put back when it stops.
    sysproxy: bool,
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
        if !plan.tun || !crate::os::TUN {
            let mut cmd = Command::new(&xray);
            cmd.args(["run", "-c"]).arg(&path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = crate::os::spawn_core(&mut cmd).map_err(|e| format!("Cannot start the core: {e}"))?;
            // The core logs to stdout; stderr carries Go panics.
            log.follow(child.stdout.take().unwrap());
            log.follow(child.stderr.take().unwrap());
            let mut core = Core { child, helper: None, helper_out: None, ports, tun: plan.tun, sysproxy: false, log, path };
            core.wait_serving(Duration::from_secs(10))?;
            if plan.tun {
                crate::os::sysproxy::set(HTTP_PORT, SOCKS_PORT).map_err(|e| format!("Cannot set the system proxy: {e}"))?;
                core.sysproxy = true;
            }
            return Ok(core);
        }
        #[cfg(not(target_os = "linux"))]
        unreachable!("system-wide mode is the system proxy outside Linux");
        #[cfg(target_os = "linux")]
        Self::start_helper(&xray, path, ports, log, plan)
    }

    /// System-wide mode: the app itself as root (`--tun-helper`, through pkexec) runs the core behind a TUN device.
    #[cfg(target_os = "linux")]
    fn start_helper(xray: &PathBuf, path: PathBuf, ports: Ports, log: Log, plan: &Plan) -> Result<Core, String> {
        // Tests run the built app as the helper.
        let exe = if cfg!(test) { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/hitori") } else { std::env::current_exe().map_err(|e| e.to_string())? };
        // SAFETY: getuid/getgid/geteuid cannot fail.
        let (uid, gid, root) = unsafe { (libc::getuid(), libc::getgid(), libc::geteuid() == 0) };
        let mut cmd = if root {
            Command::new(exe)
        } else {
            let mut cmd = Command::new("pkexec");
            cmd.arg(exe);
            cmd
        };
        cmd.arg("--tun-helper")
            .arg(xray)
            .arg(&path)
            .args([uid.to_string(), gid.to_string(), (plan.kill_switch as u8).to_string()])
            // pkexec refuses to run when SHELL is not in /etc/shells (e.g. launched from a nix shell).
            .env_remove("SHELL")
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
        let mut core = Core { child, helper, helper_out: Some(rx), ports, tun: true, sysproxy: false, log, path };
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
        if self.helper.is_some() {
            self.send("restart")?;
            return self.wait_ready(Duration::from_secs(30));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        // The system proxy stays in place for the core that replaces this one.
        self.sysproxy = false;
        let mut fresh = Core::start(plan)?;
        std::mem::swap(self, &mut fresh);
        Ok(())
    }

    fn send(&mut self, line: &str) -> Result<(), String> {
        let helper = self.helper.as_mut().ok_or("no helper")?;
        writeln!(helper, "{line}").and_then(|_| helper.flush()).map_err(|_| "The system-wide helper stopped".to_owned())
    }

    pub fn set_kill_switch(&mut self, on: bool) {
        if self.helper.is_some() {
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

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if std::mem::take(&mut self.sysproxy) {
            crate::os::sysproxy::restore();
        }
        if self.helper.take().is_some() {
            // EOF: the helper stops the core and removes its routes.
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(5) && matches!(self.child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        crate::os::reaped(&self.child);
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

