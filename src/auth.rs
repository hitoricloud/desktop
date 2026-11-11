//! Telegram sign-in through the Hitori backend (`/app/v1`): PKCE with a loopback redirect (RFC 8252), tokens kept in the
//! config dir that refresh themselves once on a 401, the profile with its subscription link, and the avatar.
use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use gpui::RenderImage;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::data::{self, Account};
use crate::store;

/// `HITORI_API` or the production backend.
pub fn api() -> String {
    std::env::var("HITORI_API").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "https://sub.scamhost.online".into()).trim().trim_end_matches('/').to_owned()
}

pub fn session_path() -> PathBuf {
    store::config_dir().join("session.json")
}

#[derive(Debug, PartialEq)]
pub enum Error {
    /// The access token is invalid and could not be refreshed: signed out.
    Unauthorized,
    Cancelled,
    TimedOut,
    /// The backend sent the browser back with `?error=<slug>`.
    Denied(String),
    Api {
        status: u16,
        code: String,
        message: String,
    },
    Net(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Unauthorized => write!(f, "Your session has ended. Sign in again."),
            Error::Cancelled => write!(f, "Sign-in cancelled"),
            Error::TimedOut => write!(f, "Sign-in timed out. Try again."),
            Error::Denied(slug) if slug == "access_denied" => write!(f, "Sign-in was cancelled in Telegram."),
            Error::Denied(slug) => write!(f, "Telegram sign-in failed ({slug}). Try again."),
            Error::Api { message, .. } if !message.is_empty() => write!(f, "{message}"),
            Error::Api { status, .. } => write!(f, "The Hitori server answered {status}"),
            Error::Net(e) => write!(f, "Cannot reach the Hitori server ({e})"),
        }
    }
}

fn unexpected() -> Error {
    Error::Api { status: 200, code: "bad_response".into(), message: "The Hitori server sent an unexpected answer".into() }
}

fn net(e: impl std::fmt::Display) -> Error {
    Error::Net(e.to_string())
}

fn request(method: &str, url: &str, token: Option<&str>) -> ureq::Request {
    let req = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(25))
        .build()
        .request(method, url)
        .set("User-Agent", &format!("Hitori/{} ({})", env!("CARGO_PKG_VERSION"), crate::sub::OS));
    match token {
        Some(t) => req.set("Authorization", &format!("Bearer {t}")),
        None => req,
    }
}

fn send(req: ureq::Request, body: Option<Value>) -> Result<ureq::Response, Error> {
    let got = match body {
        Some(b) => req.send_json(b),
        None => req.call(),
    };
    got.map_err(|e| match e {
        ureq::Error::Status(401, _) => Error::Unauthorized,
        ureq::Error::Status(status, resp) => {
            let v: Value = resp.into_json().unwrap_or_default();
            Error::Api { status, code: v["code"].as_str().unwrap_or_default().into(), message: v["error"].as_str().unwrap_or_default().into() }
        }
        ureq::Error::Transport(t) => Error::Net(t.kind().to_string()),
    })
}

fn parse<T: DeserializeOwned>(resp: ureq::Response) -> Result<T, Error> {
    resp.into_json().map_err(|_| unexpected())
}

/// How this computer shows up in the user's device list.
fn device() -> Value {
    json!({"os": crate::sub::OS, "name": crate::sub::hostname(), "hwid": crate::sub::hwid(), "version": env!("CARGO_PKG_VERSION")})
}

// ---------- PKCE and the loopback redirect ----------

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// 32 random bytes: a 43-character verifier.
    pub fn new() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).expect("system randomness");
        Self::from_verifier(URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn from_verifier(verifier: String) -> Self {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self { verifier, challenge }
    }
}

/// What the browser brought to the loopback port.
#[derive(Debug, PartialEq)]
pub enum Callback {
    Code(String),
    Error(String),
    NotFound,
    Bad,
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The request line `GET /callback?code=…` (or `?error=…`); anything else is not for us.
pub fn parse_callback(request: &str) -> Callback {
    let mut parts = request.lines().next().unwrap_or_default().split(' ');
    let (Some("GET"), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else { return Callback::Bad };
    if !version.starts_with("HTTP/") {
        return Callback::Bad;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/callback" {
        return Callback::NotFound;
    }
    let (mut code, mut error) = (None, None);
    for pair in query.split('&') {
        match pair.split_once('=').unwrap_or((pair, "")) {
            ("code", v) => code = Some(unescape(v)),
            ("error", v) => error = Some(unescape(v)),
            _ => {}
        }
    }
    match (error, code) {
        (Some(e), _) => Callback::Error(if e.is_empty() { "unknown".into() } else { e }),
        (None, Some(c)) if !c.is_empty() => Callback::Code(c),
        _ => Callback::Bad,
    }
}

const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Hitori VPN</title>
<style>body{margin:0;height:100vh;display:grid;place-items:center;font:16px/1.5 system-ui,sans-serif;background:#09090b;color:#fafafa}
main{text-align:center;padding:32px}h1{font-size:20px;margin:0 0 8px}p{margin:0;color:#9f9fa9}</style><main><h1>{title}</h1><p>{text}</p></main>"#;

/// Answer one connection to the loopback port; `None` for anything but the callback (a favicon, a port scan).
fn answer(mut stream: TcpStream) -> Option<Result<String, Error>> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    // Read the whole head: closing with unread bytes would reset the connection before the browser sees the page.
    let (mut buf, mut chunk) = (Vec::new(), [0u8; 1024]);
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 8192 {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let got = parse_callback(&String::from_utf8_lossy(&buf));
    let (status, title, text) = match &got {
        Callback::Code(_) => ("200 OK", "Signed in", "You can return to Hitori."),
        Callback::Error(_) => ("200 OK", "Sign-in failed", "Return to Hitori and try again."),
        Callback::NotFound => ("404 Not Found", "Not found", ""),
        Callback::Bad => ("400 Bad Request", "Bad request", ""),
    };
    let page = PAGE.replace("{title}", title).replace("{text}", text);
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    let _ = stream.flush();
    match got {
        Callback::Code(c) => Some(Ok(c)),
        Callback::Error(e) => Some(Err(Error::Denied(e))),
        _ => None,
    }
}

/// A started sign-in: the browser goes to `auth_url`, the backend sends it back to our port.
pub struct Pending {
    listener: TcpListener,
    pkce: Pkce,
    pub auth_url: String,
    deadline: Instant,
}

/// Listen on a random loopback port and ask the backend for the Telegram page.
pub fn begin(api: &str) -> Result<Pending, Error> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(net)?;
    let port = listener.local_addr().map_err(net)?.port();
    let pkce = Pkce::new();
    let body = json!({
        "redirect_uri": format!("http://127.0.0.1:{port}/callback"),
        "code_challenge": pkce.challenge,
        "code_challenge_method": "S256",
        "device": device(),
    });
    #[derive(Deserialize)]
    struct Started {
        auth_url: String,
        expires_in: Option<u64>,
    }
    let started: Started = parse(send(request("POST", &format!("{api}/app/v1/auth/start"), None), Some(body))?)?;
    // Only a web page goes to the system opener.
    if !started.auth_url.starts_with("https://") {
        return Err(unexpected());
    }
    let ttl = started.expires_in.unwrap_or(600).clamp(30, 600);
    Ok(Pending { listener, pkce, auth_url: started.auth_url, deadline: Instant::now() + Duration::from_secs(ttl) })
}

impl Pending {
    pub fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// The app code from the browser, polling so `cancel` and the login's lifetime are noticed.
    fn wait(&self, cancel: &AtomicBool) -> Result<String, Error> {
        self.listener.set_nonblocking(true).map_err(net)?;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if Instant::now() >= self.deadline {
                return Err(Error::TimedOut);
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(result) = answer(stream) {
                        return result;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(net(e)),
            }
        }
    }

    /// Wait for the browser, then trade its code and our verifier for tokens.
    pub fn finish(self, api: &str, cancel: &AtomicBool) -> Result<Session, Error> {
        let code = self.wait(cancel)?;
        drop(self.listener);
        let tokens: Tokens = parse(send(request("POST", &format!("{api}/app/v1/auth/token"), None), Some(json!({"code": code, "code_verifier": self.pkce.verifier})))?)?;
        Ok(Session::new(api, tokens))
    }
}

// ---------- session ----------

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct User {
    pub name: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub has_avatar: bool,
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    user: User,
}

/// Saved in the config dir (0600), for the backend it came from.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Session {
    pub api: String,
    pub access_token: String,
    pub refresh_token: String,
    pub user: User,
}

impl Session {
    fn new(api: &str, t: Tokens) -> Self {
        Self { api: api.to_owned(), access_token: t.access_token, refresh_token: t.refresh_token, user: t.user }
    }
}

/// The subscription link and what the account sheet shows about it.
#[derive(Clone, PartialEq)]
pub struct Plan {
    pub url: String,
    pub account: Account,
}

#[derive(Clone, PartialEq)]
pub struct Me {
    pub user: User,
    pub plan: Option<Plan>,
}

/// Unix seconds → `4 Feb 2029` (days to civil date, proleptic Gregorian).
fn unix_date(secs: i64) -> String {
    const MONTHS: [&str; 12] = ["Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec", "Jan", "Feb"];
    let z = secs.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let year = yoe + era * 400 + i64::from(mp >= 10);
    format!("{day} {} {year}", MONTHS[mp as usize])
}

/// The subscription of `/me`, also sent live over the socket.
pub fn account(s: &Value, name: String) -> Account {
    let status = s["status"].as_str().unwrap_or_default().to_owned();
    let num = |k: &str| s[k].as_f64().unwrap_or(0.).max(0.);
    Account {
        name,
        title: String::new(),
        active: status.eq_ignore_ascii_case("active"),
        status,
        days_left: s["days_left"].as_f64().map(|d| d.max(0.) as u32),
        expires: s["expires_at"].as_str().and_then(data::date).or_else(|| s["expires_at"].as_i64().map(unix_date)),
        used: num("traffic_used") as u64,
        limit: num("traffic_limit") as u64,
        support: None,
    }
}

fn me_from(v: Value) -> Result<Me, Error> {
    let user: User = serde_json::from_value(v["user"].clone()).map_err(|_| unexpected())?;
    let s = &v["subscription"];
    let plan = match s["url"].as_str() {
        Some(url) => Some(Plan { url: url.to_owned(), account: account(s, user.name.clone()) }),
        None if s.is_null() => None,
        None => return Err(unexpected()),
    };
    Ok(Me { user, plan })
}

/// The signed-in session, shared by every request so a refresh happens once and rotated tokens are never reused.
pub struct Client {
    pub api: String,
    path: PathBuf,
    session: Mutex<Option<Session>>,
}

impl Client {
    pub fn new(api: String, path: PathBuf) -> Self {
        let session = store::load::<Session>(&path).filter(|s| s.api == api);
        Self { api, path, session: Mutex::new(session) }
    }

    pub fn session(&self) -> Option<Session> {
        self.session.lock().unwrap().clone()
    }

    pub fn set(&self, session: Option<Session>) {
        let mut slot = self.session.lock().unwrap();
        self.persist(session.as_ref());
        *slot = session;
    }

    /// Forget the session here; `revoke` it on the server off the main thread.
    pub fn take(&self) -> Option<Session> {
        let mut slot = self.session.lock().unwrap();
        self.persist(None);
        slot.take()
    }

    fn persist(&self, session: Option<&Session>) {
        match session {
            Some(s) => store::save(&self.path, s, true),
            None => {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    /// A request with the access token; after a 401 the tokens are refreshed once and it is tried again.
    fn authed<T>(&self, f: impl Fn(&str) -> Result<T, Error>) -> Result<T, Error> {
        let token = self.session().map(|s| s.access_token).ok_or(Error::Unauthorized)?;
        match f(&token) {
            Err(Error::Unauthorized) => {}
            other => return other,
        }
        let fresh = self.refresh(&token)?;
        let again = f(&fresh);
        if matches!(again, Err(Error::Unauthorized)) {
            self.set(None);
        }
        again
    }

    /// New tokens, unless another request already replaced `stale`. A refused refresh signs out; a network error doesn't.
    pub fn refresh(&self, stale: &str) -> Result<String, Error> {
        let mut slot = self.session.lock().unwrap();
        let Some(s) = slot.clone() else { return Err(Error::Unauthorized) };
        if s.access_token != stale {
            return Ok(s.access_token);
        }
        let got = send(request("POST", &format!("{}/app/v1/auth/refresh", self.api), None), Some(json!({"refresh_token": s.refresh_token}))).and_then(parse::<Tokens>);
        match got {
            Ok(t) => {
                let s = Session::new(&self.api, t);
                self.persist(Some(&s));
                let token = s.access_token.clone();
                *slot = Some(s);
                Ok(token)
            }
            Err(Error::Unauthorized | Error::Api { status: 400..=499, .. }) => {
                self.persist(None);
                *slot = None;
                Err(Error::Unauthorized)
            }
            Err(e) => Err(e),
        }
    }

    /// Profile and subscription; the saved user follows the server's.
    pub fn me(&self) -> Result<Me, Error> {
        let me = self.authed(|t| me_from(parse(send(request("GET", &format!("{}/app/v1/me", self.api), Some(t)), None)?)?))?;
        let mut slot = self.session.lock().unwrap();
        if let Some(s) = slot.as_mut().filter(|s| s.user != me.user) {
            s.user = me.user.clone();
            self.persist(Some(s));
        }
        Ok(me)
    }

    /// The profile picture (at most 1 MiB), or `None` when there is none.
    pub fn avatar(&self) -> Result<Option<Vec<u8>>, Error> {
        self.authed(|t| match send(request("GET", &format!("{}/app/v1/me/avatar", self.api), Some(t)), None) {
            Ok(resp) => {
                let mut bytes = vec![];
                resp.into_reader().take((1 << 20) + 1).read_to_end(&mut bytes).map_err(net)?;
                if bytes.len() > 1 << 20 { Err(unexpected()) } else { Ok(Some(bytes)) }
            }
            Err(Error::Api { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        })
    }
}

/// End a session on the server (best effort), refreshing first if its access token has expired.
pub fn revoke(s: &Session) {
    let logout = |t: &str| send(request("POST", &format!("{}/app/v1/auth/logout", s.api), Some(t)), None);
    if let Err(Error::Unauthorized) = logout(&s.access_token)
        && let Ok(t) = send(request("POST", &format!("{}/app/v1/auth/refresh", s.api), None), Some(json!({"refresh_token": s.refresh_token}))).and_then(parse::<Tokens>)
    {
        let _ = logout(&t.access_token);
    }
}

// ---------- avatar ----------

/// Decode a profile picture into a 128 px square (centre crop), ready to draw.
pub fn picture(bytes: &[u8]) -> Option<Arc<RenderImage>> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let side = img.width().min(img.height());
    if side == 0 {
        return None;
    }
    let square = img.crop_imm((img.width() - side) / 2, (img.height() - side) / 2, side, side);
    let mut rgba = square.resize_exact(128, 128, image::imageops::FilterType::Lanczos3).into_rgba8();
    // GPUI draws BGRA.
    for p in rgba.pixels_mut() {
        p.0.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
}

pub fn avatar_path() -> PathBuf {
    store::cache_dir().join("avatar")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn pkce_matches_rfc7636() {
        let p = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into());
        assert_eq!(p.challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let a = Pkce::new();
        assert_eq!(a.verifier.len(), 43);
        assert!(a.verifier.bytes().all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c)));
        assert_eq!(a.challenge, Pkce::from_verifier(a.verifier.clone()).challenge);
        assert_ne!(a.verifier, Pkce::new().verifier);
    }

    #[test]
    fn loopback_requests() {
        let req = |line: &str| parse_callback(&format!("{line}\r\nHost: 127.0.0.1\r\n\r\n"));
        assert_eq!(req("GET /callback?code=a%2Bb+c%3D HTTP/1.1"), Callback::Code("a+b c=".into()));
        assert_eq!(req("GET /callback?state=x&code=abc HTTP/1.1"), Callback::Code("abc".into()));
        assert_eq!(req("GET /callback?error=access_denied HTTP/1.1"), Callback::Error("access_denied".into()));
        assert_eq!(req("GET /callback?code=abc&error=expired HTTP/1.1"), Callback::Error("expired".into()));
        assert_eq!(req("GET /callback?error= HTTP/1.1"), Callback::Error("unknown".into()));
        assert_eq!(req("GET /callback HTTP/1.1"), Callback::Bad);
        assert_eq!(req("GET /callback?code= HTTP/1.1"), Callback::Bad);
        assert_eq!(req("GET /favicon.ico HTTP/1.1"), Callback::NotFound);
        assert_eq!(req("GET /callback/x?code=abc HTTP/1.1"), Callback::NotFound);
        assert_eq!(req("POST /callback?code=abc HTTP/1.1"), Callback::Bad);
        assert_eq!(req("GET /callback?code=abc"), Callback::Bad);
        assert_eq!(parse_callback(""), Callback::Bad);
        assert_eq!(parse_callback("\u{16}\u{3}\u{1}\u{0}garbage"), Callback::Bad);
        assert_eq!(unescape("%zz%4"), "%zz%4");
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hitori-auth-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("session.json")
    }

    fn session(api: &str, access: &str, refresh: &str) -> Session {
        Session { api: api.into(), access_token: access.into(), refresh_token: refresh.into(), user: User { name: "Ann".into(), username: Some("ann".into()), has_avatar: true } }
    }

    #[test]
    fn tokens_persist_privately() {
        let path = temp("persist");
        let s = session("https://api.example", "a1", "r1");
        Client::new("https://api.example".into(), path.clone()).set(Some(s.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert_eq!(Client::new("https://api.example".into(), path.clone()).session(), Some(s.clone()));
        // Tokens from one backend are never sent to another.
        assert_eq!(Client::new("https://other.example".into(), path.clone()).session(), None);
        assert_eq!(Client::new("https://api.example".into(), path.clone()).take(), Some(s));
        assert!(!path.exists());
        assert_eq!(Client::new("https://api.example".into(), path).session(), None);
    }

    #[test]
    fn expiry_as_unix_seconds() {
        assert_eq!(unix_date(1864906214), "4 Feb 2029");
        assert_eq!(unix_date(0), "1 Jan 1970");
        assert_eq!(unix_date(951782400), "29 Feb 2000");
        let me = me_from(json!({"user": {"name": "A"}, "subscription": {"url": "https://s/x", "status": "active", "expires_at": 1864906214}})).unwrap();
        assert_eq!(me.plan.unwrap().account.expires.as_deref(), Some("4 Feb 2029"));
        assert!(me_from(json!({"user": {"name": "A"}, "subscription": null})).unwrap().plan.is_none());
    }

    #[test]
    fn decodes_avatars() {
        let mut png = vec![];
        image::RgbaImage::from_pixel(300, 200, image::Rgba([255, 0, 0, 255])).write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let pic = picture(&png).unwrap();
        assert_eq!(pic.size(0), gpui::size(gpui::DevicePixels(128), gpui::DevicePixels(128)));
        // Red, stored as BGRA.
        assert_eq!(&pic.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);
        assert!(picture(b"<html>not an image</html>").is_none());
    }

    // ---------- fake backend ----------

    struct State {
        challenge: Option<String>,
        redirect: Option<String>,
        access: String,
        refresh: String,
        refreshes: u32,
        log: Vec<String>,
    }

    fn reply(stream: &mut TcpStream, status: u16, kind: &str, body: &[u8]) {
        let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let _ = stream.write_all(body);
    }

    fn serve(stream: &mut TcpStream, st: &Mutex<State>) {
        let mut buf = vec![];
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let headers: HashMap<String, String> = head.lines().skip(1).filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_owned())).collect();
        let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        while buf.len() < head_end + len {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body: Value = serde_json::from_slice(&buf[head_end..]).unwrap_or_default();
        let mut words = head.split(' ');
        let route = format!("{} {}", words.next().unwrap_or_default(), words.next().unwrap_or_default());
        let mut s = st.lock().unwrap();
        s.log.push(route.clone());
        let bearer = headers.get("authorization").and_then(|v| v.strip_prefix("Bearer ")).map(str::to_owned);
        let unauthorized = br#"{"error":"Unauthorized","code":"unauthorized"}"#;
        let user = json!({"telegram_id": 42, "name": "Ann Example", "username": "ann", "has_avatar": true});
        let tokens = |s: &State| json!({"access_token": s.access, "refresh_token": s.refresh, "expires_in": 900, "user": user}).to_string();
        match route.as_str() {
            "POST /app/v1/auth/start" => {
                let redirect = body["redirect_uri"].as_str().unwrap_or_default();
                let port_ok = redirect.strip_prefix("http://127.0.0.1:").and_then(|r| r.strip_suffix("/callback")).is_some_and(|p| p.parse::<u16>().is_ok());
                assert!(port_ok && body["code_challenge_method"] == "S256" && body["device"]["hwid"].is_string(), "{body}");
                s.challenge = body["code_challenge"].as_str().map(str::to_owned);
                s.redirect = Some(redirect.to_owned());
                reply(stream, 200, "application/json", br#"{"auth_url":"https://oauth.telegram.org/auth?state=s1","expires_in":600}"#);
            }
            "POST /app/v1/auth/token" => {
                let verifier = body["code_verifier"].as_str().unwrap_or_default().to_owned();
                if body["code"] == "app-code" && s.challenge.as_deref() == Some(Pkce::from_verifier(verifier).challenge.as_str()) {
                    reply(stream, 200, "application/json", tokens(&s).as_bytes());
                } else {
                    reply(stream, 400, "application/json", br#"{"error":"Invalid code","code":"invalid_grant"}"#);
                }
            }
            "POST /app/v1/auth/refresh" => {
                if body["refresh_token"] == s.refresh.as_str() {
                    s.refreshes += 1;
                    (s.access, s.refresh) = (format!("a{}", s.refreshes + 1), format!("r{}", s.refreshes + 1));
                    reply(stream, 200, "application/json", tokens(&s).as_bytes());
                } else {
                    reply(stream, 401, "application/json", unauthorized);
                }
            }
            _ if bearer.as_deref() != Some(s.access.as_str()) => reply(stream, 401, "application/json", unauthorized),
            "GET /app/v1/me" => {
                let me = json!({"user": user, "subscription": {"url": "https://sub.example/sub/AbC", "status": "ACTIVE", "expires_at": "2029-02-04T13:30:14Z",
                                "days_left": 12, "traffic_used": 1024, "traffic_limit": null}});
                reply(stream, 200, "application/json", me.to_string().as_bytes());
            }
            "GET /app/v1/me/avatar" => {
                let mut png = vec![];
                image::RgbaImage::from_pixel(64, 64, image::Rgba([0, 128, 255, 255])).write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
                reply(stream, 200, "image/png", &png);
            }
            "POST /app/v1/auth/logout" => reply(stream, 204, "application/json", b""),
            _ => reply(stream, 404, "application/json", br#"{"error":"Not found","code":"not_found"}"#),
        }
    }

    fn fake() -> (String, Arc<Mutex<State>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let st = Arc::new(Mutex::new(State { challenge: None, redirect: None, access: "a1".into(), refresh: "r1".into(), refreshes: 0, log: vec![] }));
        let shared = st.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                serve(&mut stream, &shared);
            }
        });
        (api, st)
    }

    /// What a browser sends to the loopback port; returns the response.
    fn browse(port: u16, path: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/html\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn signs_in_against_a_fake_backend() {
        let (api, st) = fake();
        let pending = begin(&api).unwrap();
        assert_eq!(pending.auth_url, "https://oauth.telegram.org/auth?state=s1");
        let port = pending.port();
        assert_eq!(st.lock().unwrap().redirect.as_deref(), Some(format!("http://127.0.0.1:{port}/callback").as_str()));
        let browser = std::thread::spawn(move || {
            let favicon = browse(port, "/favicon.ico");
            let junk = {
                let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
                s.write_all(b"\x16\x03\x01garbage\r\n\r\n").unwrap();
                let mut out = String::new();
                s.read_to_string(&mut out).unwrap();
                out
            };
            (favicon, junk, browse(port, "/callback?code=app-code"))
        });
        let session = pending.finish(&api, &AtomicBool::new(false)).unwrap();
        let (favicon, junk, page) = browser.join().unwrap();
        assert!(favicon.starts_with("HTTP/1.1 404"), "{favicon}");
        assert!(junk.starts_with("HTTP/1.1 400"), "{junk}");
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("Signed in"), "{page}");
        assert_eq!((session.access_token.as_str(), session.user.name.as_str()), ("a1", "Ann Example"));

        let client = Client::new(api.clone(), temp("signin"));
        client.set(Some(session));
        let me = client.me().unwrap();
        let plan = me.plan.unwrap();
        assert_eq!(plan.url, "https://sub.example/sub/AbC");
        assert_eq!((plan.account.active, plan.account.days_left, plan.account.limit), (true, Some(12), 0));
        assert_eq!(plan.account.expires.as_deref(), Some("4 Feb 2029"));
        let pic = client.avatar().unwrap().unwrap();
        assert!(picture(&pic).is_some());
        let s = client.take().unwrap();
        revoke(&s);
        assert_eq!(st.lock().unwrap().log.last().map(String::as_str), Some("POST /app/v1/auth/logout"));
    }

    #[test]
    fn sign_in_errors_and_cancel() {
        let (api, _) = fake();
        let pending = begin(&api).unwrap();
        let port = pending.port();
        let browser = std::thread::spawn(move || browse(port, "/callback?error=access_denied"));
        assert_eq!(pending.finish(&api, &AtomicBool::new(false)).unwrap_err(), Error::Denied("access_denied".into()));
        assert!(browser.join().unwrap().contains("Sign-in failed"));

        let cancel = Arc::new(AtomicBool::new(false));
        let pending = begin(&api).unwrap();
        let flag = cancel.clone();
        let waiting = std::thread::spawn(move || pending.finish(&api, &flag));
        std::thread::sleep(Duration::from_millis(100));
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(waiting.join().unwrap().unwrap_err(), Error::Cancelled);
    }

    #[test]
    fn refreshes_once_on_401() {
        let (api, st) = fake();
        let path = temp("refresh");
        let client = Client::new(api.clone(), path.clone());
        // An expired access token with a live refresh token: one refresh, then the request goes through.
        client.set(Some(session(&api, "stale", "r1")));
        assert_eq!(client.me().unwrap().user.name, "Ann Example");
        assert_eq!(st.lock().unwrap().refreshes, 1);
        let saved = Client::new(api.clone(), path.clone()).session().unwrap();
        assert_eq!((saved.access_token.as_str(), saved.refresh_token.as_str()), ("a2", "r2"));
        assert_eq!(saved.user.name, "Ann Example");
        client.avatar().unwrap().unwrap();
        assert_eq!(st.lock().unwrap().refreshes, 1);

        // A rotated refresh token is refused: signed out, nothing left on disk.
        client.set(Some(session(&api, "stale", "r1")));
        assert!(matches!(client.me(), Err(Error::Unauthorized)));
        assert_eq!(client.session(), None);
        assert!(!path.exists());
        assert!(matches!(client.me(), Err(Error::Unauthorized)));
    }
}
