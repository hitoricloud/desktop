//! Sheet (Account, Settings, Routing rules) and the location palette, ported from vpn.tsx, pages/* and routing.tsx.
use gpui::{AnyElement, Context, Div, MouseButton, SharedString, Window, div, hsla, prelude::*, px};

use crate::app::{Hitori, Panel, Place, Protocol};
use crate::data::ACCOUNT;
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
        let vp = window.viewport_size();
        let w = if f32::from(vp.width) >= 640. { 448. } else { f32::from(vp.width) };
        let (title, desc) = match panel {
            Panel::Account => ("Account", format!("Signed in with Telegram as {}", ACCOUNT.telegram)),
            Panel::Routes => ("Routing rules", "Apps and sites without a rule are autorouted.".into()),
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
                        Some("Block all traffic while the tunnel is down."),
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

    fn account_body(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
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
                    .child(div().flex_1().flex().flex_col().gap(px(8.)).child(div().sm().text_color(t.muted_fg).child("Plan")).child(div().text_px(24., 32.).semibold().child(ACCOUNT.plan)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_end()
                            .child(div().xl().semibold().child(format!("{} days", ACCOUNT.days_left)))
                            .child(div().xs().text_color(t.muted_fg).child(format!("renews {}", ACCOUNT.renews_on))),
                    ),
            )
            .child(div().px(px(24.)).child(progress(ACCOUNT.days_left as f32 / ACCOUNT.period_days as f32, &t)))
            .child(div().px(px(24.)).flex().items_center().child(button("extend", Variant::Default, Size::Default, false, &t).child("Extend subscription")));
        let devices: Vec<AnyElement> = ACCOUNT
            .devices
            .iter()
            .map(|(name, kind, current)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .p(px(16.))
                    .sm()
                    .child(div().h(px(44.)).flex().items_start().child(icon(if *kind == "Android" { "smartphone" } else { "laptop" }, 20., t.muted_fg)))
                    .child(div().flex_1().flex().flex_col().gap(px(4.)).child(div().sm().medium().child(*name)).child(div().sm().text_color(t.muted_fg).child(*kind)))
                    .child(if *current {
                        badge_secondary("This device", &t).into_any_element()
                    } else {
                        button(SharedString::from(format!("remove-{name}")), Variant::Ghost, Size::Sm, false, &t).child("Remove").into_any_element()
                    })
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(24.))
            .child(card)
            .child(section(format!("Devices · {} of {}", ACCOUNT.devices.len(), ACCOUNT.max_devices), &t, devices))
            .child(section(
                "Session".into(),
                &t,
                vec![row(
                    "Sign out",
                    Some("Removes this device's key; the tunnel stops."),
                    hint_room(2. + 24. + 16. + 8. + crate::ui::text_width("Sign out", 14., gpui::FontWeight::MEDIUM, window)),
                    button("signout", Variant::Outline, Size::Sm, true, &t).child(icon("log-out", 16., t.foreground)).child("Sign out"),
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
            sites = sites.child(self.site_rule_el(target, rule, false, Place::Sheet, cx));
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
            .child(div().flex().flex_col().gap(px(8.)).child(heading(format!("Sites · {}", self.s.sites.len()))).child(sites))
            .child(div().xs().text_color(t.muted_fg).child("Everything else is autorouted: direct first, VPN when it fails."))
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

