//! File installation, independent of GPUI. Updates run in a copied worker after the app exits.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
pub enum Kind { Windows, Linux, Macos }

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Destination { pub root: PathBuf, pub kind: Kind }

impl Destination {
    pub fn executable(&self) -> PathBuf {
        self.root.join(match self.kind { Kind::Windows => "hitori.exe", Kind::Linux => "bin/hitori", Kind::Macos => "Contents/MacOS/hitori" })
    }

    fn files(&self) -> &'static [&'static str] {
        match self.kind {
            Kind::Windows => &["hitori.exe", "xray.exe", "wintun.dll", "LICENSE", "NOTICE"],
            Kind::Linux => &["bin/hitori", "libexec/hitori/xray", "share/applications/hitori.desktop", "share/icons/hicolor/scalable/apps/hitori.svg", "share/icons/hicolor/1024x1024/apps/hitori.png", "share/doc/hitori/LICENSE", "share/doc/hitori/NOTICE"],
            Kind::Macos => &[],
        }
    }
}

pub fn detect(exe: &Path) -> Result<Destination> {
    let exe = exe.canonicalize().map_err(|e| e.to_string())?;
    if exe.starts_with("/nix/store") { return Err("This app is managed by Nix. Update it through your Nix configuration.".into()); }
    #[cfg(windows)]
    let dest = Destination { root: exe.parent().ok_or("Missing install directory.")?.into(), kind: Kind::Windows };
    #[cfg(target_os = "linux")]
    let dest = {
        let bin = exe.parent().ok_or("Missing install directory.")?;
        if bin.file_name().is_none_or(|n| n != "bin") { return Err("Development builds cannot replace themselves. Install a release package first.".into()); }
        Destination { root: bin.parent().ok_or("Missing install directory.")?.into(), kind: Kind::Linux }
    };
    #[cfg(target_os = "macos")]
    let dest = {
        let root = exe.parent().and_then(Path::parent).and_then(Path::parent).ok_or("Not an app bundle.")?;
        if root.extension().is_none_or(|e| e != "app") { return Err("Install Hitori VPN.app before updating.".into()); }
        Destination { root: root.into(), kind: Kind::Macos }
    };
    if dest.executable() != exe { return Err("Unrecognised installation layout.".into()); }
    validate_source(&dest.root, &dest)?;
    writable(&dest.root)?;
    if dest.kind == Kind::Macos { writable(dest.root.parent().ok_or("Missing app directory.")?)?; }
    Ok(dest)
}

fn writable(dir: &Path) -> Result<()> {
    let file = dir.join(format!(".hitori-write-check-{}", std::process::id()));
    std::fs::OpenOptions::new().write(true).create_new(true).open(&file)
        .map_err(|_| "The installation is read-only. Install Hitori in a writable user folder to update it.".to_owned())?;
    std::fs::remove_file(file).map_err(|e| e.to_string())
}

pub fn validate_source(root: &Path, dest: &Destination) -> Result<()> {
    let files = if dest.kind == Kind::Macos { &["Contents/MacOS/hitori", "Contents/MacOS/xray", "Contents/Info.plist"][..] } else { dest.files() };
    for file in files {
        let path = root.join(file);
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink()) {
            return Err(format!("The package is missing {file}."));
        }
        // A symlinked parent would bypass a leaf-only check.
        let mut parent = path.parent();
        while let Some(p) = parent {
            if p == root { break; }
            if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) { return Err("Symlinked install directories are not supported.".into()); }
            parent = p.parent();
        }
    }
    Ok(())
}

fn validate_package(root: &Path, dest: &Destination) -> Result<()> {
    validate_source(root, dest)?;
    fn walk(root: &Path, dir: &Path, dest: &Destination) -> Result<()> {
        for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|e| e.to_string())?;
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if !kind.is_dir() && !kind.is_file() { return Err("Links and special files are not allowed in install packages.".into()); }
            let expected = if dest.kind == Kind::Macos { relative.starts_with("Contents") }
                else if kind.is_dir() { dest.files().iter().any(|file| Path::new(file).starts_with(relative)) }
                else { dest.files().iter().any(|file| Path::new(file) == relative) };
            if !expected { return Err(format!("Unexpected file in update package: {}", relative.display())); }
            if kind.is_dir() { walk(root, &path, dest)?; }
        }
        Ok(())
    }
    walk(root, root, dest)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Plan {
    pub source: PathBuf,
    pub destination: Destination,
    pub parent_pid: u32,
    pub stage: PathBuf,
}

pub fn prepare_update(source: PathBuf, destination: Destination, stage: PathBuf) -> Result<PathBuf> {
    validate_package(&source, &destination)?;
    #[cfg(target_os = "macos")]
    if destination.kind == Kind::Macos && !Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(&source)
        .status().map_err(|e| e.to_string())?.success() {
        return Err("The downloaded app bundle has an invalid signature. Nothing was installed.".into());
    }
    let plan = Plan { source, destination, parent_pid: std::process::id(), stage: stage.clone() };
    let path = stage.join("plan.json");
    std::fs::write(&path, serde_json::to_vec(&plan).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(path)
}

fn hidden(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; command.creation_flags(0x0800_0000); }
    command
}

/// Handshake: the worker has opened the parent process before the GUI begins quitting.
pub fn launch_worker(plan: &Path) -> Result<()> {
    let stage = plan.parent().ok_or("Invalid update plan.")?;
    let worker = stage.join(if cfg!(windows) { "hitori-update-worker.exe" } else { "hitori-update-worker" });
    std::fs::copy(std::env::current_exe().map_err(|e| e.to_string())?, &worker).map_err(|e| e.to_string())?;
    let mut child = hidden(Command::new(worker).arg("--apply-update").arg(plan)).spawn().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if stage.join("ready").is_file() {
            std::thread::spawn(move || { let _ = child.wait(); });
            return Ok(());
        }
        if child.try_wait().map_err(|e| e.to_string())?.is_some() { return Err("The update worker could not start.".into()); }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err("The update worker did not respond. Nothing was replaced.".into())
}

pub fn wait_parent(pid: u32, ready: &Path) -> Result<()> {
    if pid == 0 || pid == std::process::id() { return Err("Invalid parent process.".into()); }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
        // SAFETY: a synchronization-only handle held until after the wait pins this exact process.
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, false, pid).map_err(|e| e.to_string())?;
            let result = (|| {
                std::fs::write(ready, b"ready").map_err(|e| e.to_string())?;
                if WaitForSingleObject(handle, 120_000) != WAIT_OBJECT_0 { return Err("VPN shutdown timed out. Nothing was replaced.".into()); }
                Ok(())
            })();
            let _ = CloseHandle(handle);
            result
        }
    }
    #[cfg(unix)]
    {
        std::fs::write(ready, b"ready").map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            // SAFETY: signal 0 tests existence; it never sends a terminating signal.
            if unsafe { libc::kill(pid as libc::pid_t, 0) } == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) { return Ok(()); }
            if Instant::now() >= deadline { return Err("VPN shutdown timed out. Nothing was replaced.".into()); }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Only called before GPUI, IPC, helpers or network threads start.
pub fn worker(plan_path: &Path) -> Result<()> {
    let plan: Plan = serde_json::from_slice(&std::fs::read(plan_path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    if !plan.source.is_absolute() || !plan.destination.root.is_absolute() || plan.stage != plan_path.parent().ok_or("Invalid plan path.")? {
        return Err("Invalid update plan paths.".into());
    }
    wait_parent(plan.parent_pid, &plan.stage.join("ready"))?;
    let result = apply(&plan.source, &plan.destination);
    std::fs::write(plan.stage.join("result.json"), serde_json::to_vec(&result).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    // Restart the old app on replacement failure so it can show the error from result.json.
    let launched = hidden(&mut Command::new(plan.destination.executable())).spawn();
    match launched {
        Ok(_) => result,
        Err(e) => Err(format!("Files installed but restart failed: {e}. Launch Hitori manually.")),
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir(to).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(from).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        let dest = to.join(entry.file_name());
        if kind.is_dir() { copy_tree(&entry.path(), &dest)?; }
        else if kind.is_file() { std::fs::copy(entry.path(), dest).map_err(|e| e.to_string())?; }
        else { return Err("Links and special files are not allowed in install packages.".into()); }
    }
    Ok(())
}

/// A same-filesystem transaction. Backups remain available; failure restores all files already moved.
pub fn apply(source: &Path, dest: &Destination) -> Result<()> {
    validate_package(source, dest)?;
    if dest.kind == Kind::Macos { return replace_bundle(source, &dest.root); }
    std::fs::create_dir_all(&dest.root).map_err(|e| e.to_string())?;
    if std::fs::symlink_metadata(&dest.root).map_err(|e| e.to_string())?.file_type().is_symlink() { return Err("Symlinked installation root.".into()); }
    let backup = backup_path(&dest.root)?;
    let new = dest.root.join(".hitori-incoming");
    std::fs::create_dir(&new).map_err(|e| format!("Cannot stage installation: {e}"))?;
    let staged = (|| {
        for file in dest.files() {
            let to = new.join(file);
            std::fs::create_dir_all(to.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::copy(source.join(file), to).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    if let Err(e) = staged { let _ = std::fs::remove_dir_all(&new); return Err(e); }
    if let Err(e) = std::fs::create_dir(&backup) { let _ = std::fs::remove_dir_all(&new); return Err(e.to_string()); }
    let mut moved = Vec::new();
    let result: Result<()> = (|| {
        for file in dest.files() {
            let target = dest.root.join(file);
            // Refuse symlinked parents or special targets, not just symlinked leaf files.
            let mut parent = target.parent();
            while let Some(p) = parent {
                if p == dest.root { break; }
                if std::fs::symlink_metadata(p).is_ok_and(|m| !m.is_dir() || m.file_type().is_symlink()) { return Err("Unsafe installation directory.".into()); }
                parent = p.parent();
            }
            std::fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
            let had_old = match std::fs::symlink_metadata(&target) {
                Ok(m) if m.is_file() && !m.file_type().is_symlink() => true,
                Ok(_) => return Err("Unsafe existing installation file.".into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(e.to_string()),
            };
            if had_old {
                let old = backup.join(file);
                std::fs::create_dir_all(old.parent().unwrap()).map_err(|e| e.to_string())?;
                std::fs::rename(&target, old).map_err(|_| "Cannot replace running/locked application files. Close Hitori and try again.".to_owned())?;
            }
            moved.push((*file, had_old));
            std::fs::rename(new.join(file), &target).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut rollback_errors = Vec::new();
        for (file, had_old) in moved.into_iter().rev() {
            let target = dest.root.join(file);
            if target.exists() { if let Err(e) = std::fs::remove_file(&target) { rollback_errors.push(e.to_string()); continue; } }
            if had_old { if let Err(e) = std::fs::rename(backup.join(file), &target) { rollback_errors.push(e.to_string()); } }
        }
        let _ = std::fs::remove_dir_all(&new);
        if rollback_errors.is_empty() { let _ = std::fs::remove_dir_all(&backup); return Err(error); }
        return Err(format!("{error}. Restore files from {}: {}", backup.display(), rollback_errors.join("; ")));
    }
    let _ = std::fs::remove_dir_all(new);
    Ok(())
}

fn backup_path(root: &Path) -> Result<PathBuf> {
    let mut random = [0u8; 8];
    getrandom::getrandom(&mut random).map_err(|e| e.to_string())?;
    let id = random.iter().map(|b| format!("{b:02x}")).collect::<String>();
    if root.extension().is_some_and(|e| e == "app") { Ok(root.with_extension(format!("app.hitori-previous-{id}"))) }
    else { Ok(root.join(format!(".hitori-previous-{id}"))) }
}

fn replace_bundle(source: &Path, target: &Path) -> Result<()> {
    let next = target.with_extension("app.hitori-incoming");
    let backup = backup_path(target)?;
    if next.exists() { return Err("An interrupted app update exists. Move the .hitori-incoming folder aside and try again.".into()); }
    if let Err(e) = copy_tree(source, &next) { let _ = std::fs::remove_dir_all(&next); return Err(e); }
    // Preserve the signed bundle as a unit; do not modify nested executables after signing.
    if let Err(e) = std::fs::rename(target, &backup) { let _ = std::fs::remove_dir_all(&next); return Err(e.to_string()); }
    if let Err(e) = std::fs::rename(&next, target) {
        std::fs::rename(&backup, target).map_err(|rollback| format!("{e}; rollback failed: {rollback}. Backup: {}", backup.display()))?;
        let _ = std::fs::remove_dir_all(next);
        return Err(e.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (PathBuf, PathBuf, Destination) {
        let base = crate::release::staging().unwrap();
        let source = base.join("source");
        std::fs::create_dir(&source).unwrap();
        let dest = Destination { root: base.join("installed"), kind: Kind::Windows };
        std::fs::create_dir(&dest.root).unwrap();
        for file in dest.files() {
            std::fs::write(source.join(file), b"new").unwrap();
            std::fs::write(dest.root.join(file), b"old").unwrap();
        }
        (base, source, dest)
    }
    #[test]
    fn replaces_all_known_files_keeps_backup_and_user_data() {
        let (base, source, dest) = fixture();
        std::fs::write(dest.root.join("settings.json"), b"private").unwrap();
        apply(&source, &dest).unwrap();
        let backup = std::fs::read_dir(&dest.root).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with(".hitori-previous-")).unwrap().path();
        for file in dest.files() {
            assert_eq!(std::fs::read(dest.root.join(file)).unwrap(), b"new");
            assert_eq!(std::fs::read(backup.join(file)).unwrap(), b"old");
        }
        assert_eq!(std::fs::read(dest.root.join("settings.json")).unwrap(), b"private");
        // Subsequent updates work without erasing earlier recovery copies.
        assert!(apply(&source, &dest).is_ok());
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn failed_replacement_rolls_back_earlier_files() {
        let (base, source, dest) = fixture();
        std::fs::remove_file(dest.root.join("wintun.dll")).unwrap();
        std::fs::create_dir(dest.root.join("wintun.dll")).unwrap();
        assert!(apply(&source, &dest).is_err());
        for file in ["hitori.exe", "xray.exe"] { assert_eq!(std::fs::read(dest.root.join(file)).unwrap(), b"old"); }
        assert!(!std::fs::read_dir(&dest.root).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with(".hitori-previous-")));
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn unexpected_package_files_never_change_installation() {
        let (base, source, dest) = fixture();
        std::fs::write(source.join("unexpected.exe"), b"extra").unwrap();
        assert!(apply(&source, &dest).is_err());
        assert_eq!(std::fs::read(dest.root.join("hitori.exe")).unwrap(), b"old");
        std::fs::remove_dir_all(base).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_source_and_destination_parents() {
        use std::os::unix::fs::symlink;
        let (base, source, mut dest) = fixture();
        std::fs::remove_file(source.join("NOTICE")).unwrap();
        symlink(dest.root.join("NOTICE"), source.join("NOTICE")).unwrap();
        assert!(apply(&source, &dest).is_err());
        std::fs::remove_file(source.join("NOTICE")).unwrap();
        std::fs::write(source.join("NOTICE"), b"new").unwrap();
        let actual = dest.root.clone();
        dest.root = base.join("linked");
        symlink(&actual, &dest.root).unwrap();
        assert!(apply(&source, &dest).is_err());
        assert_eq!(std::fs::read(actual.join("hitori.exe")).unwrap(), b"old");
        std::fs::remove_dir_all(base).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn worker_waits_until_parent_has_exited() {
        use std::process::Stdio;
        let base = crate::release::staging().unwrap();
        let ready = base.join("ready");
        let mut parent = Command::new("sh").args(["-c", "read line"]).stdin(Stdio::piped()).spawn().unwrap();
        let pid = parent.id();
        let signal = ready.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || { send.send(wait_parent(pid, &signal)).unwrap(); });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(10)); }
        let waiting = ready.exists() && matches!(receive.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
        parent.kill().unwrap();
        parent.wait().unwrap();
        assert!(waiting, "worker must signal readiness without completing while parent lives");
        receive.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        waiter.join().unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn detects_linux_release_and_refuses_development_layout() {
        let base = crate::release::staging().unwrap();
        let dest = Destination { root: base.join("installed"), kind: Kind::Linux };
        for file in dest.files() {
            let path = dest.root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"release").unwrap();
        }
        assert_eq!(detect(&dest.executable()).unwrap().root, dest.root);
        let dev = base.join("hitori");
        std::fs::write(&dev, b"dev").unwrap();
        assert!(detect(&dev).is_err());
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn complete_macos_bundle_is_replaced_as_a_unit() {
        let base = crate::release::staging().unwrap();
        let source = base.join("source.app");
        let dest = Destination { root: base.join("Hitori VPN.app"), kind: Kind::Macos };
        for root in [&source, &dest.root] {
            for file in ["Contents/MacOS/hitori", "Contents/MacOS/xray", "Contents/Info.plist", "Contents/_CodeSignature/CodeResources"] {
                let path = root.join(file);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, if root == &source { b"new" } else { b"old" }).unwrap();
            }
        }
        apply(&source, &dest).unwrap();
        assert_eq!(std::fs::read(dest.root.join("Contents/_CodeSignature/CodeResources")).unwrap(), b"new");
        assert!(std::fs::read_dir(&base).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("Hitori VPN.app.hitori-previous-")));
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn incomplete_package_never_changes_installation() {
        let (base, source, dest) = fixture();
        std::fs::remove_file(source.join("NOTICE")).unwrap();
        assert!(apply(&source, &dest).is_err());
        assert_eq!(std::fs::read(dest.root.join("hitori.exe")).unwrap(), b"old");
        std::fs::remove_dir_all(base).unwrap();
    }
}
