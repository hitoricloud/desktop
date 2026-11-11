//! Local control channel: line-delimited JSON over a Unix socket in the private runtime dir (Linux, macOS) or a named
//! pipe (Windows). The first instance listens; a second launch asks it to show its window and exits, and
//! `hitori status|connect|…` are clients of it. Protocol: docs/ipc.md.
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_channel::Sender;
use gpui::{App, Context, Entity};
use serde_json::{Value, json};

use crate::app::{Conn, Vpn};
use crate::data::{self, Server};

pub use sys::Listener;

#[derive(Debug, PartialEq)]
pub enum Request {
    Show,
    Status,
    Connect,
    Disconnect,
    Servers,
    Select(String),
    Subscribe,
}

pub fn parse(line: &str) -> Result<Request, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("bad request: {e}"))?;
    match v["cmd"].as_str() {
        Some("show") => Ok(Request::Show),
        Some("status") => Ok(Request::Status),
        Some("connect") => Ok(Request::Connect),
        Some("disconnect") => Ok(Request::Disconnect),
        Some("servers") => Ok(Request::Servers),
        Some("select") => v["id"].as_str().map(|id| Request::Select(id.to_owned())).ok_or_else(|| "select needs a server \"id\"".into()),
        Some("subscribe") => Ok(Request::Subscribe),
        Some(cmd) => Err(format!("unknown command: {cmd}")),
        None => Err("missing \"cmd\"".into()),
    }
}

/// A request on its way to the main thread; answers come back as JSON lines until `reply` is dropped.
struct Call {
    request: Request,
    reply: Sender<String>,
}

/// Single instance: a running Hitori is asked to show its window and this process exits; otherwise it becomes the one.
pub fn claim() -> Option<Listener> {
    if hand_over() {
        std::process::exit(0);
    }
    match sys::listen() {
        Ok(Some(listener)) => Some(listener),
        // Another instance holds the channel but is still starting.
        Ok(None) => {
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(100));
                if hand_over() {
                    std::process::exit(0);
                }
            }
            eprintln!("hitori: another instance is running but does not answer");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("hitori: no control channel ({e}); other launches will not find this one");
            None
        }
    }
}

fn hand_over() -> bool {
    let Ok(stream) = sys::connect() else { return false };
    if (&stream).write_all(b"{\"cmd\":\"show\"}\n").is_err() {
        return false;
    }
    BufReader::new(&stream).read_line(&mut String::new()).is_ok_and(|n| n > 0)
}

const USAGE: &str = "usage: hitori status|connect|disconnect|servers|select <id>|watch";

/// `hitori status|connect|disconnect|servers|select <id>|watch`: the exit code, or `None` for any other launch.
pub fn cli(args: &[String]) -> Option<i32> {
    let cmd = args.first()?.as_str();
    let request = match (cmd, &args[1..]) {
        ("status" | "connect" | "disconnect" | "servers", []) => json!({ "cmd": cmd }),
        ("select", [id]) => json!({ "cmd": "select", "id": id }),
        ("watch", []) => json!({ "cmd": "subscribe" }),
        ("status" | "connect" | "disconnect" | "servers" | "select" | "watch", _) => {
            eprintln!("{USAGE}");
            return Some(2);
        }
        _ => return None,
    };
    Some(ask(&request))
}

/// Print the answer (every event for `subscribe`): 0 when it says ok, 1 when not, 2 when nothing answers.
fn ask(request: &Value) -> i32 {
    // SAFETY: plain call. Windows release builds have no console of their own: print to the one they started from.
    #[cfg(windows)]
    unsafe {
        let _ = windows::Win32::System::Console::AttachConsole(windows::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
    let watch = request["cmd"] == "subscribe";
    let mut out = std::io::stdout().lock();
    let Ok(stream) = sys::connect() else {
        let _ = writeln!(out, "{}", json!({ "ok": false, "error": "Hitori is not running" }));
        return 2;
    };
    if (&stream).write_all(format!("{request}\n").as_bytes()).is_err() {
        return 2;
    }
    for line in BufReader::new(&stream).lines() {
        let Ok(line) = line else { break };
        // The reader went away (`hitori watch | head -1`).
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            return 0;
        }
        if !watch {
            return if serde_json::from_str::<Value>(&line).is_ok_and(|v| v["ok"] == true) { 0 } else { 1 };
        }
    }
    if watch { 1 } else { 2 }
}

type Watchers = Rc<RefCell<Vec<Sender<String>>>>;

/// Answer requests on the GPUI main thread; connections are served on their own threads.
pub fn serve(listener: Listener, vpn: &Entity<Vpn>, cx: &mut App) {
    let (calls, rx) = async_channel::unbounded::<Call>();
    std::thread::spawn(move || accept(listener, calls));
    let watchers = Watchers::default();
    let mut last = String::new();
    cx.observe(vpn, {
        let watchers = watchers.clone();
        move |vpn, cx| {
            let line = event(vpn.read(cx));
            if line != last {
                watchers.borrow_mut().retain(|w| w.try_send(line.clone()).is_ok());
                last = line;
            }
        }
    })
    .detach();
    let vpn = vpn.clone();
    cx.spawn(async move |cx| {
        while let Ok(call) = rx.recv().await {
            if cx.update(|cx| answer(call, &vpn, &watchers, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

fn answer(call: Call, vpn: &Entity<Vpn>, watchers: &Watchers, cx: &mut App) {
    let reply = match call.request {
        Request::Show => {
            crate::tray::handle(crate::tray::Msg::OpenMain, cx);
            cx.activate(true);
            json!({ "ok": true })
        }
        Request::Status => ok(status(vpn.read(cx)), true),
        Request::Servers => {
            let selected = vpn.read(cx).s.server_id;
            let list: Vec<Value> = data::servers()
                .into_iter()
                .map(|s| {
                    let mut j = server(s);
                    j["ping_ms"] = data::ping(s.id).into();
                    j["selected"] = (s.id == selected).into();
                    j
                })
                .collect();
            json!({ "ok": true, "servers": list })
        }
        Request::Connect => act(vpn, true, cx, |v, cx| {
            if v.s.conn == Conn::Off {
                v.connect(cx);
            }
        }),
        Request::Disconnect => act(vpn, false, cx, |v, cx| {
            if v.s.conn != Conn::Off {
                v.disconnect(cx);
            }
        }),
        // Like picking a location in the app: connects there, or moves the running connection.
        Request::Select(id) if data::has_server(&id) => {
            let id = data::server(&id).id;
            act(vpn, true, cx, |v, cx| v.go(id, cx))
        }
        Request::Select(id) => json!({ "ok": false, "error": format!("unknown server: {id}") }),
        Request::Subscribe => {
            if call.reply.try_send(event(vpn.read(cx))).is_ok() {
                watchers.borrow_mut().push(call.reply);
            }
            return;
        }
    };
    call.reply.try_send(reply.to_string()).ok();
}

/// Run a model action; it worked if the tunnel is then up or coming up (`on`), or down.
fn act(vpn: &Entity<Vpn>, on: bool, cx: &mut App, f: impl FnOnce(&mut Vpn, &mut Context<Vpn>)) -> Value {
    vpn.update(cx, f);
    let v = vpn.read(cx);
    ok(status(v), (v.s.conn != Conn::Off) == on)
}

fn ok(mut v: Value, ok: bool) -> Value {
    v["ok"] = ok.into();
    v
}

fn event(v: &Vpn) -> String {
    let mut e = status(v);
    e["event"] = "status".into();
    e.to_string()
}

fn server(s: &Server) -> Value {
    json!({ "id": s.id, "name": format!("{} · {}", s.country, s.city), "country": s.country, "code": s.code, "city": s.city })
}

fn status(v: &Vpn) -> Value {
    let s = &v.s;
    let since = s.connected_at.and_then(|t| SystemTime::now().checked_sub(t.elapsed())).and_then(|t| t.duration_since(UNIX_EPOCH).ok());
    let mbps = |x: f64| (x * 100.).round() / 100.;
    let traffic = (s.conn == Conn::On).then(|| {
        let (rx, tx) = v.traffic().unwrap_or_default();
        json!({ "rx_bytes": rx, "tx_bytes": tx, "down_mbps": mbps(s.stats.down), "up_mbps": mbps(s.stats.up), "latency_ms": s.stats.latency })
    });
    json!({
        "state": match s.conn {
            Conn::On => "connected",
            Conn::Connecting => "connecting",
            Conn::Off => "disconnected",
        },
        "signed_in": s.signed_in,
        "server": data::has_server(s.server_id).then(|| server(data::server(s.server_id))),
        "entry": s.entry_id.filter(|id| data::has_server(id)).map(|id| server(data::server(id))),
        "connected_since": since.map(|d| d.as_secs()),
        "traffic": traffic,
        "error": s.error,
    })
}

fn accept(mut listener: Listener, calls: Sender<Call>) {
    while !calls.is_closed() {
        match listener.accept() {
            Ok(stream) => {
                let calls = calls.clone();
                std::thread::spawn(move || serve_stream(stream, calls));
            }
            Err(e) => {
                eprintln!("hitori: control channel: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

fn serve_stream<S>(stream: S, calls: Sender<Call>)
where
    for<'a> &'a S: Read + Write,
{
    let mut out = &stream;
    for line in BufReader::new(&stream).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let (reply, answers) = async_channel::unbounded();
        match parse(&line) {
            Ok(request) => {
                if calls.send_blocking(Call { request, reply }).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = reply.try_send(json!({ "ok": false, "error": e }).to_string());
                drop(reply);
            }
        }
        while let Ok(answer) = answers.recv_blocking() {
            if out.write_all(format!("{answer}\n").as_bytes()).is_err() {
                return;
            }
        }
    }
}

#[cfg(unix)]
mod sys {
    use std::fs::{File, OpenOptions, Permissions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};

    pub fn path() -> PathBuf {
        crate::store::runtime_dir().join("ipc.sock")
    }

    pub fn connect() -> io::Result<UnixStream> {
        UnixStream::connect(path())
    }

    /// The socket, and the lock that makes this process its owner for as long as it runs.
    pub struct Listener {
        socket: UnixListener,
        _lock: File,
    }

    pub fn listen() -> io::Result<Option<Listener>> {
        listen_in(&crate::store::runtime_dir())
    }

    /// `None` while another process holds the lock. Whoever holds it may replace a socket file left by a crash.
    pub fn listen_in(dir: &Path) -> io::Result<Option<Listener>> {
        let lock = OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(dir.join("ipc.lock"))?;
        // SAFETY: a valid descriptor owned by `lock`.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::WouldBlock { Ok(None) } else { Err(e) };
        }
        let path = dir.join("ipc.sock");
        let _ = std::fs::remove_file(&path);
        let socket = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, Permissions::from_mode(0o600))?;
        Ok(Some(Listener { socket, _lock: lock }))
    }

    impl Listener {
        pub fn accept(&mut self) -> io::Result<UnixStream> {
            self.socket.accept().map(|(s, _)| s)
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::time::Duration;

    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE};
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows::core::HSTRING;

    pub fn path() -> String {
        format!(r"\\.\pipe\hitori-{}", std::env::var("USERNAME").unwrap_or_default().replace('\\', "_"))
    }

    pub fn connect() -> io::Result<File> {
        let mut tries = 0;
        loop {
            match OpenOptions::new().read(true).write(true).open(path()) {
                // Every instance is taken for a moment, between a client connecting and the next one being created.
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) && tries < 20 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(50));
                }
                r => return r,
            }
        }
    }

    /// The pipe instance the next client connects to.
    pub struct Listener {
        next: File,
    }

    fn instance(first: bool) -> io::Result<File> {
        let flags = if first { PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE } else { PIPE_ACCESS_DUPLEX };
        let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        // SAFETY: plain call; the handle is checked before it is owned.
        let h = unsafe { CreateNamedPipeW(&HSTRING::from(path()), flags, mode, PIPE_UNLIMITED_INSTANCES, 4096, 4096, 0, None) };
        if h.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle nothing else owns.
        Ok(unsafe { File::from_raw_handle(h.0) })
    }

    /// `None` when another process already created the pipe.
    pub fn listen() -> io::Result<Option<Listener>> {
        match instance(true) {
            Ok(next) => Ok(Some(Listener { next })),
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) => Ok(None),
            Err(e) => Err(e),
        }
    }

    impl Listener {
        pub fn accept(&mut self) -> io::Result<File> {
            // SAFETY: `self.next` owns the handle for the whole call.
            match unsafe { ConnectNamedPipe(HANDLE(self.next.as_raw_handle()), None) } {
                Err(e) if e.code() != ERROR_PIPE_CONNECTED.to_hresult() => return Err(e.into()),
                _ => {}
            }
            Ok(std::mem::replace(&mut self.next, instance(false)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests() {
        assert_eq!(parse(r#"{"cmd":"show"}"#), Ok(Request::Show));
        assert_eq!(parse(r#"{"cmd":"status"}"#), Ok(Request::Status));
        assert_eq!(parse(r#"{"cmd":"connect"}"#), Ok(Request::Connect));
        assert_eq!(parse(r#"{"cmd":"disconnect"}"#), Ok(Request::Disconnect));
        assert_eq!(parse(r#"{"cmd":"servers"}"#), Ok(Request::Servers));
        assert_eq!(parse(r#"{"cmd":"subscribe"}"#), Ok(Request::Subscribe));
        assert_eq!(parse(r#" {"cmd":"select","id":"node-1"} "#), Ok(Request::Select("node-1".into())));
        assert_eq!(parse(r#"{"cmd":"select"}"#), Err("select needs a server \"id\"".into()));
        assert_eq!(parse(r#"{"cmd":"select","id":7}"#), Err("select needs a server \"id\"".into()));
        assert_eq!(parse(r#"{"cmd":"fly"}"#), Err("unknown command: fly".into()));
        assert_eq!(parse(r#"{"id":"x"}"#), Err("missing \"cmd\"".into()));
        assert_eq!(parse(r#"["status"]"#), Err("missing \"cmd\"".into()));
        assert!(parse("status").is_err());
    }

    #[test]
    fn cli_subcommands() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(cli(&args(&[])), None);
        assert_eq!(cli(&args(&["--tray"])), None);
        assert_eq!(cli(&args(&["select"])), Some(2), "select without an id is a usage error");
        assert_eq!(cli(&args(&["status", "extra"])), Some(2));
    }

    #[test]
    fn server_json() {
        let s = Server { id: "n1", country: "Estonia", code: "EE", city: "Tallinn", ip: "", lat: 0., lon: 0., variants: vec![] };
        assert_eq!(server(&s), json!({ "id": "n1", "name": "Estonia · Tallinn", "country": "Estonia", "code": "EE", "city": "Tallinn" }));
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_round_trip() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::{UnixListener, UnixStream};

        let dir = std::env::temp_dir().join(format!("hitori-ipc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A socket file left by a crashed instance.
        drop(UnixListener::bind(dir.join("ipc.sock")).unwrap());
        let listener = sys::listen_in(&dir).unwrap().expect("first listener");
        assert!(sys::listen_in(&dir).unwrap().is_none(), "the lock keeps a second instance out");
        assert_eq!(std::fs::metadata(dir.join("ipc.sock")).unwrap().permissions().mode() & 0o777, 0o600);

        // Stands in for the main thread: echoes the request, streams two events to a subscriber.
        let (calls, rx) = async_channel::unbounded::<Call>();
        std::thread::spawn(move || accept(listener, calls));
        std::thread::spawn(move || {
            while let Ok(call) = rx.recv_blocking() {
                match call.request {
                    Request::Subscribe => (0..2).for_each(|n| call.reply.try_send(json!({ "event": "status", "n": n }).to_string()).unwrap()),
                    request => call.reply.try_send(json!({ "ok": true, "got": format!("{request:?}") }).to_string()).unwrap(),
                }
            }
        });

        let stream = UnixStream::connect(dir.join("ipc.sock")).unwrap();
        let mut lines = BufReader::new(&stream).lines();
        let mut next = || serde_json::from_str::<Value>(&lines.next().unwrap().unwrap()).unwrap();
        let send = |line: &str| (&stream).write_all(format!("{line}\n").as_bytes()).unwrap();
        send(r#"{"cmd":"status"}"#);
        assert_eq!(next(), json!({ "ok": true, "got": "Status" }));
        send("nonsense");
        assert_eq!(next()["ok"], false);
        send(r#"{"cmd":"select","id":"node-1"}"#);
        assert_eq!(next(), json!({ "ok": true, "got": "Select(\"node-1\")" }), "the connection stays usable");
        send(r#"{"cmd":"subscribe"}"#);
        assert_eq!(next(), json!({ "event": "status", "n": 0 }));
        assert_eq!(next(), json!({ "event": "status", "n": 1 }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
