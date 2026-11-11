//! Live updates from the backend (`/app/v1/ws`): the server list's fingerprint and the subscription as they change.
//! One socket on its own thread while signed in with Telegram; it reconnects with backoff and refreshes tokens once.
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{HandshakeError, Message, WebSocket};

use crate::auth;
use crate::data::Account;

const CONNECT: Duration = Duration::from_secs(10);
const HANDSHAKE: Duration = Duration::from_secs(15);
/// How often an idle socket looks for a kick.
const TICK: Duration = Duration::from_millis(250);
/// The server pings every 30 s; this long without a frame, the socket is dead (a sleeping laptop, a dropped route).
const SILENCE: Duration = Duration::from_secs(75);
/// Up this long, a connection resets the backoff.
const STABLE: Duration = Duration::from_secs(60);
/// After a network or tunnel change; further changes meanwhile push it back.
const KICK: Duration = Duration::from_millis(500);

/// A server message; unknown types are `Other`.
pub enum Msg {
    Hello,
    Servers(String),
    Subscription(Account),
    Other,
}

/// `None` for anything that is not a typed JSON object.
pub fn parse(text: &str) -> Option<Msg> {
    let v: Value = serde_json::from_str(text).ok()?;
    Some(match v["type"].as_str()? {
        "hello" => Msg::Hello,
        "servers" => Msg::Servers(v["rev"].as_str()?.to_owned()),
        // No status: nothing to show (no subscription, or the panel did not answer).
        "subscription" if v["status"].is_string() => Msg::Subscription(auth::account(&v, String::new())),
        _ => Msg::Other,
    })
}

/// Why a socket ended or never opened.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum End {
    /// HTTP 401 on the upgrade, or close 4001.
    Unauthorized,
    /// Close 4002: a newer socket of this session took over.
    Replaced,
    /// Any other close code, a network error, silence.
    Lost,
    /// The network or the tunnel changed.
    Kicked,
}

pub fn closed(code: Option<u16>) -> End {
    match code {
        Some(4001) => End::Unauthorized,
        Some(4002) => End::Replaced,
        _ => End::Lost,
    }
}

#[derive(Debug, PartialEq)]
pub enum Next {
    Refresh,
    Wait(Duration),
    Stop,
    SignOut,
}

/// Reconnect policy: exponential backoff 1–60 s with jitter, one token refresh per successful connection.
#[derive(Default)]
pub struct Backoff {
    failures: u32,
    refreshed: bool,
}

impl Backoff {
    /// `up`: how long the socket was open (`None`: it never opened); `jitter` in 0..1.
    pub fn ended(&mut self, end: End, up: Option<Duration>, jitter: f64) -> Next {
        if let Some(up) = up {
            self.refreshed = false;
            if up >= STABLE {
                self.failures = 0;
            }
        }
        match end {
            End::Replaced => Next::Stop,
            End::Unauthorized if self.refreshed => Next::SignOut,
            End::Unauthorized => Next::Refresh,
            End::Kicked => {
                self.failures = 0;
                Next::Wait(KICK)
            }
            End::Lost => self.wait(jitter),
        }
    }

    /// A refused refresh signs out; a network error is retried like a lost socket.
    pub fn refreshed(&mut self, got: Result<(), auth::Error>, jitter: f64) -> Next {
        match got {
            Ok(()) => {
                self.refreshed = true;
                Next::Wait(Duration::ZERO)
            }
            Err(auth::Error::Unauthorized) => Next::SignOut,
            Err(_) => self.wait(jitter),
        }
    }

    fn wait(&mut self, jitter: f64) -> Next {
        let base = (1u64 << self.failures.min(6)).min(60) as f64;
        self.failures += 1;
        Next::Wait(Duration::from_secs_f64((base * (0.5 + jitter)).clamp(1., 60.)))
    }
}

/// What the model hears.
pub enum Event {
    Servers(String),
    Subscription(Account),
    /// The session ended for good; the tokens are already gone.
    SignedOut,
}

/// The running socket thread; dropping it closes the socket.
pub struct Live {
    kick: mpsc::Sender<()>,
}

impl Live {
    pub fn start(client: Arc<auth::Client>) -> (Self, async_channel::Receiver<Event>) {
        let (kick, kicks) = mpsc::channel();
        let (tx, rx) = async_channel::unbounded();
        std::thread::spawn(move || run(&client, &kicks, &tx));
        (Self { kick }, rx)
    }

    /// The network or the tunnel changed: reconnect soon, whatever the backoff.
    pub fn kick(&self) {
        let _ = self.kick.send(());
    }
}

fn run(client: &auth::Client, kicks: &mpsc::Receiver<()>, events: &async_channel::Sender<Event>) {
    let mut backoff = Backoff::default();
    let mut wait = Duration::ZERO;
    loop {
        let mut until = Instant::now() + wait;
        loop {
            match kicks.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(()) => {
                    backoff.ended(End::Kicked, None, 0.);
                    until = Instant::now() + KICK;
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        let Some(token) = client.session().map(|s| s.access_token) else { return };
        let (end, up) = match open(&client.api, &token) {
            Ok(mut ws) => {
                let opened = Instant::now();
                let Some(end) = serve(&mut ws, kicks, events) else { return };
                (end, Some(opened.elapsed()))
            }
            Err(end) => (end, None),
        };
        let mut next = backoff.ended(end, up, jitter());
        if next == Next::Refresh {
            next = backoff.refreshed(client.refresh(&token).map(drop), jitter());
        }
        match next {
            Next::Wait(d) => wait = d,
            Next::Stop => return,
            _ => {
                // Not when the model already let go of this socket (signed out, maybe in again).
                if !matches!(kicks.try_recv(), Err(TryRecvError::Disconnected)) {
                    client.set(None);
                    let _ = events.send_blocking(Event::SignedOut);
                }
                return;
            }
        }
    }
}

fn jitter() -> f64 {
    let mut b = [0u8; 4];
    let _ = getrandom::getrandom(&mut b);
    u32::from_le_bytes(b) as f64 / (u32::MAX as f64 + 1.)
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

fn lost<E>(_: E) -> End {
    End::Lost
}

/// The upgrade with the access token; the socket is then non-blocking.
fn open(api: &str, token: &str) -> Result<Socket, End> {
    let mut req = format!("{}/app/v1/ws", api.replacen("http", "ws", 1)).into_client_request().map_err(lost)?;
    let headers = req.headers_mut();
    headers.insert("Authorization", format!("Bearer {token}").parse().map_err(lost)?);
    headers.insert("User-Agent", format!("Hitori/{} ({})", env!("CARGO_PKG_VERSION"), crate::sub::OS).parse().map_err(lost)?);
    let uri = req.uri();
    let host = uri.host().ok_or(End::Lost)?.trim_matches(['[', ']']).to_owned();
    let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("wss") { 443 } else { 80 });
    let tcp = (host.as_str(), port).to_socket_addrs().map_err(lost)?.find_map(|a| TcpStream::connect_timeout(&a, CONNECT).ok()).ok_or(End::Lost)?;
    tcp.set_read_timeout(Some(HANDSHAKE)).map_err(lost)?;
    tcp.set_write_timeout(Some(HANDSHAKE)).map_err(lost)?;
    let _ = tcp.set_nodelay(true);
    let sock = tcp.try_clone().map_err(lost)?;
    let config = WebSocketConfig::default().max_message_size(Some(64 << 10)).max_frame_size(Some(64 << 10));
    let (ws, _) = tungstenite::client_tls_with_config(req, tcp, Some(config), None).map_err(|e| match e {
        HandshakeError::Failure(tungstenite::Error::Http(r)) if r.status() == 401 => End::Unauthorized,
        _ => End::Lost,
    })?;
    // Non-blocking rather than a read timeout: after a timed-out receive Windows leaves a socket undefined.
    sock.set_nonblocking(true).map_err(lost)?;
    Ok(ws)
}

/// Read until the socket ends; `None` once nobody listens any more.
fn serve(ws: &mut Socket, kicks: &mpsc::Receiver<()>, events: &async_channel::Sender<Event>) -> Option<End> {
    let mut heard = Instant::now();
    let bye = |ws: &mut Socket| {
        let _ = ws.close(None);
        let _ = ws.flush();
    };
    loop {
        let event = match ws.read() {
            Ok(msg) => {
                heard = Instant::now();
                match msg {
                    Message::Text(text) => match parse(&text) {
                        Some(Msg::Servers(rev)) => Event::Servers(rev),
                        Some(Msg::Subscription(a)) => Event::Subscription(a),
                        _ => continue,
                    },
                    Message::Close(frame) => {
                        let _ = ws.flush();
                        return Some(closed(frame.map(|f| u16::from(f.code))));
                    }
                    // Pings are answered by tungstenite.
                    _ => continue,
                }
            }
            Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                if heard.elapsed() > SILENCE {
                    return Some(End::Lost);
                }
                match kicks.recv_timeout(TICK) {
                    Ok(()) => {
                        bye(ws);
                        return Some(End::Kicked);
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        bye(ws);
                        return None;
                    }
                    Err(RecvTimeoutError::Timeout) => continue,
                }
            }
            Err(_) => return Some(End::Lost),
        };
        if events.send_blocking(event).is_err() {
            bye(ws);
            return None;
        }
    }
}

/// Which server list is loaded: `servers` revs against the last fetch, so only a real change refetches.
#[derive(Default)]
pub struct Revs {
    seen: Option<String>,
    fetching: Option<String>,
    loaded: Option<String>,
}

impl Revs {
    /// A `servers` message: refetch now? Not while a fetch runs; `loaded` catches up after it.
    pub fn seen(&mut self, rev: String, busy: bool) -> bool {
        self.seen = Some(rev);
        !busy && self.stale()
    }

    /// A subscription fetch starts.
    pub fn fetching(&mut self) {
        self.fetching = self.seen.clone();
    }

    /// The fetch loaded; refetch because the list changed again meanwhile?
    pub fn loaded(&mut self) -> bool {
        self.loaded = self.fetching.take();
        self.stale()
    }

    fn stale(&self) -> bool {
        self.seen.is_some() && self.seen != self.loaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    use serde_json::json;
    use tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tungstenite::protocol::CloseFrame;

    #[test]
    fn parses_messages() {
        assert!(matches!(parse(r#"{"type":"hello","v":1}"#), Some(Msg::Hello)));
        assert!(matches!(parse(r#"{"type":"servers","rev":"abc"}"#), Some(Msg::Servers(r)) if r == "abc"));
        let sub = r#"{"type":"subscription","status":"ACTIVE","expires_at":"2029-02-04T13:30:14Z","days_left":12,"traffic_used":1024,"traffic_limit":null}"#;
        let Some(Msg::Subscription(a)) = parse(sub) else { panic!("subscription") };
        assert_eq!((a.active, a.days_left, a.used, a.limit, a.expires.as_deref()), (true, Some(12), 1024, 0, Some("4 Feb 2029")));
        let Some(Msg::Subscription(a)) = parse(r#"{"type":"subscription","status":"EXPIRED","days_left":null}"#) else { panic!("expired") };
        assert_eq!((a.active, a.days_left, a.plan()), (false, None, "Expired".to_owned()));
        // Forward compatible: unknown types and fields, a subscription without status.
        assert!(matches!(parse(r#"{"type":"pong"}"#), Some(Msg::Other)));
        assert!(matches!(parse(r#"{"type":"news","x":[1]}"#), Some(Msg::Other)));
        assert!(matches!(parse(r#"{"type":"subscription","status":null}"#), Some(Msg::Other)));
        assert!(matches!(parse(r#"{"type":"servers","rev":"r","extra":true}"#), Some(Msg::Servers(_))));
        assert!(parse(r#"{"type":"servers"}"#).is_none());
        assert!(parse(r#"{"rev":"r"}"#).is_none());
        assert!(parse("not json").is_none());
        assert!(parse("[1,2]").is_none());
    }

    fn secs(n: Next) -> f64 {
        match n {
            Next::Wait(d) => d.as_secs_f64(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn backoff_and_close_codes() {
        assert_eq!(
            [4001, 4002, 1001, 1011, 1012, 1013, 1000].map(|c| closed(Some(c))),
            [End::Unauthorized, End::Replaced, End::Lost, End::Lost, End::Lost, End::Lost, End::Lost]
        );
        assert_eq!(closed(None), End::Lost);

        let mut b = Backoff::default();
        // Doubling from 1 s up to 60 s; jitter spreads ±50 % within those bounds.
        let mid: Vec<f64> = (0..9).map(|_| secs(b.ended(End::Lost, None, 0.5))).collect();
        assert_eq!(mid, [1., 2., 4., 8., 16., 32., 60., 60., 60.]);
        assert_eq!(secs(b.ended(End::Lost, None, 0.)), 30.);
        assert_eq!(secs(b.ended(End::Lost, None, 0.999)), 60.);
        let mut low = Backoff::default();
        assert_eq!(secs(low.ended(End::Lost, None, 0.)), 1.);
        // A short-lived connection keeps backing off; one up for 60 s starts over.
        assert_eq!(secs(b.ended(End::Lost, Some(Duration::from_secs(5)), 0.5)), 60.);
        assert_eq!(secs(b.ended(End::Lost, Some(STABLE), 0.5)), 1.);
        assert_eq!(secs(b.ended(End::Lost, None, 0.5)), 2.);
        // A kick reconnects at once and resets the backoff.
        assert_eq!(b.ended(End::Kicked, None, 0.5), Next::Wait(KICK));
        assert_eq!(secs(b.ended(End::Lost, None, 0.5)), 1.);

        assert_eq!(b.ended(End::Replaced, Some(STABLE), 0.5), Next::Stop);
        assert_eq!(b.ended(End::Replaced, None, 0.5), Next::Stop);
    }

    #[test]
    fn refreshes_once_then_signs_out() {
        let mut b = Backoff::default();
        // 4001 on an open socket: refresh, reconnect at once.
        assert_eq!(b.ended(End::Unauthorized, Some(Duration::from_secs(900)), 0.5), Next::Refresh);
        assert_eq!(b.refreshed(Ok(()), 0.5), Next::Wait(Duration::ZERO));
        // Still refused with the fresh token: signed out.
        assert_eq!(b.ended(End::Unauthorized, None, 0.5), Next::SignOut);

        // Once the fresh token opened a socket, a later expiry refreshes again.
        let mut b = Backoff::default();
        assert_eq!(b.ended(End::Unauthorized, None, 0.5), Next::Refresh);
        b.refreshed(Ok(()), 0.5);
        assert_eq!(b.ended(End::Unauthorized, Some(Duration::from_secs(1)), 0.5), Next::Refresh);

        // A refused refresh signs out; an unreachable server backs off and refreshes next time.
        assert_eq!(Backoff::default().refreshed(Err(auth::Error::Unauthorized), 0.5), Next::SignOut);
        let mut b = Backoff::default();
        assert_eq!(b.refreshed(Err(auth::Error::Net("timed out".into())), 0.5), Next::Wait(Duration::from_secs(1)));
        assert_eq!(b.ended(End::Unauthorized, None, 0.5), Next::Refresh);
    }

    #[test]
    fn refetches_only_on_a_new_rev() {
        let mut r = Revs::default();
        assert!(!r.loaded(), "a pasted link has no rev");
        assert!(r.seen("a".into(), false));
        r.fetching();
        assert!(!r.seen("a".into(), true), "a fetch already runs");
        assert!(!r.loaded());
        assert!(!r.seen("a".into(), false), "a reconnect repeats the rev");
        // Changed during a fetch: one more fetch once it loaded.
        r.fetching();
        assert!(!r.seen("b".into(), true));
        assert!(r.loaded());
        r.fetching();
        assert!(!r.loaded());
        assert!(r.seen("c".into(), false));
    }

    // ---------- a local backend ----------

    enum Act {
        Send(Value),
        Close(u16),
        /// The access token expires on the server.
        Expire,
        /// The connection breaks without a close frame.
        Drop,
    }

    struct Backend {
        access: String,
        refresh: String,
        refreshes: u32,
        upgrades: Vec<String>,
        scripts: VecDeque<Vec<Act>>,
    }

    fn head(stream: &TcpStream) -> String {
        let mut buf = [0u8; 4096];
        for _ in 0..200 {
            let n = stream.peek(&mut buf).unwrap_or(0);
            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                return String::from_utf8_lossy(&buf[..n]).into_owned();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        String::new()
    }

    fn refresh(mut stream: TcpStream, st: &Mutex<Backend>) {
        let mut buf = vec![0u8; 8192];
        let mut got = 0;
        let body = loop {
            got += stream.read(&mut buf[got..]).unwrap();
            let text = String::from_utf8_lossy(&buf[..got]).into_owned();
            if let Some((h, b)) = text.split_once("\r\n\r\n") {
                let len: usize = h.lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
                if b.len() >= len {
                    break serde_json::from_str::<Value>(b).unwrap();
                }
            }
        };
        let mut s = st.lock().unwrap();
        let (status, reply) = if body["refresh_token"] == s.refresh.as_str() {
            s.refreshes += 1;
            (s.access, s.refresh) = (format!("a{}", s.refreshes + 1), format!("r{}", s.refreshes + 1));
            (200, json!({"access_token": s.access, "refresh_token": s.refresh, "expires_in": 900, "user": {"name": "Ann"}}))
        } else {
            (401, json!({"error": "Unauthorized", "code": "unauthorized"}))
        };
        let reply = reply.to_string();
        let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
    }

    fn socket(stream: TcpStream, st: &Arc<Mutex<Backend>>) {
        let check = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
            let bearer = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
            let mut s = st.lock().unwrap();
            s.upgrades.push(bearer.clone());
            if bearer == format!("Bearer {}", s.access) {
                Ok(resp)
            } else {
                Err(tungstenite::http::Response::builder().status(401).body(Some(r#"{"error":"Unauthorized","code":"unauthorized"}"#.into())).unwrap())
            }
        };
        let Ok(mut ws) = tungstenite::accept_hdr(stream, check) else { return };
        let script = st.lock().unwrap().scripts.pop_front().unwrap_or_default();
        for act in script {
            match act {
                Act::Send(v) => ws.send(Message::text(v.to_string())).unwrap(),
                Act::Expire => st.lock().unwrap().access = "expired".into(),
                Act::Close(code) => {
                    let _ = ws.close(Some(CloseFrame { code: code.into(), reason: "".into() }));
                    while ws.read().is_ok() {}
                    return;
                }
                Act::Drop => return,
            }
        }
        while ws.read().is_ok() {}
    }

    fn backend(scripts: Vec<Vec<Act>>, access: &str, refresh_token: &str) -> (String, Arc<Mutex<Backend>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let st =
            Arc::new(Mutex::new(Backend { access: access.into(), refresh: refresh_token.into(), refreshes: 0, upgrades: vec![], scripts: scripts.into() }));
        let shared = st.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let st = shared.clone();
                std::thread::spawn(move || {
                    let head = head(&stream);
                    if head.starts_with("GET /app/v1/ws ") {
                        socket(stream, &st);
                    } else if head.starts_with("POST /app/v1/auth/refresh ") {
                        refresh(stream, &st);
                    }
                });
            }
        });
        (api, st)
    }

    fn client(api: &str, name: &str, access: &str, refresh: &str) -> Arc<auth::Client> {
        let dir = std::env::temp_dir().join(format!("hitori-live-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let client = auth::Client::new(api.into(), dir.join("session.json"));
        client.set(Some(auth::Session { api: api.into(), access_token: access.into(), refresh_token: refresh.into(), user: auth::User::default() }));
        Arc::new(client)
    }

    fn next(rx: &async_channel::Receiver<Event>) -> Option<Event> {
        let (tx, got) = mpsc::channel();
        let rx = rx.clone();
        std::thread::spawn(move || tx.send(rx.recv_blocking().ok()));
        got.recv_timeout(Duration::from_secs(10)).expect("an event within 10 s")
    }

    fn servers(rev: &str) -> Act {
        Act::Send(json!({"type": "servers", "rev": rev}))
    }

    #[test]
    fn follows_a_local_backend() {
        let hello = || Act::Send(json!({"type": "hello", "v": 1}));
        let sub = json!({"type": "subscription", "status": "ACTIVE", "expires_at": null, "days_left": 9, "traffic_used": 5, "traffic_limit": 100});
        let scripts = vec![
            vec![hello(), servers("r1"), Act::Send(sub), Act::Send(json!({"type": "later", "x": 1})), servers("r1"), Act::Close(1012)],
            vec![hello(), servers("r1"), servers("r2"), Act::Drop],
            vec![hello(), servers("r2"), Act::Expire, Act::Close(4001)],
            vec![hello(), servers("r3"), Act::Close(4002)],
        ];
        let (api, st) = backend(scripts, "a1", "r1");
        let client = client(&api, "flow", "a1", "r1");
        let (live, rx) = Live::start(client.clone());
        let mut revs = Revs::default();
        let mut refetched = vec![];
        let mut account = None;
        while let Some(ev) = next(&rx) {
            match ev {
                Event::Servers(rev) => {
                    if revs.seen(rev.clone(), false) {
                        refetched.push(rev);
                        revs.fetching();
                        assert!(!revs.loaded());
                    }
                }
                Event::Subscription(a) => account = Some(a),
                Event::SignedOut => panic!("signed out"),
            }
        }
        // 4002 ended the thread: its channel closed and nothing reconnects.
        std::thread::sleep(Duration::from_millis(1500));
        drop(live);
        let a = account.unwrap();
        assert_eq!((a.active, a.days_left, a.used, a.limit), (true, Some(9), 5, 100));
        assert_eq!(refetched, ["r1", "r2", "r3"]);
        let s = st.lock().unwrap();
        // Reconnected after a 1012 close and after a dropped connection; after 4001 one refresh, then the new token.
        assert_eq!(s.upgrades, ["Bearer a1", "Bearer a1", "Bearer a1", "Bearer a2"]);
        assert_eq!(s.refreshes, 1);
        assert_eq!(client.session().unwrap().refresh_token, "r2");
    }

    #[test]
    fn signs_out_when_the_refresh_is_refused() {
        let (api, st) = backend(vec![], "a1", "r1");
        let client = client(&api, "refused", "stale", "rotated");
        let (_live, rx) = Live::start(client.clone());
        assert!(matches!(next(&rx), Some(Event::SignedOut)));
        assert!(next(&rx).is_none());
        assert!(client.session().is_none());
        assert_eq!(st.lock().unwrap().upgrades, ["Bearer stale"]);
    }

    #[test]
    #[ignore = "network: the production backend, once /app/v1/ws is deployed"]
    fn tls_upgrade_reaches_the_backend() {
        assert_eq!(open(&auth::api(), "not-a-token").err(), Some(End::Unauthorized));
    }

    #[test]
    fn kicks_and_stops() {
        let (api, st) = backend(vec![vec![servers("r1")], vec![servers("r1")]], "a1", "r1");
        let (live, rx) = Live::start(client(&api, "kick", "a1", "r1"));
        assert!(matches!(next(&rx), Some(Event::Servers(_))));
        let t = Instant::now();
        live.kick();
        assert!(matches!(next(&rx), Some(Event::Servers(_))));
        assert!(t.elapsed() < Duration::from_secs(3), "a kick reconnects without backoff");
        assert_eq!(st.lock().unwrap().upgrades.len(), 2);
        drop(live);
        assert!(next(&rx).is_none(), "dropping the handle closes the socket");
    }
}
