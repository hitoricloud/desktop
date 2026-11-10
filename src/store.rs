//! Files the app keeps: settings and the signed-in subscription in ~/.config/hitori (0600), learned exits in ~/.cache.
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::data::{AppRule, Protocol, RuleMode};

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(fallback)).join("hitori")
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache")
}

/// Generated core configs (they hold credentials): private to the user and gone after a reboot.
pub fn runtime_dir() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("hitori")).unwrap_or_else(|| cache_dir().join("run"));
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    dir
}

/// Write via a temporary file so a crash never leaves half a file; `private` files are 0600.
pub fn write(path: &PathBuf, bytes: &[u8], private: bool) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        if private {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(if private { 0o600 } else { 0o644 }).open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

pub fn load<T: DeserializeOwned>(path: &PathBuf) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn save<T: Serialize>(path: &PathBuf, value: &T, private: bool) {
    if let Ok(bytes) = serde_json::to_vec_pretty(value)
        && let Err(e) = write(path, &bytes, private)
    {
        eprintln!("hitori: cannot write {}: {e}", path.display());
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
pub enum Proto {
    Tls,
    Grpc,
    Hy2,
}

impl From<Protocol> for Proto {
    fn from(p: Protocol) -> Self {
        match p {
            Protocol::Tls => Proto::Tls,
            Protocol::Grpc => Proto::Grpc,
            Protocol::Hy2 => Proto::Hy2,
        }
    }
}

impl From<Proto> for Protocol {
    fn from(p: Proto) -> Self {
        match p {
            Proto::Tls => Protocol::Tls,
            Proto::Grpc => Protocol::Grpc,
            Proto::Hy2 => Protocol::Hy2,
        }
    }
}

/// A saved route for an app (`app` is its id) or a website (`app` is the domain).
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Rule {
    pub app: String,
    pub mode: String,
    pub exit: Option<String>,
    pub via: Option<String>,
    pub protocol: Option<Proto>,
    pub kill: bool,
}

impl Rule {
    pub fn from_app(app: &str, r: &AppRule) -> Self {
        Self { app: app.into(), mode: r.mode.key().into(), exit: r.exit.map(Into::into), via: r.via.map(Into::into), protocol: r.protocol.map(Into::into), kill: r.kill }
    }

    pub fn to_app(&self) -> (&'static str, AppRule) {
        let mode = RuleMode::parse(&self.mode);
        let i = crate::data::intern;
        (i(&self.app), AppRule { mode, exit: self.exit.as_deref().map(i), via: self.via.as_deref().map(i), protocol: self.protocol.map(Into::into), kill: self.kill })
    }
}

/// Everything the user chose, restored on the next start.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub server: Option<String>,
    pub entry: Option<String>,
    pub recents: Vec<String>,
    pub protocol: Proto,
    pub auto_connect: bool,
    pub kill_switch: bool,
    /// Where traffic without a rule goes: auto, vpn, direct or block.
    pub route: String,
    /// Route the whole system through a TUN device (needs the administrator once per connection).
    pub system_wide: bool,
    pub dark: bool,
    pub rules_big: bool,
    pub apps: Vec<Rule>,
    #[serde(deserialize_with = "sites")]
    pub sites: Vec<Rule>,
    /// Suggestions answered with "Keep auto": not offered again.
    pub dismissed: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server: None,
            entry: None,
            recents: vec![],
            protocol: Proto::Tls,
            auto_connect: false,
            kill_switch: true,
            route: "auto".into(),
            system_wide: true,
            dark: true,
            rules_big: true,
            apps: vec![],
            sites: vec![],
            dismissed: vec![],
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        config_dir().join("settings.json")
    }

    pub fn sites(&self) -> Vec<(&'static str, AppRule)> {
        self.sites.iter().map(Rule::to_app).collect()
    }
}

/// Website rules, also from settings written when a site could only be `[domain, vpn]`.
fn sites<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Rule>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Saved {
        Rule(Rule),
        Old(String, bool),
    }
    Ok(Vec::<Saved>::deserialize(d)?
        .into_iter()
        .map(|s| match s {
            Saved::Rule(r) => r,
            Saved::Old(domain, vpn) => Rule { app: domain, mode: if vpn { "vpn" } else { "direct" }.into(), exit: None, via: None, protocol: None, kill: false },
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn old_website_rules_still_load() {
        let s: Settings = serde_json::from_str(r#"{"route":"vpn","sites":[["youtube.com",true],["bank.ru",false],{"app":"x.org","mode":"block","exit":null,"via":null,"protocol":null,"kill":false}]}"#).unwrap();
        let modes: Vec<_> = s.sites().into_iter().map(|(d, r)| (d, r.mode.key())).collect();
        assert_eq!(modes, [("youtube.com", "vpn"), ("bank.ru", "direct"), ("x.org", "block")]);
        assert_eq!(s.route, "vpn");
    }
}

/// The signed-in subscription URL.
#[derive(Serialize, Deserialize)]
pub struct Login {
    pub url: String,
}

impl Login {
    pub fn path() -> PathBuf {
        config_dir().join("account.json")
    }
}
