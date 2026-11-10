//! Privileged half of TUN mode on Windows and macOS: `hitori --tun-helper …`, elevated through UAC or an administrator
//! prompt; `helper.rs` has the app's side and the protocol. It runs the core next to this executable from the config
//! it receives (never a file the user could change under it) and keeps the routing, DNS and firewall around it.
#[cfg_attr(windows, path = "elevated/windows.rs")]
#[cfg_attr(target_os = "macos", path = "elevated/macos.rs")]
mod sys;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Lines to the app; the core's log goes there too.
#[derive(Clone)]
struct Out(Arc<Mutex<Box<dyn Write + Send>>>);

impl Out {
    fn say(&self, line: &str) {
        let mut w = self.0.lock().unwrap();
        let _ = writeln!(w, "{line}").and_then(|_| w.flush());
    }

    fn forward(&self, stream: impl Read + Send + 'static) {
        let out = self.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                out.say(&format!("log {line}"));
            }
        });
    }
}

/// Lines from the app.
fn lines(stream: impl Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// The running core; dropping it stops it.
struct Running {
    child: Child,
    /// macOS: lets the next helper (or the app) stop a core this helper could not.
    pid_file: Option<PathBuf>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(f) = &self.pid_file {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// Entry point for `--tun-helper`; returns the process exit code.
pub fn helper(args: &[String]) -> i32 {
    let (out, commands, peer) = match sys::connect(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tun helper: {e}");
            return 1;
        }
    };
    let out = Out(Arc::new(Mutex::new(out)));
    // Everything is torn down when `serve` returns, so the app never sees an error with routes left behind.
    match serve(&out, commands, &peer) {
        Ok(()) => 0,
        Err(e) => {
            out.say(&format!("error {e}"));
            1
        }
    }
}

/// The core next to this executable; never one the (unprivileged) caller names.
fn xray() -> Result<PathBuf, String> {
    let name = format!("xray{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut candidates = vec![exe.with_file_name(&name)];
    if cfg!(debug_assertions) {
        candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin").join(&name));
    }
    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| format!("{name} is missing next to {}", exe.display()))
}

fn serve(out: &Out, commands: Receiver<String>, peer: &sys::Peer) -> Result<(), String> {
    let xray = xray()?;
    sys::cleanup(peer);
    let mut kill = false;
    let mut config: Option<Value> = None;
    let mut net: Option<sys::Net> = None;
    // Declared after `net`, so it is stopped before the routing goes.
    let mut core: Option<Running> = None;
    let mut checked = Instant::now();
    loop {
        if let Some(status) = core.as_mut().and_then(|c| c.child.try_wait().ok().flatten()) {
            core = None;
            out.say(&format!("down {}", status.code().unwrap_or(-1)));
        }
        let mut restart = false;
        match commands.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => match line.split_once(' ').unwrap_or((line.as_str(), "")) {
                ("config", json) => config = Some(serde_json::from_str(json).map_err(|e| format!("bad config: {e}"))?),
                ("restart", _) => restart = true,
                ("kill", on) => {
                    kill = on == "1";
                    if let Some(n) = net.as_mut() {
                        n.set_kill_switch(kill)?;
                    }
                }
                ("stop", _) => break,
                _ => eprintln!("tun helper: unknown command {line:?}"),
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Another uplink (Wi-Fi to cable, a new network): the core's own sockets must be bound to it instead.
        if let Some(n) = net.as_mut()
            && checked.elapsed() > Duration::from_secs(3)
        {
            checked = Instant::now();
            restart |= n.tick() && core.is_some();
        }
        if restart {
            drop(core.take());
            let config = config.as_ref().ok_or("no config")?;
            let n = match net.as_mut() {
                Some(n) => n,
                None => net.insert(sys::Net::up(peer, &xray, kill)?),
            };
            core = Some(start(n, &xray, config, out)?);
        }
    }
    Ok(())
}

/// Start the core and report `ready` once it serves inside the TUN (or let the main loop report it `down`).
fn start(net: &mut sys::Net, xray: &Path, config: &Value, out: &Out) -> Result<Running, String> {
    let mut config = config.clone();
    // A privileged core writes no files.
    if let Some(log) = config.get_mut("log").and_then(Value::as_object_mut) {
        log.remove("access");
        log.remove("error");
    }
    for inbound in config["inbounds"].as_array_mut().into_iter().flatten().filter(|i| i["protocol"] == "tun") {
        net.configure(&mut inbound["settings"]);
    }
    let mut cmd = net.command(xray);
    cmd.args(["run", "-c", "stdin:"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut core = net.spawn(&mut cmd).map_err(|e| format!("cannot start the core: {e}"))?;
    let written = core.child.stdin.take().map(|mut s| s.write_all(&serde_json::to_vec(&config).unwrap()));
    written.transpose().map_err(|e| format!("cannot pass the config to the core: {e}"))?;
    out.forward(core.child.stdout.take().unwrap());
    out.forward(core.child.stderr.take().unwrap());
    let metrics: Option<SocketAddr> = config["metrics"]["listen"].as_str().and_then(|a| a.parse().ok());
    let start = Instant::now();
    loop {
        if matches!(core.child.try_wait(), Ok(Some(_))) {
            return Ok(core);
        }
        if metrics.is_none_or(|a| TcpStream::connect_timeout(&a, Duration::from_millis(200)).is_ok()) && net.ready() {
            out.say("ready");
            return Ok(core);
        }
        if start.elapsed() > Duration::from_secs(20) {
            return Err("the core did not bring the TUN up".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
