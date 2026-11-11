//! The app's side of TUN mode: starting the privileged helper and talking to it. Linux: pkexec, over the helper's
//! stdin/stdout. Windows: UAC (`runas`), over a loopback TCP connection the helper opens with a one-time token.
//! macOS: an administrator prompt through osascript, over a Unix socket in the user's private runtime dir.
//! From the helper: `ready`, `down <code>`, `error <message>` and (Windows, macOS) `log <core log line>`.
//! To it: `restart`, `kill 0|1`, `stop`, and (Windows, macOS) `config <json>`: the config of a privileged core is never
//! a file the user could change under it.
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::xray::Log;

#[cfg(target_os = "linux")]
type Input = std::process::ChildStdin;
#[cfg(target_os = "macos")]
type Input = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Input = std::net::TcpStream;
#[cfg(windows)]
type Process = elevate::Elevated;
#[cfg(unix)]
type Process = Child;

/// A running helper; dropping it stops the core and tears the routing down.
pub struct Link {
    input: Option<Input>,
    lines: Receiver<String>,
    /// pkexec, osascript or the elevated helper itself.
    process: Process,
    log: Log,
}

/// Lines from the helper; `log` lines go straight into the core log.
fn read_lines(stream: impl Read + Send + 'static, log: Option<Log>) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            match (line.strip_prefix("log "), &log) {
                (Some(l), Some(log)) => log.push(l.to_owned()),
                _ => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

impl Link {
    /// Linux: the app itself as root (`--tun-helper`, through pkexec) runs the core behind a TUN device.
    #[cfg(target_os = "linux")]
    pub fn start(xray: &Path, path: &PathBuf, config: &Value, kill_switch: bool, log: &Log) -> Result<Link, String> {
        crate::xray::write_config(path, config)?;
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
            .arg(path)
            .args([uid.to_string(), gid.to_string(), (kill_switch as u8).to_string()])
            // pkexec refuses to run when SHELL is not in /etc/shells (e.g. launched from a nix shell).
            .env_remove("SHELL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("Cannot ask for administrator rights (pkexec): {e}"))?;
        log.follow(child.stderr.take().unwrap());
        let lines = read_lines(child.stdout.take().unwrap(), None);
        let mut link = Link { input: child.stdin.take(), lines, process: child, log: log.clone() };
        link.wait_ready(Duration::from_secs(180))?;
        Ok(link)
    }

    /// macOS: `hitori --tun-helper <socket>` as root through an administrator prompt; it connects back to the socket.
    #[cfg(target_os = "macos")]
    pub fn start(_xray: &Path, _path: &PathBuf, config: &Value, kill_switch: bool, log: &Log) -> Result<Link, String> {
        use std::os::unix::net::UnixListener;
        let sock = crate::store::runtime_dir().join("helper.sock");
        let _ = std::fs::remove_file(&sock);
        // The runtime dir is 0700: only this user (and root) can reach the socket.
        let listener = UnixListener::bind(&sock).map_err(|e| format!("Cannot start the TUN helper: {e}"))?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let command = format!("{} --tun-helper {}", sh_quote(&exe.to_string_lossy()), sh_quote(&sock.to_string_lossy()));
        let prompt = "Hitori VPN needs administrator rights to route all traffic through the tunnel.";
        let script = format!("do shell script {} with prompt {} with administrator privileges", applescript_str(&command), applescript_str(prompt));
        // Already root (`sudo hitori --tun-selftest`): no prompt.
        // SAFETY: geteuid cannot fail.
        let mut cmd = if unsafe { libc::geteuid() } == 0 {
            let mut cmd = Command::new(&exe);
            cmd.arg("--tun-helper").arg(&sock);
            cmd
        } else {
            let mut cmd = Command::new("/usr/bin/osascript");
            cmd.arg("-e").arg(script);
            cmd
        };
        let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| format!("Cannot ask for administrator rights: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let start = Instant::now();
        let stream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.to_string()),
            }
            if let Ok(Some(_)) = child.try_wait() {
                let mut err = String::new();
                let _ = child.stderr.take().map(|mut s| s.read_to_string(&mut err));
                let _ = std::fs::remove_file(&sock);
                // osascript: "User canceled. (-128)"
                return Err(if err.contains("-128") { "Administrator rights were not granted".into() } else { format!("TUN mode could not start: {}", err.trim()) });
            }
            if start.elapsed() > Duration::from_secs(180) {
                let _ = child.kill();
                let _ = std::fs::remove_file(&sock);
                return Err("Timed out waiting for administrator rights".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let _ = std::fs::remove_file(&sock);
        stream.set_nonblocking(false).map_err(|e| e.to_string())?;
        let lines = read_lines(stream.try_clone().map_err(|e| e.to_string())?, Some(log.clone()));
        let mut link = Link { input: Some(stream), lines, process: child, log: log.clone() };
        link.begin(config, kill_switch)?;
        Ok(link)
    }

    /// Windows: `hitori --tun-helper <port> <token>` elevated through UAC; it connects back to 127.0.0.1:<port>.
    #[cfg(windows)]
    pub fn start(_xray: &Path, _path: &PathBuf, config: &Value, kill_switch: bool, log: &Log) -> Result<Link, String> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("Cannot start the TUN helper: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let token = elevate::token()?;
        let mut process = elevate::run(&format!("--tun-helper {port} {token}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let start = Instant::now();
        let stream = loop {
            match listener.accept() {
                Ok((mut s, _)) => {
                    // Anyone local can connect: only the helper knows the token.
                    let _ = s.set_nonblocking(false);
                    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                    if first_line(&mut s).is_some_and(|l| l == format!("hello {token}")) {
                        let _ = s.set_read_timeout(None);
                        break s;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.to_string()),
            }
            if let Ok(Some(status)) = process.try_wait() {
                return Err(format!("The TUN helper stopped ({status})"));
            }
            if start.elapsed() > Duration::from_secs(60) {
                let _ = process.kill();
                return Err("The TUN helper did not start".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let lines = read_lines(stream.try_clone().map_err(|e| e.to_string())?, Some(log.clone()));
        let mut link = Link { input: Some(stream), lines, process, log: log.clone() };
        link.begin(config, kill_switch)?;
        Ok(link)
    }

    #[cfg(not(target_os = "linux"))]
    fn begin(&mut self, config: &Value, kill_switch: bool) -> Result<(), String> {
        self.send(if kill_switch { "kill 1" } else { "kill 0" })?;
        self.send(&format!("config {config}"))?;
        self.send("restart")?;
        self.wait_ready(Duration::from_secs(60))
    }

    /// A new config: the helper restarts the core under the same routing (no leak in between on Linux and macOS).
    pub fn reload(&mut self, _path: &PathBuf, config: &Value) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        crate::xray::write_config(_path, config)?;
        #[cfg(not(target_os = "linux"))]
        self.send(&format!("config {config}"))?;
        self.send("restart")?;
        self.wait_ready(Duration::from_secs(30))
    }

    pub fn send(&mut self, line: &str) -> Result<(), String> {
        let input = self.input.as_mut().ok_or("no helper")?;
        writeln!(input, "{line}").and_then(|_| input.flush()).map_err(|_| "The TUN helper stopped".to_owned())
    }

    /// Helper lines until `ready`; `down`/exit means it failed.
    fn wait_ready(&mut self, limit: Duration) -> Result<(), String> {
        let start = Instant::now();
        while start.elapsed() < limit {
            match self.lines.recv_timeout(Duration::from_millis(200)) {
                Ok(line) if line == "ready" => return Ok(()),
                Ok(line) if line.starts_with("error ") => return Err(line[6..].to_owned()),
                Ok(line) if line.starts_with("down") => return Err(self.log.last_error().unwrap_or_else(|| "The core stopped".into())),
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let status = self.process.wait().ok().and_then(|s| s.code());
                    return Err(match status {
                        Some(126) | Some(127) if cfg!(target_os = "linux") => "Administrator rights were not granted".into(),
                        _ => self.log.last_error().unwrap_or_else(|| "TUN mode could not start".into()),
                    });
                }
            }
        }
        Err(if cfg!(target_os = "linux") { "Timed out waiting for administrator rights" } else { "TUN mode did not start in time" }.into())
    }

    /// Still running? The helper outlives a crashed core and reports it.
    pub fn alive(&mut self) -> bool {
        !matches!(self.process.try_wait(), Ok(Some(_))) && !self.lines.try_iter().any(|l| l.starts_with("down"))
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.send("stop");
        // Linux: EOF on stdin; elsewhere the reader thread holds the socket too, so shut it down explicitly.
        #[cfg(not(target_os = "linux"))]
        if let Some(s) = &self.input {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        self.input = None;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) && matches!(self.process.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// `--tun-selftest` (Windows, macOS; CI): TUN mode with the kill switch and a core that sends everything direct.
/// Plain and named HTTPS must go through the TUN, and work again after the teardown.
#[cfg(not(target_os = "linux"))]
pub fn selftest() -> i32 {
    let log = Log::default();
    let fetch = |url: &str| ureq::get(url).timeout(Duration::from_secs(15)).call().map_err(|e| format!("{url}: {e}")).and_then(|r| r.into_string().map_err(|e| e.to_string()));
    let run = || -> Result<u64, String> {
        let xray = crate::xray::xray_path().ok_or("the Xray core is missing")?;
        let metrics = crate::xray::free_port();
        #[allow(unused_mut)]
        let mut link = Link::start(&xray, &PathBuf::new(), &crate::xray::selftest_config(metrics), true, &log)?;
        println!("tun selftest: up");
        if let Err(e) = fetch("https://1.1.1.1/cdn-cgi/trace") {
            #[cfg(target_os = "macos")]
            for args in [&["/usr/sbin/netstat", "-rn", "-f", "inet"][..], &["/sbin/route", "-n", "get", "1.1.1.1"], &["/sbin/route", "-n", "get", "-ifscope", "en0", "1.1.1.1"], &["/sbin/ifconfig", "en0"], &["/sbin/pfctl", "-a", "com.apple/hitori", "-vsr"], &["/bin/ps", "-axo", "pid,uid,gid,rgid,comm"]] {
                let out = std::process::Command::new(args[0]).args(&args[1..]).output();
                let text = out.map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))).unwrap_or_else(|e| e.to_string());
                let text = if args[0] == "/bin/ps" { text.lines().filter(|l| l.contains("xray") || l.contains("PID")).collect::<Vec<_>>().join("\n") } else { text };
                eprintln!("--- {}\n{text}", args.join(" "));
            }
            #[cfg(target_os = "macos")]
            {
                let _ = link.send("kill 0");
                std::thread::sleep(Duration::from_secs(1));
                eprintln!("--- without the kill switch: {:?}", fetch("https://1.1.1.1/cdn-cgi/trace").map(|_| "ok"));
            }
            return Err(e);
        }
        fetch("https://one.one.one.one/cdn-cgi/trace")?;
        let vars: Value = ureq::get(&format!("http://127.0.0.1:{metrics}/debug/vars")).call().map_err(|e| e.to_string())?.into_json().map_err(|e| e.to_string())?;
        let down = vars["stats"]["outbound"]["direct"]["downlink"].as_u64().unwrap_or(0);
        drop(link);
        if down == 0 {
            return Err("the requests did not go through the TUN".into());
        }
        // No routes, resolver or kill switch left behind.
        fetch("https://one.one.one.one/cdn-cgi/trace").map_err(|e| format!("after teardown: {e}"))?;
        Ok(down)
    };
    let result = run();
    for line in log.take() {
        eprintln!("core: {line}");
    }
    match result {
        Ok(down) => {
            println!("tun selftest: ok, {down} bytes through the TUN, torn down");
            0
        }
        Err(e) => {
            eprintln!("tun selftest: {e}");
            1
        }
    }
}

/// One `\n`-terminated line, read byte by byte so nothing after it is consumed.
#[cfg(windows)]
fn first_line(s: &mut impl Read) -> Option<String> {
    let mut line = vec![];
    let mut b = [0u8];
    while line.len() < 256 {
        s.read_exact(&mut b).ok()?;
        if b[0] == b'\n' {
            return String::from_utf8(line).ok();
        }
        line.push(b[0]);
    }
    None
}

/// `'…'` for /bin/sh.
#[cfg(any(target_os = "macos", test))]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `"…"` for AppleScript.
#[cfg(any(target_os = "macos", test))]
fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

#[cfg(windows)]
mod elevate {
    use std::os::windows::process::ExitStatusExt;
    use std::process::ExitStatus;

    use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
    use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, TerminateProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    use windows::core::{HRESULT, PCWSTR, w};

    /// The elevated helper's process; the app cannot put it in its job object, so it watches the connection instead.
    pub struct Elevated(HANDLE);

    // SAFETY: a process handle may be used from any thread.
    unsafe impl Send for Elevated {}

    impl Elevated {
        pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            // SAFETY: the handle is open until drop.
            unsafe {
                if WaitForSingleObject(self.0, 0) != WAIT_OBJECT_0 {
                    return Ok(None);
                }
                let mut code = 0;
                GetExitCodeProcess(self.0, &mut code).map_err(std::io::Error::other)?;
                Ok(Some(ExitStatus::from_raw(code)))
            }
        }

        pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
            // SAFETY: as above.
            unsafe { WaitForSingleObject(self.0, INFINITE) };
            self.try_wait()?.ok_or_else(|| std::io::Error::other("still running"))
        }

        /// Usually refused (the app is not elevated); the helper stops by itself when the connection closes.
        pub fn kill(&mut self) -> std::io::Result<()> {
            // SAFETY: as above.
            unsafe { TerminateProcess(self.0, 1) }.map_err(std::io::Error::other)
        }
    }

    impl Drop for Elevated {
        fn drop(&mut self) {
            // SAFETY: closed once, here.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    pub fn token() -> Result<String, String> {
        let mut bytes = [0u8; 16];
        // SAFETY: fills the buffer it is given.
        unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.ok().map_err(|e| e.to_string())?;
        Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// This executable with `args`, elevated: the UAC prompt.
    pub fn run(args: &str) -> Result<Elevated, String> {
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let exe = wide(&std::env::current_exe().map_err(|e| e.to_string())?.to_string_lossy());
        let params = wide(args);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: w!("runas"),
            lpFile: PCWSTR(exe.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        // SAFETY: the strings outlive the call.
        unsafe { ShellExecuteExW(&mut info) }.map_err(|e| {
            if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
                "Administrator rights were not granted".to_owned()
            } else {
                format!("Cannot ask for administrator rights: {e}")
            }
        })?;
        if info.hProcess.is_invalid() {
            return Err("Cannot ask for administrator rights".into());
        }
        Ok(Elevated(info.hProcess))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn osascript_command_quoting() {
        let cmd = format!("{} --tun-helper {}", super::sh_quote("/Applications/Hitori VPN.app/Contents/MacOS/hitori"), super::sh_quote("/Users/o'neil/x.sock"));
        assert_eq!(cmd, r"'/Applications/Hitori VPN.app/Contents/MacOS/hitori' --tun-helper '/Users/o'\''neil/x.sock'");
        assert_eq!(super::applescript_str(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
