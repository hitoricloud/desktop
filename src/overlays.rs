//! Sheet (Account, Settings, Routing rules) and the location palette.
use gpui::{AnyElement, Context, Div, Focusable, MouseButton, SharedString, Window, div, hsla, prelude::*, px};

use crate::app::{Hitori, Panel, Place, Protocol};
use crate::data;
use crate::theme::{Theme, shadow_lg};
use crate::ui::{LG, MD, Size, Tw, Variant, XL, badge_secondary, button, icon, progress, switch};


fn section(title: String, t: &Theme, rows: Vec<AnyElement>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(div().xs().medium().text_color(t.muted_fg).child(title))
        .child(div().flex().flex_col().rounded(px(XL)).bg(t.muted.opacity(0.5)).children(rows))
}

/// Content width of a sheet `<Item>` row next to a control of this width: sheet 448 − border − body padding − item padding − gap.
fn hint_room(control: f32) -> f32 {
    448. - 1. - 32. - 32. - 16. - control
}

/// `<Item>` row: title and hint on the left, a control on the right. `room` balances the hint's lines.
fn row(label: &str, hint: Option<&str>, room: f32, control: impl IntoElement, t: &Theme, window: &Window) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(16.))
        .p(px(16.))
        .rounded(px(MD))
        .sm()
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(4.))
                .child(div().sm().medium().child(label.to_owned()))
                .children(hint.map(|h| {
                    let balanced = crate::ui::balanced_width(h, 14., room, window);
                    div().sm().normal().text_color(t.muted_fg).line_clamp(2).when_some(balanced, |d, w| d.w(px(w))).child(h.to_owned())
                })),
        )
        .child(div().flex().items_center().gap(px(8.)).child(control))
        .into_any_element()
}

impl Hitori {
    // ---------- sheet ----------

    pub fn sheet(&self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let vp = self.viewport(window);
        let w = if f32::from(vp.width) >= 640. { 448. } else { f32::from(vp.width) };
        let (title, desc) = match panel {
            Panel::Account => match &self.s.account {
                Some(a) if self.s.signed_in => ("Account", format!("Signed in as {}", a.name)),
                _ if self.s.signed_in => ("Account", "Loading your subscription…".into()),
                _ => ("Sign in", "Paste the subscription link from the Hitori bot.".into()),
            },
            Panel::Routes => ("Routing rules", format!("Apps and sites without a rule follow the default route: {}.", self.s.default_route.label())),
            Panel::Settings => ("Settings", "Changes apply on the next connection.".into()),
        };
        // Sheet: overlay `fade-in-0` 150ms; content `slide-in-from-right` 500ms in / 300ms out, ease-in-out.
        let dim = self.panel_fx.phase(panel, 150., 150., crate::fx::ease).map_or(0., |p| p.0);
        let slide = self.panel_fx.phase(panel, 500., 300., crate::fx::ease_in_out).map_or(0., |p| p.0);
        let body = match panel {
            Panel::Account => self.account_body(window, cx),
            Panel::Settings => self.settings_body(window, cx),
            Panel::Routes => self.routes_body(window, cx),
        };
        div()
            .absolute()
            .inset_0()
            .when(dim > 0., |d| {
                d.child(div().id("sheet-overlay").absolute().inset_0().bg(hsla(0., 0., 0., 0.5)).opacity(dim).occlude().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.panel = None;
                        cx.notify();
                    }),
                ))
            })
            .child(
                div()
                    .id("sheet")
                    .occlude()
                    .absolute()
                    .top_0()
                    .right(px(-(1. - slide) * w))
                    .bottom_0()
                    .w(px(w))
                    .bg(t.background)
                    .shadow(shadow_lg())
                    .border_l_1()
                    .border_color(t.border)
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .p(px(16.))
                            .child(div().base().semibold().text_color(t.foreground).child(title))
                            .child(div().sm().text_color(t.muted_fg).child(desc)),
                    )
                    .child(div().id("sheet-body").flex_1().min_h_0().overflow_y_scroll().px(px(16.)).pb(px(16.)).child(body))
                    .child(close_button("sheet-close", &t).on_click(cx.listener(|this, _, _, cx| {
                        this.panel = None;
                        cx.notify();
                    }))),
            )
            .into_any_element()
    }

    fn settings_body(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let mut group = div().flex().bg(t.background).rounded(px(MD)).p(px(4.));
        let n = Protocol::ALL.len();
        for (i, p) in Protocol::ALL.into_iter().enumerate() {
            let on = self.s.protocol == p;
            let (muted, muted_fg) = (t.muted, t.muted_fg);
            group = group.child(
                div()
                    .id(SharedString::from(format!("proto-{}", p.label())))
                    .h(px(32.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .sm()
                    .medium()
                    .cursor_pointer()
                    .when(i == 0, |d| d.rounded_l(px(MD)))
                    .when(i == n - 1, |d| d.rounded_r(px(MD)))
                    .when(on, |d| d.bg(t.accent))
                    .when(!on, |d| d.hover(move |s| s.bg(muted).text_color(muted_fg)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.s.protocol = p;
                        cx.notify();
                    }))
                    .child(p.label()),
            );
        }
        let dark = t.dark;
        let toggle_w = 8. + Protocol::ALL.iter().map(|p| crate::ui::text_width(p.label(), 14., gpui::FontWeight::MEDIUM, window) + 24.).sum::<f32>();
        div()
            .flex()
            .flex_col()
            .gap(px(24.))
            .child(section(
                "Connection".into(),
                &t,
                vec![
                    row("Protocol", Some("TLS gets through the most filters."), hint_room(toggle_w), group, &t, window),
                    row(
                        "System-wide",
                        Some(if !self.s.system_wide {
                            "Only apps set to use the proxy: SOCKS5 127.0.0.1:10808, HTTP 10809."
                        } else if crate::os::TUN {
                            "Every app goes through Hitori. Asks for your password to connect."
                        } else {
                            "Sets the system proxy: browsers and most apps go through Hitori."
                        }),
                        hint_room(32.),
                        switch("system-wide", self.s.system_wide, &t).on_click(cx.listener(|this, _, _, cx| {
                            this.s.system_wide = !this.s.system_wide;
                            cx.notify();
                        })),
                        &t,
                        window,
                    ),
                    row(
                        "Connect on startup",
                        None,
                        hint_room(32.),
                        switch("startup", self.s.auto_connect, &t).on_click(cx.listener(|this, _, _, cx| {
                            this.s.auto_connect = !this.s.auto_connect;
                            cx.notify();
                        })),
                        &t,
                        window,
                    ),
                    row(
                        "Kill switch",
                        Some(if !crate::os::TUN {
                            "Only on Linux for now."
                        } else if self.s.system_wide {
                            "Block all traffic while the tunnel is down."
                        } else {
                            "Needs system-wide mode."
                        }),
                        hint_room(32.),
                        switch("kill-switch", self.s.kill_switch, &t).on_click(cx.listener(|this, _, _, cx| {
                            this.s.kill_switch = !this.s.kill_switch;
                            cx.notify();
                        })),
                        &t,
                        window,
                    ),
                ],
            ))
            .child(section(
                "Appearance".into(),
                &t,
                vec![row("Dark theme", None, hint_room(32.), switch("dark", dark, &t).on_click(cx.listener(move |this, _, _, cx| this.set_dark(!dark, cx))), &t, window)],
            ))
            .into_any_element()
    }

    fn sign_in_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let focus = self.link.read(cx).focus_handle(cx);
        let busy = self.s.busy;
        let field = div()
            .id("link-field")
            .h(px(36.))
            .px(px(12.))
            .flex()
            .items_center()
            .rounded(px(MD))
            .border_1()
            .border_color(t.border)
            .bg(t.background)
            .sm()
            .cursor_text()
            .on_mouse_down(MouseButton::Left, move |_, window, _| window.focus(&focus))
            .child(div().flex_1().min_w_0().child(self.link.clone()));
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(field)
            .children(self.s.error.clone().map(|e| div().sm().text_color(t.destructive).child(e)))
            .child(
                div().flex().child(
                    button("signin", Variant::Default, Size::Default, false, &t)
                        .when(busy, |b| b.opacity(0.5))
                        .child(if busy { "Signing in…" } else { "Sign in" })
                        .on_click(cx.listener(|this, _, window, cx| this.sign_in(window, cx))),
                ),
            )
            .child(div().xs().text_color(t.muted_fg).child("The link stays on this computer; it is the only key to your subscription."))
            .into_any_element()
    }

    fn account_body(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.s.signed_in {
            return self.sign_in_body(cx);
        }
        let t = self.theme(cx);
        let a = self.s.account.clone().unwrap_or_default();
        let days = a.days_left.map_or("—".to_owned(), |d| format!("{d} days"));
        let renews = a.expires.clone().map(|d| format!("until {d}")).unwrap_or_default();
        // Days of a 30-day period, or the traffic allowance when there is one.
        let fill = if a.limit > 0 { 1. - (a.used as f32 / a.limit as f32).min(1.) } else { a.days_left.map_or(0., |d| (d as f32 / 30.).min(1.)) };
        let traffic = if a.limit > 0 { format!("{} of {} used", data::bytes(a.used), data::bytes(a.limit)) } else { format!("{} used · no traffic limit", data::bytes(a.used)) };
        let support = a.support.clone();
        let card = div()
            .flex()
            .flex_col()
            .gap(px(24.))
            .py(px(24.))
            .rounded(px(XL))
            .bg(t.muted.opacity(0.5))
            .child(
                div()
                    .px(px(24.))
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .child(div().flex_1().flex().flex_col().gap(px(8.)).child(div().sm().text_color(t.muted_fg).child("Plan")).child(div().text_px(24., 32.).semibold().child(a.plan())))
                    .child(div().flex().flex_col().items_end().child(div().xl().semibold().child(days)).child(div().xs().text_color(t.muted_fg).child(renews))),
            )
            .child(div().px(px(24.)).flex().flex_col().gap(px(8.)).child(progress(fill, &t)).child(div().xs().text_color(t.muted_fg).child(traffic)))
            .children(support.map(|url| {
                div().px(px(24.)).flex().items_center().child(
                    button("extend", Variant::Default, Size::Default, false, &t)
                        .child("Extend subscription")
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
            }));
        let device = div()
            .flex()
            .items_center()
            .gap(px(16.))
            .p(px(16.))
            .sm()
            .child(div().h(px(44.)).flex().items_start().child(icon("laptop", 20., t.muted_fg)))
            .child(div().flex_1().flex().flex_col().gap(px(4.)).child(div().sm().medium().child(crate::sub::hostname())).child(div().sm().text_color(t.muted_fg).child(format!("{} desktop", crate::sub::OS))))
            .child(badge_secondary("This device", &t))
            .into_any_element();
        div()
            .flex()
            .flex_col()
            .gap(px(24.))
            .child(card)
            .child(section("Devices".into(), &t, vec![device]))
            .child(section(
                "Session".into(),
                &t,
                vec![row(
                    "Sign out",
                    Some("Forgets the subscription link on this computer; the tunnel stops."),
                    hint_room(2. + 24. + 16. + 8. + crate::ui::text_width("Sign out", 14., gpui::FontWeight::MEDIUM, window)),
                    button("signout", Variant::Outline, Size::Sm, true, &t)
                        .child(icon("log-out", 16., t.foreground))
                        .child("Sign out")
                        .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx))),
                    &t,
                    window,
                )],
            ))
            .into_any_element()
    }

    fn routes_body(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let suggestions = self.s.suggestions.clone();
        let heading = |s: String| div().xs().medium().text_color(t.muted_fg).child(s);
        let mut sites = div().flex().flex_col().rounded(px(LG)).p(px(4.)).bg(t.muted.opacity(0.5));
        for (target, rule) in self.s.sites.clone() {
            sites = sites.child(self.site_rule_el(target, &rule, false, Place::Sheet, cx));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(24.))
            .when(!suggestions.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(heading(format!("Suggestions · {}", suggestions.len())))
                        .children(suggestions.iter().map(|s| self.suggestion_card(s, false, cx).into_any_element())),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().mr(px(-4.)).h(px(32.)).flex().items_center().justify_between().child(heading(format!("Apps · {}", self.s.app_routes.len()))).child(self.add_app(cx)))
                    .child(self.active_rules(true, false, Place::Sheet, window, cx)),
            )
            .child(div().flex().flex_col().gap(px(8.)).child(heading(format!("Sites · {}", self.s.sites.len()))).child(self.site_field(cx)).when(!self.s.sites.is_empty(), |d| d.child(sites)))
            .child(div().xs().text_color(t.muted_fg).child(format!("Everything else: {} · {}.", self.s.default_route.label(), self.s.default_route.default_hint().to_lowercase())))
            .into_any_element()
    }
}

/// Dialog/sheet close: X at top-4 right-4, 70% opacity until hovered.
pub fn close_button(id: &'static str, t: &Theme) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .top(px(16.))
        .right(px(16.))
        .size(px(16.))
        .rounded(px(2.))
        .opacity(0.7)
        .hover(|s| s.opacity(1.))
        .cursor_pointer()
        .child(icon("x", 16., t.foreground))
}

