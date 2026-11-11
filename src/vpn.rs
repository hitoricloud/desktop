//! The VPN model every window and the tray mirror: account, servers, the running core and its numbers.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::{Context, RenderImage, Task};
use serde_json::Value;

use crate::app::Conn;
use crate::auth;
use crate::data::{self, Account, AppRule, Protocol, RuleMode, Scope, Suggestion};
use crate::live;
use crate::net::{self, Geo};
use crate::region::{self, Location};
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
}

/// Telegram sign-in progress.
#[derive(Clone, PartialEq)]
pub enum Telegram {
    Idle,
    /// Asking the backend for the Telegram page.
    Starting,
    /// The browser is at this page; waiting for it to come back.
    Browser(String),
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
    pub default_route: RuleMode,
    pub system_wide: bool,
    pub tun: bool,
    pub app_routes: Vec<(&'static str, AppRule)>,
    pub sites: Vec<(&'static str, AppRule)>,
    pub suggestions: Vec<Suggestion>,
    pub snoozed: Vec<&'static str>,
    pub dismissed: Vec<String>,
    pub rules_big: bool,
    pub dark: bool,
    /// Chosen UI language; `None` follows the system.
    pub language: Option<crate::i18n::Lang>,
    pub signed_in: bool,
    /// Signed in with Telegram: name and avatar.
    pub user: Option<auth::User>,
    pub avatar: Option<Arc<RenderImage>>,
    pub telegram: Telegram,
    /// Signed in with Telegram, but the account has no subscription.
    pub no_subscription: bool,
    pub account: Option<Account>,
    /// Signing in or refreshing the subscription.
    pub busy: bool,
    pub error: Option<String>,
    pub stats: Stats,
    /// Bumped when the server list, pings or exits change.
    pub servers_gen: u32,
    /// A latency test is running; these servers have not answered yet.
    pub testing: bool,
    pub pending: Vec<&'static str>,
    /// When the last latency test finished.
    pub tested_at: Option<Instant>,
    /// Detected country and its blocked lists, from launch on (also before signing in).
    pub region: region::Summary,
}

impl Shared {
    /// System-wide through a TUN device: always on Linux, by choice on Windows and macOS.
    pub fn uses_tun(&self) -> bool {
        self.system_wide && (self.tun || !crate::os::SYSTEM_PROXY)
    }

    /// Fields only the model changes; a window adopts them even while it has edits of its own to push.
    pub fn adopt(&mut self, model: &Shared) {
        self.conn = model.conn;
        self.connected_at = model.connected_at;
        self.me = model.me.clone();
        self.recents = model.recents.clone();
        self.signed_in = model.signed_in;
        self.user = model.user.clone();
        self.avatar = model.avatar.clone();
        self.telegram = model.telegram.clone();
        self.no_subscription = model.no_subscription;
        self.account = model.account.clone();
        self.busy = model.busy;
        self.error = model.error.clone();
        self.stats = model.stats.clone();
        self.servers_gen = model.servers_gen;
        self.testing = model.testing;
        self.pending = model.pending.clone();
        self.tested_at = model.tested_at;
        self.region = model.region.clone();
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
            route: self.default_route.key().into(),
            system_wide: self.system_wide,
            tun: self.tun,
            dark: self.dark,
            language: self.language.map_or("system", |l| l.code()).into(),
            rules_big: self.rules_big,
            apps: self.app_routes.iter().map(|(a, r)| Rule::from_app(a, r)).collect(),
            sites: self.sites.iter().map(|(t, r)| Rule::from_app(t, r)).collect(),
            dismissed: self.dismissed.clone(),
        }
    }

    /// What a running core has to be restarted for.
    fn routing(&self) -> Routing {
        (self.default_route, self.app_routes.clone(), self.sites.clone())
    }
}

/// What a running core has to be restarted for: default route, app rules, site rules.
type Routing = (RuleMode, Vec<(&'static str, AppRule)>, Vec<(&'static str, AppRule)>);

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
    tester: Option<Task<()>>,
    login: Option<String>,
    auth: Arc<auth::Client>,
    /// Set to stop the Telegram sign-in in progress.
    cancel: Option<Arc<AtomicBool>>,
    lists: Lists,
    saved: Settings,
    /// Routing the running core was started with.
    applied: Option<Routing>,
    bytes: Option<(Instant, u64, u64)>,
    failures: u32,
    exits: HashMap<String, Exit>,
    autoconnect_done: bool,
    tracker: Tracker,
    _scanner: Task<()>,
    location: Option<Location>,
    /// The region's lists as tunnel rules, merged after the subscription's.
    region_rules: Arc<Vec<Value>>,
    checking: Option<Task<()>>,
    last_check: Option<Instant>,
    net_addr: Option<std::net::IpAddr>,
    _region_watch: Task<()>,
    /// Live updates while signed in with Telegram, and the server list they last loaded.
    live: Option<(live::Live, Task<()>)>,
    revs: live::Revs,
    /// The app's own requests go through the TUN; a change reconnects the live socket.
    tunneled: bool,
    /// Servers gone from the subscription that the running tunnel still uses, listed offline until it leaves them.
    gone: Vec<&'static str>,
}

impl Vpn {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let saved: Settings = store::load(&Settings::path()).unwrap_or_default();
        let login = store::load::<Login>(&Login::path()).map(|l| l.url).or_else(|| std::env::var("HITORI_SUB").ok());
        let exits: HashMap<String, Exit> = store::load(&exits_path()).unwrap_or_default();
        let location: Option<Location> = store::load(&Location::path());
        let auth = Arc::new(auth::Client::new(auth::api(), auth::session_path()));
        let session = auth.session();
        let i = data::intern;
        let s = Shared {
            conn: Conn::Off,
            connected_at: None,
            server_id: saved.server.as_deref().map(i).unwrap_or("none"),
            entry_id: saved.entry.as_deref().map(i),
            recents: saved.recents.iter().map(|r| i(r)).collect(),
            me: location.as_ref().map(|l| Me { ip: l.ip.clone(), geo: Some(l.geo()) }),
            protocol: saved.protocol.into(),
            auto_connect: saved.auto_connect,
            kill_switch: saved.kill_switch,
            default_route: RuleMode::parse(&saved.route),
            system_wide: saved.system_wide,
            tun: saved.tun,
            app_routes: saved.apps.iter().map(Rule::to_app).collect(),
            sites: saved.sites(),
            suggestions: vec![],
            snoozed: vec![],
            dismissed: saved.dismissed.clone(),
            rules_big: saved.rules_big,
            dark: saved.dark,
            language: crate::i18n::Lang::parse(&saved.language),
            signed_in: login.is_some() || session.is_some(),
            user: session.as_ref().map(|s| s.user.clone()),
            avatar: None,
            telegram: Telegram::Idle,
            no_subscription: false,
            account: None,
            busy: false,
            error: None,
            stats: Stats::default(),
            servers_gen: 0,
            testing: false,
            pending: vec![],
            tested_at: None,
            region: region::Summary {
                country: location.as_ref().map(|l| l.country.clone()),
                name: location.as_ref().and_then(|l| l.name.clone()),
                ..Default::default()
            },
        };
        let mut this = Self {
            s,
            core: None,
            attempt: 0,
            starting: None,
            ticker: None,
            reload: None,
            pinger: None,
            tester: None,
            login,
            auth,
            cancel: None,
            lists: Lists::default(),
            saved,
            applied: None,
            bytes: None,
            failures: 0,
            exits,
            autoconnect_done: false,
            tracker: Tracker::new(),
            _scanner: Self::scanner(cx),
            location,
            region_rules: Arc::default(),
            checking: None,
            last_check: None,
            net_addr: region::local_addr(),
            _region_watch: Self::region_watch(cx),
            live: None,
            revs: live::Revs::default(),
            tunneled: false,
            gone: vec![],
        };
        if this.s.signed_in
            && let Ok(body) = std::fs::read(subscription_path())
        {
            let _ = this.load_subscription(&body, cx);
        }
        if session.is_some() {
            this.cached_avatar(cx);
            this.sync(cx);
        } else {
            this.refresh(None, cx);
        }
        this.check_region(false, cx);
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
        if fresh.is_empty() || !self.s.default_route.tries_direct() {
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

    // ---------- region ----------

    /// The app's own requests skip the tunnel: while disconnected, and in proxy mode (the app itself is not proxied).
    fn direct(&self) -> bool {
        self.s.conn == Conn::Off || !self.core.as_ref().map_or(self.s.uses_tun(), |c| c.tun)
    }

    /// Detect the country again and download its lists now (Settings, the onboarding screen).
    pub fn recheck_region(&mut self, cx: &mut Context<Self>) {
        self.check_region(true, cx);
    }

    /// Public IP → country → lists. Through the tunnel only the lists of the known country are refreshed.
    fn check_region(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.checking.is_some() {
            return;
        }
        let lookup = self.direct();
        let cached = self.location.clone();
        // Before the network answers: the cached lists, so routing has them from the start.
        let offline = cached.as_ref().filter(|_| self.region_rules.is_empty()).map(|l| l.country.clone());
        let exits: Vec<String> =
            self.exits.values().map(|e| e.ip.clone()).chain(data::servers().iter().map(|s| s.ip.to_owned())).filter(|ip| !ip.is_empty()).collect();
        let attempt = self.attempt;
        self.last_check = Some(Instant::now());
        self.s.region.status = region::Status::Computing;
        cx.notify();
        self.checking = Some(cx.spawn(async move |this, cx| {
            if let Some(country) = offline {
                let (lists, _) = cx.background_executor().spawn(async move { region::load(&country, region::Fetch::Never) }).await;
                this.update(cx, |this, cx| this.set_region_lists(&lists, cx)).ok();
            }
            let got = cx.background_executor().spawn(async move { region::check(cached, lookup, force, &exits) }).await;
            this.update(cx, |this, cx| this.region_checked(lookup && attempt != this.attempt, got, cx)).ok();
        }));
    }

    fn region_checked(&mut self, raced: bool, got: Result<Option<region::Found>, String>, cx: &mut Context<Self>) {
        self.checking = None;
        // A connection started meanwhile: in system-wide mode the lookup may have gone through it.
        let raced = raced && self.s.uses_tun();
        self.s.region.status = match got {
            Ok(Some(found)) if !raced => {
                let l = found.location;
                (self.s.region.country, self.s.region.name) = (Some(l.country.clone()), l.name.clone());
                self.s.me = Some(Me { ip: l.ip.clone(), geo: Some(l.geo()) });
                store::save(&Location::path(), &l, false);
                self.location = Some(l);
                self.set_region_lists(&found.lists, cx);
                if found.missing { region::Status::Failed(t!("vpn.region.cannot_download", place = self.s.region.place())) } else { region::Status::Ready }
            }
            Ok(_) => region::Status::Ready,
            Err(e) => region::Status::Failed(e),
        };
        cx.notify();
    }

    fn set_region_lists(&mut self, lists: &region::Lists, cx: &mut Context<Self>) {
        (self.s.region.domains, self.s.region.subnets) = (lists.domains.len(), lists.subnets.len());
        let rules = lists.rules();
        if *self.region_rules != rules {
            self.region_rules = Arc::new(rules);
            // A running core picks the new lists up like a rules edit.
            if self.s.conn == Conn::On {
                self.applied = None;
            }
        }
        cx.notify();
    }

    /// Every 30 s: a new local network, or an old check, means another look at the public IP.
    fn region_watch(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(30)).await;
                let addr = cx.background_executor().spawn(async { region::local_addr() }).await;
                let alive = this.update(cx, |this, cx| {
                    let changed = std::mem::replace(&mut this.net_addr, addr) != addr;
                    if changed {
                        this.kick_live();
                    }
                    let backoff = this.last_check.is_some_and(|t| t.elapsed() < Duration::from_secs(120));
                    if this.direct() && (changed || !backoff) && region::due(this.location.as_ref(), region::now(), changed) {
                        this.check_region(false, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
    }

    // ---------- account ----------

    /// Parse and adopt a subscription body; servers, lists and remembered exits.
    fn load_subscription(&mut self, body: &[u8], cx: &mut Context<Self>) -> Result<(), String> {
        let mut parsed = sub::parse(body)?;
        let on = self.s.conn != Conn::Off;
        self.gone = [Some(self.s.server_id), self.s.entry_id]
            .into_iter()
            .flatten()
            .filter(|id| on && data::has_server(id) && parsed.servers.iter().all(|s| s.id != *id))
            .collect();
        parsed.servers.extend(self.gone.iter().map(|id| data::server(id).clone()));
        data::set_servers(parsed.servers);
        data::set_pings(self.gone.iter().map(|id| (*id, None)));
        for (id, e) in &self.exits {
            data::learn_exit(id, &e.ip, e.city.as_deref(), e.lat, e.lon);
        }
        self.lists = Lists { tunnel: Arc::new(parsed.tunnel_rules), base: Arc::new(parsed.base_rules), dns: Arc::new(parsed.dns) };
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
                        if this.revs.loaded() {
                            this.sync(cx);
                        }
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
        self.cancel_sign_in(cx);
        self.s.error = None;
        self.refresh(Some(link), cx);
    }

    /// Telegram sign-in: the browser opens at the backend's Telegram page and comes back to a loopback port.
    pub fn telegram_sign_in(&mut self, cx: &mut Context<Self>) {
        if self.s.telegram != Telegram::Idle {
            return;
        }
        let flag = Arc::new(AtomicBool::new(false));
        self.cancel = Some(flag.clone());
        self.s.telegram = Telegram::Starting;
        self.s.error = None;
        cx.notify();
        let api = self.auth.api.clone();
        cx.spawn(async move |this, cx| {
            let begun = cx.background_executor().spawn({
                let api = api.clone();
                async move { auth::begin(&api) }
            });
            let pending = match begun.await {
                Ok(p) => p,
                Err(e) => {
                    this.update(cx, |this, cx| this.signed_in_with(&flag, Err(e), cx)).ok();
                    return;
                }
            };
            let url = pending.auth_url.clone();
            let live = this.update(cx, |this, cx| {
                let live = this.current(&flag);
                if live {
                    this.s.telegram = Telegram::Browser(url.clone());
                    cx.notify();
                }
                live
            });
            if !live.unwrap_or(false) {
                return;
            }
            let stop = flag.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    crate::os::open_url(&url);
                    pending.finish(&api, &stop)
                })
                .await;
            this.update(cx, |this, cx| this.signed_in_with(&flag, result, cx)).ok();
        })
        .detach();
    }

    /// The sign-in `flag` belongs to is still the one on screen.
    fn current(&self, flag: &Arc<AtomicBool>) -> bool {
        self.cancel.as_ref().is_some_and(|c| Arc::ptr_eq(c, flag)) && !flag.load(Ordering::Relaxed)
    }

    fn signed_in_with(&mut self, flag: &Arc<AtomicBool>, result: Result<auth::Session, auth::Error>, cx: &mut Context<Self>) {
        if !self.current(flag) {
            return;
        }
        self.cancel = None;
        self.s.telegram = Telegram::Idle;
        match result {
            Ok(session) => {
                // The Telegram account replaces a pasted link.
                let _ = std::fs::remove_file(Login::path());
                self.login = None;
                self.s.user = Some(session.user.clone());
                self.s.signed_in = true;
                self.auth.set(Some(session));
                self.sync(cx);
            }
            Err(auth::Error::Cancelled) => {}
            Err(e) => self.s.error = Some(e.to_string()),
        }
        cx.notify();
    }

    pub fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        if let Some(flag) = self.cancel.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if self.s.telegram != Telegram::Idle {
            self.s.telegram = Telegram::Idle;
            cx.notify();
        }
    }

    /// Profile and subscription from the backend; the subscription link then loads servers like a pasted one.
    pub fn sync(&mut self, cx: &mut Context<Self>) {
        let client = self.auth.clone();
        self.s.busy = true;
        self.s.error = None;
        self.revs.fetching();
        self.go_live(cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let me = cx.background_executor().spawn(async move { client.me() }).await;
            this.update(cx, |this, cx| {
                this.s.busy = false;
                if this.auth.session().is_none() {
                    return;
                }
                match me {
                    Ok(me) => this.adopt_me(me, cx),
                    Err(auth::Error::Unauthorized) => {
                        this.sign_out(cx);
                        this.s.error = Some(auth::Error::Unauthorized.to_string());
                    }
                    // Keep working from the saved subscription.
                    Err(e) => this.s.error = Some(e.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn adopt_me(&mut self, me: auth::Me, cx: &mut Context<Self>) {
        let had_avatar = self.s.user.as_ref().is_some_and(|u| u.has_avatar);
        self.s.no_subscription = me.plan.is_none();
        if me.user.has_avatar {
            self.fetch_avatar(cx);
        } else if had_avatar || self.s.avatar.is_some() {
            self.s.avatar = None;
            let _ = std::fs::remove_file(auth::avatar_path());
        }
        self.s.user = Some(me.user);
        match me.plan {
            Some(plan) => {
                self.s.account = Some(plan.account);
                self.login = Some(plan.url);
                self.refresh(None, cx);
            }
            // The saved subscription stays: the backend also answers null when it cannot reach the panel.
            None => {
                self.disconnect(cx);
                self.login = None;
                self.s.account = None;
                data::set_servers(vec![]);
                self.s.servers_gen += 1;
                self.pinger = None;
                if self.revs.loaded() {
                    self.sync(cx);
                }
            }
        }
    }

    /// Live updates while signed in with Telegram (not with a pasted link).
    fn go_live(&mut self, cx: &mut Context<Self>) {
        if self.live.is_some() || self.auth.session().is_none() {
            return;
        }
        let (live, rx) = live::Live::start(self.auth.clone());
        let task = cx.spawn(async move |this, cx| {
            while let Ok(ev) = rx.recv().await {
                if this.update(cx, |this, cx| this.live_event(ev, cx)).is_err() {
                    break;
                }
            }
        });
        self.live = Some((live, task));
    }

    fn kick_live(&self) {
        if let Some((live, _)) = &self.live {
            live.kick();
        }
    }

    fn live_event(&mut self, ev: live::Event, cx: &mut Context<Self>) {
        match ev {
            // Refetched like a manual refresh; a running tunnel stays up.
            live::Event::Servers(rev) => {
                if self.revs.seen(rev, self.s.busy) {
                    self.sync(cx);
                }
            }
            live::Event::Subscription(a) => {
                // Before the profile arrives `sync` brings the same numbers.
                let Some(old) = self.s.account.take() else { return };
                self.s.account = Some(Account { name: old.name, title: old.title, support: old.support, ..a });
                cx.notify();
            }
            live::Event::SignedOut => {
                self.sign_out(cx);
                self.s.error = Some(auth::Error::Unauthorized.to_string());
                cx.notify();
            }
        }
    }

    /// The tunnel left servers that are no longer in the subscription: back to the saved list.
    fn forget_gone(&mut self, cx: &mut Context<Self>) {
        if let Ok(body) = std::fs::read(subscription_path()) {
            let _ = self.load_subscription(&body, cx);
        }
    }

    /// The avatar saved last time, until the backend's arrives.
    fn cached_avatar(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let pic = cx.background_executor().spawn(async { std::fs::read(auth::avatar_path()).ok().and_then(|b| auth::picture(&b)) }).await;
            this.update(cx, |this, cx| {
                if this.s.avatar.is_none() && this.s.user.is_some() {
                    this.s.avatar = pic;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn fetch_avatar(&mut self, cx: &mut Context<Self>) {
        let client = self.auth.clone();
        cx.spawn(async move |this, cx| {
            let got = cx
                .background_executor()
                .spawn(async move {
                    let bytes = client.avatar().ok()?;
                    let Some(bytes) = bytes else { return Some(None) };
                    let pic = auth::picture(&bytes)?;
                    let _ = store::write(&auth::avatar_path(), &bytes, true);
                    Some(Some(pic))
                })
                .await;
            // A failed download keeps the cached picture.
            let Some(pic) = got else { return };
            this.update(cx, |this, cx| {
                if this.s.user.is_some() {
                    if pic.is_none() {
                        let _ = std::fs::remove_file(auth::avatar_path());
                    }
                    this.s.avatar = pic;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.cancel_sign_in(cx);
        self.disconnect(cx);
        self.live = None;
        self.revs = live::Revs::default();
        if let Some(session) = self.auth.take() {
            cx.background_executor().spawn(async move { auth::revoke(&session) }).detach();
        }
        let _ = std::fs::remove_file(auth::avatar_path());
        let _ = std::fs::remove_file(Login::path());
        let _ = std::fs::remove_file(subscription_path());
        self.login = None;
        data::set_servers(vec![]);
        self.s.signed_in = false;
        self.s.user = None;
        self.s.avatar = None;
        self.s.no_subscription = false;
        self.s.error = None;
        self.s.account = None;
        self.s.server_id = "none";
        self.s.entry_id = None;
        self.s.recents.clear();
        self.s.servers_gen += 1;
        self.pinger = None;
        self.tester = None;
        self.s.testing = false;
        self.s.pending.clear();
        cx.notify();
    }

    // ---------- connection ----------

    fn plan(&self) -> Plan {
        Plan {
            exit: data::server(self.s.server_id),
            entry: self.s.entry_id.map(data::server),
            protocol: self.s.protocol,
            default_route: self.s.default_route,
            tun: self.s.uses_tun(),
            system_proxy: self.s.system_wide && !self.s.tun && crate::os::SYSTEM_PROXY,
            kill_switch: self.s.kill_switch,
            apps: self.s.app_routes.clone(),
            sites: self.s.sites.clone(),
            servers: data::servers(),
            tunnel_rules: if self.region_rules.is_empty() {
                self.lists.tunnel.clone()
            } else {
                Arc::new(self.lists.tunnel.iter().chain(self.region_rules.iter()).cloned().collect())
            },
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
            self.s.error = Some(if self.s.signed_in { t!("vpn.error.no_server").into() } else { t!("vpn.error.sign_in_to_connect").into() });
            cx.notify();
            return;
        }
        if !self.gone.is_empty() && [Some(self.s.server_id), self.s.entry_id].iter().flatten().all(|id| !self.gone.contains(id)) {
            self.forget_gone(cx);
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

    fn started(&mut self, attempt: u64, routing: Routing, result: Result<Core, String>, cx: &mut Context<Self>) {
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
            self.s.error = Some(why.map_or_else(|| t!("vpn.error.connection_lost").into(), |w| t!("vpn.error.connection_lost_why", why = w)));
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
            None => (0., 0.),
        };
        self.bytes = Some((now, m.down, m.up));
        let latency = m.delays.get("proxy").copied().flatten().or(self.s.stats.latency);
        if latency.is_some() {
            self.failures = 0;
        }
        self.s.stats = Stats { down, up, latency };
        self.suggest(cx);
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
        let stopped = self.core.take().map(|core| cx.background_executor().spawn(async move { core.stop() }));
        // Off the tunnel now: has the network changed meanwhile? (Once the core is down, so the lookup is direct.)
        cx.spawn(async move |this, cx| {
            if let Some(stopped) = stopped {
                stopped.await;
            }
            this.update(cx, |this, cx| this.check_region(false, cx)).ok();
        })
        .detach();
        self.s.conn = Conn::Off;
        self.s.connected_at = None;
        self.s.stats = Stats::default();
        if !self.gone.is_empty() {
            self.forget_gone(cx);
        }
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
        // Tunnel up or down: the live socket's route changed under it.
        let tunneled = self.s.conn == Conn::On && self.core.as_ref().is_some_and(|c| c.tun);
        if std::mem::replace(&mut self.tunneled, tunneled) != tunneled {
            self.kick_live();
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

    // ---------- latency ----------

    /// A latency test now and every 5 minutes, connected or not.
    fn start_pinger(&mut self, cx: &mut Context<Self>) {
        if self.pinger.is_some() || data::servers().is_empty() {
            return;
        }
        self.pinger = Some(cx.spawn(async move |this, cx| {
            while this.update(cx, |this, cx| this.test_pings(cx)).is_ok() {
                cx.background_executor().timer(Duration::from_secs(300)).await;
            }
        }));
    }

    /// Measure every server with a real handshake, unless a test is already running. Connected, the running core
    /// does it beside the tunnel; otherwise a throwaway core.
    pub fn test_pings(&mut self, cx: &mut Context<Self>) {
        let servers: Vec<_> = data::servers().into_iter().filter(|s| !self.gone.contains(&s.id)).collect();
        if self.s.testing || servers.is_empty() {
            return;
        }
        let via = self.core.as_ref().map(|c| c.ports.ping);
        let rx = xray::ping_all(servers.clone(), self.s.protocol, via);
        self.s.testing = true;
        self.s.pending = servers.iter().map(|s| s.id).collect();
        self.tester = Some(cx.spawn(async move |this, cx| {
            while let Ok(got) = rx.recv().await {
                if this.update(cx, |this, cx| this.pinged(got, via, cx)).is_err() {
                    return;
                }
            }
            this.update(cx, |this, cx| this.tested(cx)).ok();
        }));
        cx.notify();
    }

    fn pinged(&mut self, got: xray::Pinged, via: Option<u16>, cx: &mut Context<Self>) {
        match got {
            Ok((id, ms)) => {
                self.s.pending.retain(|p| *p != id);
                // A failure only counts if the core it went through is still the one running.
                let gone = via.is_some() && self.core.as_ref().map(|c| c.ports.ping) != via;
                if ms.is_some() || !gone {
                    data::set_pings([(id, ms)]);
                }
            }
            Err(e) => self.s.error = Some(e),
        }
        self.s.servers_gen += 1;
        cx.notify();
    }

    fn tested(&mut self, cx: &mut Context<Self>) {
        self.tester = None;
        self.s.testing = false;
        // Every server reported (the test core started).
        if self.s.pending.is_empty() {
            self.s.tested_at = Some(Instant::now());
        }
        self.s.pending.clear();
        if self.s.conn == Conn::Off && self.s.recents.is_empty() && self.s.entry_id.is_none() {
            self.s.server_id = data::fastest().id;
        }
        self.s.servers_gen += 1;
        cx.notify();
    }
}
