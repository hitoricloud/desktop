//! Sheet (Account, Settings, Routing rules) and the location palette.
use gpui::{AnyElement, Context, Div, Focusable, MouseButton, SharedString, Window, div, hsla, prelude::*, px};

use crate::app::{Hitori, Panel, Place, Protocol, SettingsTab};
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

/// Content width of a settings-page row next to a control of this width: card width − border − item padding − gap.
fn page_room(card: f32, control: f32) -> f32 {
    card - 2. - 32. - 16. - control
}

/// Section heading above a bordered card whose rows are split by dividers.
fn page_section(title: &str, t: &Theme, rows: Vec<AnyElement>) -> AnyElement {
    let mut card = div().flex().flex_col().rounded(px(XL)).border_1().border_color(t.border).bg(t.muted.opacity(0.3)).overflow_hidden();
    for (i, r) in rows.into_iter().enumerate() {
        card = card.child(div().when(i > 0, |d| d.border_t_1().border_color(t.border)).child(r));
    }
    div().flex().flex_col().gap(px(12.)).child(div().px(px(4.)).base().semibold().child(title.to_owned())).child(card).into_any_element()
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
        if panel == Panel::Settings {
            return self.settings_page(window, cx);
        }
        let vp = self.viewport(window);
        let w = if f32::from(vp.width) >= 640. { 448. } else { f32::from(vp.width) };
        let (title, desc) = match panel {
            Panel::Account => match &self.s.account {
                Some(_) if self.s.signed_in => (t!("overlays.account.title"), t!("overlays.account.signed_in_as", name = self.account_name())),
                _ if self.s.signed_in => (t!("overlays.account.title"), t!("overlays.account.loading").into()),
                _ => (t!("overlays.signin.title"), t!("overlays.signin.desc").into()),
            },
            Panel::Routes => (t!("overlays.routes.title"), t!("overlays.routes.desc", route = self.s.default_route.label())),
            Panel::Settings => unreachable!(),
        };
        // Sheet: overlay `fade-in-0` 150ms; content `slide-in-from-right` 500ms in / 300ms out, ease-in-out.
        let dim = self.panel_fx.phase(panel, 150., 150., crate::fx::ease).map_or(0., |p| p.0);
        let slide = self.panel_fx.phase(panel, 500., 300., crate::fx::ease_in_out).map_or(0., |p| p.0);
        let body = match panel {
            Panel::Account => self.account_body(window, cx),
            Panel::Settings => unreachable!(),
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

    /// Full-window settings: category sidebar (tabs on a narrow window), breadcrumb header, sections of bordered cards.
    fn settings_page(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let fade = self.panel_fx.phase(Panel::Settings, 150., 150., crate::fx::ease).map_or(0., |p| p.0);
        let vp = self.viewport(window);
        let dw = (f32::from(vp.width) - 32.).min(880.);
        let dh = (f32::from(vp.height) - 32.).min(640.);
        let wide = dw >= 640.;
        let side = if wide { 220. } else { 0. };
        let cw = (dw - side - 48.).min(760.);
        let tab = self.settings_tab;
        let mut nav = div().flex().gap(px(4.)).when(wide, |d| d.flex_col().w(px(side)).flex_none().p(px(12.)).border_r_1().border_color(t.border)).when(!wide, |d| d.px(px(12.)).py(px(8.)).border_b_1().border_color(t.border));
        for k in SettingsTab::ALL {
            let on = k == tab;
            let (muted, muted_fg) = (t.muted, t.muted_fg);
            nav = nav.child(
                div()
                    .id(SharedString::from(format!("settings-tab-{}", k.icon())))
                    .h(px(36.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .rounded(px(MD))
                    .sm()
                    .medium()
                    .cursor_pointer()
                    .when(!wide, |d| d.flex_1().justify_center())
                    .when(on, |d| d.bg(t.accent))
                    .when(!on, |d| d.text_color(t.muted_fg).hover(move |s| s.bg(muted).text_color(muted_fg)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_tab = k;
                        cx.notify();
                    }))
                    .child(icon(k.icon(), 16., if on { t.foreground } else { t.muted_fg }))
                    .child(k.label()),
            );
        }
        let content = match tab {
            SettingsTab::Connection => self.connection_sections(cw, window, cx),
            SettingsTab::Location => vec![page_section(t!("overlays.settings.location"), &t, vec![self.region_row(cw, window, cx)])],
            SettingsTab::Appearance => {
                let dark = t.dark;
                let room = page_room(cw, 32.);
                vec![
                    page_section(t!("overlays.settings.theme"), &t, vec![row(t!("overlays.settings.dark"), Some(t!("overlays.settings.dark_hint")), room, switch("dark", dark, &t).on_click(cx.listener(move |this, _, _, cx| this.set_dark(!dark, cx))), &t, window)]),
                    page_section(t!("overlays.settings.language"), &t, vec![self.language_row(cw, window, cx)]),
                ]
            }
        };
        let dialog = div()
            .id("settings-page")
            .occlude()
            .w(px(dw))
            .h(px(dh))
            .rounded(px(XL))
            .border_1()
            .border_color(t.border)
            .overflow_hidden()
            .shadow(shadow_lg())
            .bg(t.background)
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .h(px(53.))
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(t.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .sm()
                            .child(div().text_color(t.muted_fg).child(t!("overlays.settings.title")))
                            .child(div().text_color(t.muted_fg).child("/"))
                            .child(div().medium().child(tab.label())),
                    )
                    .child(
                        div()
                            .id("settings-close")
                            .size(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(MD))
                            .opacity(0.7)
                            .hover(|s| s.opacity(1.))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.panel = None;
                                cx.notify();
                            }))
                            .child(icon("x", 16., t.foreground)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .when(wide, |d| d.flex_row())
                    .when(!wide, |d| d.flex_col())
                    .child(nav)
                    .child(
                        div().id("settings-scroll").flex_1().min_w_0().min_h_0().overflow_y_scroll().child(
                            div().w(px(cw)).mx_auto().py(px(24.)).flex().flex_col().gap(px(32.)).children(content),
                        ),
                    ),
            )
            ;
        div()
            .absolute()
            .inset_0()
            .opacity(fade)
            .flex()
            .items_center()
            .justify_center()
            .child(div().id("settings-overlay").absolute().inset_0().bg(hsla(0., 0., 0., 0.5)).occlude().on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.panel = None;
                    cx.notify();
                }),
            ))
            .child(dialog)
            .into_any_element()
    }

    fn connection_sections(&self, cw: f32, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = self.theme(cx);
        let segmented = |id: &'static str, items: Vec<(String, bool)>, on_pick: Vec<Box<dyn Fn(&mut Self, &mut Context<Self>)>>| {
            let mut group = div().flex().bg(t.background).rounded(px(MD)).p(px(4.));
            let n = items.len();
            for (i, ((label, on), pick)) in items.into_iter().zip(on_pick).enumerate() {
                let (muted, muted_fg) = (t.muted, t.muted_fg);
                group = group.child(
                    div()
                        .id(SharedString::from(format!("{id}-{label}")))
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
                            pick(this, cx);
                            cx.notify();
                        }))
                        .child(label),
                );
            }
            group
        };
        let proto_items: Vec<(String, bool)> = Protocol::ALL.into_iter().map(|p| (p.label().to_owned(), self.s.protocol == p)).collect();
        let proto_picks: Vec<Box<dyn Fn(&mut Self, &mut Context<Self>)>> = Protocol::ALL.into_iter().map(|p| Box::new(move |this: &mut Self, _: &mut Context<Self>| this.s.protocol = p) as Box<dyn Fn(&mut Self, &mut Context<Self>)>).collect();
        let group = segmented("proto", proto_items, proto_picks);
        let toggle_w = 8. + Protocol::ALL.iter().map(|p| crate::ui::text_width(p.label(), 14., gpui::FontWeight::MEDIUM, window) + 24.).sum::<f32>();
        let mode = (crate::os::SYSTEM_PROXY && self.s.system_wide).then(|| self.mode_row(cw, window, cx));
        let room = page_room(cw, 32.);
        vec![page_section(
            t!("overlays.connection.title"),
            &t,
            [row(t!("overlays.connection.protocol"), Some(t!("overlays.connection.protocol_hint")), page_room(cw, toggle_w), group, &t, window)]
                .into_iter()
                .chain([row(
                    t!("overlays.connection.system_wide"),
                    Some(if !self.s.system_wide {
                        t!("overlays.connection.system_wide_off")
                    } else if crate::os::SYSTEM_PROXY {
                        t!("overlays.connection.system_wide_proxy")
                    } else {
                        t!("overlays.connection.system_wide_tun")
                    }),
                    room,
                    switch("system-wide", self.s.system_wide, &t).on_click(cx.listener(|this, _, _, cx| {
                        this.s.system_wide = !this.s.system_wide;
                        cx.notify();
                    })),
                    &t,
                    window,
                )])
                .chain(mode)
                .chain([
                    row(
                        t!("overlays.connection.startup"),
                        Some(t!("overlays.connection.startup_hint")),
                        room,
                        switch("startup", self.s.auto_connect, &t).on_click(cx.listener(|this, _, _, cx| {
                            this.s.auto_connect = !this.s.auto_connect;
                            cx.notify();
                        })),
                        &t,
                        window,
                    ),
                    row(
                        t!("overlays.connection.kill_switch"),
                        Some(if !self.s.system_wide {
                            t!("overlays.connection.kill_switch_needs_system_wide")
                        } else if crate::os::SYSTEM_PROXY && !self.s.tun {
                            t!("overlays.connection.kill_switch_needs_tun")
                        } else {
                            t!("overlays.connection.kill_switch_hint")
                        }),
                        room,
                        switch("kill-switch", self.s.kill_switch, &t).on_click(cx.listener(|this, _, _, cx| {
                            this.s.kill_switch = !this.s.kill_switch;
                            cx.notify();
                        })),
                        &t,
                        window,
                    ),
                ])
                .collect(),
        )]
    }

    /// Detected country, how much of it is routed through the VPN, and a recheck button.
    fn region_row(&self, cw: f32, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let r = &self.s.region;
        let busy = r.status == crate::region::Status::Computing;
        let hint = match (&r.country, &r.status) {
            (Some(_), crate::region::Status::Failed(e)) if r.domains + r.subnets > 0 => t!("overlays.region.failed_saved", place = r.place(), error = e),
            (Some(_), crate::region::Status::Failed(e)) => t!("overlays.region.failed", place = r.place(), error = e),
            (Some(_), _) => format!("{} · {}", r.place(), r.line()),
            (None, _) => r.line(),
        };
        let label = if busy { t!("overlays.region.checking") } else { t!("overlays.region.recheck") };
        let w = 2. + 24. + 16. + 8. + crate::ui::text_width(label, 14., gpui::FontWeight::MEDIUM, window);
        let button = button("region-recheck", Variant::Outline, Size::Sm, true, &t)
            .when(busy, |b| b.opacity(0.5))
            .child(if busy { crate::ui::spinner("region-spin", 16., t.foreground).into_any_element() } else { icon("refresh-cw", 16., t.foreground).into_any_element() })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                if !busy {
                    this.recheck_region(cx);
                }
            }));
        row(t!("overlays.region.detected"), Some(&hint), page_room(cw, w), button, &t, window)
    }

    /// System, English, Русский: switches every window and the tray right away.
    fn language_row(&self, cw: f32, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let choices: Vec<(Option<crate::i18n::Lang>, &str)> =
            std::iter::once((None, t!("overlays.settings.language_system"))).chain(crate::i18n::Lang::ALL.map(|l| (Some(l), l.name()))).collect();
        let n = choices.len();
        let mut group = div().flex().bg(t.background).rounded(px(MD)).p(px(4.));
        for (i, (choice, label)) in choices.iter().copied().enumerate() {
            let (muted, muted_fg) = (t.muted, t.muted_fg);
            let on = self.s.language == choice;
            group = group.child(
                div()
                    .id(SharedString::from(format!("language-{}", choice.map_or("system", |l| l.code()))))
                    .h(px(32.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .sm()
                    .medium()
                    .cursor_pointer()
                    .when(i == 0, |d| d.rounded_l(px(MD)))
                    .when(i == n - 1, |d| d.rounded_r(px(MD)))
                    .when(on, |d| d.bg(t.accent))
                    .when(!on, |d| d.hover(move |s| s.bg(muted).text_color(muted_fg)))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_language(choice, cx)))
                    .child(label),
            );
        }
        let w = 8. + choices.iter().map(|(_, l)| crate::ui::text_width(l, 14., gpui::FontWeight::MEDIUM, window) + 24.).sum::<f32>();
        row(t!("overlays.settings.language_row"), Some(t!("overlays.settings.language_hint")), page_room(cw, w), group, &t, window)
    }

    /// Windows and macOS: how system-wide mode works, the system proxy or a TUN.
    fn mode_row(&self, cw: f32, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let modes = [(false, t!("overlays.mode.system_proxy")), (true, "TUN")];
        let mut group = div().flex().bg(t.background).rounded(px(MD)).p(px(4.));
        for (i, (tun, label)) in modes.into_iter().enumerate() {
            let (muted, muted_fg) = (t.muted, t.muted_fg);
            group = group.child(
                div()
                    .id(SharedString::from(format!("mode-{tun}")))
                    .h(px(32.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .sm()
                    .medium()
                    .cursor_pointer()
                    .when(i == 0, |d| d.rounded_l(px(MD)))
                    .when(i == 1, |d| d.rounded_r(px(MD)))
                    .when(self.s.tun == tun, |d| d.bg(t.accent))
                    .when(self.s.tun != tun, |d| d.hover(move |s| s.bg(muted).text_color(muted_fg)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.s.tun = tun;
                        cx.notify();
                    }))
                    .child(label),
            );
        }
        let w = 8. + modes.iter().map(|(_, l)| crate::ui::text_width(l, 14., gpui::FontWeight::MEDIUM, window) + 24.).sum::<f32>();
        let hint = match (self.s.tun, cfg!(windows)) {
            (false, _) => t!("overlays.mode.proxy_hint"),
            (true, true) => t!("overlays.mode.tun_hint_windows"),
            (true, false) => t!("overlays.mode.tun_hint"),
        };
        row(t!("overlays.mode.title"), Some(hint), page_room(cw, w), group, &t, window)
    }

    pub fn sign_in_body(&self, cx: &mut Context<Self>) -> AnyElement {
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
                        .child(if busy { t!("overlays.signin.busy") } else { t!("overlays.signin.button") })
                        .on_click(cx.listener(|this, _, window, cx| this.sign_in(window, cx))),
                ),
            )
            .child(div().xs().text_color(t.muted_fg).child(t!("overlays.signin.note")))
            .into_any_element()
    }

    fn account_body(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.s.signed_in {
            return self.sign_in_body(cx);
        }
        let t = self.theme(cx);
        let a = self.s.account.clone().unwrap_or_default();
        let days = a.days_left.map_or("—".to_owned(), |d| t!("overlays.account.days", n = d));
        let renews = a.expires.clone().map(|d| t!("overlays.account.until", date = d)).unwrap_or_default();
        // Days of a 30-day period, or the traffic allowance when there is one.
        let fill = if a.limit > 0 { 1. - (a.used as f32 / a.limit as f32).min(1.) } else { a.days_left.map_or(0., |d| (d as f32 / 30.).min(1.)) };
        let traffic = if a.limit > 0 { t!("overlays.account.used_of", used = data::bytes(a.used), limit = data::bytes(a.limit)) } else { t!("overlays.account.used_unlimited", used = data::bytes(a.used)) };
        let support = a.support.clone();
        let profile = self.s.user.as_ref().map(|u| {
            div()
                .flex()
                .items_center()
                .gap(px(16.))
                .p(px(16.))
                .rounded(px(XL))
                .bg(t.muted.opacity(0.5))
                .child(crate::ui::avatar(&self.initial(), self.s.avatar.clone(), 48., &t))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().base().semibold().truncate().child(u.name.clone()))
                        .child(div().sm().text_color(t.muted_fg).truncate().child(u.username.as_ref().map_or(t!("overlays.account.telegram_account").into(), |n| format!("@{n}")))),
                )
                .child(badge_secondary("Telegram", &t))
        });
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
                    .child(div().flex_1().flex().flex_col().gap(px(8.)).child(div().sm().text_color(t.muted_fg).child(t!("overlays.account.plan"))).child(div().text_px(24., 32.).semibold().child(a.plan())))
                    .child(div().flex().flex_col().items_end().child(div().xl().semibold().child(days)).child(div().xs().text_color(t.muted_fg).child(renews))),
            )
            .child(div().px(px(24.)).flex().flex_col().gap(px(8.)).child(progress(fill, &t)).child(div().xs().text_color(t.muted_fg).child(traffic)))
            .children(support.map(|url| {
                div().px(px(24.)).flex().items_center().child(
                    button("extend", Variant::Default, Size::Default, false, &t)
                        .child(t!("overlays.account.extend"))
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
            .child(div().flex_1().flex().flex_col().gap(px(4.)).child(div().sm().medium().child(crate::sub::hostname())).child(div().sm().text_color(t.muted_fg).child(t!("overlays.account.desktop", os = crate::sub::OS))))
            .child(badge_secondary(t!("overlays.account.this_device"), &t))
            .into_any_element();
        div()
            .flex()
            .flex_col()
            .gap(px(24.))
            .children(profile)
            .child(card)
            .child(section(t!("overlays.account.devices").into(), &t, vec![device]))
            .child(section(
                t!("overlays.account.session").into(),
                &t,
                vec![row(
                    t!("overlays.account.sign_out"),
                    Some(if self.s.user.is_some() {
                        t!("overlays.account.sign_out_telegram")
                    } else {
                        t!("overlays.account.sign_out_link")
                    }),
                    hint_room(2. + 24. + 16. + 8. + crate::ui::text_width(t!("overlays.account.sign_out"), 14., gpui::FontWeight::MEDIUM, window)),
                    button("signout", Variant::Outline, Size::Sm, true, &t)
                        .child(icon("log-out", 16., t.foreground))
                        .child(t!("overlays.account.sign_out"))
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
                        .child(heading(t!("overlays.routes.suggestions", count = suggestions.len())))
                        .children(suggestions.iter().map(|s| self.suggestion_card(s, false, cx).into_any_element())),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().mr(px(-4.)).h(px(32.)).flex().items_center().justify_between().child(heading(t!("overlays.routes.apps", count = self.s.app_routes.len()))).child(self.add_app(cx)))
                    .child(self.active_rules(true, false, Place::Sheet, window, cx)),
            )
            .child(div().flex().flex_col().gap(px(8.)).child(heading(t!("overlays.routes.sites", count = self.s.sites.len()))).child(self.site_field(cx)).when(!self.s.sites.is_empty(), |d| d.child(sites)))
            .child(div().xs().text_color(t.muted_fg).child(t!("overlays.routes.everything_else", route = self.s.default_route.label(), hint = self.s.default_route.default_hint().to_lowercase())))
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

