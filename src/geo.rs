//! geoip.dat and geosite.dat for subscription rules such as `geoip:ru` or `geosite:youtube`: without them the core
//! refuses the config. Downloaded anew on every app start; the last verified copy is kept for starts without a network.
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde_json::Value;
use sha2::{Digest, Sha256};

const REPO: &str = "Loyalsoldier/v2ray-rules-dat";
const FILES: [&str; 2] = ["geoip.dat", "geosite.dat"];
const LIMIT: u64 = 64 << 20;

/// Downloaded during this app run; held while downloading, so a core start waits for the files.
static DONE: Mutex<bool> = Mutex::new(false);

type Fetch = dyn Fn(&str, u64) -> Option<Vec<u8>> + Sync;

/// Where the core finds them (`XRAY_LOCATION_ASSET`).
pub fn dir() -> PathBuf {
    crate::store::cache_dir().join("geo")
}

fn urls(file: &str) -> [String; 2] {
    [format!("https://raw.githubusercontent.com/{REPO}/release/{file}"), format!("https://cdn.jsdelivr.net/gh/{REPO}@release/{file}")]
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The file, if it matches the `.sha256sum` published next to it (a captive portal page or a cut download does not).
fn verified(url: &str, fetch: &Fetch) -> Option<Vec<u8>> {
    let sum = fetch(&format!("{url}.sha256sum"), 1024)?;
    let want = String::from_utf8_lossy(&sum).split_whitespace().next()?.to_ascii_lowercase();
    let bytes = fetch(url, LIMIT)?;
    (hex(&Sha256::digest(&bytes)) == want).then_some(bytes)
}

/// Both files into `dir`, in parallel; a failed download keeps the copy already there.
fn update(dir: &Path, fetch: &Fetch) {
    std::thread::scope(|scope| {
        for file in FILES {
            scope.spawn(move || {
                let path = dir.join(file);
                match urls(file).iter().find_map(|url| verified(url, fetch)) {
                    Some(bytes) => {
                        if let Err(e) = crate::store::write(&path, &bytes, false) {
                            eprintln!("hitori: cannot save {}: {e}", path.display());
                        }
                    }
                    None => eprintln!("hitori: cannot download {file}, keeping {}", if path.is_file() { "the old copy" } else { "none" }),
                }
            });
        }
    });
}

fn present(dir: &Path) -> bool {
    FILES.iter().all(|f| dir.join(f).is_file())
}

/// Once per app run, at start.
pub fn refresh() {
    let mut done = DONE.lock().unwrap_or_else(PoisonError::into_inner);
    if !*done {
        update(&dir(), &crate::net::large);
        *done = true;
    }
}

/// Rules of the config that need the files.
pub fn needed(config: &Value) -> bool {
    let text = config["routing"].to_string() + &config["dns"].to_string();
    ["\"geoip:", "\"geosite:", "\"ext:", "\"ext-ip:", "\"ext-domain:"].iter().any(|p| text.contains(p))
}

/// Before a core starts: the directory to give it. When the config needs files that are not there yet, waits for the
/// start-up download, or downloads now.
pub fn ensure(config: &Value) -> PathBuf {
    let dir = dir();
    if needed(config) && !present(&dir) {
        let mut done = DONE.lock().unwrap_or_else(PoisonError::into_inner);
        if !present(&dir) {
            update(&dir, &crate::net::large);
            *done = true;
        }
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hitori-geo-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sum(bytes: &[u8]) -> Vec<u8> {
        format!("{}  file\n", hex(&Sha256::digest(bytes))).into_bytes()
    }

    #[test]
    fn saves_verified_files_and_falls_back_to_the_mirror() {
        let dir = temp("mirror");
        update(&dir, &|url, _| {
            let body: &[u8] = if url.contains("geoip") { b"ip" } else { b"site" };
            match url {
                u if u.starts_with("https://raw.githubusercontent.com/") => None,
                u if u.ends_with(".sha256sum") => Some(sum(body)),
                _ => Some(body.to_vec()),
            }
        });
        assert_eq!(std::fs::read(dir.join("geoip.dat")).unwrap(), b"ip");
        assert_eq!(std::fs::read(dir.join("geosite.dat")).unwrap(), b"site");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_the_old_copy_when_the_download_does_not_match() {
        let dir = temp("junk");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("geoip.dat"), b"old").unwrap();
        update(&dir, &|url, _| if url.ends_with(".sha256sum") { Some(sum(b"real")) } else { Some(b"<html>portal</html>".to_vec()) });
        assert_eq!(std::fs::read(dir.join("geoip.dat")).unwrap(), b"old");
        assert!(!dir.join("geosite.dat").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_geo_rules_need_the_files() {
        let plain = json!({"routing": {"rules": [{"domain": ["domain:example.org"], "outboundTag": "proxy"}]}, "dns": {"servers": ["1.1.1.1"]}});
        assert!(!needed(&plain));
        assert!(needed(&json!({"routing": {"rules": [{"ip": ["geoip:private"], "outboundTag": "direct"}]}})));
        assert!(needed(&json!({"dns": {"servers": [{"address": "1.1.1.1", "domains": ["geosite:google"]}]}})));
        assert!(needed(&json!({"routing": {"rules": [{"domain": ["ext:geosite.dat:ru"]}]}})));
    }

    /// The real files load in the bundled core: HITORI_TEST_GEO=1 cargo test geo -- --ignored
    #[test]
    #[ignore]
    fn live_files_load_in_the_core() {
        if std::env::var_os("HITORI_TEST_GEO").is_none() {
            return;
        }
        let dir = temp("live");
        update(&dir, &crate::net::large);
        assert!(present(&dir));
        let config = json!({"outbounds": [{"protocol": "freedom", "tag": "direct"}],
            "routing": {"rules": [{"type": "field", "ip": ["geoip:ru", "geoip:private"], "outboundTag": "direct"},
                                  {"type": "field", "domain": ["geosite:category-ru", "geosite:youtube"], "outboundTag": "direct"}]}});
        let path = dir.join("config.json");
        std::fs::write(&path, config.to_string()).unwrap();
        let xray = crate::xray::xray_path().expect("bin/xray");
        let out = std::process::Command::new(xray).args(["run", "-test", "-c"]).arg(&path).env("XRAY_LOCATION_ASSET", &dir).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
