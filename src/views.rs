//! Main window (Cockpit) and tray.
use gpui::{AnyElement, Context, Corner, Div, Focusable, Hsla, MouseButton, SharedString, Window, anchored, deferred, div, prelude::*, px};

use crate::app::{Conn, Hitori, Menu, Mode, Page, Panel, Place};
use crate::data::{self, AppInfo, AppRule, RuleMode, Scope, Suggestion, ago, app, fixed1, js_num, server};
use crate::theme::{AMBER, EMERALD};
use crate::map::Padding;
use crate::fx::{Fx, fx};
use crate::theme::{Theme, color};
use crate::ui::{
    LG, MD, SM, Size, Tw, Variant, XL, avatar, button, button_icon, flag, icon, menu, menu_item, menu_label, menu_separator, meter,
    pulse, skeleton, spinner, tip,
};

pub const SIDEBAR: f32 = 288.;
pub const INSPECTOR: f32 = 320.;
/// Smallest main window: the narrow layout (the tray card's width over the map) and the tallest dialog with its 16 px
/// margins. Wider breakpoints only add the sidebar and inspector beside the map.
pub const MIN_SIZE: (f32, f32) = (crate::tray::POPUP.0, crate::palette::PALETTE_MAX_H + 32.);

/// State colour: blue protected, red unprotected, muted while connecting.
pub fn tone(conn: Conn, t: &Theme) -> Hsla {
    match conn {
        Conn::On => t.primary,
        Conn::Connecting => t.muted_fg,
        Conn::Off => t.destructive,
    }
}

/// Server load colour: green while there is room, amber when busy, red when nearly full.
pub fn load_tone(pct: u8) -> Hsla {
    color(match pct {
        0..60 => EMERALD,
        60..85 => AMBER,
        _ => crate::theme::RED,
    })
}

pub fn route_label(r: &AppRule) -> String {
    match (r.mode, r.exit) {
        (RuleMode::Vpn, Some(id)) => format!("Exit {}", server(id).country),
        (RuleMode::Vpn, None) => "Always VPN".into(),
        (RuleMode::Direct, _) => "Direct".into(),
        (RuleMode::Block, _) => "Blocked".into(),
        (RuleMode::Auto, _) => "Auto".into(),
    }
}

pub fn subject(s: &Suggestion) -> String {
    if s.scope == Scope::Site || s.app_id.is_empty() { s.target.to_owned() } else { app(s.app_id).name.to_owned() }
}

pub fn suggestion_detail(s: &Suggestion) -> String {
    let mut parts = vec![s.reason.to_owned(), format!("worked via {}", server(s.via_id).country)];
    if let Some((a, b)) = s.speed {
        parts.push(format!("{} → {} Mbit/s", js_num(a), js_num(b)));
    }
    if s.hits > 1 {
        parts.push(format!("{}× today", s.hits));
    }
    parts.join(" · ")
}

pub fn app_badge(a: &AppInfo) -> Div {
    div()
        .flex_none()
        .size(px(20.))
        .rounded(px(SM))
        .flex()
        .items_center()
        .justify_center()
        .text_px(10., 20.)
        .bold()
        .bg(color(a.badge.0).opacity(0.2))
        .text_color(color(a.badge.1))
        .child(a.initials)
}

pub fn site_badge(t: &Theme) -> Div {
    div().flex_none().size(px(20.)).rounded(px(SM)).bg(t.muted).flex().items_center().justify_center().child(icon("globe", 12., t.muted_fg))
}

#[derive(Clone, Copy)]
pub enum Placement {
    BelowEnd,
    AboveStart,
}

/// Radix-style dropdown placement: anchored to the trigger, painted above everything.
/// `phase` is the eased visibility from `Presence`: tw-animate's `fade-in-0 slide-in-from-{top,bottom}-2` on open and
/// `fade-out-0` on close (150ms, ease). GPUI cannot scale a subtree, so `zoom-in-95` is left out.
pub fn popover(placement: Placement, phase: (f32, bool), content: impl IntoElement) -> AnyElement {
    let (pos, corner, gap) = match placement {
        Placement::BelowEnd => (div().absolute().top_full().right_0(), Corner::TopRight, (4., 0.)),
        Placement::AboveStart => (div().absolute().bottom_full().left_0(), Corner::BottomLeft, (0., 8.)),
    };
    // Offsets as padding: `anchored` sizes its child without margins.
    pos.child(
        deferred(anchored().anchor(corner).snap_to_window_with_margin(px(8.)).child({
            let (v, entering) = phase;
            let dir = if matches!(placement, Placement::AboveStart) { 1. } else { -1. };
            let dy = if entering { dir * 8. * (1. - v) } else { 0. };
            div().pt(px(gap.0)).pb(px(gap.1)).child(div().relative().top(px(dy)).opacity(v).child(content))
        }))
        .with_priority(2),
    )
    .into_any_element()
}

impl Hitori {
    // ---------- windows ----------

    pub fn cockpit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let vp = window.viewport_size();
        let (w, h) = (f32::from(vp.width), f32::from(vp.height));
        let wide = w >= 1024.;
        // Sidebar collapse: `transition-[left,right,width] duration-200 ease-linear`.
        let side_w = if w >= 768. { crate::fx::tween("sidebar-w", if self.sidebar_open { SIDEBAR } else { 0. }, 200., crate::fx::linear, window, cx) } else { 0. };
        let side = side_w > 0.;
        let map_w = w - side_w - if wide { INSPECTOR } else { 0. };
        let sidebar = side.then(|| {
            div()
                .relative()
                .flex_none()
                .w(px(side_w))
                .h_full()
                .overflow_hidden()
                .child(div().absolute().top_0().bottom_0().left(px(side_w - SIDEBAR)).child(self.sidebar(window, cx)))
                .into_any_element()
        });
        let inspector = wide.then(|| self.inspector(window, cx).into_any_element());
        let trigger = div().flex().gap(px(8.)).items_center().child(
            button("trigger", Variant::Ghost, Size::IconSm, true, &t)
                .rest(t.card)
                .shadow(t.panel_shadow())
                .child(icon("panel-left", 16., t.foreground))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.sidebar_open = !this.sidebar_open;
                    cx.notify();
                })),
        );
        let trigger = trigger.child(self.page_switch(cx));
        // The Rules page has no sidebar: just the account avatar where the sidebar keeps it.
        if self.page == Page::Rules {
            let page_w = w - if wide { INSPECTOR } else { 0. };
            let page = self.rules_page(div().flex().items_center().child(self.page_switch(cx)), wide, page_w - 48., window, cx);
            return div()
                .relative()
                .flex()
                .size_full()
                .bg(t.background)
                .child(page)
                .children(inspector)
                .child(div().absolute().left(px(8.)).bottom(px(8.)).child(self.nav_avatar(cx)))
                .into_any_element();
        }
        self.ring_clip.set((side_w, 0., map_w, h));
        {
            let mut keep = self.keepout.borrow_mut();
            keep.clear();
            keep.push([16., 16., 112., 32.]);
            if wide {
                keep.push([(map_w - 360.) / 2. - 8., h - 24. - 340., 376., 348.]);
            } else {
                keep.push([0., h - 280., map_w, 280.]);
            }
        }
        let map = self.map_view((map_w, h), Padding { top: 64., bottom: if wide { 64. } else { 260. }, left: 64., right: 64. }, window, cx);
        let toasts = if wide { self.toasts(map_w, window, cx) } else { None };
        let narrow = (!wide).then(|| {
            div()
                .absolute()
                .left(px(16.))
                .right(px(16.))
                .bottom(px(16.))
                .flex()
                .flex_col()
                .gap(px(12.))
                .p(px(16.))
                .rounded(px(XL))
                .bg(t.card)
                .shadow(t.panel_shadow())
                .child(self.status(cx))
                .child(self.route_field(false, window, cx))
                .child(self.power_button(cx))
                .into_any_element()
        });
        div()
            .flex()
            .size_full()
            .bg(t.background)
            .children(sidebar)
            .child(div().relative().flex_1().min_w_0().h_full().child(map).child(trigger.absolute().top(px(16.)).left(px(16.))).children(toasts).children(narrow))
            .children(inspector)
            .into_any_element()
    }

    pub fn tray(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let vp = self.viewport(window);
        let (w, h) = crate::tray::POPUP;
        // A floating card on a backdrop when the window has room for all of it (main window, key 2); else it fills the window.
        let float = !self.popover && f32::from(vp.width) >= 768. && f32::from(vp.height) >= h + 64.;
        let band = match self.s.conn {
            Conn::On => t.primary.opacity(0.1),
            Conn::Off => t.destructive.opacity(0.1),
            Conn::Connecting => t.muted,
        };
        let card = div()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(t.card)
            .when(self.popover || float, |d| d.rounded(px(XL)).shadow(t.panel_shadow()))
            .when(float, |d| d.w(px(w)).h(px(h)).max_h_full())
            .when(!float, |d| d.size_full())
            .child(
                div()
                    .h(px(56.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(12.))
                    .child(self.brand(cx).px(px(4.)))
                    .child(self.account_menu(window, cx)),
            )
            .child(
                fx("band", band, None)
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .p(px(16.))
                    .child(self.status(cx))
                    .child(self.route_field(true, window, cx))
                    .child(self.power_button(cx)),
            )
            .child(div().flex_1().min_h_0().pt(px(8.)).flex().flex_col().child(self.routing_panel(false, window, cx)));
        if self.popover {
            return card.into_any_element();
        }
        div().size_full().flex().items_center().justify_center().bg(t.background).child(
            div().size_full().flex().items_center().justify_center().bg(t.muted.opacity(0.4)).when(float, |d| d.p(px(32.))).child(card),
        )
        .into_any_element()
    }

    // ---------- sidebar ----------

    pub fn brand(&self, cx: &mut Context<Self>) -> Div {
        let t = self.theme(cx);
        let color = if self.s.conn == Conn::Off { t.muted_fg } else { t.primary };
        let shield = icon("shield-check", 24., color);
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(40.))
            .lg()
            .semibold()
            .tight()
            .child(if self.s.conn == Conn::Connecting { pulse("brand-pulse", shield).into_any_element() } else { shield.into_any_element() })
            .child("Hitori VPN")
    }

    fn sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        div()
            .flex_none()
            .w(px(SIDEBAR))
            .h_full()
            .bg(t.card)
            .text_color(t.foreground)
            .flex()
            .flex_col()
            .child(div().flex_none().h(px(56.)).flex().flex_col().justify_center().px(px(16.)).py(px(8.)).child(self.brand(cx)))
            // The Rules page shows every rule itself.
            .child(div().flex_1().min_h_0().flex().flex_col().when(self.page == Page::Map, |d| d.child(self.routing_panel(true, window, cx))))
            .child(div().flex_none().p(px(8.)).flex().flex_col().child(self.nav_user(window, cx)))
    }

    fn nav_user(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let open = self.menu == Some(Menu::NavUser);
        let accent = t.accent;
        let trigger = div()
            .id("nav-user")
            .flex()
            .items_center()
            .gap(px(12.))
            .w_full()
            .h(px(48.))
            .p(px(8.))
            .rounded(px(MD))
            .sm()
            .cursor_pointer()
            .hover(move |s| s.bg(accent))
            .when(open, |d| d.bg(accent))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::NavUser, cx)))
            .child(avatar(&self.initial(), self.s.avatar.clone(), 32., &t))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_px(14., 16.).medium().truncate().child(self.account_name()))
                    .child(div().text_px(12., 16.).text_color(t.muted_fg).truncate().child(self.account_line())),
            )
            .child(icon("chevrons-up-down", 16., t.foreground));
        let _ = window;
        div().relative().w_full().child(trigger).children(self.nav_menu(&t, cx))
    }

    /// The account avatar alone, at the same size and place as in the sidebar's user button.
    fn nav_avatar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let open = self.menu == Some(Menu::NavUser);
        let accent = t.accent;
        let trigger = div()
            .id("nav-avatar")
            .size(px(48.))
            .p(px(8.))
            .rounded(px(MD))
            .cursor_pointer()
            .hover(move |s| s.bg(accent))
            .when(open, |d| d.bg(accent))
            .tooltip(tip(self.account_name(), &t))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::NavUser, cx)))
            .child(avatar(&self.initial(), self.s.avatar.clone(), 32., &t));
        div().relative().child(trigger).children(self.nav_menu(&t, cx))
    }

    fn nav_menu(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.menu_phase(Menu::NavUser).map(|ph| {
            popover(
                Placement::AboveStart,
                ph,
                menu(SIDEBAR - 16., t, {
                    let mut items = vec![
                        menu_label(t)
                            .flex()
                            .flex_col()
                            .child(self.account_name())
                            .child(div().xs().normal().text_color(t.muted_fg).child(self.renews()))
                            .into_any_element(),
                        menu_separator(t).into_any_element(),
                    ];
                    items.extend(self.account_items(t, cx));
                    items
                }),
            )
        })
    }

    fn account_items(&self, t: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        vec![
            menu_item("m-account", t, false)
                .child(icon("user-round", 16., t.muted_fg))
                .child("Account")
                .on_click(cx.listener(|this, _, _, cx| this.open_panel(Panel::Account, cx)))
                .into_any_element(),
            menu_item("m-settings", t, false)
                .child(icon("settings-2", 16., t.muted_fg))
                .child("Settings")
                .on_click(cx.listener(|this, _, _, cx| this.open_panel(Panel::Settings, cx)))
                .into_any_element(),
            menu_separator(t).into_any_element(),
            if self.s.signed_in {
                menu_item("m-signout", t, true)
                    .child(icon("log-out", 16., t.destructive))
                    .child("Sign out")
                    .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                    .into_any_element()
            } else {
                menu_item("m-signin", t, false)
                    .child(icon("log-in", 16., t.muted_fg))
                    .child("Sign in")
                    .on_click(cx.listener(|this, _, _, cx| this.open_panel(Panel::Account, cx)))
                    .into_any_element()
            },
        ]
    }

    /// Tray header: avatar button with Account, Settings and Sign out.
    fn account_menu(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let open = self.menu == Some(Menu::TrayAccount);
        let trigger = button("tray-account", Variant::Ghost, Size::IconLg, true, &t)
            .rest(if open { t.ghost_hover() } else { gpui::transparent_black() })
            .child(avatar(&self.initial(), self.s.avatar.clone(), 32., &t))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::TrayAccount, cx)));
        let popup = self.menu_phase(Menu::TrayAccount).map(|ph| {
            popover(
                Placement::BelowEnd,
                ph,
                menu(224., &t, {
                    let mut items = vec![
                        menu_label(&t)
                            .flex()
                            .flex_col()
                            .child(self.account_name())
                            .child(div().xs().normal().text_color(t.muted_fg).child(self.account_line()))
                            .into_any_element(),
                        menu_separator(&t).into_any_element(),
                    ];
                    items.extend(self.account_items(&t, cx));
                    items
                }),
            )
        });
        let _ = window;
        div().relative().child(trigger).children(popup)
    }

    // ---------- routing ----------

    /// Default route, Review for suggestions not on screen, every active rule; the tray also gets Manage rules,
    /// since it has no Rules page.
    pub fn routing_panel(&self, toasts: bool, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let wide = f32::from(window.viewport_size().width) >= 1024.;
        let review = if toasts && wide { self.s.suggestions.iter().filter(|s| self.s.snoozed.contains(&s.id)).count() } else { self.s.suggestions.len() };
        let idle = self.s.app_routes.iter().filter(|(a, _)| !data::active(a)).count();
        let count = self.s.app_routes.len() - idle + self.s.sites.len();
        let big = self.s.rules_big;
        let primary = t.primary;
        let fg = t.foreground;
        let review_btn = (review > 0).then(|| {
            fx("review", primary.opacity(0.1), Some(primary.opacity(0.2)))
                .mx(px(8.))
                .mb(px(8.))
                .h(px(40.))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(12.))
                .rounded(px(MD))
                .text_color(primary)
                .sm()
                .medium()
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.open_rules(cx)))
                .child(icon("message-circle-question", 16., primary))
                .child(div().flex_1().child(format!("{review} need VPN")))
                .child("Review")
        });
        let next_small = big;
        let view_btn = button("rules-view", Variant::Ghost, Size::IconSm, true, &t)
            .tooltip(tip(if next_small { "Compact list" } else { "Cards" }, &t))
            .child(icon(if next_small { "rows-3" } else { "layout-list" }, 16., t.foreground))
            .on_click(cx.listener(|this, _, _, cx| {
                this.s.rules_big = !this.s.rules_big;
                cx.notify();
            }));
        let muted = t.muted_fg;
        let hover_bg = t.ghost_hover();
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .child(self.default_route(cx))
            .children(review_btn)
            .child(
                div()
                    .h(px(32.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(8.))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .xs()
                            .text_color(t.muted_fg)
                            .child(div().medium().child(format!("Active rules · {count}")))
                            .when(idle > 0, |d| d.child(format!("{idle} idle"))),
                    )
                    .child(view_btn),
            )
            .child(
                div()
                    .id(if toasts { "rules-side" } else { "rules-tray" })
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(8.))
                    .pb(px(8.))
                    .child(self.active_rules(big, true, Place::Side, window, cx)),
            )
            .when(!toasts, |d| d.child(
                div().flex_none().p(px(8.)).child(
                    fx("manage", gpui::transparent_black(), Some(hover_bg))
                        .fg(muted, fg)
                        .h(px(40.))
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(12.))
                        .rounded(px(MD))
                        .sm()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.open_rules(cx)))
                        .child("Manage rules")
                        .child(icon("chevron-right", 16., muted)),
                ),
            ))
    }

    /// Map or every rule beside the sidebar.
    fn page_switch(&self, cx: &mut Context<Self>) -> Div {
        let t = self.theme(cx);
        let mut seg = div().flex().p(px(2.)).gap(px(2.)).rounded(px(MD)).bg(t.card).shadow(t.panel_shadow());
        for (page, name, label) in [(Page::Map, "map", "Map"), (Page::Rules, "list", "Rules")] {
            let on = self.page == page;
            seg = seg.child(
                fx(SharedString::from(format!("page-{name}")), if on { t.accent } else { gpui::transparent_black() }, Some(t.accent))
                    .h(px(28.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .rounded(px(6.))
                    .xs()
                    .medium()
                    .text_color(if on { t.foreground } else { t.muted_fg })
                    .cursor_pointer()
                    .child(icon(name, 14., if on { t.foreground } else { t.muted_fg }))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.page = page;
                        cx.notify();
                    })),
            );
        }
        seg
    }

    /// Manage rules at full size: the default route, suggestions, and every app and website rule as cards.
    fn rules_page(&self, trigger: Div, wide: bool, width: f32, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        // As many columns of at least 240px as fit, filling the width.
        let cols = ((width + 12.) / 252.).floor().max(1.);
        let card_w = ((width - 12. * (cols - 1.)) / cols).floor();
        let card = |el: AnyElement| div().w(px(card_w)).flex_none().child(el);
        let heading = |text: String, extra: Option<AnyElement>| {
            div().h(px(32.)).flex().items_center().gap(px(8.)).child(div().sm().semibold().child(text)).child(div().flex_1()).children(extra)
        };
        let mut apps: Vec<&'static AppInfo> = self.s.app_routes.iter().map(|(a, _)| app(a)).collect();
        apps.sort_by_key(|a| (!data::active(a.id), a.name.to_lowercase()));
        let idle = apps.iter().filter(|a| !data::active(a.id)).count();
        let mut app_grid = div().flex().flex_wrap().gap(px(12.));
        for a in &apps {
            app_grid = app_grid.child(card(self.app_rule(a, true, Place::Page, window, cx).into_any_element()));
        }
        if apps.is_empty() {
            app_grid = app_grid.child(div().sm().text_color(t.muted_fg).child("No app rules yet. Add a running app with +, or accept a suggestion."));
        }
        let mut site_grid = div().flex().flex_wrap().gap(px(12.));
        for (target, rule) in self.s.sites.clone() {
            site_grid = site_grid.child(card(self.site_rule_el(target, &rule, true, Place::Page, cx).into_any_element()));
        }
        let suggestions = self.s.suggestions.clone();
        let narrow = (!wide).then(|| {
            div().flex().flex_col().gap(px(12.)).p(px(16.)).rounded(px(XL)).bg(t.card).child(self.status(cx)).child(self.route_field(false, window, cx)).child(self.power_button(cx))
        });
        div()
            .id("rules-page")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .bg(t.background)
            .text_color(t.foreground)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(28.))
                    .p(px(16.))
                    // Clear of the account avatar in the bottom-left corner.
                    .pb(px(72.))
                    .child(trigger)
                    .children(narrow)
                    .child(
                        div()
                            .px(px(8.))
                            .flex()
                            .flex_col()
                            .gap(px(28.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.))
                                    .child(div().text_size(px(24.)).semibold().tight().child("Routing rules"))
                                    .child(div().sm().text_color(t.muted_fg).child("Apps and websites without a rule follow the default route.")),
                            )
                            .child(div().rounded(px(XL)).bg(t.card).p(px(8.)).child(self.default_route(cx)))
                            .when(!suggestions.is_empty(), |d| {
                                d.child(
                                    div().flex().flex_col().gap(px(8.)).child(heading(format!("Suggestions · {}", suggestions.len()), None)).child(
                                        div().flex().flex_wrap().gap(px(12.)).children(suggestions.iter().map(|s| card(self.suggestion_card(s, false, cx).into_any_element()))),
                                    ),
                                )
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(8.))
                                    .child(heading(
                                        if idle > 0 { format!("Apps · {} · {idle} idle", apps.len()) } else { format!("Apps · {}", apps.len()) },
                                        Some(self.add_app(cx).into_any_element()),
                                    ))
                                    .child(app_grid),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(8.))
                                    .child(heading(format!("Websites · {}", self.s.sites.len()), None))
                                    .child(self.site_field(cx))
                                    .child(site_grid),
                            ),
                    ),
            )
    }

    /// Where traffic without a rule goes: a segmented control with the chosen mode explained under it.
    pub fn default_route(&self, cx: &mut Context<Self>) -> Div {
        let t = self.theme(cx);
        let current = self.s.default_route;
        let mut seg = div().flex().p(px(2.)).gap(px(2.)).rounded(px(MD)).bg(t.muted);
        for mode in RuleMode::ALL {
            let on = mode == current;
            let fg = if on { if mode == RuleMode::Block { t.destructive } else { t.foreground } } else { t.muted_fg };
            seg = seg.child(
                fx(SharedString::from(format!("default-{}", mode.key())), if on { t.background } else { gpui::transparent_black() }, Some(if on { t.background } else { t.ghost_hover() }))
                    .flex_1()
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(4.))
                    .rounded(px(6.))
                    .when(on, |d| d.shadow(crate::theme::shadow_xs()))
                    .xs()
                    .medium()
                    .text_color(fg)
                    .cursor_pointer()
                    .child(icon(mode.icon(), 14., fg))
                    .child(mode.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.s.default_route = mode;
                        cx.notify();
                    })),
            );
        }
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(8.))
            .py(px(8.))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .child(div().sm().medium().child("Default route"))
                    .child(div().xs().text_color(t.muted_fg).child(current.default_hint())),
            )
            .child(seg)
    }

    /// Every app and site with a rule, apps by traffic; anything else follows the default route and is not listed.
    /// The sidebar leaves out apps that are not using the network; their rules stay and apply when they run again.
    pub fn active_rules(&self, big: bool, sites: bool, place: Place, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut apps: Vec<&'static AppInfo> =
            self.s.app_routes.iter().filter(|(a, _)| place == Place::Sheet || data::active(a)).map(|(a, _)| app(a)).collect();
        // Recently active first; apps not seen this session last.
        apps.sort_by_key(|a| (data::seen_min(a.id).unwrap_or(u32::MAX), a.name));
        let mut col = div().flex().flex_col().when(big, |d| d.gap(px(8.)));
        for a in apps {
            col = col.child(self.app_rule(a, big, place, window, cx));
        }
        if sites {
            for (target, rule) in self.s.sites.clone() {
                col = col.child(self.site_rule_el(target, &rule, big, place, cx));
            }
        }
        col
    }

    fn rule_card(&self, id: SharedString, open: bool, t: &Theme) -> Fx {
        fx(id, if open { t.accent } else { t.muted.opacity(0.5) }, Some(t.accent)).flex().flex_col().gap(px(4.)).w_full().p(px(12.)).rounded(px(LG)).cursor_pointer()
    }

    fn rule_line(&self, id: SharedString, open: bool, t: &Theme) -> Fx {
        fx(id, if open { t.accent } else { gpui::transparent_black() }, Some(t.accent)).flex().items_center().gap(px(8.)).h(px(32.)).w_full().px(px(8.)).rounded(px(MD)).sm().cursor_pointer()
    }

    /// Where traffic under this rule ends up; the main exit is muted because it follows the connection.
    /// Where traffic under this rule ends up; muted when it follows the main connection, which can change.
    pub fn destination(&self, rule: &AppRule) -> (String, bool) {
        match rule.mode {
            RuleMode::Direct => return ("Bypasses VPN".into(), false),
            RuleMode::Block => return ("Blocked".into(), false),
            _ => {}
        }
        let via = rule.via.or(if rule.exit.is_some() { None } else { self.s.entry_id });
        let country = format!("{}{}", server(rule.exit.unwrap_or(self.s.server_id)).country, via.map(|v| format!(" via {}", server(v).country)).unwrap_or_default());
        if rule.mode == RuleMode::Auto { (format!("Auto · {country}"), true) } else { (country, rule.exit.is_none()) }
    }

    fn glyphs(&self, rule: &AppRule, t: &Theme) -> Div {
        let g = |name: &str, label: &str, c: Hsla, id: String| {
            div().id(SharedString::from(id)).size(px(16.)).flex().items_center().justify_center().tooltip(tip(label.to_owned(), t)).child(icon(name, 16., c))
        };
        let row = div().flex().items_center().gap(px(4.));
        match rule.mode {
            RuleMode::Block => row.child(g("ban", "All traffic blocked", t.destructive, "g-ban".into())),
            RuleMode::Direct => row.child(g("globe", "Uses your real connection", t.muted_fg, "g-globe".into())),
            mode => {
                let on = self.s.conn == Conn::On;
                row.when(mode == RuleMode::Auto, |r| r.child(g("wand-sparkles", "Direct first, VPN when it fails", t.muted_fg, "g-auto".into())))
                    .when(rule.exit.is_some(), |r| r.child(g("route", "Own exit, separate from the main route", t.muted_fg, "g-route".into())))
                    .when(rule.via.is_some() || (rule.exit.is_none() && self.s.entry_id.is_some()), |r| r.child(g("layers", "Cascaded through two servers", t.muted_fg, "g-layers".into())))
                    .child(g("shield-check", if on { "Encrypted" } else { "Waiting for the tunnel" }, if on { t.primary } else { t.muted_fg }, "g-shield".into()))
            }
        }
    }

    fn card_body(&self, badge: Div, caption: &str, corner: Option<String>, kind: &AppRule, footer: Div, t: &Theme) -> Vec<AnyElement> {
        let (title, muted) = self.destination(kind);
        let title_color = if kind.mode == RuleMode::Block { t.destructive } else if muted { t.muted_fg } else { t.foreground };
        vec![
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(20.))
                .child(badge)
                .child(div().flex_1().min_w_0().truncate().xs().text_color(t.muted_fg).child(caption.to_owned()))
                .children(corner.map(|c| div().xs().text_color(t.muted_fg).child(c)))
                .into_any_element(),
            div().truncate().sm().semibold().text_color(title_color).child(title).into_any_element(),
            div()
                .flex()
                .items_center()
                .h(px(16.))
                .xs()
                .text_color(t.muted_fg)
                .child(footer.flex().items_center().gap(px(8.)))
                .child(div().flex_1())
                .child(self.glyphs(kind, t))
                .into_any_element(),
        ]
    }

    /// An app's rule; clicking it opens its route settings in the palette.
    fn app_rule(&self, a: &'static AppInfo, big: bool, place: Place, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let route = self.app_route(a.id).cloned().unwrap();
        let open = self.palette == Some(Mode::Rule(a.id));
        let id: SharedString = format!("rule-{}-{:?}", a.id, place).into();
        let trigger = if big {
            let mut footer = div().child(div().child(route_label(&route)));
            if let Some(p) = route.protocol {
                footer = footer.child(div().child("·")).child(div().child(p.label()));
            }
            self.rule_card(id, open, &t).children(self.card_body(app_badge(a), a.process, data::seen_min(a.id).map(ago), &route, footer, &t))
        } else {
            self.rule_line(id, open, &t)
                .child(app_badge(a))
                .child(div().flex_1().min_w_0().truncate().child(a.name))
                .child(div().xs().text_color(if route.mode == RuleMode::Block { t.destructive } else { t.muted_fg }).child(route_label(&route)))
        }
        .on_click(cx.listener(move |this, _, window, cx| this.open_palette(Mode::Rule(a.id), window, cx)));
        let _ = window;
        div().relative().w_full().child(trigger)
    }

    /// A website's rule; clicking it opens its route settings in the palette, like an app's.
    pub fn site_rule_el(&self, target: &'static str, rule: &AppRule, big: bool, place: Place, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let open = self.palette == Some(Mode::Site(target));
        let id: SharedString = format!("site-{target}-{place:?}").into();
        let trigger = if big {
            let mut footer = div().child(div().child(route_label(rule)));
            if let Some(p) = rule.protocol {
                footer = footer.child(div().child("·")).child(div().child(p.label()));
            }
            footer = footer.child(div().child("·")).child(div().child("website"));
            self.rule_card(id, open, &t).children(self.card_body(site_badge(&t), target, None, rule, footer, &t))
        } else {
            self.rule_line(id, open, &t)
                .child(site_badge(&t))
                .child(div().flex_1().min_w_0().truncate().child(target))
                .child(div().xs().text_color(if rule.mode == RuleMode::Block { t.destructive } else { t.muted_fg }).child(route_label(rule)))
        }
        .on_click(cx.listener(move |this, _, window, cx| this.open_palette(Mode::Site(target), window, cx)));
        div().relative().w_full().child(trigger)
    }

    /// Type or paste a website (any URL works), then Enter or Add: it gets a rule and its settings open.
    pub fn site_field(&self, cx: &mut Context<Self>) -> Div {
        let t = self.theme(cx);
        let focus = self.site_input.read(cx).focus_handle(cx);
        let valid = data::domain(&self.site_input.read(cx).content).is_some();
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .id("site-field")
                    .key_context("SiteField")
                    .on_action(cx.listener(|this, _: &crate::app::Confirm, window, cx| this.add_site(window, cx)))
                    .flex_1()
                    .min_w_0()
                    .h(px(36.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(MD))
                    .border_1()
                    .border_color(t.border)
                    .bg(t.background)
                    .sm()
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, move |_, window, _| window.focus(&focus))
                    .child(icon("globe", 16., t.muted_fg))
                    .child(div().flex_1().min_w_0().child(self.site_input.clone())),
            )
            .child(
                button("add-site", Variant::Secondary, Size::Default, false, &t)
                    .when(!valid, |b| b.opacity(0.5))
                    .child(icon("plus", 16., t.foreground))
                    .child("Add")
                    .on_click(cx.listener(|this, _, window, cx| this.add_site(window, cx))),
            )
    }

    pub fn add_app(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let rest: Vec<&'static AppInfo> = data::running().into_iter().filter(|a| self.app_route(a.id).is_none()).collect();
        let trigger = button("add-app", Variant::Ghost, Size::IconSm, true, &t)
            .when(rest.is_empty(), |d| d.opacity(0.5))
            .child(icon("plus", 16., t.foreground))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::AddApp, cx)));
        let popup = self.menu_phase(Menu::AddApp).filter(|_| !rest.is_empty()).map(|ph| {
            let mut items = vec![menu_label(&t).child("Running apps").into_any_element()];
            for a in rest {
                let id = a.id;
                items.push(
                    menu_item(SharedString::from(format!("add-{id}")), &t, false)
                        .child(app_badge(a))
                        .child(div().flex_1().child(a.name))
                        .children(data::seen_min(a.id).map(|m| div().xs().text_color(t.muted_fg).child(ago(m))))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.route(id, Some(AppRule::mode(RuleMode::Vpn)), window, cx);
                            this.open_palette(Mode::Rule(id), window, cx);
                        }))
                        .into_any_element(),
                );
            }
            popover(Placement::BelowEnd, ph, menu(224., &t, items))
        });
        div().relative().child(trigger).children(popup)
    }

    pub fn suggestion_card(&self, s: &Suggestion, floating: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let known = (!s.app_id.is_empty()).then(|| app(s.app_id));
        let (sid, accept_id, dismiss_id) = (s.id, s.id, s.id);
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(12.))
            .rounded(px(LG))
            .when(floating, |d| d.bg(t.card).shadow(t.panel_shadow()))
            .when(!floating, |d| d.bg(t.muted.opacity(0.5)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(20.))
                    .child(match known {
                        Some(a) if s.scope == Scope::App => app_badge(a),
                        _ => site_badge(&t),
                    })
                    .child(div().flex_1().min_w_0().truncate().xs().text_color(t.muted_fg).child(match known {
                        Some(a) if s.scope == Scope::Site => format!("in {}", a.name),
                        Some(_) => s.target.to_owned(),
                        None => "website".to_owned(),
                    }))
                    .child(div().xs().text_color(t.muted_fg).child(ago((s.at.elapsed().as_secs() / 60) as u32)))
                    .when(floating, |d| {
                        d.child(
                            button(SharedString::from(format!("later-{sid}")), Variant::Ghost, Size::IconXs, true, &t)
                                .mr(px(-4.))
                                .child(icon("x", button_icon(Size::IconXs), t.foreground))
                                .on_click(cx.listener(move |this, _, _, cx| this.snooze(sid, cx))),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().truncate().sm().semibold().child(format!("{} needs VPN", subject(s))))
                    .child(div().xs().text_color(t.muted_fg).child(suggestion_detail(s))),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        button(SharedString::from(format!("accept-{sid}")), Variant::Default, Size::Sm, false, &t)
                            .flex_1()
                            .child("Always VPN")
                            .on_click(cx.listener(move |this, _, _, cx| this.accept(accept_id, cx))),
                    )
                    .child(
                        button(SharedString::from(format!("keep-{sid}")), Variant::Ghost, Size::Sm, false, &t)
                            .child("Keep auto")
                            .on_click(cx.listener(move |this, _, _, cx| this.dismiss(dismiss_id, cx))),
                    ),
            )
    }

    /// New suggestions pop up at the bottom middle of the map; ✕ moves them to the sidebar's Review button.
    fn toasts(&self, map_w: f32, _window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = self.theme(cx);
        let live: Vec<Suggestion> = self.s.suggestions.iter().filter(|s| !self.s.snoozed.contains(&s.id)).cloned().collect();
        if live.is_empty() {
            return None;
        }
        let more = (live.len() > 2).then(|| {
            button("more", Variant::Secondary, Size::Default, false, &t)
                .shadow(t.panel_shadow())
                .child(format!("{} more · Review", live.len() - 2))
                .on_click(cx.listener(|this, _, _, cx| this.open_rules(cx)))
        });
        Some(
            div()
                .absolute()
                .bottom(px(24.))
                .left(px((map_w - 360.) / 2.))
                .w(px(360.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .children(live.iter().take(2).map(|s| self.suggestion_card(s, true, cx).into_any_element()))
                .children(more.map(|m| div().flex().justify_center().child(m)))
                .into_any_element(),
        )
    }

    // ---------- inspector ----------

    fn inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        div()
            .flex_none()
            .w(px(INSPECTOR))
            .h_full()
            .bg(t.card)
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .h(px(56.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .pl(px(16.))
                    .pr(px(12.))
                    .child(div().flex_1().min_w_0().flex().flex_col().child(self.status(cx)))
                    .child(self.kill_toggle(cx)),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .px(px(16.))
                    .pb(px(12.))
                    .child(self.route_field(false, window, cx))
                    .child(self.power_button(cx))
                    .child(self.metrics(cx)),
            )
            .child(div().flex_none().h(px(1.)).bg(t.border))
            .child(self.server_list(window, cx))
    }

    pub fn status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let s = server(self.s.server_id);
        let tone = tone(self.s.conn, &t);
        let title = match self.s.conn {
            Conn::On => "Protected",
            Conn::Connecting => "Connecting…",
            Conn::Off => "Unprotected",
        };
        let error = self.s.error.clone().filter(|_| self.s.conn == Conn::Off);
        let where_: Option<String> = match self.s.conn {
            Conn::On if s.ip.is_empty() => Some(format!("{}, {}", s.city, s.country)),
            Conn::On => Some(format!("{} · {}, {}", s.ip, s.city, s.country)),
            Conn::Connecting => Some(format!("{}, {}", s.city, s.country)),
            Conn::Off => self.s.me.as_ref().map(|m| {
                let place = m.geo.as_ref().map(|g| [g.city.clone(), g.country.clone()].into_iter().flatten().collect::<Vec<_>>().join(", ")).filter(|p| !p.is_empty());
                [Some(m.ip.clone()), place].into_iter().flatten().collect::<Vec<_>>().join(" · ")
            }),
        };
        let shield = icon(if self.s.conn == Conn::On { "shield-check" } else { "shield-alert" }, 24., tone);
        div()
            .flex()
            .min_w_0()
            .items_center()
            .gap(px(12.))
            .child(if self.s.conn == Conn::Connecting { pulse("status-pulse", shield).into_any_element() } else { shield.into_any_element() })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().base().semibold().tight().text_color(tone).child(title))
                    .child(match (error, where_) {
                        (Some(e), _) => div().h(px(16.)).truncate().xs().text_color(t.destructive).child(e).into_any_element(),
                        (None, Some(w)) => div().h(px(16.)).truncate().xs().text_color(t.muted_fg).child(w).into_any_element(),
                        (None, None) => div().h(px(16.)).child(skeleton("status-skeleton", 144., 16., &t)).into_any_element(),
                    }),
            )
    }

    fn field(&self, id: &'static str, t: &Theme) -> Fx {
        fx(id, gpui::transparent_black(), Some(t.accent)).flex().items_center().gap(px(12.)).w_full().rounded(px(MD)).px(px(12.)).cursor_pointer()
    }

    /// Exit (what websites see) and an optional server traffic passes through first; both open the palette.
    pub fn route_field(&self, on_card: bool, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let exit = server(self.s.server_id);
        let label = match self.s.conn {
            Conn::On => "Connected to",
            Conn::Connecting => "Connecting to",
            Conn::Off if exit.id == data::fastest().id && self.s.entry_id.is_none() => "Fastest location",
            Conn::Off => "Location",
        };
        let exit_btn = self
            .field("exit-field", &t)
            .h(px(64.))
            .on_click(cx.listener(|this, _, window, cx| this.open_palette(Mode::Exit, window, cx)))
            .child(flag(exit.code, true, window))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().xs().text_color(t.muted_fg).child(label))
                    .child(div().truncate().base().semibold().child(exit.country))
                    .child(div().truncate().xs().text_color(t.muted_fg).child(exit.city)),
            )
            .child(icon("chevron-down", 16., t.muted_fg));
        let second = match self.s.entry_id {
            Some(e) => {
                let entry = server(e);
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        self.field("entry-field", &t)
                            .h(px(40.))
                            .flex_1()
                            .min_w_0()
                            .on_click(cx.listener(|this, _, window, cx| this.open_palette(Mode::Entry, window, cx)))
                            .child(div().w(px(24.)).flex_none().flex().justify_center().xs().text_color(t.muted_fg).child("via"))
                            .child(
                                div()
                                    .flex()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .sm()
                                    .child(entry.country)
                                    .child(div().text_color(t.muted_fg).child(format!(" · {}", entry.city))),
                            ),
                    )
                    .child(
                        button("go-direct", Variant::Ghost, Size::Icon, true, &t)
                            .tooltip(tip("Go direct", &t))
                            .child(icon("x", 16., t.foreground))
                            .on_click(cx.listener(|this, _, window, cx| this.via(None, window, cx))),
                    )
                    .into_any_element()
            }
            None => {
                let (muted, fg) = (t.muted_fg, t.foreground);
                self.field("add-entry", &t)
                    .fg(muted, fg)
                    .h(px(40.))
                    .sm()
                    .on_click(cx.listener(|this, _, window, cx| this.open_palette(Mode::Entry, window, cx)))
                    .child(icon("plus", 16., muted))
                    .child("Route through another server")
                    .into_any_element()
            }
        };
        div().flex().flex_col().rounded(px(LG)).p(px(4.)).bg(if on_card { t.card } else { t.muted.opacity(0.5) }).child(exit_btn).child(second)
    }

    pub fn power_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let (variant, label, fg) = match self.s.conn {
            Conn::Off if !self.s.signed_in => (Variant::Default, "Sign in", t.primary_fg),
            Conn::Off => (Variant::Default, "Connect", t.primary_fg),
            Conn::Connecting => (Variant::Secondary, "Cancel", t.secondary_fg),
            Conn::On => (Variant::Secondary, "Disconnect", t.secondary_fg),
        };
        button("power", variant, Size::Lg, true, &t)
            .w_full()
            .h(px(48.))
            .text_px(16., 24.)
            .child(if self.s.conn == Conn::Connecting { spinner("power-spin", 16., fg).into_any_element() } else { icon("power", 16., fg).into_any_element() })
            .child(label)
            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)))
    }

    /// Latency, download and upload in one row. Disconnected, the latency is the last test's to the chosen server.
    fn metrics(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let on = self.s.conn == Conn::On;
        let st = &self.s.stats;
        let latency = if on { st.latency } else { data::ping(self.s.server_id) };
        let cell = |id: &'static str, name: &str, value: String, unit: &str, hint: &str| {
            div()
                .id(id)
                .flex_1()
                .min_w_0()
                .h(px(36.))
                .flex()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .tooltip(tip(hint.to_owned(), &t))
                .child(icon(name, 14., t.muted_fg))
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(3.))
                        .whitespace_nowrap()
                        .when(value == "—", |d| d.text_color(t.muted_fg))
                        .child(div().sm().medium().child(value.clone()))
                        .when(value != "—", |d| d.child(div().xs().text_color(t.muted_fg).child(unit.to_owned()))),
                )
        };
        let speed = |v: f64| if on { fixed1(v) } else { "—".into() };
        div()
            .flex()
            .px(px(4.))
            .rounded(px(LG))
            .bg(t.muted.opacity(0.5))
            .child(cell(
                "m-latency",
                "activity",
                latency.map_or("—".into(), |ms| ms.to_string()),
                "ms",
                if on { "Latency through the tunnel" } else { "Latency to this server, from the last test" },
            ))
            .child(cell("m-down", "arrow-down", speed(st.down), "Mbit/s", "Download"))
            .child(cell("m-up", "arrow-up", speed(st.up), "Mbit/s", "Upload"))
    }

    /// Kill switch as an icon toggle, blue while on; the full switch and its explanation live in Settings.
    fn kill_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let on = self.s.kill_switch;
        let hint = match (on, self.s.uses_tun()) {
            (true, true) => "Kill switch on · traffic is blocked while the tunnel is down",
            (true, false) if crate::os::SYSTEM_PROXY => "Kill switch on · takes effect in TUN mode",
            (true, false) => "Kill switch on · takes effect in system-wide mode",
            (false, _) => "Kill switch off · block traffic while the tunnel is down",
        };
        button("kill", Variant::Ghost, Size::IconSm, true, &t)
            .when(on, |b| b.rest(t.primary.opacity(0.1)))
            .tooltip(tip(hint, &t))
            .child(icon("shield-x", 16., if on { t.primary } else { t.muted_fg }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.s.kill_switch = !this.s.kill_switch;
                cx.notify();
            }))
    }

    /// Every server; clicking one connects there. The header runs the latency test and sorts by its results.
    fn server_list(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let mut servers = data::servers();
        if self.sort_ping {
            data::by_latency(&mut servers);
        }
        let testing = self.s.testing;
        let tested = self.s.tested_at.map(|at| match ago((at.elapsed().as_secs() / 60) as u32) {
            now if now == "now" => "tested just now".to_owned(),
            a => format!("tested {a} ago"),
        });
        let sort = button("sort-ping", Variant::Ghost, Size::IconXs, true, &t)
            .when(self.sort_ping, |b| b.rest(t.accent))
            .tooltip(tip(if self.sort_ping { "Sorted by latency" } else { "Sort by latency" }, &t))
            .child(icon("arrow-down-narrow-wide", 12., if self.sort_ping { t.foreground } else { t.muted_fg }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.sort_ping = !this.sort_ping;
                cx.notify();
            }));
        let test = button("ping-test", Variant::Ghost, Size::Xs, true, &t)
            .when(testing || servers.is_empty(), |b| b.opacity(0.5))
            .tooltip(tip(if testing { "Measuring every server…" } else { "Measure every server through its own protocol" }, &t))
            .child(if testing { spinner("ping-spin", 12., t.foreground).into_any_element() } else { icon("gauge", 12., t.foreground).into_any_element() })
            .child("Test")
            .on_click(cx.listener(|this, _, _, cx| this.test_pings(cx)));
        let n = servers.len();
        // Servers under their country, in latency order when sorted (so countries by their best server). Servers that did
        // not answer the last test are left out, except the one in use.
        let mut groups: Vec<Vec<&'static data::Server>> = vec![];
        for s in servers.into_iter().filter(|s| s.id == self.s.server_id || !data::timed_out(s.id) || self.s.pending.contains(&s.id)) {
            match groups.iter_mut().find(|g| g[0].code == s.code) {
                Some(g) => g.push(s),
                None => groups.push(vec![s]),
            }
        }
        let mut rows: Vec<AnyElement> = vec![];
        for g in &groups {
            rows.push(self.country_row(g, window, cx).into_any_element());
            if self.open_countries.contains(&g[0].code) {
                rows.extend(g.iter().map(|s| self.server_row(s, cx).into_any_element()));
            }
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(40.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .pt(px(8.))
                    .pl(px(16.))
                    .pr(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .gap(px(6.))
                            .xs()
                            .text_color(t.muted_fg)
                            .child(div().medium().child("Servers"))
                            .children(tested.map(|s| div().truncate().child(s))),
                    )
                    .child(sort)
                    .child(test),
            )
            .child(
                div()
                    .id("servers")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .px(px(8.))
                    .pb(px(8.))
                    .when(n == 0, |d| d.child(div().px(px(8.)).py(px(8.)).sm().text_color(t.muted_fg).child("Sign in to see your servers.")))
                    .children(rows),
            )
    }

    /// Load ring, like Proton's, in the flag column; empty when the provider does not report load.
    fn load_el(&self, id: &'static str, t: &Theme) -> AnyElement {
        let col = div().id(SharedString::from(format!("load-{id}"))).w(px(24.)).flex_none().flex().items_center().justify_center();
        let Some(pct) = data::load(id) else { return col.into_any_element() };
        col.tooltip(tip(format!("Server load {pct}%"), t)).child(meter(pct as f32 / 100., 16., 2., load_tone(pct), t.muted_fg.opacity(0.25))).into_any_element()
    }

    /// Row background: blue for the live exit, grey for the chosen one.
    fn row_fx(&self, id: SharedString, current: bool, t: &Theme) -> Fx {
        let live = current && self.s.conn == Conn::On;
        let (rest, hover) = if live { (t.primary.opacity(0.1), t.primary.opacity(0.15)) } else { (if current { t.accent } else { gpui::transparent_black() }, t.accent) };
        fx(id, rest, Some(hover)).flex_none().flex().items_center().gap(px(12.)).px(px(8.)).rounded(px(MD)).cursor_pointer()
    }

    /// The last test's latency to a server, or a spinner while it is measured or being connected to.
    fn latency_el(&self, id: &'static str, connecting: bool, spin: String, t: &Theme) -> AnyElement {
        if connecting || self.s.pending.contains(&id) {
            return spinner(SharedString::from(spin), 14., t.muted_fg).into_any_element();
        }
        let text = match data::ping(id) {
            Some(ms) => format!("{ms} ms"),
            None if data::timed_out(id) => "offline".into(),
            None => "—".into(),
        };
        div().flex_none().whitespace_nowrap().xs().text_color(t.muted_fg).child(text).into_any_element()
    }

    /// One server under its country: load on the left, its id, and the last test's latency on the right (a spinner
    /// while it runs). The current exit is highlighted.
    fn server_row(&self, s: &'static data::Server, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let id = s.id;
        let current = id == self.s.server_id;
        let live = current && self.s.conn == Conn::On;
        let latency = self.latency_el(id, current && self.s.conn == Conn::Connecting, format!("srv-spin-{id}"), &t);
        self.row_fx(SharedString::from(format!("srv-{id}")), current, &t)
            .h(px(40.))
            .child(self.load_el(id, &t))
            .child(div().flex_1().min_w_0().truncate().sm().text_color(if live { t.primary } else { t.foreground }).child(s.tag()))
            .when(self.s.entry_id == Some(id), |d| d.child(div().xs().text_color(t.muted_fg).child("via")))
            .when(live, |d| d.child(div().size(px(6.)).rounded_full().bg(t.primary)))
            .child(latency)
            .on_click(cx.listener(move |this, _, window, cx| this.go(id, window, cx)))
    }

    /// A country: flag, name, the latency of the server a click connects to, and a chevron; its servers below while
    /// open. Closed over the current exit, it stands in for it and names it.
    fn country_row(&self, list: &[&'static data::Server], window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let code = list[0].code;
        let open = self.open_countries.contains(&code);
        let current = list.iter().find(|s| s.id == self.s.server_id).filter(|_| !open);
        let live = current.is_some() && self.s.conn == Conn::On;
        let shown = current.map_or_else(|| data::best_in(code), |s| s.id);
        let latency = self.latency_el(shown, current.is_some() && self.s.conn == Conn::Connecting, format!("country-spin-{code}"), &t);
        self.row_fx(SharedString::from(format!("country-{code}")), current.is_some(), &t)
            .h(px(48.))
            .child(flag(code, true, window))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().text_px(14., 20.).medium().text_color(if live { t.primary } else { t.foreground }).child(list[0].country))
                    .children(current.map(|s| div().truncate().xs().text_color(t.muted_fg).child(s.tag()))),
            )
            .when(live, |d| d.child(div().size(px(6.)).rounded_full().bg(t.primary)))
            .child(latency)
            .child(
                button(SharedString::from(format!("country-open-{code}")), Variant::Ghost, Size::IconSm, true, &t)
                    .tooltip(tip(if open { "Hide servers" } else { "Show servers" }, &t))
                    .child(icon(if open { "chevron-up" } else { "chevron-down" }, 16., t.muted_fg))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        match this.open_countries.iter().position(|c| *c == code) {
                            Some(i) => _ = this.open_countries.remove(i),
                            None => this.open_countries.push(code),
                        }
                        cx.notify();
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                let best = data::best_in(code);
                this.go(best, window, cx);
            }))
    }

    pub fn menu_catcher(&self, cx: &mut Context<Self>) -> AnyElement {
        deferred(
            div().id("menu-catcher").absolute().inset_0().occlude().on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.menu = None;
                    cx.notify();
                }),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }
}


impl Hitori {
    pub fn menu_phase(&self, which: Menu) -> Option<(f32, bool)> {
        self.menu_fx.phase(which, 150., 150., crate::fx::ease)
    }
}

impl Hitori {
    pub fn account_name(&self) -> String {
        if let Some(u) = self.s.user.as_ref().filter(|u| !u.name.is_empty()) {
            return u.name.clone();
        }
        match &self.s.account {
            Some(a) if !a.name.is_empty() => a.name.clone(),
            _ if self.s.signed_in => "Hitori".into(),
            _ => "Not signed in".into(),
        }
    }

    pub fn initial(&self) -> String {
        self.account_name().chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()
    }

    pub fn account_line(&self) -> String {
        match &self.s.account {
            Some(a) => a.summary(),
            None if self.s.signed_in => "Loading…".into(),
            None => "Paste your subscription link".into(),
        }
    }

    pub fn renews(&self) -> String {
        self.s.account.as_ref().and_then(|a| a.expires.clone()).map(|d| format!("renews {d}")).unwrap_or_default()
    }
}
