//! macOS: the helper opens a utun device, routes the internet into it and hands it to the core (`XRAY_TUN_FD`), which
//! runs as the user in a group of its own and binds its sockets to the uplink. The resolver of every network service
//! points into the TUN; pf blocks DNS elsewhere and, with the kill switch, everything outside it. What was changed is
//! kept in STATE until undone, so the next helper cleans up after one that was killed. Closing the utun removes its
//! routes.
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[path = "darwin.rs"]
mod text;

use super::Running;
use crate::sysproxy::{networksetup, networksetup_run};
use crate::xray::{TUN_ADDRS, TUN_DNS, TUN_ROUTES};

const STATE: &str = "/Library/Application Support/Hitori VPN/tun.json";
const ANCHOR: &str = "com.apple/hitori";
/// The core's group: pf lets its sockets past the kill switch, and nothing else of the user's.
const CORE_GID: u32 = 26869;
const SYSPROTO_CONTROL: libc::c_int = 2;
const UTUN_OPT_IFNAME: libc::c_int = 2;

/// The user who started us: the owner of the socket, in their private runtime dir.
pub struct Peer {
    uid: u32,
    dir: PathBuf,
}

/// `<socket>`: connect back to the app.
pub fn connect(args: &[String]) -> Result<(Box<dyn Write + Send>, Receiver<String>, Peer), String> {
    let [sock] = args else { return Err("usage: --tun-helper <socket>".into()) };
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return Err("the TUN helper must run as root".into());
    }
    let sock = PathBuf::from(sock);
    let meta = std::fs::symlink_metadata(&sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    if !meta.file_type().is_socket() || !sock.is_absolute() {
        return Err(format!("not a socket: {}", sock.display()));
    }
    let dir = sock.parent().map(Path::to_path_buf).ok_or("no runtime dir")?;
    let stream = UnixStream::connect(&sock).map_err(|e| format!("cannot reach the app: {e}"))?;
    let lines = super::lines(stream.try_clone().map_err(|e| e.to_string())?);
    Ok((Box::new(stream), lines, Peer { uid: meta.uid(), dir }))
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    /// Network services and their DNS servers before ours (empty: from DHCP).
    dns: Vec<(String, Vec<String>)>,
    pf_token: Option<String>,
}

fn run(tool: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(tool).args(args).stdin(Stdio::null()).output().map_err(|e| format!("{tool}: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() { Ok(text) } else { Err(format!("{tool} {}: {}", args.join(" "), text.trim())) }
}

fn flush_dns() {
    let _ = run("/usr/bin/dscacheutil", &["-flushcache"]);
    let _ = run("/usr/bin/killall", &["-HUP", "mDNSResponder"]);
}

fn undo(state: &State) {
    for (svc, servers) in &state.dns {
        let servers: Vec<&str> = if servers.is_empty() { vec!["Empty"] } else { servers.iter().map(String::as_str).collect() };
        if let Err(e) = networksetup_run(&[&["-setdnsservers", svc.as_str()][..], &servers].concat()) {
            eprintln!("tun helper: {e}");
        }
    }
    flush_dns();
    let _ = run("/sbin/pfctl", &["-a", ANCHOR, "-F", "all"]);
    if let Some(token) = &state.pf_token {
        let _ = run("/sbin/pfctl", &["-X", token]);
    }
}

/// Leftovers of a helper that was killed: its cores (which hold its utun) and its DNS and pf changes.
pub fn cleanup(peer: &Peer) {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, sysinfo::ProcessRefreshKind::new().with_exe(sysinfo::UpdateKind::Always));
    for entry in std::fs::read_dir(&peer.dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix("tun-core-").and_then(|n| n.strip_suffix(".pid")).and_then(|n| n.parse::<usize>().ok()) else { continue };
        if let Some(p) = sys.process(sysinfo::Pid::from(pid))
            && p.exe().and_then(|e| e.file_name()).is_some_and(|n| n == "xray")
        {
            p.kill();
        }
        let _ = std::fs::remove_file(entry.path());
    }
    if let Ok(bytes) = std::fs::read(STATE) {
        undo(&serde_json::from_slice(&bytes).unwrap_or_default());
        let _ = std::fs::remove_file(STATE);
    }
}

pub struct Net {
    utun: OwnedFd,
    name: String,
    uplink: Option<String>,
    kill: bool,
    state: State,
    peer_uid: u32,
    dir: PathBuf,
}

fn uplink(exclude: &str) -> Option<String> {
    text::default_interface(&run("/usr/sbin/netstat", &["-rn", "-f", "inet"]).ok()?, exclude)
}

/// A new utun device (the kernel picks the unit) and its name.
fn open_utun() -> Result<(OwnedFd, String), String> {
    let err = |what: &str| format!("cannot open a utun device ({what}): {}", std::io::Error::last_os_error());
    // SAFETY: plain socket calls on an fd we own, with buffers of the sizes passed.
    unsafe {
        let fd = libc::socket(libc::PF_SYSTEM, libc::SOCK_DGRAM, SYSPROTO_CONTROL);
        if fd < 0 {
            return Err(err("socket"));
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut info: libc::ctl_info = std::mem::zeroed();
        for (d, s) in info.ctl_name.iter_mut().zip(b"com.apple.net.utun_control") {
            *d = *s as libc::c_char;
        }
        if libc::ioctl(fd.as_raw_fd(), libc::CTLIOCGINFO, &mut info) < 0 {
            return Err(err("CTLIOCGINFO"));
        }
        let mut addr: libc::sockaddr_ctl = std::mem::zeroed();
        addr.sc_len = size_of::<libc::sockaddr_ctl>() as u8;
        addr.sc_family = libc::AF_SYSTEM as u8;
        addr.ss_sysaddr = libc::AF_SYS_CONTROL as u16;
        addr.sc_id = info.ctl_id;
        addr.sc_unit = 0;
        if libc::connect(fd.as_raw_fd(), &addr as *const _ as *const libc::sockaddr, size_of::<libc::sockaddr_ctl>() as libc::socklen_t) < 0 {
            return Err(err("connect"));
        }
        let mut name = [0u8; libc::IFNAMSIZ];
        let mut len = name.len() as libc::socklen_t;
        if libc::getsockopt(fd.as_raw_fd(), SYSPROTO_CONTROL, UTUN_OPT_IFNAME, name.as_mut_ptr() as *mut _, &mut len) < 0 {
            return Err(err("UTUN_OPT_IFNAME"));
        }
        let name = std::ffi::CStr::from_bytes_until_nul(&name).map_err(|e| e.to_string())?.to_string_lossy().into_owned();
        Ok((fd, name))
    }
}

impl Net {
    pub fn up(peer: &Peer, _xray: &Path, kill: bool) -> Result<Net, String> {
        let (utun, name) = open_utun()?;
        let mut net = Net { uplink: uplink(&name), utun, name, kill, state: State::default(), peer_uid: peer.uid, dir: peer.dir.clone() };
        net.save()?;
        let (local, remote, mask) = text::p2p(TUN_ADDRS[0]).ok_or("bad TUN address")?;
        run("/sbin/ifconfig", &[&net.name, "inet", &local, &remote, "netmask", &mask, "mtu", "1500", "up"])?;
        let _ = run("/sbin/ifconfig", &[&net.name, "inet6", TUN_ADDRS[1].split('/').next().unwrap(), "prefixlen", "126"]);
        for route in TUN_ROUTES {
            let family = if route.contains(':') { "-inet6" } else { "-inet" };
            let added = run("/sbin/route", &["-n", "add", family, "-net", route, "-interface", &net.name]);
            // IPv6 may be off; IPv4 must work.
            if family == "-inet" {
                added?;
            }
        }
        net.set_dns()?;
        net.load_pf()?;
        Ok(net)
    }

    fn save(&self) -> Result<(), String> {
        let path = Path::new(STATE);
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        crate::store::write(&path.to_path_buf(), &serde_json::to_vec(&self.state).unwrap(), true).map_err(|e| format!("{STATE}: {e}"))
    }

    /// Every enabled service resolves through the TUN; services that appear later are picked up by `tick`.
    fn set_dns(&mut self) -> Result<(), String> {
        let services = networksetup::services(&networksetup_run(&["-listallnetworkservices"])?, false);
        let fresh: Vec<String> = services.into_iter().filter(|s| self.state.dns.iter().all(|(d, _)| d != s)).collect();
        for svc in &fresh {
            let before = networksetup::list(&networksetup_run(&["-getdnsservers", svc]).unwrap_or_default());
            self.state.dns.push((svc.clone(), before));
            self.save()?;
            networksetup_run(&["-setdnsservers", svc, TUN_DNS])?;
        }
        if !fresh.is_empty() {
            flush_dns();
        }
        Ok(())
    }

    fn load_pf(&mut self) -> Result<(), String> {
        if self.state.pf_token.is_none() {
            // A reference on pf, released on teardown; pf stays on if something else enabled it too.
            self.state.pf_token = text::pf_token(&run("/sbin/pfctl", &["-E"])?);
            self.save()?;
        }
        let mut pfctl = Command::new("/sbin/pfctl")
            .args(["-a", ANCHOR, "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("pfctl: {e}"))?;
        let rules = text::pf_rules(&self.name, CORE_GID, self.kill);
        let _ = pfctl.stdin.take().map(|mut s| s.write_all(rules.as_bytes()));
        let out = pfctl.wait_with_output().map_err(|e| format!("pfctl: {e}"))?;
        if out.status.success() { Ok(()) } else { Err(format!("pfctl: {}", String::from_utf8_lossy(&out.stderr).trim())) }
    }

    pub fn set_kill_switch(&mut self, on: bool) -> Result<(), String> {
        self.kill = on;
        self.load_pf()
    }

    pub fn configure(&self, settings: &mut Value) {
        if let Some(uplink) = &self.uplink {
            settings["autoOutboundsInterface"] = json!(uplink);
        }
    }

    /// The core as the user, in its own group, with the utun as fd `XRAY_TUN_FD`.
    pub fn command(&self, xray: &Path) -> Command {
        let mut cmd = Command::new(xray);
        cmd.uid(self.peer_uid).gid(CORE_GID).env("XRAY_TUN_FD", self.utun.as_raw_fd().to_string()).current_dir("/");
        cmd
    }

    pub fn spawn(&self, cmd: &mut Command) -> std::io::Result<Running> {
        let child = cmd.spawn()?;
        let pid_file = self.dir.join(format!("tun-core-{}.pid", child.id()));
        let _ = std::fs::write(&pid_file, "");
        Ok(Running { child, pid_file: Some(pid_file) })
    }

    /// The utun is routed before the core starts.
    pub fn ready(&self) -> bool {
        true
    }

    /// New network services get our resolver; true when the uplink changed.
    pub fn tick(&mut self) -> bool {
        if let Err(e) = self.set_dns() {
            eprintln!("tun helper: {e}");
        }
        let now = uplink(&self.name);
        let changed = now.is_some() && now != self.uplink;
        if changed {
            self.uplink = now;
        }
        changed
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        undo(&self.state);
        let _ = std::fs::remove_file(STATE);
    }
}
