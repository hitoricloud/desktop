//! GitHub release discovery and bounded, verified downloads. No installation or GUI here.
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const VERSION: &str = env!("HITORI_VERSION");
pub const REPO: &str = "hitoricloud/desktop";
const MAX_DOWNLOAD: u64 = 256 * 1024 * 1024;
const MAX_UNPACKED: u64 = 768 * 1024 * 1024;
type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
    pub digest: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub draft: bool,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug)]
pub struct Available {
    pub version: semver::Version,
    pub asset: Asset,
}

pub fn platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok("windows-x86_64.zip"),
        ("linux", "x86_64") => Ok("linux-x86_64.tar.gz"),
        ("macos", "x86_64" | "aarch64") => Ok("macos-universal.tar.gz"),
        _ => Err("Automatic updates are not available for this OS/architecture.".into()),
    }
}

pub fn select(releases: Vec<Release>, current: &str, platform: &str) -> Result<Option<Available>> {
    let current = semver::Version::parse(current).map_err(|_| "This development build has no release version.")?;
    // Local and branch builds (0.1.0-dev, 0.1.0-<commit>) are not releases and never update themselves.
    if !current.pre.is_empty() { return Ok(None); }
    Ok(releases.into_iter().filter(|r| !r.draft && !r.prerelease).filter_map(|r| {
        let tag = r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name);
        let version = semver::Version::parse(tag.strip_suffix("-macos").unwrap_or(tag)).ok()?;
        if !version.pre.is_empty() || version <= current { return None; }
        let name = format!("hitori-{version}-{platform}");
        let asset = r.assets.into_iter().find(|a| a.name == name)?;
        Some(Available { version, asset })
    }).max_by(|a, b| a.version.cmp(&b.version)))
}

pub fn check() -> Result<Option<Available>> {
    let response = agent().get(&format!("https://api.github.com/repos/{REPO}/releases?per_page=100"))
        .set("Accept", "application/vnd.github+json").call().map_err(|_| "Cannot check GitHub releases. Try again later.")?;
    let mut body = Vec::new();
    response.into_reader().take(4 * 1024 * 1024).read_to_end(&mut body).map_err(|e| e.to_string())?;
    let releases = serde_json::from_slice(&body).map_err(|_| "Invalid GitHub release response.")?;
    select(releases, VERSION, platform()?)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(15)).timeout_read(Duration::from_secs(60))
        .timeout(Duration::from_secs(300)).user_agent(&format!("Hitori/{VERSION}")).build()
}

fn expected_digest(asset: &Asset) -> Result<&str> {
    let digest = asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:"))
        .filter(|d| d.len() == 64 && d.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or("The GitHub asset has no valid SHA-256 digest. Update refused.")?;
    if asset.size == 0 || asset.size > MAX_DOWNLOAD { return Err("The update package exceeds the download limit.".into()); }
    let prefix = format!("https://github.com/{REPO}/releases/download/");
    if !asset.browser_download_url.starts_with(&prefix) { return Err("Unexpected update download URL.".into()); }
    Ok(digest)
}

pub fn download(asset: &Asset, path: &Path) -> Result<()> {
    expected_digest(asset)?;
    let response = agent().get(&asset.browser_download_url).call().map_err(|_| "Cannot download the update. Try again later.")?;
    let result = verify_copy(response.into_reader(), asset, &mut std::fs::File::create(path).map_err(|e| e.to_string())?);
    if result.is_err() { let _ = std::fs::remove_file(path); }
    result
}

fn verify_copy(mut input: impl Read, asset: &Asset, out: &mut impl Write) -> Result<()> {
    let digest = expected_digest(asset)?;
    let mut hash = Sha256::new();
    let mut total = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = input.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 { break; }
        total += n as u64;
        if total > asset.size { return Err("Downloaded update exceeds its declared size.".into()); }
        hash.update(&buffer[..n]);
        out.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
    }
    if total != asset.size || !format!("{:x}", hash.finalize()).eq_ignore_ascii_case(digest) {
        return Err("Update checksum mismatch. Nothing was installed.".into());
    }
    Ok(())
}

/// Always create staging directories privately and exclusively, never in a predictable shared path.
pub fn staging() -> Result<PathBuf> {
    let mut random = [0u8; 16];
    getrandom::getrandom(&mut random).map_err(|e| e.to_string())?;
    let id = random.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let path = std::env::temp_dir().join(format!("hitori-update-{id}"));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    { use std::os::unix::fs::DirBuilderExt; builder.mode(0o700); }
    builder.create(&path).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Our archives contain one installation, never sibling scripts or auxiliary executables.
pub fn package_root(unpacked: &Path, expected: &str) -> Result<PathBuf> {
    let entries: Vec<_> = std::fs::read_dir(unpacked).map_err(|e| e.to_string())?
        .collect::<std::io::Result<_>>().map_err(|e| e.to_string())?;
    if entries.len() != 1 || entries[0].file_name() != expected
        || !entries[0].file_type().map_err(|e| e.to_string())?.is_dir() {
        return Err("Unexpected update package layout.".into());
    }
    Ok(entries[0].path())
}

fn safe_relative(name: &Path) -> Result<()> {
    if name.as_os_str().is_empty() || name.components().any(|p| !matches!(p, Component::Normal(_)))
        || name.to_string_lossy().contains(['\\', ':', '<', '>', '"', '|', '?', '*'])
        || name.components().any(|c| {
            let text = c.as_os_str().to_string_lossy();
            let stem = text.split('.').next().unwrap_or("").to_ascii_uppercase();
            text.ends_with(['.', ' ']) || text.chars().any(char::is_control)
                || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        }) {
        return Err("Unsafe path in the update archive.".into());
    }
    Ok(())
}

/// Links and special files are never needed by our release packages and must not escape staging.
pub fn unpack(path: &Path, out: &Path, zipped: bool) -> Result<()> {
    std::fs::create_dir(out).map_err(|e| e.to_string())?;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if zipped {
        let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        let mut total = 0u64;
        if zip.len() > 4096 { return Err("Too many archive entries.".into()); }
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
            let name = entry.enclosed_name().ok_or("Unsafe ZIP path.")?;
            safe_relative(&name)?;
            if entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) { return Err("Archive links are not allowed.".into()); }
            total = total.checked_add(entry.size()).ok_or("Archive size overflow.")?;
            if total > MAX_UNPACKED { return Err("Update archive is too large.".into()); }
            let dest = out.join(name);
            if entry.is_dir() { std::fs::create_dir_all(dest).map_err(|e| e.to_string())?; }
            else {
                std::fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
                let mut target = std::fs::OpenOptions::new().write(true).create_new(true).open(dest).map_err(|e| e.to_string())?;
                let size = entry.size();
                let written = std::io::copy(&mut entry.by_ref().take(size + 1), &mut target).map_err(|e| e.to_string())?;
                if written != size { return Err("Archive entry size mismatch.".into()); }
            }
        }
    } else {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
        let mut total = 0u64;
        for (count, entry) in archive.entries().map_err(|e| e.to_string())?.enumerate() {
            if count >= 4096 { return Err("Too many archive entries.".into()); }
            let mut entry = entry.map_err(|e| e.to_string())?;
            let name = entry.path().map_err(|e| e.to_string())?.into_owned();
            safe_relative(&name)?;
            let kind = entry.header().entry_type();
            if !kind.is_file() && !kind.is_dir() { return Err("Archive links and special files are not allowed.".into()); }
            total = total.checked_add(entry.size()).ok_or("Archive size overflow.")?;
            if total > MAX_UNPACKED { return Err("Update archive is too large.".into()); }
            if !entry.unpack_in(out).map_err(|e| e.to_string())? { return Err("Unsafe archive entry.".into()); }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn asset(bytes: &[u8]) -> Asset {
        Asset { name: "hitori-0.5.0-linux-x86_64.tar.gz".into(), browser_download_url: format!("https://github.com/{REPO}/releases/download/v0.5.0/a"), size: bytes.len() as u64, digest: Some(format!("sha256:{:x}", Sha256::digest(bytes))) }
    }
    #[test]
    fn download_requires_exact_size_and_sha256() {
        let a = asset(b"good");
        assert!(verify_copy(&b"good"[..], &a, &mut Vec::new()).is_ok());
        for bytes in [&b"evil"[..], &b"goo"[..], &b"good!"[..]] { assert!(verify_copy(bytes, &a, &mut Vec::new()).is_err()); }
        let mut missing = a.clone(); missing.digest = None;
        assert!(verify_copy(&b"good"[..], &missing, &mut Vec::new()).is_err());
        missing = a; missing.browser_download_url = "https://evil.invalid/a".into();
        assert!(verify_copy(&b"good"[..], &missing, &mut Vec::new()).is_err());
    }
    #[test]
    fn selects_newer_stable_matching_platform_not_publish_order() {
        let releases = || vec![
            Release { tag_name: "v0.5.0-macos".into(), draft: false, prerelease: false, assets: vec![] },
            Release { tag_name: "v0.5.0".into(), draft: false, prerelease: false, assets: vec![asset(b"a")] },
            Release { tag_name: "v0.6.0".into(), draft: false, prerelease: true, assets: vec![asset(b"a")] },
        ];
        assert_eq!(select(releases(), "0.4.1", "linux-x86_64.tar.gz").unwrap().unwrap().version.to_string(), "0.5.0");
        assert!(select(releases(), "0.5.0", "linux-x86_64.tar.gz").unwrap().is_none());
        assert!(select(releases(), "0.4.1", "windows-x86_64.zip").unwrap().is_none());
        assert!(select(releases(), "0.1.0-dev", "linux-x86_64.tar.gz").unwrap().is_none(), "dev builds are not offered updates");
    }
    fn tar_fixture(header: &tar::Header, body: &[u8], path: &Path) {
        let file = std::fs::File::create(path).unwrap();
        let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        gz.write_all(header.as_bytes()).unwrap();
        gz.write_all(body).unwrap();
        gz.write_all(&vec![0; (512 - body.len() % 512) % 512 + 1024]).unwrap();
        gz.finish().unwrap();
    }
    fn header(name: &str, size: u64, kind: tar::EntryType) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        header.set_size(size);
        header.set_mode(0o755);
        header.set_entry_type(kind);
        header.set_cksum();
        header
    }
    #[test]
    fn tar_unpack_rejects_traversal_links_special_files_and_oversize() {
        let base = staging().unwrap();
        let archive = base.join("archive.tar.gz");
        let mut link = header("link", 0, tar::EntryType::Symlink);
        link.set_link_name("../outside").unwrap();
        link.set_cksum();
        let cases = [
            header("../outside", 1, tar::EntryType::Regular),
            header("/outside", 1, tar::EntryType::Regular),
            link,
            header("fifo", 0, tar::EntryType::Fifo),
            header("big", MAX_UNPACKED + 1, tar::EntryType::Regular),
        ];
        for (i, header) in cases.iter().enumerate() {
            tar_fixture(header, b"x", &archive);
            assert!(unpack(&archive, &base.join(format!("case-{i}")), false).is_err(), "case {i}");
        }
        assert!(!base.join("outside").exists());
        tar_fixture(&header("package/bin/hitori", 1, tar::EntryType::Regular), b"x", &archive);
        let out = base.join("valid");
        unpack(&archive, &out, false).unwrap();
        assert_eq!(std::fs::read(out.join("package/bin/hitori")).unwrap(), b"x");
        #[cfg(unix)]
        { use std::os::unix::fs::PermissionsExt; assert_ne!(std::fs::metadata(out.join("package/bin/hitori")).unwrap().permissions().mode() & 0o111, 0); }
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn zip_unpack_rejects_traversal_links_duplicates_and_entry_floods() {
        let base = staging().unwrap();
        for case in 0..5 {
            let archive = base.join(format!("case-{case}.zip"));
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            match case {
                0 => { zip.start_file("../outside", options).unwrap(); zip.write_all(b"x").unwrap(); }
                1 => { zip.add_symlink("link", "../outside", options).unwrap(); }
                2 => {
                    // ZIP treats these names as distinct, but both resolve to the same destination.
                    for name in ["file", "./file"] { zip.start_file(name, options).unwrap(); zip.write_all(b"x").unwrap(); }
                }
                3 => { for i in 0..4097 { zip.add_directory(format!("dir-{i}/"), options).unwrap(); } }
                _ => { zip.start_file("package/hitori.exe", options).unwrap(); zip.write_all(b"exe").unwrap(); }
            }
            zip.finish().unwrap();
            let out = base.join(format!("out-{case}"));
            if case < 4 { assert!(unpack(&archive, &out, true).is_err(), "case {case}"); }
            else { unpack(&archive, &out, true).unwrap(); assert_eq!(std::fs::read(out.join("package/hitori.exe")).unwrap(), b"exe"); }
        }
        assert!(!base.join("outside").exists());
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn archive_requires_exactly_the_expected_package_root() {
        let base = staging().unwrap();
        let expected = "package";
        let root = base.join(expected);
        std::fs::create_dir(&root).unwrap();
        assert_eq!(package_root(&base, expected).unwrap(), root);
        std::fs::write(base.join("extra"), b"extra").unwrap();
        assert!(package_root(&base, expected).is_err());
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn rejects_escaping_and_windows_special_paths_on_every_os() {
        for path in ["../evil", "/evil", "C:/evil", "foo\\..\\evil", "a:b", ".. /evil", "file.", "CON", "dir/NUL.txt", "COM1", ""] { assert!(safe_relative(Path::new(path)).is_err(), "{path}"); }
        assert!(safe_relative(Path::new("package/bin/hitori")).is_ok());
    }
}
