//! The system proxy in system-wide "System proxy" mode (Windows, macOS). What was set before is kept in the config dir
//! until it is put back, so a crash cannot leave the system pointing at a proxy that no longer runs.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(not(target_os = "linux"))]
fn saved() -> std::path::PathBuf {
    crate::store::config_dir().join("system-proxy.json")
}

#[cfg(not(target_os = "linux"))]
fn keep(previous: &platform::Previous) -> Result<(), String> {
    crate::store::write(&saved(), &serde_json::to_vec(previous).unwrap_or_default(), true).map_err(|e| e.to_string())
}

/// The system proxy points at the core while this lives; dropping it puts the previous settings back.
pub struct Active {
    stop: Arc<AtomicBool>,
}

/// Point the system's HTTP(S) and SOCKS proxy at the core.
#[cfg(not(target_os = "linux"))]
pub fn set(http: u16, socks: u16) -> Result<Active, String> {
    // A file left by a crashed run still holds what was there before that run: keep it.
    let previous = match crate::store::load(&saved()) {
        Some(p) => p,
        None => {
            let p = platform::current();
            keep(&p)?;
            p
        }
    };
    platform::apply(&previous, http, socks).inspect_err(|_| restore())?;
    let stop = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "macos")]
    {
        // Network services added or enabled while connected (a new Wi-Fi, a USB adapter) get the proxy too.
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_secs(3));
                let Some(mut previous) = crate::store::load::<platform::Previous>(&saved()) else { break };
                if !stop.load(Ordering::SeqCst) && platform::adopt_new(&mut previous) && keep(&previous).is_ok() {
                    let _ = platform::apply(&previous, http, socks);
                }
            }
        });
    }
    Ok(Active { stop })
}

/// Linux system-wide mode is always a TUN.
#[cfg(target_os = "linux")]
pub fn set(_http: u16, _socks: u16) -> Result<Active, String> {
    Err(t!("sysproxy.error.linux_is_tun").into())
}

impl Drop for Active {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        restore();
    }
}

/// Put back what was there before `set` (also after a crash: called when the app starts).
pub fn restore() {
    #[cfg(not(target_os = "linux"))]
    {
        let Some(previous) = crate::store::load(&saved()) else { return };
        match platform::put_back(&previous) {
            Ok(()) => {
                let _ = std::fs::remove_file(saved());
            }
            Err(e) => eprintln!("hitori: cannot restore the system proxy: {e}"),
        }
    }
}

/// WinINet's per-connection settings of the LAN connection, which browsers and most apps follow.
#[cfg(any(windows, test))]
mod wininet {
    use serde::{Deserialize, Serialize};

    /// Local and private addresses never go through the proxy (WinINet wildcards).
    const BYPASS: [&str; 26] = [
        "<local>",
        "localhost",
        "127.*",
        "10.*",
        "169.254.*",
        "192.168.*",
        "172.16.*",
        "172.17.*",
        "172.18.*",
        "172.19.*",
        "172.20.*",
        "172.21.*",
        "172.22.*",
        "172.23.*",
        "172.24.*",
        "172.25.*",
        "172.26.*",
        "172.27.*",
        "172.28.*",
        "172.29.*",
        "172.30.*",
        "172.31.*",
        "100.64.*",
        "[::1]",
        "*.local",
        "fe80::*",
    ];
    pub const DIRECT: u32 = 1;
    pub const PROXY: u32 = 2;

    /// Exactly what the LAN connection had: `PROXY_TYPE_*` flags, manual server, bypass list and PAC URL.
    #[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
    pub struct Previous {
        pub flags: u32,
        pub server: Option<String>,
        pub bypass: Option<String>,
        pub pac: Option<String>,
    }

    /// Ours: only the manual proxy (no PAC, no auto-detect, which would win over it), the user's bypass entries kept.
    pub fn ours(previous: &Previous, http: u16) -> Previous {
        let mut bypass: Vec<&str> = BYPASS.to_vec();
        for entry in previous.bypass.iter().flat_map(|b| b.split([';', ' ', '\t', '\r', '\n'])).map(str::trim).filter(|e| !e.is_empty()) {
            if !bypass.iter().any(|b| b.eq_ignore_ascii_case(entry)) {
                bypass.push(entry);
            }
        }
        Previous { flags: DIRECT | PROXY, server: Some(format!("127.0.0.1:{http}")), bypass: Some(bypass.join(";")), pac: previous.pac.clone() }
    }
}

#[cfg(windows)]
mod platform {
    use windows::Win32::Foundation::{GlobalFree, HGLOBAL};
    use windows::Win32::Networking::WinInet::{
        INTERNET_OPTION_PER_CONNECTION_OPTION, INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED, INTERNET_PER_CONN, INTERNET_PER_CONN_AUTOCONFIG_URL,
        INTERNET_PER_CONN_FLAGS, INTERNET_PER_CONN_OPTION_LISTW, INTERNET_PER_CONN_OPTIONW, INTERNET_PER_CONN_PROXY_BYPASS, INTERNET_PER_CONN_PROXY_SERVER, InternetQueryOptionW,
        InternetSetOptionW,
    };
    use windows::core::PWSTR;

    pub use super::wininet::Previous;

    const OPTIONS: [INTERNET_PER_CONN; 4] = [INTERNET_PER_CONN_FLAGS, INTERNET_PER_CONN_PROXY_SERVER, INTERNET_PER_CONN_PROXY_BYPASS, INTERNET_PER_CONN_AUTOCONFIG_URL];

    fn list(options: &mut [INTERNET_PER_CONN_OPTIONW; 4]) -> INTERNET_PER_CONN_OPTION_LISTW {
        INTERNET_PER_CONN_OPTION_LISTW {
            dwSize: size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32,
            pszConnection: PWSTR::null(), // the LAN connection
            dwOptionCount: 4,
            dwOptionError: 0,
            pOptions: options.as_mut_ptr(),
        }
    }

    pub fn current() -> Previous {
        let mut options = OPTIONS.map(|o| INTERNET_PER_CONN_OPTIONW { dwOption: o, ..Default::default() });
        let mut list = list(&mut options);
        let mut len = size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32;
        // SAFETY: `list` points at `options`, both live across the call; returned strings are freed with GlobalFree.
        unsafe {
            if InternetQueryOptionW(None, INTERNET_OPTION_PER_CONNECTION_OPTION, Some(&mut list as *mut _ as *mut _), &mut len).is_err() {
                return Previous { flags: super::wininet::DIRECT, ..Default::default() };
            }
            let text = |o: &INTERNET_PER_CONN_OPTIONW| {
                let p = o.Value.pszValue;
                if p.is_null() {
                    return None;
                }
                let s = p.to_string().ok();
                let _ = GlobalFree(Some(HGLOBAL(p.0 as _)));
                s.filter(|s| !s.is_empty())
            };
            Previous { flags: options[0].Value.dwValue, server: text(&options[1]), bypass: text(&options[2]), pac: text(&options[3]) }
        }
    }

    fn write(p: &Previous) -> Result<(), String> {
        let wide = |s: &Option<String>| s.as_ref().map(|s| s.encode_utf16().chain([0]).collect::<Vec<u16>>());
        let (mut server, mut bypass, mut pac) = (wide(&p.server), wide(&p.bypass), wide(&p.pac));
        let ptr = |w: &mut Option<Vec<u16>>| w.as_mut().map_or(PWSTR::null(), |w| PWSTR(w.as_mut_ptr()));
        let mut options = OPTIONS.map(|o| INTERNET_PER_CONN_OPTIONW { dwOption: o, ..Default::default() });
        options[0].Value.dwValue = p.flags;
        options[1].Value.pszValue = ptr(&mut server);
        options[2].Value.pszValue = ptr(&mut bypass);
        options[3].Value.pszValue = ptr(&mut pac);
        let list = list(&mut options);
        // SAFETY: every pointer in `list` refers to locals alive across the calls; the refresh options take no buffer.
        unsafe {
            InternetSetOptionW(None, INTERNET_OPTION_PER_CONNECTION_OPTION, Some(&list as *const _ as *const _), size_of_val(&list) as u32).map_err(|e| e.to_string())?;
            // Running apps re-read the settings.
            let _ = InternetSetOptionW(None, INTERNET_OPTION_SETTINGS_CHANGED, None, 0);
            let _ = InternetSetOptionW(None, INTERNET_OPTION_REFRESH, None, 0);
        }
        Ok(())
    }

    pub fn apply(previous: &Previous, http: u16, _socks: u16) -> Result<(), String> {
        write(&super::wininet::ours(previous, http))
    }

    pub fn put_back(previous: &Previous) -> Result<(), String> {
        write(previous)
    }
}

/// `networksetup` on every network service: proxies, bypass domains, PAC and auto-discovery, per service.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub mod networksetup {
    use serde::{Deserialize, Serialize};

    const BYPASS: [&str; 9] = ["localhost", "127.0.0.1", "::1", "*.local", "169.254/16", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10"];
    /// Proxy kinds as in `-set<kind>` / `-get<kind>`.
    pub const KINDS: [&str; 3] = ["webproxy", "securewebproxy", "socksfirewallproxy"];

    #[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
    pub struct Proxy {
        pub on: bool,
        pub server: String,
        pub port: u16,
    }

    /// One network service as it was before we changed it.
    #[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
    pub struct Service {
        pub name: String,
        /// Web, secure web and SOCKS, in `KINDS` order.
        pub proxies: Vec<Proxy>,
        pub bypass: Vec<String>,
        pub pac: Option<String>,
        pub pac_on: bool,
        pub discovery: bool,
    }

    #[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
    pub struct Previous {
        pub services: Vec<Service>,
    }

    /// `-listallnetworkservices`: names, without the header and (unless `disabled`) the services marked `*`.
    pub fn services(text: &str, disabled: bool) -> Vec<String> {
        text.lines()
            .skip(1)
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| match l.strip_prefix('*') {
                Some(name) => disabled.then(|| name.to_owned()),
                None => Some(l.to_owned()),
            })
            .collect()
    }

    fn value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')).map(str::trim)
    }

    /// `-getwebproxy` and friends: `Enabled: Yes`, `Server: …`, `Port: …`.
    pub fn proxy(text: &str) -> Proxy {
        Proxy {
            on: value(text, "Enabled") == Some("Yes"),
            server: value(text, "Server").unwrap_or_default().to_owned(),
            port: value(text, "Port").and_then(|p| p.parse().ok()).unwrap_or(0),
        }
    }

    /// `-getproxybypassdomains`, `-getdnsservers`: one entry per line, or a "There aren't any …" sentence.
    pub fn list(text: &str) -> Vec<String> {
        if text.contains("There aren't any") || text.contains("is not a recognized") {
            return vec![];
        }
        text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()
    }

    /// `-getautoproxyurl`: `URL: (null)` when none is set.
    pub fn pac(text: &str) -> (Option<String>, bool) {
        let url = value(text, "URL").filter(|u| !u.is_empty() && *u != "(null)").map(str::to_owned);
        (url, value(text, "Enabled") == Some("Yes"))
    }

    /// `-getproxyautodiscovery`: `Auto Proxy Discovery: On`.
    pub fn discovery(text: &str) -> bool {
        value(text, "Auto Proxy Discovery") == Some("On")
    }

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| (*p).to_owned()).collect()
    }

    fn on(b: bool) -> &'static str {
        if b { "on" } else { "off" }
    }

    /// Ours on this service: every proxy at the core, the user's bypass domains kept, PAC and discovery off.
    pub fn apply(svc: &Service, http: u16, socks: u16) -> Vec<Vec<String>> {
        let (http, socks) = (http.to_string(), socks.to_string());
        let name = svc.name.as_str();
        let mut bypass: Vec<&str> = BYPASS.to_vec();
        bypass.extend(svc.bypass.iter().map(String::as_str).filter(|d| !BYPASS.contains(d)));
        vec![
            cmd(&["-setwebproxy", name, "127.0.0.1", &http]),
            cmd(&["-setsecurewebproxy", name, "127.0.0.1", &http]),
            cmd(&["-setsocksfirewallproxy", name, "127.0.0.1", &socks]),
            cmd(&[&["-setproxybypassdomains", name][..], &bypass].concat()),
            cmd(&["-setautoproxystate", name, "off"]),
            cmd(&["-setproxyautodiscovery", name, "off"]),
        ]
    }

    /// Back to how it was. A proxy that never had a server keeps ours, switched off: networksetup cannot clear it.
    pub fn restore(svc: &Service) -> Vec<Vec<String>> {
        let name = svc.name.as_str();
        let mut out = vec![];
        for (kind, p) in KINDS.iter().zip(&svc.proxies) {
            if !p.server.is_empty() {
                out.push(cmd(&[&format!("-set{kind}"), name, &p.server, &p.port.to_string()]));
            }
            out.push(cmd(&[&format!("-set{kind}state"), name, on(p.on)]));
        }
        let bypass: Vec<&str> = if svc.bypass.is_empty() { vec!["Empty"] } else { svc.bypass.iter().map(String::as_str).collect() };
        out.push(cmd(&[&["-setproxybypassdomains", name][..], &bypass].concat()));
        if let Some(url) = &svc.pac {
            out.push(cmd(&["-setautoproxyurl", name, url]));
        }
        out.push(cmd(&["-setautoproxystate", name, on(svc.pac_on)]));
        out.push(cmd(&["-setproxyautodiscovery", name, on(svc.discovery)]));
        out
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn networksetup_run(args: &[impl AsRef<str>]) -> Result<String, String> {
    let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    let out = std::process::Command::new("/usr/sbin/networksetup").args(&args).stdin(std::process::Stdio::null()).output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    // networksetup reports some failures on stdout with status 0.
    if !out.status.success() || text.starts_with("Error") || text.contains("** Error") {
        return Err(format!("networksetup {}: {}{}", args[0], text.trim(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(text)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::networksetup::{self as ns, KINDS, Service};
    use super::networksetup_run as run;

    pub use ns::Previous;

    fn services(disabled: bool) -> Vec<String> {
        run(&["-listallnetworkservices"]).map(|t| ns::services(&t, disabled)).unwrap_or_default()
    }

    fn read(name: &str) -> Service {
        let get = |what: &str| run(&[what, name]).unwrap_or_default();
        let (pac, pac_on) = ns::pac(&get("-getautoproxyurl"));
        Service {
            name: name.to_owned(),
            proxies: KINDS.iter().map(|k| ns::proxy(&get(&format!("-get{k}")))).collect(),
            bypass: ns::list(&get("-getproxybypassdomains")),
            pac,
            pac_on,
            discovery: ns::discovery(&get("-getproxyautodiscovery")),
        }
    }

    pub fn current() -> Previous {
        Previous { services: services(false).iter().map(|s| read(s)).collect() }
    }

    /// Record services enabled since; true when there are new ones.
    pub fn adopt_new(previous: &mut Previous) -> bool {
        let fresh: Vec<Service> = services(false).into_iter().filter(|s| previous.services.iter().all(|p| p.name != *s)).map(|s| read(&s)).collect();
        previous.services.extend(fresh.iter().cloned());
        !fresh.is_empty()
    }

    pub fn apply(previous: &Previous, http: u16, socks: u16) -> Result<(), String> {
        for svc in &previous.services {
            for args in ns::apply(svc, http, socks) {
                run(&args)?;
            }
        }
        Ok(())
    }

    pub fn put_back(previous: &Previous) -> Result<(), String> {
        let exists = services(true);
        let mut error = None;
        // Services removed meanwhile have nothing to restore.
        for svc in previous.services.iter().filter(|s| exists.contains(&s.name)) {
            for args in ns::restore(svc) {
                if let Err(e) = run(&args) {
                    error.get_or_insert(e);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::networksetup as ns;
    use super::wininet::{self, Previous};

    #[test]
    fn wininet_keeps_the_users_bypass_and_restores_exactly() {
        let before = Previous { flags: 1 | 4, server: Some("corp:3128".into()), bypass: Some("*.corp.example; 10.*;<local>".into()), pac: Some("http://wpad/proxy.pac".into()) };
        let ours = wininet::ours(&before, 10809);
        assert_eq!(ours.flags, wininet::DIRECT | wininet::PROXY, "no PAC or auto-detect over the manual proxy");
        assert_eq!(ours.server.as_deref(), Some("127.0.0.1:10809"));
        let bypass: Vec<_> = ours.bypass.as_deref().unwrap().split(';').collect();
        assert!(bypass.starts_with(&["<local>", "localhost", "127.*", "10.*"]));
        assert!(bypass.contains(&"*.corp.example") && bypass.contains(&"172.31.*") && !bypass.iter().any(|b| *b == "172.2*"));
        assert_eq!(bypass.iter().filter(|b| **b == "10.*").count(), 1);
        assert_eq!(ours.pac, before.pac, "kept so restoring needs nothing but `before`");
        let json = serde_json::to_string(&before).unwrap();
        assert_eq!(serde_json::from_str::<Previous>(&json).unwrap(), before, "the crash file round-trips");
    }

    const LIST: &str = "An asterisk (*) denotes that a network service is disabled.\nWi-Fi\n*Thunderbolt Bridge\nUSB 10/100/1000 LAN\n\n";

    #[test]
    fn networksetup_output_parses() {
        assert_eq!(ns::services(LIST, false), ["Wi-Fi", "USB 10/100/1000 LAN"]);
        assert_eq!(ns::services(LIST, true), ["Wi-Fi", "Thunderbolt Bridge", "USB 10/100/1000 LAN"]);
        let p = ns::proxy("Enabled: Yes\nServer: corp.example\nPort: 3128\nAuthenticated Proxy Enabled: 0\n");
        assert_eq!(p, ns::Proxy { on: true, server: "corp.example".into(), port: 3128 });
        assert_eq!(ns::proxy("Enabled: No\nServer: \nPort: 0\nAuthenticated Proxy Enabled: 0\n"), ns::Proxy::default());
        assert!(ns::list("There aren't any bypass domains set on Wi-Fi.\n").is_empty());
        assert_eq!(ns::list("*.local\n169.254/16\n"), ["*.local", "169.254/16"]);
        assert_eq!(ns::list("192.168.1.1\n2001:db8::1\n"), ["192.168.1.1", "2001:db8::1"]);
        assert_eq!(ns::pac("URL: (null)\nEnabled: No\n"), (None, false));
        assert_eq!(ns::pac("URL: http://wpad/proxy.pac\nEnabled: Yes\n"), (Some("http://wpad/proxy.pac".into()), true));
        assert!(ns::discovery("Auto Proxy Discovery: On\n") && !ns::discovery("Auto Proxy Discovery: Off\n"));
    }

    #[test]
    fn networksetup_restores_every_setting() {
        let svc = ns::Service {
            name: "Wi-Fi".into(),
            proxies: vec![ns::Proxy { on: true, server: "corp".into(), port: 3128 }, ns::Proxy::default(), ns::Proxy { on: false, server: "socks.corp".into(), port: 1080 }],
            bypass: vec!["*.corp".into(), "*.local".into()],
            pac: Some("http://wpad/p.pac".into()),
            pac_on: true,
            discovery: false,
        };
        let ours = ns::apply(&svc, 10809, 10808);
        assert_eq!(ours[0], ["-setwebproxy", "Wi-Fi", "127.0.0.1", "10809"]);
        assert_eq!(ours[2], ["-setsocksfirewallproxy", "Wi-Fi", "127.0.0.1", "10808"]);
        assert!(ours[3].contains(&"*.corp".to_owned()) && ours[3].iter().filter(|d| *d == "*.local").count() == 1);
        assert!(ours.contains(&vec!["-setautoproxystate".to_owned(), "Wi-Fi".into(), "off".into()]));
        let back: Vec<String> = ns::restore(&svc).into_iter().map(|c| c.join(" ")).collect();
        assert_eq!(
            back,
            [
                "-setwebproxy Wi-Fi corp 3128",
                "-setwebproxystate Wi-Fi on",
                "-setsecurewebproxystate Wi-Fi off",
                "-setsocksfirewallproxy Wi-Fi socks.corp 1080",
                "-setsocksfirewallproxystate Wi-Fi off",
                "-setproxybypassdomains Wi-Fi *.corp *.local",
                "-setautoproxyurl Wi-Fi http://wpad/p.pac",
                "-setautoproxystate Wi-Fi on",
                "-setproxyautodiscovery Wi-Fi off",
            ]
        );
        let plain = ns::Service { name: "LAN".into(), proxies: vec![ns::Proxy::default(); 3], ..Default::default() };
        assert!(ns::restore(&plain).iter().any(|c| c.join(" ") == "-setproxybypassdomains LAN Empty"));
    }
}
