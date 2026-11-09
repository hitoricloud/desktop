//! Root half of system-wide mode, run through pkexec as `hitori --tun-helper <xray> <config> <uid> <gid> <kill 0|1>`.
//! It runs the core with only network capabilities and routes everything the core did not send itself into `hitori0`.
//! Protocol with `core.rs`: stdout lines `ready`, `down <code>`, `error <message>`; stdin lines `restart`, `kill 0|1`, `stop`/EOF.
use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::xray::{MARK, TUN_NAME};

const TABLE: &str = "26869";
const PREFS: [&str; 5] = ["7330", "7331", "7332", "7333", "7334"];
/// Set on packets the core writes into the TUN, so reverse-path checks find them in the TUN table.
const IN_MARK: u32 = 0x686a;
/// pkexec clears PATH, so tools are only taken from these system directories.
const BIN_DIRS: [&str; 5] = ["/run/current-system/sw/bin", "/usr/sbin", "/sbin", "/usr/bin", "/bin"];
const CAPS: &str = "+net_admin,+net_raw,+net_bind_service";
const SRC_VALID_MARK: &str = "/proc/sys/net/ipv4/conf/all/src_valid_mark";

static TERMINATED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigterm(_: libc::c_int) {
    TERMINATED.store(true, Ordering::SeqCst);
}

/// Entry point for `--tun-helper`; returns the process exit code.
pub fn helper(args: &[String]) -> i32 {
    // SAFETY: installing SIG_IGN or a handler that only stores an atomic is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, on_sigterm as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
    // The routing is already torn down when `run` returns, so the app never sees an error with routes left behind.
    match run(args) {
        Ok(()) => 0,
        Err(e) => {
            say(&format!("error {e}"));
            1
        }
    }
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}").and_then(|_| out.flush());
}

struct Args {
    xray: PathBuf,
    config: PathBuf,
    uid: u32,
    gid: u32,
    kill_switch: bool,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let [xray, config, uid, gid, kill] = args else { return Err("usage: --tun-helper <xray> <config> <uid> <gid> <kill 0|1>".into()) };
    let file = |p: &str| {
        let path = PathBuf::from(p);
        if path.is_absolute() && path.is_file() { Ok(path) } else { Err(format!("not an absolute path to a file: {p}")) }
    };
    let id = |s: &str| s.parse::<u32>().map_err(|_| format!("not a numeric id: {s}"));
    let kill_switch = match kill.as_str() {
        "0" => false,
        "1" => true,
        _ => return Err(format!("kill switch must be 0 or 1: {kill}")),
    };
    Ok(Args { xray: file(xray)?, config: file(config)?, uid: id(uid)?, gid: id(gid)?, kill_switch })
}

fn find(tool: &str) -> Result<PathBuf, String> {
    BIN_DIRS.iter().map(|d| Path::new(d).join(tool)).find(|p| p.is_file()).ok_or_else(|| format!("{tool} not found"))
}

fn run(args: &[String]) -> Result<(), String> {
    let args = parse(args)?;
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return Err("the system-wide helper must run as root".into());
    }
    let ip = find("ip")?;
    let setpriv = if args.uid == 0 { None } else { Some(find("setpriv")?) };
    let iptables = ["iptables", "ip6tables"].into_iter().filter_map(|t| find(t).ok()).collect();
    let commands = stdin_lines();
    let mut session = Session { ip, setpriv, iptables, args, core: None, src_valid_mark: None };
    session.teardown(); // leftovers of a helper that was killed
    session.add_rules()?;
    session.add_connmark();
    session.set_kill_switch(session.args.kill_switch);
    session.start_core()?;
    while !TERMINATED.load(Ordering::SeqCst) {
        if let Some(status) = session.core.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
            session.core = None;
            say(&format!("down {}", status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))));
        }
        match commands.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => match line.trim() {
                "restart" => session.start_core()?,
                "kill 1" => session.set_kill_switch(true),
                "kill 0" => session.set_kill_switch(false),
                "stop" => break,
                other => eprintln!("tun helper: unknown command {other:?}"),
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn stdin_lines() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// The running core and the routing around it; dropping it undoes everything.
struct Session {
    ip: PathBuf,
    setpriv: Option<PathBuf>,
    iptables: Vec<PathBuf>,
    args: Args,
    core: Option<Child>,
    /// The value before we set it, to put back on teardown.
    src_valid_mark: Option<String>,
}

impl Session {
    fn ip(&self, args: &[&str]) -> Result<(), String> {
        let out = Command::new(&self.ip).args(args).stdin(Stdio::null()).output().map_err(|e| format!("ip: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("ip {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
        }
    }

    /// The same command for IPv4 (must work) and IPv6 (may be disabled).
    fn ip46(&self, args: &[&str]) -> Result<(), String> {
        let _ = self.ip(&[&["-6"], args].concat());
        self.ip(&[&["-4"], args].concat())
    }

    fn add_rules(&self) -> Result<(), String> {
        // Unmarked (mark 0) DNS always goes into the tunnel, even to LAN resolvers; `not` would invert the whole selector.
        for (pref, proto) in [(PREFS[0], "udp"), (PREFS[1], "tcp")] {
            self.ip46(&["rule", "add", "pref", pref, "fwmark", "0/0xffffffff", "ipproto", proto, "dport", "53", "lookup", TABLE])?;
        }
        // Replies from the TUN (e.g. from a LAN resolver) would otherwise fail the reverse-path check against `main`.
        self.ip46(&["rule", "add", "pref", PREFS[2], "fwmark", &format!("{IN_MARK:#x}"), "lookup", TABLE])?;
        // Every non-default route (LAN, links, incoming connections) keeps working as before.
        self.ip46(&["rule", "add", "pref", PREFS[3], "lookup", "main", "suppress_prefixlength", "0"])?;
        self.ip46(&["rule", "add", "pref", PREFS[4], "not", "fwmark", &format!("{MARK:#x}"), "lookup", TABLE])
    }

    /// Strict reverse-path filters (the NixOS firewall's `rpfilter --validmark`, `rp_filter=1`) honour marks: replies to
    /// the core's connections get its mark back (looked up in `main`), packets from the TUN get IN_MARK (the TUN table).
    fn add_connmark(&mut self) {
        for (chain, at, rule) in connmark_rules() {
            for t in &self.iptables {
                let mut args: Vec<String> = ["-w", "-t", "mangle", at, chain].map(str::to_owned).to_vec();
                if at == "-I" {
                    args.push("1".into());
                }
                args.extend(rule.iter().cloned());
                if let Err(e) = run_tool(t, &args) {
                    eprintln!("tun helper: {e}");
                }
            }
        }
        if let Ok(old) = std::fs::read_to_string(SRC_VALID_MARK)
            && old.trim() != "1"
            && std::fs::write(SRC_VALID_MARK, "1").is_ok()
        {
            self.src_valid_mark = Some(old.trim().to_owned());
        }
    }

    /// An unreachable fallback under the TUN route: while the core is down, traffic is refused instead of leaking.
    fn set_kill_switch(&self, on: bool) {
        let verb = if on { "replace" } else { "del" };
        let _ = self.ip46(&["route", verb, "unreachable", "default", "metric", "4000", "table", TABLE]);
    }

    fn stop_core(&mut self) {
        if let Some(mut core) = self.core.take() {
            let _ = core.kill();
            let _ = core.wait();
        }
    }

    /// (Re)start the core, wait for its device and route into it, then report `ready` (or `down` if it exits first).
    fn start_core(&mut self) -> Result<(), String> {
        self.stop_core();
        let a = &self.args;
        let mut cmd = match &self.setpriv {
            Some(setpriv) => {
                let mut cmd = Command::new(setpriv);
                cmd.arg(format!("--reuid={}", a.uid))
                    .arg(format!("--regid={}", a.gid))
                    .args(["--clear-groups", &format!("--inh-caps={CAPS}"), &format!("--ambient-caps={CAPS}"), "--pdeathsig=keep", "--"])
                    .arg(&a.xray);
                cmd
            }
            None => Command::new(&a.xray),
        };
        // Core logs go to our stderr (the app's log); our stdout is the protocol.
        let log = std::io::stderr().as_fd().try_clone_to_owned().map_err(|e| e.to_string())?;
        cmd.args(["run", "-c"]).arg(&a.config).stdin(Stdio::null()).stdout(log).stderr(Stdio::inherit());
        // SAFETY: prctl is async-signal-safe; this runs between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        self.core = Some(cmd.spawn().map_err(|e| format!("cannot start the core: {e}"))?);

        let start = Instant::now();
        while self.ip(&["link", "show", "dev", TUN_NAME]).is_err() {
            if TERMINATED.load(Ordering::SeqCst) || matches!(self.core.as_mut().map(|c| c.try_wait()), Some(Ok(Some(_)))) {
                return Ok(()); // the main loop reports the exit or shuts down
            }
            if start.elapsed() > Duration::from_secs(10) {
                return Err(format!("the core did not create {TUN_NAME}"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.ip(&["link", "set", "dev", TUN_NAME, "up"])?;
        self.ip46(&["route", "replace", "default", "dev", TUN_NAME, "table", TABLE])?;
        say("ready");
        Ok(())
    }

    fn teardown(&mut self) {
        self.stop_core();
        for family in ["-4", "-6"] {
            for pref in PREFS {
                // Duplicates are possible after crashes; delete until none is left.
                for _ in 0..100 {
                    if self.ip(&[family, "rule", "del", "pref", pref]).is_err() {
                        break;
                    }
                }
            }
            let _ = self.ip(&[family, "route", "flush", "table", TABLE]);
        }
        for (chain, _, rule) in connmark_rules() {
            for t in &self.iptables {
                let args: Vec<String> = ["-w", "-t", "mangle", "-D", chain].iter().map(|s| s.to_string()).chain(rule.iter().cloned()).collect();
                for _ in 0..100 {
                    if run_tool(t, &args).is_err() {
                        break;
                    }
                }
            }
        }
        if let Some(old) = self.src_valid_mark.take() {
            let _ = std::fs::write(SRC_VALID_MARK, old);
        }
    }
}

/// (chain, how to add, match + target): mark packets from the TUN, restore the mark on incoming packets of the core's
/// connections, save it on outgoing ones.
fn connmark_rules() -> [(&'static str, &'static str, Vec<String>); 3] {
    let (mark, in_mark) = (format!("{MARK:#x}"), format!("{IN_MARK:#x}"));
    let rule = |head: &[&str], target: &[&str]| -> Vec<String> {
        [head, &["-m", "comment", "--comment", "hitori", "-j"], target].concat().into_iter().map(str::to_owned).collect()
    };
    [
        ("PREROUTING", "-I", rule(&["-i", TUN_NAME], &["MARK", "--set-mark", &in_mark])),
        ("PREROUTING", "-I", rule(&["-m", "connmark", "--mark", &mark], &["CONNMARK", "--restore-mark"])),
        ("POSTROUTING", "-A", rule(&["-m", "mark", "--mark", &mark], &["CONNMARK", "--save-mark"])),
    ]
}

fn run_tool(tool: &Path, args: &[impl AsRef<std::ffi::OsStr>]) -> Result<(), String> {
    let out = Command::new(tool).args(args).stdin(Stdio::null()).output().map_err(|e| format!("{}: {e}", tool.display()))?;
    if out.status.success() { Ok(()) } else { Err(format!("{}: {}", tool.display(), String::from_utf8_lossy(&out.stderr).trim())) }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.teardown();
    }
}
