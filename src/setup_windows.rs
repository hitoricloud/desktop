//! Windows-only integration for our GPUI setup: per-user files, shortcut and uninstall registration.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::os::windows::{fs::OpenOptionsExt, process::CommandExt};
use crate::{install::{self, Destination, Kind}, release};
type Result<T> = std::result::Result<T, String>;
const REG_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\HitoriVPN";

fn user_dir(var: &str) -> Result<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).ok_or_else(|| format!("{var} is not available."))
}
pub fn destination() -> Result<PathBuf> { Ok(user_dir("LOCALAPPDATA")?.join("Programs/Hitori VPN")) }
fn shortcut() -> Result<PathBuf> { Ok(user_dir("APPDATA")?.join("Microsoft/Windows/Start Menu/Programs/Hitori VPN.lnk")) }
fn run(command: &mut Command) -> Result<()> {
    let output = command.creation_flags(0x0800_0000).output().map_err(|e| e.to_string())?;
    if output.status.success() { Ok(()) } else { Err("Windows could not register the installation. Close Hitori and try again.".into()) }
}
fn ensure_closed(root: &Path) -> Result<()> {
    // A sharing violation is also a refusal: never force-kill the VPN or overwrite a mapped core/DLL.
    for name in ["hitori.exe", "xray.exe", "wintun.dll"] {
        let path = root.join(name);
        if path.exists() {
            std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(path)
                .map_err(|_| "Close Hitori VPN and its running tunnel before installing or uninstalling.".to_owned())?;
        }
    }
    Ok(())
}
fn register(root: &Path) -> Result<()> {
    let link = shortcut()?;
    std::fs::create_dir_all(link.parent().unwrap()).map_err(|e| e.to_string())?;
    // Paths are passed as environment values, never interpolated into PowerShell code.
    run(Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'; $s=(New-Object -ComObject WScript.Shell).CreateShortcut($env:HITORI_SHORTCUT); $s.TargetPath=$env:HITORI_TARGET; $s.WorkingDirectory=$env:HITORI_DIRECTORY; $s.Save()"])
        .env("HITORI_SHORTCUT", link).env("HITORI_TARGET", root.join("hitori.exe")).env("HITORI_DIRECTORY", root))?;
    for (name, value) in [
        ("DisplayName", "Hitori VPN".to_owned()),
        ("DisplayVersion", release::VERSION.into()),
        ("Publisher", "Hitori".into()),
        ("InstallLocation", root.to_string_lossy().into_owned()),
        ("DisplayIcon", root.join("hitori.exe").to_string_lossy().into_owned()),
        ("UninstallString", format!("\"{}\" --uninstall", root.join("uninstall.exe").display())),
    ] {
        run(Command::new("reg.exe").args(["add", REG_KEY, "/v", name, "/t", "REG_SZ", "/d", &value, "/f"]))?;
    }
    for name in ["NoModify", "NoRepair"] { run(Command::new("reg.exe").args(["add", REG_KEY, "/v", name, "/t", "REG_DWORD", "/d", "1", "/f"]))?; }
    Ok(())
}

pub fn perform(root: PathBuf, uninstall: bool, payload: &[u8]) -> Result<()> {
    if root != destination()? { return Err("Unexpected installation directory.".into()); }
    if root.is_symlink() { return Err("Symlinked installation directories are not supported.".into()); }
    ensure_closed(&root)?;
    let stage = release::staging()?;
    if uninstall {
        let worker = stage.join("remove.exe");
        std::fs::copy(std::env::current_exe().map_err(|e| e.to_string())?, &worker).map_err(|e| e.to_string())?;
        let mut child = Command::new(worker).arg("--remove").arg(std::process::id().to_string()).arg(&stage).creation_flags(0x0800_0000).spawn().map_err(|e| e.to_string())?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if stage.join("ready").exists() { std::thread::spawn(move || { let _ = child.wait(); }); return Ok(()); }
            if child.try_wait().map_err(|e| e.to_string())?.is_some() { return Err("Uninstaller could not start.".into()); }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.kill(); let _ = child.wait();
        return Err("Uninstaller did not respond.".into());
    }
    let result = (|| {
        if payload.is_empty() { return Err("No payload is embedded in this development installer. Use the release setup executable.".into()); }
        let archive = stage.join("payload.zip");
        std::fs::write(&archive, payload).map_err(|e| e.to_string())?;
        let unpacked = stage.join("unpacked");
        release::unpack(&archive, &unpacked, true)?;
        let source = unpacked.join(format!("hitori-{}-windows-x86_64", release::VERSION));
        install::apply(&source, &Destination { root: root.clone(), kind: Kind::Windows })?;
        std::fs::copy(std::env::current_exe().map_err(|e| e.to_string())?, root.join("uninstall.exe")).map_err(|e| e.to_string())?;
        register(&root)
    })();
    let _ = std::fs::remove_dir_all(stage);
    result
}

pub fn worker_args() -> Option<Result<()>> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("--remove") { return None; }
    Some((|| {
        let pid = args.get(2).and_then(|s| s.parse().ok()).ok_or("Missing uninstaller parent.")?;
        let stage = PathBuf::from(args.get(3).ok_or("Missing uninstaller staging path.")?);
        if !stage.is_absolute() { return Err("Invalid uninstaller staging path.".into()); }
        install::wait_parent(pid, &stage.join("ready"))?;
        let root = destination()?;
        ensure_closed(&root)?;
        // Remove only owned files. Unknown files and account/settings directories are untouched.
        for name in ["hitori.exe", "xray.exe", "wintun.dll", "LICENSE", "NOTICE", "uninstall.exe"] {
            let path = root.join(name);
            if path.exists() { std::fs::remove_file(path).map_err(|e| e.to_string())?; }
        }
        let _ = std::fs::remove_file(shortcut()?);
        run(Command::new("reg.exe").args(["delete", REG_KEY, "/f"]))?;
        let _ = std::fs::remove_dir(&root);
        Ok(())
    })())
}

pub fn show_error(text: &str) {
    use windows::{core::{HSTRING, w}, Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK}};
    // SAFETY: both strings live through the modal call.
    unsafe { MessageBoxW(None, &HSTRING::from(text), w!("Hitori Setup"), MB_OK | MB_ICONERROR); }
}
