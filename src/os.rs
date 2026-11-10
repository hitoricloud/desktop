//! What differs between Linux, Windows and macOS in running the core: tying its process to the app's, and (outside
//! Linux, where system-wide mode is a TUN) pointing the system proxy at it. The device identity is in `sub.rs`.
use std::process::{Child, Command};

/// System-wide mode routes every app through a TUN (Linux); elsewhere it sets the system proxy.
pub const TUN: bool = cfg!(target_os = "linux");
/// Xray can tell which app opened a connection only on Linux and Windows, so per-app rules exist only there.
pub const PER_APP: bool = !cfg!(target_os = "macos");

/// Start a core process that goes away with the app however the app ends, without a console window on Windows.
pub fn spawn_core(cmd: &mut Command) -> std::io::Result<Child> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: prctl is async-signal-safe; this runs between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        cmd.spawn()
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let child = cmd.creation_flags(CREATE_NO_WINDOW).spawn()?;
        job::kill_with_app(&child);
        Ok(child)
    }
    #[cfg(target_os = "macos")]
    {
        // No parent-death signal here: remember the core, and `reap_orphans` stops it if the app died without doing so.
        let child = cmd.spawn()?;
        let _ = std::fs::write(crate::store::runtime_dir().join(format!("core-{}.pid", child.id())), "");
        Ok(child)
    }
}

/// Open a web page in the default browser (from any thread).
pub fn open_url(url: &str) {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use windows::core::{HSTRING, PCWSTR, w};
        // SAFETY: the strings outlive the call.
        unsafe {
            let _ = ShellExecuteW(None, w!("open"), &HSTRING::from(url), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
        }
    }
    #[cfg(not(windows))]
    {
        use std::process::Stdio;
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        match Command::new(opener).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            // Some openers run the browser in the foreground: reap it whenever it ends.
            Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
            Err(e) => eprintln!("hitori: cannot open the browser ({opener}): {e}"),
        }
    }
}

/// The child is gone; forget it.
pub fn reaped(_child: &Child) {
    #[cfg(target_os = "macos")]
    let _ = std::fs::remove_file(crate::store::runtime_dir().join(format!("core-{}.pid", _child.id())));
}

/// Cores left by an app that crashed (macOS only; elsewhere the OS stops them with the app).
pub fn reap_orphans() {
    #[cfg(target_os = "macos")]
    {
        let dir = crate::store::runtime_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let mut sys = sysinfo::System::new();
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, sysinfo::ProcessRefreshKind::new().with_exe(sysinfo::UpdateKind::Always));
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(pid) = name.strip_prefix("core-").and_then(|n| n.strip_suffix(".pid")).and_then(|n| n.parse::<usize>().ok()) else { continue };
            if let Some(p) = sys.process(sysinfo::Pid::from(pid))
                && p.exe().and_then(|e| e.file_name()).is_some_and(|n| n == "xray")
            {
                p.kill();
            }
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The system proxy while connected in system-wide mode outside Linux. What was set before is kept in the config dir
/// until it is put back, so a crash cannot leave the system pointing at a proxy that no longer runs.
pub mod sysproxy {
    #[cfg(not(target_os = "linux"))]
    fn saved() -> std::path::PathBuf {
        crate::store::config_dir().join("system-proxy.json")
    }

    /// Point the system's HTTP(S) and SOCKS proxy at the core.
    #[cfg(not(target_os = "linux"))]
    pub fn set(http: u16, socks: u16) -> Result<(), String> {
        let previous = if saved().exists() { None } else { Some(super::platform_proxy::current()) };
        if let Some(p) = previous {
            crate::store::write(&saved(), &serde_json::to_vec(&p).unwrap_or_default(), true).map_err(|e| e.to_string())?;
        }
        super::platform_proxy::apply(http, socks).inspect_err(|_| restore())
    }

    /// Linux system-wide mode is a TUN; the system proxy is never touched.
    #[cfg(target_os = "linux")]
    pub fn set(_http: u16, _socks: u16) -> Result<(), String> {
        Err("system-wide mode uses a TUN on Linux".into())
    }

    /// Put back what was there before `set` (also after a crash: called when the app starts).
    pub fn restore() {
        #[cfg(not(target_os = "linux"))]
        {
            let Ok(bytes) = std::fs::read(saved()) else { return };
            let previous = serde_json::from_slice(&bytes).unwrap_or_default();
            if super::platform_proxy::put_back(previous).is_ok() {
                let _ = std::fs::remove_file(saved());
            }
        }
    }
}

#[cfg(windows)]
mod platform_proxy {
    //! WinINet's per-user settings, which browsers and most apps follow.
    use serde::{Deserialize, Serialize};

    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    const BYPASS: &str = "<local>;localhost;127.*;10.*;172.16.*;172.17.*;172.18.*;172.19.*;172.2*;172.30.*;172.31.*;192.168.*;169.254.*";

    #[derive(Serialize, Deserialize, Default)]
    pub struct Previous {
        enable: u32,
        server: Option<String>,
        bypass: Option<String>,
    }

    pub fn current() -> Previous {
        let Ok(k) = windows_registry::CURRENT_USER.open(KEY) else { return Previous::default() };
        Previous { enable: k.get_u32("ProxyEnable").unwrap_or(0), server: k.get_string("ProxyServer").ok(), bypass: k.get_string("ProxyOverride").ok() }
    }

    pub fn apply(http: u16, _socks: u16) -> Result<(), String> {
        let k = windows_registry::CURRENT_USER.create(KEY).map_err(|e| e.to_string())?;
        k.set_string("ProxyServer", format!("127.0.0.1:{http}")).map_err(|e| e.to_string())?;
        k.set_string("ProxyOverride", BYPASS).map_err(|e| e.to_string())?;
        k.set_u32("ProxyEnable", 1).map_err(|e| e.to_string())?;
        notify();
        Ok(())
    }

    pub fn put_back(p: Previous) -> Result<(), String> {
        let k = windows_registry::CURRENT_USER.create(KEY).map_err(|e| e.to_string())?;
        k.set_u32("ProxyEnable", p.enable).map_err(|e| e.to_string())?;
        for (name, value) in [("ProxyServer", p.server), ("ProxyOverride", p.bypass)] {
            match value {
                Some(v) => k.set_string(name, v).map_err(|e| e.to_string())?,
                None => {
                    let _ = k.remove_value(name);
                }
            }
        }
        notify();
        Ok(())
    }

    /// Running apps re-read the settings.
    fn notify() {
        use windows::Win32::Networking::WinInet::{INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED, InternetSetOptionW};
        // SAFETY: both options take no buffer.
        unsafe {
            let _ = InternetSetOptionW(None, INTERNET_OPTION_SETTINGS_CHANGED, None, 0);
            let _ = InternetSetOptionW(None, INTERNET_OPTION_REFRESH, None, 0);
        }
    }
}

#[cfg(target_os = "macos")]
mod platform_proxy {
    //! `networksetup` on every enabled network service (Wi-Fi, Ethernet, …).
    use std::process::Command;

    use serde::{Deserialize, Serialize};

    const BYPASS: [&str; 6] = ["localhost", "127.0.0.1", "*.local", "169.254/16", "10.0.0.0/8", "192.168.0.0/16"];

    /// Which services had which proxies on: they are switched back on with their old servers untouched.
    #[derive(Serialize, Deserialize, Default)]
    pub struct Previous {
        on: Vec<(String, String)>,
    }

    const KINDS: [(&str, &str); 3] = [("webproxy", "-getwebproxy"), ("securewebproxy", "-getsecurewebproxy"), ("socksfirewallproxy", "-getsocksfirewallproxy")];

    fn run(args: &[&str]) -> Result<String, String> {
        let out = Command::new("/usr/sbin/networksetup").args(args).output().map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        // networksetup reports some failures on stdout with status 0.
        if !out.status.success() || text.contains("Error") {
            return Err(format!("networksetup {}: {}{}", args[0], text.trim(), String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(text)
    }

    fn services() -> Vec<String> {
        run(&["-listallnetworkservices"]).map(|t| t.lines().skip(1).filter(|l| !l.starts_with('*') && !l.trim().is_empty()).map(str::to_owned).collect()).unwrap_or_default()
    }

    pub fn current() -> Previous {
        let mut on = vec![];
        for svc in services() {
            for (kind, get) in KINDS {
                if run(&[get, &svc]).is_ok_and(|t| t.lines().any(|l| l.trim() == "Enabled: Yes")) {
                    on.push((svc.clone(), kind.to_owned()));
                }
            }
        }
        Previous { on }
    }

    pub fn apply(http: u16, socks: u16) -> Result<(), String> {
        let (http, socks) = (http.to_string(), socks.to_string());
        for svc in services() {
            run(&["-setwebproxy", &svc, "127.0.0.1", &http])?;
            run(&["-setsecurewebproxy", &svc, "127.0.0.1", &http])?;
            run(&["-setsocksfirewallproxy", &svc, "127.0.0.1", &socks])?;
            run(&[&["-setproxybypassdomains", svc.as_str()][..], &BYPASS].concat())?;
        }
        Ok(())
    }

    pub fn put_back(p: Previous) -> Result<(), String> {
        for svc in services() {
            for (kind, _) in KINDS {
                let state = if p.on.iter().any(|(s, k)| *s == svc && k == kind) { "on" } else { "off" };
                run(&[&format!("-set{kind}state"), &svc, state])?;
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
mod job {
    //! A job object that kills every core when the app's last handle to it closes, crash included.
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    static JOB: OnceLock<Option<usize>> = OnceLock::new();

    fn job() -> Option<HANDLE> {
        let raw = JOB.get_or_init(|| {
            // SAFETY: plain Win32 calls on a handle this process owns for its whole life.
            unsafe {
                let job = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?;
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let size = std::mem::size_of_val(&info) as u32;
                SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const _, size).ok()?;
                Some(job.0 as usize)
            }
        });
        raw.map(|h| HANDLE(h as *mut _))
    }

    pub fn kill_with_app(child: &std::process::Child) {
        if let Some(job) = job() {
            // SAFETY: the child's process handle is valid while `child` lives.
            unsafe {
                let _ = AssignProcessToJobObject(job, HANDLE(child.as_raw_handle()));
            }
        }
    }
}
