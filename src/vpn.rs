//! The VPN model every window and the tray mirror: account, servers, the running core and its numbers.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{Context, Task};
use serde_json::Value;

use crate::app::Conn;
use crate::data::{self, Account, AppRule, Protocol, Scope, SiteRule, Suggestion};
use crate::net::{self, Geo};
use crate::store::{self, Login, Rule, Settings};
use crate::sub;
use crate::suggest::Tracker;
use crate::xray::{self, Core, Plan};

#[derive(Clone, PartialEq)]
pub struct Me {
    pub ip: String,
    pub geo: Option<Geo>,
}

/// Live numbers while connected.
#[derive(Clone, PartialEq, Default)]
pub struct Stats {
    /// Mbit/s over the last second.
    pub down: f64,
    pub up: f64,
    /// Probe round trip through the whole route, ms.
    pub latency: Option<u32>,
    /// Bytes this session.
    pub total: u64,
}

/// VPN state every window shows: the main window and the tray popup mirror one `Vpn` model.
#[derive(Clone, PartialEq)]
pub struct Shared {
    pub conn: Conn,
    pub connected_at: Option<Instant>,
    pub server_id: &'static str,
    pub entry_id: Option<&'static str>,
    pub recents: Vec<&'static str>,
    pub me: Option<Me>,
    pub protocol: Protocol,
    pub auto_connect: bool,
    pub kill_switch: bool,
    pub autoroute: bool,
    pub system_wide: bool,
    pub app_routes: Vec<(&'static str, AppRule)>,
    pub sites: Vec<(&'static str, SiteRule)>,
    pub suggestions: Vec<Suggestion>,
    pub snoozed: Vec<&'static str>,
    pub dismissed: Vec<String>,
    pub rules_big: bool,
    pub dark: bool,
    pub signed_in: bool,
    pub account: Option<Account>,
    /// Signing in or refreshing the subscription.
    pub busy: bool,
    pub error: Option<String>,
    pub stats: Stats,
    /// Bumped when the server list, pings or exits change.
    pub servers_gen: u32,
}

impl Shared {
    /// Fields only the model changes; a window adopts them even while it has edits of its own to push.
    pub fn adopt(&mut self, model: &Shared) {
        self.conn = model.conn;
        self.connected_at = model.connected_at;
        self.me = model.me.clone();
        self.recents = model.recents.clone();
        self.signed_in = model.signed_in;
        self.account = model.account.clone();
        self.busy = model.busy;
        self.error = model.error.clone();
        self.stats = model.stats.clone();
        self.servers_gen = model.servers_gen;
        if !data::has_server(self.server_id) {
            self.server_id = model.server_id;
        }
    }

    fn settings(&self) -> Settings {
        Settings {
            server: data::has_server(self.server_id).then(|| self.server_id.to_owned()),
            entry: self.entry_id.map(Into::into),
            recents: self.recents.iter().map(|r| (*r).to_owned()).collect(),
            protocol: self.protocol.into(),
            auto_connect: self.auto_connect,
            kill_switch: self.kill_switch,
            autoroute: self.autoroute,
            system_wide: self.system_wide,
            dark: self.dark,
            rules_big: self.rules_big,
            apps: self.app_routes.iter().map(|(a, r)| Rule::from_app(a, r)).collect(),
            sites: self.sites.iter().map(|(t, r)| ((*t).to_owned(), *r == SiteRule::Vpn)).collect(),
            dismissed: self.dismissed.clone(),
        }
    }

    /// What a running core has to be restarted for.
    fn routing(&self) -> (bool, Vec<(&'static str, AppRule)>, Vec<(&'static str, SiteRule)>) {
        (self.autoroute, self.app_routes.clone(), self.sites.clone())
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct Exit {
    ip: String,
    city: Option<String>,
    lat: f64,
    lon: f64,
}

fn exits_path() -> std::path::PathBuf {
    store::cache_dir().join("exits.json")
}

fn subscription_path() -> std::path::PathBuf {
    store::config_dir().join("subscription.json")
}

#[derive(Default)]
struct Lists {
    tunnel: Arc<Vec<Value>>,
    base: Arc<Vec<Value>>,
    dns: Arc<Value>,
}

/// The connection and everything it needs.
pub struct Vpn {
    pub s: Shared,
    core: Option<Core>,
    /// Bumped by every connect/disconnect, so a late result of an older attempt is thrown away.
    attempt: u64,
    starting: Option<Task<()>>,
    ticker: Option<Task<()>>,
    reload: Option<Task<()>>,
    pinger: Option<Task<()>>,
    probing: bool,
    login: Option<String>,
    lists: Lists,
    saved: Settings,
    /// Routing the running core was started with.
    applied: Option<(bool, Vec<(&'static str, AppRule)>, Vec<(&'static str, SiteRule)>)>,
    bytes: Option<(Instant, u64, u64)>,
    first_bytes: u64,
    failures: u32,
    exits: HashMap<String, Exit>,
    autoconnect_done: bool,
    tracker: Tracker,
    _scanner: Task<()>,
}

impl Vpn {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let saved: Settings = store::load(&Settings::path()).unwrap_or_default();
        let login = store::load::<Login>(&Login::path()).map(|l| l.url).or_else(|| std::env::var("HITORI_SUB").ok());
        let exits: HashMap<String, Exit> = store::load(&exits_path()).unwrap_or_default();
        let i = data::intern;
        let s = Shared {
            conn: Conn::Off,
            connected_at: None,
            server_id: saved.server.as_deref().map(i).unwrap_or("none"),
            entry_id: saved.entry.as_deref().map(i),
            recents: saved.recents.iter().map(|r| i(r)).collect(),
            me: None,
            protocol: saved.protocol.into(),
            auto_connect: saved.auto_connect,
            kill_switch: saved.kill_switch,
            autoroute: saved.autoroute,
            system_wide: saved.system_wide,
            app_routes: saved.apps.iter().map(Rule::to_app).collect(),
            sites: saved.sites(),
            suggestions: vec![],
            snoozed: vec![],
            dismissed: saved.dismissed.clone(),
            rules_big: saved.rules_big,
            dark: saved.dark,
            signed_in: login.is_some(),
            account: None,
            busy: false,
            error: None,
            stats: Stats::default(),
            servers_gen: 0,
        };
        let mut this = Self {
            s,
            core: None,
            attempt: 0,
            starting: None,
            ticker: None,
            reload: None,
            pinger: None,
            probing: false,
            login,
            lists: Lists::default(),
            saved,
            applied: None,
            bytes: None,
            first_bytes: 0,
            failures: 0,
            exits,
            autoconnect_done: false,
            tracker: Tracker::new(),
            _scanner: Self::scanner(cx),
        };
        if this.login.is_some()
            && let Ok(body) = std::fs::read(subscription_path())
        {
            let _ = this.load_subscription(&body, cx);
        }
        this.refresh(None, cx);
        this.load_me(cx);
        cx.observe_self(|this, cx| this.changed(cx)).detach();
        cx.on_app_quit(|this, _| {
            let core = this.core.take();
            async move { drop(core) }
        })
        .detach();
        this
    }

    /// Which apps use the network, every 10 s (the Add app menu and rule cards).
    fn scanner(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let exes = cx.background_executor().spawn(async { crate::apps::scan() }).await;
                data::saw(&exes);
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(10)).await;
            }
        })
    }

    /// Sites that failed directly a few times and do answer through the tunnel become suggestions.
    fn suggest(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.core.as_ref() else { return };
        self.tracker.feed(&core.log.take());
        let probe = core.ports.probe;
        let known = |host: &str, s: &Shared| {
            s.sites.iter().any(|(t, _)| host == *t || host.ends_with(&format!(".{t}")))
                || s.suggestions.iter().any(|x| x.target == host)
                || s.dismissed.iter().any(|d| d == host)
        };
        let mut fresh = vec![];
        for (host, reason, hits) in self.tracker.candidates(Instant::now()) {
            if known(&host, &self.s) {
                self.tracker.forget(&host);
            } else {
                fresh.push((host, reason, hits));
            }
        }
        if fresh.is_empty() || !self.s.autoroute {
            return;
        }
        let via = self.s.server_id;
        cx.spawn(async move |this, cx| {
            for (host, reason, hits) in fresh {
                let h = host.clone();
                if !cx.background_executor().spawn(async move { xray::reachable(probe, &h) }).await {
                    continue;
                }
                let gone = this
                    .update(cx, |this, cx| {
                        if known(&host, &this.s) {
                            return;
                        }
                        let target = data::intern(&host);
                        this.s.suggestions.push(Suggestion {
                            id: data::intern(&format!("s-{host}")),
                            app_id: "",
                            target,
                            scope: Scope::Site,
                            reason,
                            via_id: via,
                            hits,
                            at: Instant::now(),
                            speed: None,
                        });
                        cx.notify();
                    })
                    .is_err();
                if gone {
                    break;
                }
            }
        })
        .detach();
    }

    fn load_me(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let Some(ip) = cx.background_executor().spawn(async { net::my_ip() }).await else { return };
            let ip2 = ip.clone();
            this.update(cx, |this, cx| {
                this.s.me = Some(Me { ip: ip.clone(), geo: None });
                cx.notify();
            })
            .ok();
            let geo = cx.background_executor().spawn(async move { net::locate(&ip2) }).await;
            this.update(cx, |this, cx| {
                this.s.me = Some(Me { ip, geo });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---------- account ----------

    /// Parse and adopt a subscription body; servers, lists and remembered exits.
    fn load_subscription(&mut self, body: &[u8], cx: &mut Context<Self>) -> Result<(), String> {
        let parsed = sub::parse(body)?;
        data::set_servers(parsed.servers);
        for (id, e) in &self.exits {
            data::learn_exit(id, &e.ip, e.city.as_deref(), e.lat, e.lon);
        }
        self.lists = Lists { tunnel: Arc::new(parsed.tunnel_rules), base: Arc::new(parsed.base_rules), dns: Arc::new(parsed.dns) };
        if self.lists.tunnel.is_empty() {
            cx.spawn(async move |this, cx| {
                let list = cx.background_executor().spawn(async { sub::community_list() }).await;
                this.update(cx, |this, _| this.lists.tunnel = Arc::new(list)).ok();
            })
            .detach();
        }
        if !data::has_server(self.s.server_id) {
            self.s.server_id = data::fastest().id;
        }
        if self.s.entry_id.is_some_and(|e| !data::has_server(e)) {
            self.s.entry_id = None;
        }
        self.s.recents.retain(|r| data::has_server(r));
        self.s.servers_gen += 1;
        self.start_pinger(cx);
        cx.notify();
        Ok(())
    }

    /// Fetch the subscription and account; with `link`, sign in with it (saved only if it works).
    fn refresh(&mut self, link: Option<String>, cx: &mut Context<Self>) {
        let Some(url) = link.clone().or_else(|| self.login.clone()) else { return };
        let base = match sub::base(&url) {
            Ok(b) => b,
            Err(e) => {
                self.s.error = Some(e);
                cx.notify();
                return;
            }
        };
        self.s.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let got = cx
                .background_executor()
                .spawn(async move {
                    let fetched = sub::fetch(&base)?;
                    sub::parse(&fetched.body)?;
                    let account = sub::info(&base, fetched.title.clone(), fetched.support.clone());
                    Ok::<_, String>((fetched.body, account))
                })
                .await;
            this.update(cx, |this, cx| {
                this.s.busy = false;
                match got {
                    Ok((body, account)) => {
                        if let Some(url) = link {
                            store::save(&Login::path(), &Login { url: url.trim().to_owned() }, true);
                            this.login = Some(url);
                            this.s.signed_in = true;
                        }
                        let _ = store::write(&subscription_path(), &body, true);
                        this.s.error = this.load_subscription(&body, cx).err();
                        this.s.account = account.or(this.s.account.take());
                        if this.s.auto_connect && !this.autoconnect_done && this.s.conn == Conn::Off {
                            this.connect(cx);
                        }
                        this.autoconnect_done = true;
                    }
                    // Keep working from the saved copy when only the refresh failed.
                    Err(e) => this.s.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn sign_in(&mut self, link: String, cx: &mut Context<Self>) {
        self.s.error = None;
        self.refresh(Some(link), cx);
    }

    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.disconnect(cx);
        let _ = std::fs::remove_file(Login::path());
        let _ = std::fs::remove_file(subscription_path());
        self.login = None;
        data::set_servers(vec![]);
        self.s.signed_in = false;
        self.s.account = None;
        self.s.server_id = "none";
        self.s.entry_id = None;
        self.s.recents.clear();
        self.s.servers_gen += 1;
        self.pinger = None;
        cx.notify();
    }

    // ---------- connection ----------

    fn plan(&self) -> Plan {
        Plan {
            exit: data::server(self.s.server_id),
            entry: self.s.entry_id.map(data::server),
            protocol: self.s.protocol,
            autoroute: self.s.autoroute,
            tun: self.s.system_wide,
            kill_switch: self.s.kill_switch,
            apps: self.s.app_routes.clone(),
            sites: self.s.sites.clone(),
            servers: data::servers(),
            tunnel_rules: self.lists.tunnel.clone(),
            base_rules: self.lists.base.clone(),
            dns: self.lists.dns.clone(),
        }
    }

    fn remember(&mut self) {
        let id = self.s.server_id;
        self.s.recents.retain(|r| *r != id);
        self.s.recents.insert(0, id);
        self.s.recents.truncate(3);
    }

    /// Connect, or move the running connection to the current server and rules.
    pub fn connect(&mut self, cx: &mut Context<Self>) {
        if !self.s.signed_in || !data::has_server(self.s.server_id) {
            self.s.error = Some(if self.s.signed_in { "No server to connect to".into() } else { "Sign in to connect".into() });
            cx.notify();
            return;
        }
        self.remember();
        self.attempt += 1;
        let attempt = self.attempt;
        self.s.conn = Conn::Connecting;
        self.s.connected_at = None;
        self.s.error = None;
        self.s.stats = Stats::default();
        self.ticker = None;
        self.reload = None;
        let plan = self.plan();
        let routing = self.s.routing();
        let previous = self.core.take();
        self.starting = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(mut core) = previous {
                        if core.tun == plan.tun && core.reload(&plan).is_ok() {
                            return Ok(core);
                        }
                        core.stop();
                    }
                    Core::start(&plan)
                })
                .await;
            this.update(cx, |this, cx| this.started(attempt, routing, result, cx)).ok();
        }));
        cx.notify();
    }

    fn started(&mut self, attempt: u64, routing: (bool, Vec<(&'static str, AppRule)>, Vec<(&'static str, SiteRule)>), result: Result<Core, String>, cx: &mut Context<Self>) {
        if attempt != self.attempt {
            if let Ok(core) = result {
                cx.background_executor().spawn(async move { core.stop() }).detach();
            }
            return;
        }
        self.starting = None;
        match result {
            Ok(core) => {
                let probe = core.ports.probe;
                self.core = Some(core);
                self.applied = Some(routing);
                self.bytes = None;
                self.connected(cx);
                self.lookup_exit(probe, cx);
            }
            Err(e) => {
                self.s.conn = Conn::Off;
                self.s.error = Some(e);
                cx.notify();
            }
        }
    }

    /// Connected now: the session clock and the 1 s metrics ticker start. (Also used without a core for screenshots.)
    pub fn connected(&mut self, cx: &mut Context<Self>) {
        if self.s.conn == Conn::Off {
            self.remember();
        }
        self.starting = None;
        self.s.conn = Conn::On;
        self.s.connected_at = Some(Instant::now());
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let Ok(port) = this.update(cx, |this, cx| this.check(cx)) else { break };
                let Some(port) = port else {
                    if this.update(cx, |this, _| this.s.conn != Conn::On).unwrap_or(true) {
                        break;
                    }
                    continue;
                };
                let m = cx.background_executor().spawn(async move { xray::metrics(port) }).await;
                if this.update(cx, |this, cx| this.measured(m, cx)).is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Ticker: is the core still up? Returns the metrics port to read.
    fn check(&mut self, cx: &mut Context<Self>) -> Option<u16> {
        let Some(core) = self.core.as_mut() else {
            cx.notify();
            return None;
        };
        if core.alive() {
            return Some(core.ports.metrics);
        }
        let why = core.log.last_error();
        self.failures += 1;
        // With the kill switch the helper keeps traffic blocked while we keep trying.
        let keep_trying = self.failures <= 3 || (core.tun && self.s.kill_switch);
        if keep_trying {
            self.connect(cx);
        } else {
            let core = self.core.take();
            cx.background_executor().spawn(async move { drop(core) }).detach();
            self.ticker = None;
            self.s.conn = Conn::Off;
            self.s.connected_at = None;
            self.s.error = Some(format!("Connection lost{}", why.map(|w| format!(": {w}")).unwrap_or_default()));
            cx.notify();
        }
        None
    }

    fn measured(&mut self, m: Option<xray::Metrics>, cx: &mut Context<Self>) {
        let Some(m) = m else { return };
        let now = Instant::now();
        let (down, up) = match self.bytes {
            Some((t, d, u)) => {
                let dt = now.duration_since(t).as_secs_f64().max(0.1);
                (m.down.saturating_sub(d) as f64 * 8. / 1e6 / dt, m.up.saturating_sub(u) as f64 * 8. / 1e6 / dt)
            }
            None => {
                self.first_bytes = m.down + m.up;
                (0., 0.)
            }
        };
        self.bytes = Some((now, m.down, m.up));
        let latency = m.delays.get("proxy").copied().flatten().or(self.s.stats.latency);
        if latency.is_some() {
            self.failures = 0;
        }
        self.s.stats = Stats { down, up, latency, total: (m.down + m.up).saturating_sub(self.first_bytes) };
        self.suggest(cx);
        let pings: Vec<_> = m.delays.iter().filter_map(|(tag, d)| tag.strip_prefix("p-").filter(|id| data::has_server(id)).map(|id| (data::intern(id), *d))).collect();
        if pings.iter().any(|(id, d)| data::ping(id) != *d) {
            data::set_pings(pings);
            self.s.servers_gen += 1;
        }
        cx.notify();
    }

    /// Where the tunnel comes out: shown in the status line and on the map, remembered for next time.
    fn lookup_exit(&mut self, probe: u16, cx: &mut Context<Self>) {
        let id = self.s.server_id;
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    let ip = xray::exit_ip(probe)?;
                    let geo = net::locate(&ip);
                    Some((ip, geo))
                })
                .await;
            let Some((ip, geo)) = found else { return };
            this.update(cx, |this, cx| {
                let base = data::server(id);
                let exit = Exit {
                    ip: ip.clone(),
                    city: geo.as_ref().and_then(|g| g.city.clone()),
                    lat: geo.as_ref().map_or(base.lat, |g| g.lat),
                    lon: geo.as_ref().map_or(base.lon, |g| g.lon),
                };
                data::learn_exit(id, &exit.ip, exit.city.as_deref(), exit.lat, exit.lon);
                this.exits.insert(id.to_owned(), exit);
                store::save(&exits_path(), &this.exits, false);
                this.s.servers_gen += 1;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.attempt += 1;
        self.starting = None;
        self.ticker = None;
        self.reload = None;
        self.failures = 0;
        if let Some(core) = self.core.take() {
            cx.background_executor().spawn(async move { core.stop() }).detach();
        }
        self.s.conn = Conn::Off;
        self.s.connected_at = None;
        self.s.stats = Stats::default();
        self.start_pinger(cx);
        cx.notify();
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.s.conn == Conn::Off { self.connect(cx) } else { self.disconnect(cx) }
    }

    /// Connect to a location (tray menu); picking the live one again does nothing.
    pub fn go(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let was = (self.s.conn, self.s.server_id);
        self.s.server_id = id;
        if self.s.entry_id == Some(id) {
            self.s.entry_id = None;
        }
        if was.0 == Conn::Off || was.1 != id { self.connect(cx) } else { cx.notify() }
    }

    /// Every model change: persist settings, and bring a running core in line with new rules.
    fn changed(&mut self, cx: &mut Context<Self>) {
        let settings = self.s.settings();
        if settings != self.saved {
            if settings.kill_switch != self.saved.kill_switch
                && let Some(core) = self.core.as_mut()
            {
                core.set_kill_switch(settings.kill_switch);
            }
            store::save(&Settings::path(), &settings, false);
            self.saved = settings;
        }
        if self.s.conn == Conn::On && self.core.is_some() && self.applied.as_ref() != Some(&self.s.routing()) && self.reload.is_none() {
            // Debounced: several quick edits restart the core once.
            self.reload = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(700)).await;
                this.update(cx, |this, cx| this.apply_routing(cx)).ok();
            }));
        }
    }

    fn apply_routing(&mut self, cx: &mut Context<Self>) {
        self.reload = None;
        let Some(mut core) = self.core.take() else { return };
        let plan = self.plan();
        let routing = self.s.routing();
        let attempt = self.attempt;
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { core.reload(&plan).map(|_| core) }).await;
            this.update(cx, |this, cx| {
                if attempt != this.attempt {
                    if let Ok(core) = result {
                        cx.background_executor().spawn(async move { core.stop() }).detach();
                    }
                    return;
                }
                match result {
                    Ok(core) => {
                        this.core = Some(core);
                        this.applied = Some(routing);
                        this.bytes = None;
                        // Rules may have changed again while restarting.
                        this.changed(cx);
                    }
                    Err(e) => {
                        this.s.error = Some(e);
                        this.connect(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---------- latency while disconnected ----------

    /// Measure every server with a throwaway core now and every 5 minutes while disconnected.
    fn start_pinger(&mut self, cx: &mut Context<Self>) {
        if self.pinger.is_some() || data::servers().is_empty() {
            return;
        }
        self.pinger = Some(cx.spawn(async move |this, cx| {
            loop {
                let job = this.update(cx, |this, _| {
                    let idle = this.s.conn == Conn::Off && !this.probing;
                    if idle {
                        this.probing = true;
                    }
                    idle.then(|| (data::servers(), this.s.protocol))
                });
                let Ok(job) = job else { break };
                if let Some((servers, protocol)) = job {
                    let result = cx.background_executor().spawn(async move { xray::probe(&servers, protocol) }).await;
                    let gone = this
                        .update(cx, |this, cx| {
                            this.probing = false;
                            if let Ok(pings) = result {
                                data::set_pings(pings);
                                if this.s.conn == Conn::Off && this.s.recents.is_empty() && this.s.entry_id.is_none() {
                                    this.s.server_id = data::fastest().id;
                                }
                                this.s.servers_gen += 1;
                                cx.notify();
                            }
                        })
                        .is_err();
                    if gone {
                        break;
                    }
                }
                cx.background_executor().timer(Duration::from_secs(300)).await;
            }
        }));
    }
}
