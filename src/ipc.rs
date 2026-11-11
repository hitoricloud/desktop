//! Local control channel: line-delimited JSON over a Unix socket in the private runtime dir (Linux, macOS) or a named
//! pipe (Windows). The first instance listens; a second launch asks it to show its window and exits.
use std::io::{BufRead, BufReader, Read, Write};
use std::time::Duration;

use async_channel::Sender;
use gpui::App;
use serde_json::{Value, json};

pub use sys::Listener;

#[derive(Debug, PartialEq)]
pub enum Request {
    Show,
}

pub fn parse(line: &str) -> Result<Request, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("bad request: {e}"))?;
    match v["cmd"].as_str() {
        Some("show") => Ok(Request::Show),
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

/// Answer requests on the GPUI main thread; connections are served on their own threads.
pub fn serve(listener: Listener, cx: &mut App) {
    let (calls, rx) = async_channel::unbounded::<Call>();
    std::thread::spawn(move || accept(listener, calls));
    cx.spawn(async move |cx| {
        while let Ok(call) = rx.recv().await {
            if cx.update(|cx| answer(call, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

fn answer(call: Call, cx: &mut App) {
    let reply = match call.request {
        Request::Show => {
            crate::tray::handle(crate::tray::Msg::OpenMain, cx);
            cx.activate(true);
            json!({ "ok": true })
        }
    };
    call.reply.try_send(reply.to_string()).ok();
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
        assert_eq!(parse(r#"{"cmd":"fly"}"#), Err("unknown command: fly".into()));
        assert_eq!(parse(r#"{"id":"x"}"#), Err("missing \"cmd\"".into()));
        assert!(parse("show").is_err());
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

        let (calls, rx) = async_channel::unbounded::<Call>();
        std::thread::spawn(move || accept(listener, calls));
        std::thread::spawn(move || {
            while let Ok(call) = rx.recv_blocking() {
                assert_eq!(call.request, Request::Show);
                call.reply.try_send(json!({ "ok": true }).to_string()).unwrap();
            }
        });

        let stream = UnixStream::connect(dir.join("ipc.sock")).unwrap();
        let mut lines = BufReader::new(&stream).lines();
        let mut ask = |line: &str| {
            (&stream).write_all(format!("{line}\n").as_bytes()).unwrap();
            serde_json::from_str::<Value>(&lines.next().unwrap().unwrap()).unwrap()
        };
        assert_eq!(ask(r#"{"cmd":"show"}"#), json!({ "ok": true }));
        assert_eq!(ask("nonsense")["ok"], false);
        assert_eq!(ask(r#"{"cmd":"show"}"#), json!({ "ok": true }), "the connection stays usable");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
