//! Application state and actions.
use std::time::Duration;

use gpui::{
    App, AppContext, Context, Element, Entity, FocusHandle, Focusable, KeyBinding, Refineable, Render, Subscription, Window, actions, div, prelude::*,
};

use crate::data::{self, AppRule, RuleMode};
use crate::fx::Presence;
use crate::input::TextInput;
use crate::map::MapState;
use crate::theme::{Theme, theme};
use crate::ui::Tw;

actions!(hitori, [TogglePalette, Dismiss, MainWindow, TrayWindow, SelectNext, SelectPrev, Confirm]);

pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("secondary-k", TogglePalette, Some("Hitori")),
        KeyBinding::new("escape", Dismiss, Some("Hitori")),
        KeyBinding::new("1", MainWindow, Some("Hitori && !TextInput")),
        KeyBinding::new("2", TrayWindow, Some("Hitori && !TextInput")),
        KeyBinding::new("down", SelectNext, Some("Palette")),
        KeyBinding::new("up", SelectPrev, Some("Palette")),
        KeyBinding::new("enter", Confirm, Some("Palette")),
        KeyBinding::new("enter", Confirm, Some("SiteField")),
    ]
}

#[derive(Clone, Copy, PartialEq)]
pub enum Conn {
    Off,
    Connecting,
    On,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Variant {
    Main,
    Tray,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Panel {
    Account,
    Settings,
    Routes,
    Log,
}

#[derive(Clone, Copy, PartialEq)]
pub enum SettingsTab {
    Connection,
    Network,
    Location,
    Appearance,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 4] = [Self::Connection, Self::Network, Self::Location, Self::Appearance];

    pub fn label(self) -> &'static str {
        match self {
            Self::Connection => t!("app.settings_tab.connection"),
            Self::Network => t!("app.settings_tab.network"),
            Self::Location => t!("app.settings_tab.location"),
            Self::Appearance => t!("app.settings_tab.appearance"),
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Connection => "shield-check",
            Self::Network => "globe",
            Self::Location => "map",
            Self::Appearance => "wand-sparkles",
        }
    }
}

pub use crate::data::Protocol;

/// What the palette is for: the main exit, the main entry hop, or one app's or website's route settings.
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Exit,
    Entry,
    Rule(&'static str),
    Site(&'static str),
}

/// What the main window shows beside the sidebar: the map, or every rule at full size.
#[derive(Clone, Copy, PartialEq)]
pub enum Page {
    Map,
    Rules,
}

/// Where a menu trigger lives; the same rule can be on screen twice (sidebar and sheet).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Place {
    Side,
    Sheet,
    Page,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Menu {
    NavUser,
    TrayAccount,
    AddApp,
    /// A settings dropdown, by its id.
    Select(&'static str),
}

/// Where the isolated ping ring goes this frame, in window pixels: centre and the map rect that clips it.
#[derive(Clone, Copy, PartialEq)]
pub struct Ring {
    pub at: (f32, f32),
    pub clip: (f32, f32, f32, f32),
    pub on: bool,
}

/// Window root: the cached app view plus the ping-ring layer, whose 100 Hz frames redraw only the ring.
pub struct Shell {
    pub hitori: Entity<Hitori>,
    /// Tray popover: the window is a transparent overlay (of the work area, or just the card) and the card sits in it.
    pub card: Option<crate::tray::Card>,
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(card) = self.card {
            // Placed in the window the compositor actually gave us, which can differ from the one asked for.
            let vp = window.viewport_size();
            let (x, y, w, h) = card.place((f32::from(vp.width), f32::from(vp.height)));
            self.hitori.read(cx).frame.set(Some(gpui::size(gpui::px(w), gpui::px(h))));
            // Clicks outside the card close the popover (the overlay covers everything but the bar).
            return div()
                .size_full()
                .on_mouse_down(gpui::MouseButton::Left, |_, window, _| window.remove_window())
                .on_mouse_down(gpui::MouseButton::Right, |_, window, _| window.remove_window())
                .child(
                    div()
                        .absolute()
                        .left(gpui::px(x))
                        .top(gpui::px(y))
                        .w(gpui::px(w))
                        .h(gpui::px(h))
                        .occlude()
                        .child(gpui::AnyView::from(self.hitori.clone()).cached(gpui::StyleRefinement::default().size_full())),
                );
        }
        div()
            .size_full()
            .relative()
            .child(gpui::AnyView::from(self.hitori.clone()).cached(gpui::StyleRefinement::default().size_full()))
            .child(RingLayer(self.hitori.clone()))
    }
}

/// The isolated ping ring, built in prepaint after the cached app view has placed it, so it never trails the map by a frame.
struct RingLayer(Entity<Hitori>);

impl IntoElement for RingLayer {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for RingLayer {
    type RequestLayoutState = ();
    type PrepaintState = Option<gpui::AnyElement>;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, _: Option<&gpui::GlobalElementId>, _: Option<&gpui::InspectorElementId>, window: &mut Window, cx: &mut App) -> (gpui::LayoutId, ()) {
        let mut style = gpui::Style::default();
        style.refine(&gpui::StyleRefinement::default().absolute().inset_0());
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<gpui::Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::AnyElement> {
        let r = self.0.read(cx).ring.get()?;
        let t = theme(cx);
        let (x, y, w, h) = r.clip;
        let mut ring = div()
            .w(gpui::px(w))
            .h(gpui::px(h))
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .left(gpui::px(r.at.0 - x - 12.))
                    .top(gpui::px(r.at.1 - y - 12.))
                    .size(gpui::px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(crate::map::ping_ring(r.on, &t))
                    .child(crate::map::selected_dot(r.on, &t)),
            )
            .into_any_element();
        ring.layout_as_root(bounds.size.into(), window, cx);
        ring.prepaint_at(bounds.origin + gpui::point(gpui::px(x), gpui::px(y)), window, cx);
        Some(ring)
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: gpui::Bounds<gpui::Pixels>,
        _: &mut (),
        ring: &mut Option<gpui::AnyElement>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(ring) = ring {
            ring.paint(window, cx);
        }
    }
}

pub use crate::vpn::{Shared, Vpn};

pub struct Hitori {
    pub focus: FocusHandle,
    pub variant: Variant,
    /// Shown as the tray icon's popover card (no backdrop, transparent corners).
    pub popover: bool,
    /// The popover card's size: what the layout fills instead of the window.
    pub frame: std::cell::Cell<Option<gpui::Size<gpui::Pixels>>>,
    /// This window's copy of the shared state; edits are pushed to `vpn` after each render.
    pub s: Shared,
    published: Shared,
    pub vpn: Entity<Vpn>,
    pub sidebar_open: bool,
    pub panel: Option<Panel>,
    pub palette: Option<Mode>,
    pub palette_index: usize,
    palette_query: gpui::SharedString,
    pub search: Entity<TextInput>,
    /// Subscription link field on the Account sheet.
    pub link: Entity<TextInput>,
    /// The sign-in screen shows the subscription link field instead of only Telegram.
    pub paste_link: bool,
    /// New website rule on the rules page and sheet.
    pub site_input: Entity<TextInput>,
    /// Local proxy ports in Settings › Network.
    pub socks_input: Entity<TextInput>,
    pub http_input: Entity<TextInput>,
    /// The DoH URL or DoT server, whichever the DNS mode uses.
    pub dns_input: Entity<TextInput>,
    pub page: Page,
    /// The inspector's server list is ordered by the last latency test.
    pub sort_ping: bool,
    /// Countries opened in that list (flag codes).
    pub open_countries: Vec<&'static str>,
    pub menu: Option<Menu>,
    pub map: MapState,
    pub ring: std::cell::Cell<Option<Ring>>,
    pub ring_isolated: std::cell::Cell<bool>,
    pub ring_clip: std::cell::Cell<(f32, f32, f32, f32)>,
    /// Map-local rects that float over the map (toasts, the narrow card, the sidebar button).
    pub keepout: std::cell::RefCell<Vec<[f32; 4]>>,
    pub menu_fx: Presence<Menu>,
    pub panel_fx: Presence<Panel>,
    pub settings_tab: SettingsTab,
    pub palette_fx: Presence<Mode>,
    /// The language this window's titles and placeholders are in.
    lang: crate::i18n::Lang,
    _search_changes: Subscription,
    _site_changes: Subscription,
    /// What the Network fields held at the last edit; caret blinks notify too.
    network_typed: Vec<gpui::SharedString>,
    _network_changes: Vec<Subscription>,
    _vpn_changes: Subscription,
}

/// A port typed into a settings field.
pub fn port(input: &Entity<TextInput>, cx: &App) -> Option<u16> {
    input.read(cx).content.trim().parse().ok().filter(|p| *p > 0)
}

/// The server the DNS field shows in this mode.
pub fn dns_server(dns: &crate::xray::Dns) -> &str {
    if dns.mode == crate::xray::DnsMode::Tls { &dns.tls } else { &dns.https }
}

impl Hitori {
    pub fn new(variant: Variant, vpn: Entity<Vpn>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let t = theme(cx);
        let search = cx.new(|cx| TextInput::new(&search_placeholder(Mode::Exit), t.muted_fg, t.foreground, cx));
        let link = cx.new(|cx| TextInput::new("https://…/sub/…", t.muted_fg, t.foreground, cx));
        let site_input = cx.new(|cx| TextInput::new(t!("app.add_site_placeholder"), t.muted_fg, t.foreground, cx));
        let site_changes = cx.observe(&site_input, |_, _, cx| cx.notify());
        let (proxy, dns) = (vpn.read(cx).s.proxy.clone(), vpn.read(cx).s.dns.clone());
        let field = |text: &str, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut input = TextInput::new("", t.muted_fg, t.foreground, cx);
                input.set_content(text);
                input
            })
        };
        let (socks_input, http_input) = (field(&proxy.socks.to_string(), cx), field(&proxy.http.to_string(), cx));
        let dns_input = field(dns_server(&dns), cx);
        let network_changes = [&socks_input, &http_input, &dns_input].map(|i| cx.observe(i, |this, _, cx| this.network_edited(cx))).into();
        // The caret blink notifies too; only an edit moves the selection back to the top.
        let changes = cx.observe(&search, |this, search, cx| {
            let query = search.read(cx).content.clone();
            if query != this.palette_query {
                this.palette_query = query;
                this.palette_index = 0;
                cx.notify();
            }
        });
        let focus = cx.focus_handle();
        window.focus(&focus);
        // Model changes (handshake, IP lookup, other windows, the tray) flow back here. Local edits not yet pushed win,
        // except for the fields only the model changes on its own.
        let vpn_changes = cx.observe(&vpn, |this, vpn, cx| {
            let model = vpn.read(cx).s.clone();
            if this.s != this.published {
                this.s.adopt(&model);
                this.published = model;
                this.push(cx);
            } else {
                this.s = model.clone();
                this.published = model;
            }
            cx.notify();
        });
        let shared = vpn.read(cx).s.clone();
        let mut this = Self {
            focus,
            variant,
            popover: false,
            frame: Default::default(),
            s: shared.clone(),
            published: shared,
            vpn,
            sidebar_open: true,
            panel: None,
            palette: None,
            palette_index: 0,
            palette_query: Default::default(),
            search,
            link,
            paste_link: false,
            site_input,
            socks_input,
            http_input,
            dns_input,
            page: Page::Map,
            sort_ping: false,
            open_countries: vec![],
            menu: None,
            map: MapState::new(),
            ring: Default::default(),
            ring_isolated: Default::default(),
            ring_clip: Default::default(),
            keepout: Default::default(),
            menu_fx: Presence::new(),
            panel_fx: Presence::new(),
            settings_tab: SettingsTab::Connection,
            palette_fx: Presence::new(),
            lang: crate::i18n::current(),
            _search_changes: changes,
            _site_changes: site_changes,
            network_typed: vec![],
            _network_changes: network_changes,
            _vpn_changes: vpn_changes,
        };
        if let Ok(state) = std::env::var("HITORI_STATE") {
            this.apply_state(&state, window, cx);
        }
        this
    }

    /// Start in a given state, for screenshots: `on,entry:node-05,menu:nav,panel:routes,palette:exit,tray,light,small,snooze:s-discord`.
    fn apply_state(&mut self, spec: &str, window: &mut Window, cx: &mut Context<Self>) {
        let leak = |s: &str| -> &'static str { Box::leak(s.to_owned().into_boxed_str()) };
        let tokens: Vec<&str> = spec.split(',').map(str::trim).collect();
        for (i, token) in tokens.iter().enumerate() {
            let (key, value) = token.split_once(':').unwrap_or((token, ""));
            match key {
                // Apply the remaining tokens later, to capture transitions.
                "delay" => {
                    let rest = tokens[i + 1..].join(",");
                    let ms = value.parse().unwrap_or(1000);
                    cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(Duration::from_millis(ms)).await;
                        this.update_in(cx, |this, window, cx| this.apply_state(&rest, window, cx)).ok();
                    })
                    .detach();
                    return;
                }
                "close" => {
                    self.menu = None;
                    self.panel = None;
                    self.close_palette(window, cx);
                }
                "server" => self.s.server_id = data::server(value).id,
                "entry" => self.s.entry_id = Some(data::server(value).id),
                "pick" => self.map.pick = Some(data::server(value).id),
                "on" => self.with_vpn(cx, |v, cx| v.connected(cx)),
                "connecting" => self.connect(cx),
                "ping" => self.test_pings(cx),
                "sort" => self.sort_ping = true,
                "open" => self.open_countries.push(data::intern(&value.to_uppercase())),
                "load" => {
                    if let Some((id, pct)) = value.split_once('=') {
                        data::set_loads([(data::intern(id), pct.parse().unwrap_or(0))]);
                    }
                }
                "tray" => self.variant = Variant::Tray,
                "light" => self.set_dark(false, cx),
                "lang" => self.set_language(crate::i18n::Lang::parse(value), cx),
                "tab" => self.settings_tab = SettingsTab::ALL.into_iter().find(|k| k.icon() == value).unwrap_or(SettingsTab::Connection),
                "select" => self.menu = Some(Menu::Select(leak(value))),
                "small" => self.s.rules_big = false,
                "route" => self.s.default_route = RuleMode::parse(value),
                "page" => self.page = if value == "rules" { Page::Rules } else { Page::Map },
                "siterule" => self.open_palette(Mode::Site(data::intern(value)), window, cx),
                "collapsed" => self.sidebar_open = false,
                // A synthetic pointer move, to capture hover states.
                "mouse" => {
                    let (x, y) = value.split_once('x').map_or((0., 0.), |(x, y)| (x.parse().unwrap_or(0.), y.parse().unwrap_or(0.)));
                    let event = gpui::MouseMoveEvent { position: gpui::point(gpui::px(x), gpui::px(y)), ..Default::default() };
                    window.defer(cx, move |window, cx| {
                        window.dispatch_event(gpui::PlatformInput::MouseMove(event), cx);
                    });
                }
                "zoom" | "pan" => self.map.script(key, value),
                "snooze" => self.s.snoozed.push(leak(value)),
                // Sign-in screen: `signin` (signed out), `waiting`, `paste`, `error:<text>`, `nosub:<name>`.
                "signin" => self.s.signed_in = false,
                "waiting" => self.s.telegram = crate::vpn::Telegram::Browser("https://oauth.telegram.org/auth".into()),
                "paste" => self.paste_link = true,
                "error" => self.s.error = Some(value.to_owned()),
                "nosub" => {
                    self.s.signed_in = true;
                    self.s.no_subscription = true;
                    self.s.user = Some(crate::auth::User { name: value.to_owned(), username: Some(value.to_lowercase()), has_avatar: false });
                }
                "panel" => {
                    self.panel = Some(match value {
                        "account" => Panel::Account,
                        "routes" => Panel::Routes,
                        "log" => Panel::Log,
                        _ => Panel::Settings,
                    })
                }
                "palette" => self.open_palette(if value == "entry" { Mode::Entry } else { Mode::Exit }, window, cx),
                "rule" => self.open_palette(Mode::Rule(data::app(value).id), window, cx),
                "type" => self.search.update(cx, |input, cx| {
                    input.content = value.to_owned().into();
                    cx.notify();
                }),
                "menu" => {
                    self.menu = Some(match value {
                        "tray" => Menu::TrayAccount,
                        "add" => Menu::AddApp,
                        _ => Menu::NavUser,
                    });
                }
                _ => {}
            }
        }
        cx.notify();
    }

    pub fn theme(&self, cx: &App) -> Theme {
        theme(cx)
    }

    /// The area this view lays out in: the popover card, else the window.
    pub fn viewport(&self, window: &Window) -> gpui::Size<gpui::Pixels> {
        self.frame.get().unwrap_or_else(|| window.viewport_size())
    }

    /// Send local edits to the model, which tells the other windows and the tray.
    pub fn push(&mut self, cx: &mut Context<Self>) {
        if self.s != self.published {
            let s = self.s.clone();
            self.published = s.clone();
            self.vpn.update(cx, |v, cx| {
                v.s = s;
                cx.notify();
            });
        }
    }

    /// Run a model operation on the current state and take its result back.
    pub fn with_vpn(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Vpn, &mut Context<Vpn>)) {
        self.push(cx);
        self.vpn.update(cx, f);
        self.s = self.vpn.read(cx).s.clone();
        self.published = self.s.clone();
        cx.notify();
    }

    pub fn connect(&mut self, cx: &mut Context<Self>) {
        self.with_vpn(cx, |v, cx| v.connect(cx));
    }

    pub fn test_pings(&mut self, cx: &mut Context<Self>) {
        self.with_vpn(cx, |v, cx| v.test_pings(cx));
    }

    /// Power button: without an account it opens sign-in instead.
    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        if !self.s.signed_in {
            self.open_panel(Panel::Account, cx);
            return;
        }
        self.with_vpn(cx, |v, cx| v.toggle(cx));
    }

    pub fn sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let link = self.link.read(cx).content.to_string();
        if link.trim().is_empty() {
            window.focus(&self.link.read(cx).focus_handle(cx));
            return;
        }
        self.with_vpn(cx, |v, cx| v.sign_in(link, cx));
    }

    /// Detect the location again and reload its blocked lists; `self.s.region` has the result (Settings, onboarding).
    pub fn recheck_region(&mut self, cx: &mut Context<Self>) {
        self.with_vpn(cx, |v, cx| v.recheck_region(cx));
    }

    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        self.panel = None;
        self.palette = None;
        self.paste_link = false;
        self.link.update(cx, |input, _| input.reset());
        self.with_vpn(cx, |v, cx| v.sign_out(cx));
    }

    /// Picking a location always connects there; picking the live one again does nothing.
    pub fn go(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let was = (self.s.conn, self.s.server_id);
        self.s.server_id = id;
        self.map.pick = None;
        if self.s.entry_id == Some(id) {
            self.s.entry_id = None;
        }
        self.close_palette(window, cx);
        if was.0 == Conn::Off || was.1 != id {
            self.connect(cx);
        }
    }

    /// Servers without the transport disappear from the lists, so the exit and first hop move to ones that have it.
    pub fn set_protocol(&mut self, p: Protocol, cx: &mut Context<Self>) {
        self.s.protocol = p;
        self.with_vpn(cx, |v, _| v.keep_shown());
    }

    pub fn via(&mut self, id: Option<&'static str>, window: &mut Window, cx: &mut Context<Self>) {
        let was = (self.s.conn, self.s.entry_id);
        self.s.entry_id = if id == Some(self.s.server_id) { None } else { id };
        self.close_palette(window, cx);
        if was.0 == Conn::Off || was.1 != id {
            self.connect(cx);
        }
    }

    pub fn route(&mut self, app: &'static str, route: Option<AppRule>, window: &mut Window, cx: &mut Context<Self>) {
        match route {
            Some(r) => match self.s.app_routes.iter_mut().find(|(a, _)| *a == app) {
                Some(slot) => slot.1 = r,
                None => self.s.app_routes.push((app, r)),
            },
            None => self.s.app_routes.retain(|(a, _)| *a != app),
        }
        self.menu = None;
        self.close_palette(window, cx);
        cx.notify();
    }

    pub fn site(&mut self, target: &'static str, rule: Option<AppRule>, cx: &mut Context<Self>) {
        match rule {
            Some(r) => match self.s.sites.iter_mut().find(|(t, _)| *t == target) {
                Some(slot) => slot.1 = r,
                None => self.s.sites.push((target, r)),
            },
            None => self.s.sites.retain(|(t, _)| *t != target),
        }
        self.menu = None;
        cx.notify();
    }

    pub fn app_route(&self, app: &str) -> Option<&AppRule> {
        self.s.app_routes.iter().find(|(a, _)| *a == app).map(|(_, r)| r)
    }

    pub fn site_rule(&self, target: &str) -> Option<&AppRule> {
        self.s.sites.iter().find(|(t, _)| *t == target).map(|(_, r)| r)
    }

    /// The typed website gets a rule (the opposite of the default, so it changes something) and its settings open.
    pub fn add_site(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.site_input.read(cx).content.to_string();
        let Some(host) = data::domain(&typed) else {
            window.focus(&self.site_input.read(cx).focus_handle(cx));
            return;
        };
        let target = data::intern(&host);
        if self.site_rule(target).is_none() {
            let mode = if self.s.default_route == RuleMode::Vpn { RuleMode::Direct } else { RuleMode::Vpn };
            self.site(target, Some(AppRule::mode(mode)), cx);
        }
        self.site_input.update(cx, |input, _| input.reset());
        self.open_palette(Mode::Site(target), window, cx);
    }

    /// Accept a suggestion: the app, or just the site for browsers, always goes through the VPN.
    pub fn accept(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(hit) = self.s.suggestions.iter().find(|s| s.id == id).cloned() else { return };
        self.s.suggestions.retain(|s| s.id != id);
        match hit.scope {
            data::Scope::Site => match self.s.sites.iter_mut().find(|(t, _)| *t == hit.target) {
                Some(slot) => slot.1.mode = RuleMode::Vpn,
                None => self.s.sites.push((hit.target, AppRule::mode(RuleMode::Vpn))),
            },
            data::Scope::App => match self.s.app_routes.iter_mut().find(|(a, _)| *a == hit.app_id) {
                Some(slot) => slot.1.mode = RuleMode::Vpn,
                None => self.s.app_routes.push((hit.app_id, AppRule::mode(RuleMode::Vpn))),
            },
        }
        cx.notify();
    }

    pub fn dismiss(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(hit) = self.s.suggestions.iter().find(|s| s.id == id) {
            self.s.dismissed.push(hit.target.to_owned());
        }
        self.s.suggestions.retain(|s| s.id != id);
        cx.notify();
    }

    pub fn snooze(&mut self, id: &'static str, cx: &mut Context<Self>) {
        self.s.snoozed.push(id);
        cx.notify();
    }

    pub fn open_palette(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.palette = Some(mode);
        self.palette_index = 0;
        self.palette_query = Default::default();
        let placeholder = search_placeholder(mode);
        self.search.update(cx, |input, cx| {
            input.reset();
            input.set_placeholder(&placeholder);
            input.restart_blink(cx);
        });
        window.focus(&self.search.read(cx).focus_handle(cx));
        cx.notify();
    }

    pub fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            window.focus(&self.focus);
        }
        cx.notify();
    }

    /// Every rule: a page beside the sidebar in the main window, the sheet in the tray.
    pub fn open_rules(&mut self, cx: &mut Context<Self>) {
        if self.variant == Variant::Main {
            self.menu = None;
            self.page = Page::Rules;
            cx.notify();
        } else {
            self.open_panel(Panel::Routes, cx);
        }
    }

    pub fn open_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        self.menu = None;
        self.panel = Some(panel);
        cx.notify();
    }

    /// Valid values go to the settings; invalid ones stay in their fields, flagged.
    fn network_edited(&mut self, cx: &mut Context<Self>) {
        let typed: Vec<_> = [&self.socks_input, &self.http_input, &self.dns_input].iter().map(|i| i.read(cx).content.clone()).collect();
        if typed == self.network_typed {
            return;
        }
        self.network_typed = typed;
        cx.notify();
        if let (Some(socks), Some(http)) = (port(&self.socks_input, cx), port(&self.http_input, cx)) {
            let proxy = crate::xray::LocalProxy { socks, http, ..self.s.proxy.clone() };
            if proxy.check().is_ok() {
                self.s.proxy = proxy;
            }
        }
        if let Ok(dns) = self.typed_dns(cx) {
            self.s.dns = dns;
        }
    }

    /// The DNS settings with the typed server, if it is valid.
    pub fn typed_dns(&self, cx: &App) -> Result<crate::xray::Dns, String> {
        let text = self.dns_input.read(cx).content.trim().to_owned();
        let mut dns = self.s.dns.clone();
        if dns.mode != crate::xray::DnsMode::Subscription {
            dns.set_server(&text);
        }
        dns.check().map(|_| dns).map_err(|_| t!("xray.error.dns_server").into())
    }

    /// Own DNS server on: the typed one, or Cloudflare's DoH when the field holds nothing valid. Off: the subscription's.
    pub fn set_custom_dns(&mut self, on: bool, cx: &mut Context<Self>) {
        let mut dns = self.s.dns.clone();
        if !on {
            dns.mode = crate::xray::DnsMode::Subscription;
        } else {
            dns.set_server(&self.dns_input.read(cx).content);
            if dns.check().is_err() {
                let fallback = crate::xray::DNS_PRESETS[0].1;
                dns.set_server(fallback);
                self.dns_input.update(cx, |input, cx| {
                    input.set_content(fallback);
                    cx.notify();
                });
            }
        }
        self.s.dns = dns;
    }

    /// What is wrong with the typed ports.
    pub fn ports_error(&self, cx: &App) -> Option<String> {
        let (Some(socks), Some(http)) = (port(&self.socks_input, cx), port(&self.http_input, cx)) else {
            return Some(t!("xray.error.ports_range").into());
        };
        crate::xray::LocalProxy { socks, http, ..self.s.proxy.clone() }.check().err()
    }

    pub fn set_dark(&mut self, dark: bool, cx: &mut Context<Self>) {
        cx.set_global(if dark { Theme::dark() } else { Theme::light() });
        self.s.dark = dark;
        let t = theme(cx);
        for input in [&self.search, &self.link, &self.socks_input, &self.http_input, &self.dns_input] {
            input.update(cx, |input, _| {
                input.placeholder_color = t.muted_fg;
                input.caret = t.foreground;
            });
        }
        self.map.restyle();
        cx.notify();
    }

    /// Switch every window and the tray now; `None` follows the system.
    pub fn set_language(&mut self, choice: Option<crate::i18n::Lang>, cx: &mut Context<Self>) {
        self.s.language = choice;
        crate::i18n::set(crate::i18n::resolve(choice));
        data::relabel();
        #[cfg(target_os = "macos")]
        crate::set_menus(cx);
        cx.notify();
    }

    /// Window title and text field placeholders after a language change.
    fn relabel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lang = crate::i18n::current();
        window.set_window_title(t!("app.title"));
        let search = search_placeholder(self.palette.unwrap_or(Mode::Exit));
        for (input, text) in [(&self.search, search.as_str()), (&self.site_input, t!("app.add_site_placeholder"))] {
            input.update(cx, |input, cx| {
                input.set_placeholder(text);
                cx.notify();
            });
        }
    }

    pub fn toggle_menu(&mut self, menu: Menu, cx: &mut Context<Self>) {
        self.menu = if self.menu == Some(menu) { None } else { Some(menu) };
        cx.notify();
    }

    fn on_toggle_palette(&mut self, _: &TogglePalette, window: &mut Window, cx: &mut Context<Self>) {
        if self.gated() {
            return;
        }
        if self.palette.is_some() { self.close_palette(window, cx) } else { self.open_palette(Mode::Exit, window, cx) }
    }

    fn on_dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() || self.map.pick.take().is_some() {
        } else if self.palette.is_some() {
            self.close_palette(window, cx);
        } else if self.panel.is_some() {
            self.panel = None;
        } else if self.popover {
            window.remove_window();
        }
        cx.notify();
    }

    fn on_variant(&mut self, variant: Variant, cx: &mut Context<Self>) {
        // The cockpit is laid out for a whole window, not the popover's card.
        if self.popover {
            return;
        }
        self.variant = variant;
        self.menu = None;
        self.map.refit();
        cx.notify();
    }

    fn on_select(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.palette_move(delta, cx);
    }

    fn on_confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_confirm(window, cx);
    }
}

fn search_placeholder(mode: Mode) -> String {
    match mode {
        Mode::Entry => t!("app.search.entry").to_owned(),
        Mode::Rule(a) => t!("app.search.rule", name = data::app(a).name),
        Mode::Site(s) => t!("app.search.rule", name = s),
        Mode::Exit => t!("app.search.exit").to_owned(),
    }
}

impl Focusable for Hitori {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Hitori {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        self.menu_fx.sync(self.menu);
        self.panel_fx.sync(self.panel);
        self.palette_fx.sync(self.palette);
        if self.menu_fx.animating(150., 150.) || self.panel_fx.animating(500., 300.) || self.palette_fx.animating(200., 200.) {
            window.request_animation_frame();
        }
        if self.s != self.published {
            cx.defer_in(window, |this, _, cx| this.push(cx));
        }
        if self.lang != crate::i18n::current() {
            self.relabel(window, cx);
        }
        self.ring.set(None);
        // Signed out (or without a subscription) every window shows only the sign-in screen.
        if self.gated() {
            (self.menu, self.panel, self.palette) = (None, None, None);
            return div()
                .key_context("Hitori")
                .track_focus(&self.focus)
                .on_action(cx.listener(Self::on_dismiss))
                .size_full()
                .font_family("Google Sans")
                .tnum()
                .text_size(gpui::px(16.))
                .line_height(gpui::px(24.))
                .text_color(t.foreground)
                .child(self.onboarding(window, cx))
                .into_any_element();
        }
        let open = self.menu.is_some() || self.menu_fx.shown(150.).is_some() || self.panel_fx.shown(300.).is_some() || self.palette_fx.shown(200.).is_some();
        self.ring_isolated.set(!open);
        let body = match self.variant {
            Variant::Main => self.cockpit(window, cx).into_any_element(),
            Variant::Tray => self.tray(window, cx).into_any_element(),
        };
        div()
            .key_context("Hitori")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_dismiss))
            .on_action(cx.listener(|this, _: &MainWindow, _, cx| this.on_variant(Variant::Main, cx)))
            .on_action(cx.listener(|this, _: &TrayWindow, _, cx| this.on_variant(Variant::Tray, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.on_select(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| this.on_select(-1, cx)))
            .on_action(cx.listener(Self::on_confirm))
            .relative()
            .size_full()
            .font_family("Google Sans")
            .tnum()
            .text_size(gpui::px(16.))
            .line_height(gpui::px(24.))
            .when(!self.popover, |d| d.bg(t.background))
            .text_color(t.foreground)
            .child(body)
            .children(self.menu.map(|_| self.menu_catcher(cx)))
            .children(self.panel_fx.shown(300.).map(|panel| self.sheet(panel, window, cx)))
            .children(self.palette_fx.shown(200.).map(|mode| self.palette_dialog(mode, window, cx)))
            .into_any_element()
    }
}
