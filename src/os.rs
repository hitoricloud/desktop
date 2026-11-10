//! What differs between Linux, Windows and macOS in running the core: tying its process to the app's (or the TUN
//! helper's). The system proxy is in `sysproxy.rs`, the device identity in `sub.rs`.
use std::process::{Child, Command};

/// System-wide mode can also be the system proxy (no administrator rights) instead of a TUN: Windows and macOS.
pub const SYSTEM_PROXY: bool = !cfg!(target_os = "linux");
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
            // `tun-core-`: started by the TUN helper (as this user), which could not stop it.
            let Some(pid) = name.strip_prefix("core-").or_else(|| name.strip_prefix("tun-core-")).and_then(|n| n.strip_suffix(".pid")).and_then(|n| n.parse::<usize>().ok()) else { continue };
            if let Some(p) = sys.process(sysinfo::Pid::from(pid))
                && p.exe().and_then(|e| e.file_name()).is_some_and(|n| n == "xray")
            {
                p.kill();
            }
            let _ = std::fs::remove_file(entry.path());
        }
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
