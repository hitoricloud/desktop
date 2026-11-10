//! Application state and actions.
use std::time::Duration;

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, KeyBinding, Render, Subscription, Window, actions, div, prelude::*,
};

use crate::data::{self, AppRule, RuleMode, SiteRule};
use crate::fx::Presence;
use crate::input::TextInput;
use crate::map::MapState;
use crate::theme::{Theme, theme};
use crate::ui::Tw;

actions!(hitori, [TogglePalette, Dismiss, MainWindow, TrayWindow, SelectNext, SelectPrev, Confirm]);

pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-k", TogglePalette, Some("Hitori")),
        KeyBinding::new("escape", Dismiss, Some("Hitori")),
        KeyBinding::new("1", MainWindow, Some("Hitori && !TextInput")),
        KeyBinding::new("2", TrayWindow, Some("Hitori && !TextInput")),
        KeyBinding::new("down", SelectNext, Some("Palette")),
        KeyBinding::new("up", SelectPrev, Some("Palette")),
        KeyBinding::new("enter", Confirm, Some("Palette")),
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
}

pub use crate::data::Protocol;

/// What the palette is for: the main exit, the main entry hop, or one app's route settings.
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Exit,
    Entry,
    Rule(&'static str),
}

/// Where a menu trigger lives; the same rule can be on screen twice (sidebar and sheet).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Place {
    Side,
    Sheet,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Menu {
    NavUser,
    TrayAccount,
    SiteRule(&'static str, Place),
    AddApp,
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
    /// Tray popover: the window is a transparent overlay of the work area and the card sits at this point.
    pub card: Option<(f32, f32)>,
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let ring = self.hitori.read(cx).ring.get();
        if let Some((x, y)) = self.card {
            let (w, h) = crate::tray::POPUP;
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
            .children(ring.map(|r| {
                let (x, y, w, h) = r.clip;
                div().absolute().left(gpui::px(x)).top(gpui::px(y)).w(gpui::px(w)).h(gpui::px(h)).overflow_hidden().child(
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
            }))
    }
}

pub use crate::vpn::{Shared, Vpn};

pub struct Hitori {
    pub focus: FocusHandle,
    pub variant: Variant,
    /// Shown as the tray icon's popover card (no backdrop, transparent corners).
    pub popover: bool,
    /// This window's copy of the shared state; edits are pushed to `vpn` after each render.
    pub s: Shared,
    published: Shared,
    pub vpn: Entity<Vpn>,
    pub sidebar_open: bool,
    pub panel: Option<Panel>,
    pub palette: Option<Mode>,
    pub palette_index: usize,
    pub search: Entity<TextInput>,
    /// Subscription link field on the Account sheet.
    pub link: Entity<TextInput>,
    pub menu: Option<Menu>,
    pub map: MapState,
    pub ring: std::cell::Cell<Option<Ring>>,
    pub ring_isolated: std::cell::Cell<bool>,
    pub ring_clip: std::cell::Cell<(f32, f32, f32, f32)>,
    /// Map-local rects that float over the map (toasts, the narrow card, the sidebar button).
    pub keepout: std::cell::RefCell<Vec<[f32; 4]>>,
    pub menu_fx: Presence<Menu>,
    pub panel_fx: Presence<Panel>,
    pub palette_fx: Presence<Mode>,
    _search_changes: Subscription,
    _vpn_changes: Subscription,
}

impl Hitori {
    pub fn new(variant: Variant, vpn: Entity<Vpn>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let t = theme(cx);
        let search = cx.new(|cx| TextInput::new("Search countries", t.muted_fg, t.foreground, cx));
        let link = cx.new(|cx| TextInput::new("https://…/sub/…", t.muted_fg, t.foreground, cx));
        let changes = cx.observe(&search, |this, _, cx| {
            this.palette_index = 0;
            cx.notify();
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
            s: shared.clone(),
            published: shared,
            vpn,
            sidebar_open: true,
            panel: None,
            palette: None,
            palette_index: 0,
            search,
            link,
            menu: None,
            map: MapState::new(),
            ring: Default::default(),
            ring_isolated: Default::default(),
            ring_clip: Default::default(),
            keepout: Default::default(),
            menu_fx: Presence::new(),
            panel_fx: Presence::new(),
            palette_fx: Presence::new(),
            _search_changes: changes,
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
                "on" => self.with_vpn(cx, |v, cx| v.connected(cx)),
                "connecting" => self.connect(cx),
                "tray" => self.variant = Variant::Tray,
                "light" => self.set_dark(false, cx),
                "small" => self.s.rules_big = false,
                "route" => self.s.default_route = RuleMode::parse(value),
                "routes" => self.s.all_routes = value != "off",
                "collapsed" => self.sidebar_open = false,
                "snooze" => self.s.snoozed.push(leak(value)),
                "panel" => {
                    self.panel = Some(match value {
                        "account" => Panel::Account,
                        "routes" => Panel::Routes,
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
                    let (kind, arg) = value.split_once(':').unwrap_or((value, ""));
                    self.menu = Some(match kind {
                        "tray" => Menu::TrayAccount,
                        "site" => Menu::SiteRule(leak(arg), Place::Side),
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
    fn with_vpn(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Vpn, &mut Context<Vpn>)) {
        self.push(cx);
        self.vpn.update(cx, f);
        self.s = self.vpn.read(cx).s.clone();
        self.published = self.s.clone();
        cx.notify();
    }

    pub fn connect(&mut self, cx: &mut Context<Self>) {
        self.with_vpn(cx, |v, cx| v.connect(cx));
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

    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        self.link.update(cx, |input, _| input.reset());
        self.with_vpn(cx, |v, cx| v.sign_out(cx));
    }

    /// Picking a location always connects there; picking the live one again does nothing.
    pub fn go(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let was = (self.s.conn, self.s.server_id);
        self.s.server_id = id;
        if self.s.entry_id == Some(id) {
            self.s.entry_id = None;
        }
        self.close_palette(window, cx);
        if was.0 == Conn::Off || was.1 != id {
            self.connect(cx);
        }
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

    pub fn site(&mut self, target: &'static str, rule: Option<SiteRule>, cx: &mut Context<Self>) {
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

    pub fn site_rule(&self, target: &str) -> Option<SiteRule> {
        self.s.sites.iter().find(|(t, _)| *t == target).map(|(_, r)| *r)
    }

    /// Accept a suggestion: the app, or just the site for browsers, always goes through the VPN.
    pub fn accept(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(hit) = self.s.suggestions.iter().find(|s| s.id == id).cloned() else { return };
        self.s.suggestions.retain(|s| s.id != id);
        match hit.scope {
            data::Scope::Site => match self.s.sites.iter_mut().find(|(t, _)| *t == hit.target) {
                Some(slot) => slot.1 = SiteRule::Vpn,
                None => self.s.sites.push((hit.target, SiteRule::Vpn)),
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
        let placeholder = match mode {
            Mode::Entry => "Route through…".to_owned(),
            Mode::Rule(a) => format!("Search {} settings", data::app(a).name),
            Mode::Exit => "Search countries".to_owned(),
        };
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

    pub fn open_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        self.menu = None;
        self.panel = Some(panel);
        cx.notify();
    }

    pub fn set_dark(&mut self, dark: bool, cx: &mut Context<Self>) {
        cx.set_global(if dark { Theme::dark() } else { Theme::light() });
        self.s.dark = dark;
        let t = theme(cx);
        for input in [&self.search, &self.link] {
            input.update(cx, |input, _| {
                input.placeholder_color = t.muted_fg;
                input.caret = t.foreground;
            });
        }
        self.map.restyle();
        cx.notify();
    }

    pub fn toggle_menu(&mut self, menu: Menu, cx: &mut Context<Self>) {
        self.menu = if self.menu == Some(menu) { None } else { Some(menu) };
        cx.notify();
    }

    fn on_toggle_palette(&mut self, _: &TogglePalette, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() { self.close_palette(window, cx) } else { self.open_palette(Mode::Exit, window, cx) }
    }

    fn on_dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
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
        self.ring.set(None);
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
    }
}
