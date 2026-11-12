//! Sheet (Account, Settings, Routing rules) and the location palette.
use gpui::{AnyElement, Context, Div, Entity, Focusable, MouseButton, SharedString, Window, div, hsla, prelude::*, px};

use crate::app::{Hitori, Panel, Place, Protocol, SettingsTab};
use crate::data;
use crate::input::TextInput;
use crate::theme::{Theme, shadow_lg};
use crate::ui::{LG, MD, Size, Tw, Variant, XL, badge_secondary, button, icon, progress, switch};

type Pick = Box<dyn Fn(&mut Hitori, &mut Context<Hitori>)>;

/// Width of a segmented control with these labels.
fn segmented_width<'a>(labels: impl IntoIterator<Item = &'a str>, window: &Window) -> f32 {
    8. + labels.into_iter().map(|l| crate::ui::text_width(l, 14., gpui::FontWeight::MEDIUM, window) + 24.).sum::<f32>()
}

/// Single-line field around a `TextInput`; `invalid` gives it the destructive border.
fn text_field(id: &'static str, input: &Entity<TextInput>, w: f32, invalid: bool, t: &Theme, cx: &gpui::App) -> impl IntoElement {
    let focus = input.read(cx).focus_handle(cx);
    div()
        .id(id)
        .w(px(w))
        .h(px(36.))
        .px(px(12.))
        .flex()
        .items_center()
        .rounded(px(MD))
        .border_1()
        .border_color(if invalid { t.destructive } else { t.border })
        .bg(t.background)
        .sm()
        .cursor_text()
        .on_mouse_down(MouseButton::Left, move |_, window, _| window.focus(&focus))
        .child(div().flex_1().min_w_0().child(input.clone()))
}


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
                Some(_) if self.s.signed_in => ("Account", format!("Signed in as {}", self.account_name())),
                _ if self.s.signed_in => ("Account", "Loading your subscription…".into()),
                _ => ("Sign in", "Paste the subscription link from the Hitori bot.".into()),
            },
            Panel::Routes => ("Routing rules", format!("Apps and sites without a rule follow the default route: {}.", self.s.default_route.label())),
            Panel::Log => ("Connection log", "The last error and the core's recent output.".into()),
            Panel::Settings => unreachable!(),
        };
        // Sheet: overlay `fade-in-0` 150ms; content `slide-in-from-right` 500ms in / 300ms out, ease-in-out.
        let dim = self.panel_fx.phase(panel, 150., 150., crate::fx::ease).map_or(0., |p| p.0);
        let slide = self.panel_fx.phase(panel, 500., 300., crate::fx::ease_in_out).map_or(0., |p| p.0);
        let body = match panel {
            Panel::Account => self.account_body(window, cx),
            Panel::Settings => unreachable!(),
            Panel::Routes => self.routes_body(window, cx),
            Panel::Log => self.log_body(cx),
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
                    .id(SharedString::from(format!("settings-tab-{}", k.label())))
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
            SettingsTab::Network => self.network_sections(cw, window, cx),
            SettingsTab::Location => vec![page_section("Location", &t, vec![self.region_row(cw, window, cx)])],
            SettingsTab::Appearance => {
                let dark = t.dark;
                let room = page_room(cw, 32.);
                vec![page_section("Theme", &t, vec![row("Dark theme", Some("Use the dark color scheme."), room, switch("dark", dark, &t).on_click(cx.listener(move |this, _, _, cx| this.set_dark(!dark, cx))), &t, window)])]
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
                            .child(div().text_color(t.muted_fg).child("Settings"))
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

    /// Segmented control: one button per item, the `on` one highlighted.
    fn segmented(&self, id: &str, items: Vec<(String, bool, Pick)>, cx: &mut Context<Self>) -> Div {
        let t = self.theme(cx);
        let mut group = div().flex().bg(t.background).rounded(px(MD)).p(px(4.));
        let n = items.len();
        for (i, (label, on, pick)) in items.into_iter().enumerate() {
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
    }

    fn connection_sections(&self, cw: f32, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = self.theme(cx);
        let proto_items = Protocol::ALL.into_iter().map(|p| (p.label().to_owned(), self.s.protocol == p, Box::new(move |this: &mut Self, _: &mut Context<Self>| this.s.protocol = p) as Pick)).collect();
        let group = self.segmented("proto", proto_items, cx);
        let toggle_w = segmented_width(Protocol::ALL.iter().map(|p| p.label()), window);
        let mode = (crate::os::SYSTEM_PROXY && self.s.system_wide).then(|| self.mode_row(cw, window, cx));
        let room = page_room(cw, 32.);
        let p = &self.s.proxy;
        let proxy_hint = format!("Only apps set to use the proxy: SOCKS5 {}:{}, HTTP {}.", p.host(), p.socks, p.http);
        vec![page_section(
            "Connection",
            &t,
            [row("Protocol", Some("TLS gets through the most filters."), page_room(cw, toggle_w), group, &t, window)]
                .into_iter()
                .chain([row(
                    "System-wide",
                    Some(if !self.s.system_wide {
                        &proxy_hint
                    } else if crate::os::SYSTEM_PROXY {
                        "Browsers and most apps, or every app in TUN mode, go through Hitori."
                    } else {
                        "Every app goes through Hitori. Asks for your password to connect."
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
                        "Connect on startup",
                        Some("Connect as soon as the app starts."),
                        room,
                        switch("startup", self.s.auto_connect, &t).on_click(cx.listener(|this, _, _, cx| {
                            this.s.auto_connect = !this.s.auto_connect;
                            cx.notify();
                        })),
                        &t,
                        window,
                    ),
                    row(
                        "Kill switch",
                        Some(if !self.s.system_wide {
                            "Needs system-wide mode."
                        } else if crate::os::SYSTEM_PROXY && !self.s.tun {
                            "Needs TUN mode."
                        } else {
                            "Block all traffic while the tunnel is down."
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

    /// DNS: the subscription's servers or a DoH/DoT server, through the tunnel or not.
    fn dns_section(&self, cw: f32, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        use crate::xray::{DNS_PRESETS, DnsMode};
        let t = self.theme(cx);
        let dns = self.s.dns.clone();
        let modes = [(DnsMode::Subscription, "Subscription"), (DnsMode::Https, "HTTPS"), (DnsMode::Tls, "TLS")];
        let width = segmented_width(modes.iter().map(|(_, l)| *l), window);
        let items = modes
            .into_iter()
            .map(|(m, label)| {
                let pick = move |this: &mut Self, cx: &mut Context<Self>| this.set_dns(crate::xray::Dns { mode: m, ..this.s.dns.clone() }, cx);
                (label.to_owned(), dns.mode == m, Box::new(pick) as Pick)
            })
            .collect();
        let hint = match dns.mode {
            DnsMode::Subscription => "The servers your subscription sets.",
            DnsMode::Https => "Encrypted DNS over HTTPS.",
            DnsMode::Tls => "Encrypted DNS over TLS.",
        };
        let mut rows = vec![row("Servers", Some(hint), page_room(cw, width), self.segmented("dns-mode", items, cx), &t, window)];
        if dns.mode != DnsMode::Subscription {
            let tls = dns.mode == DnsMode::Tls;
            let typed = self.dns_input.read(cx).content.trim().to_owned();
            let width = segmented_width(DNS_PRESETS.iter().map(|p| p.0), window);
            let presets = DNS_PRESETS
                .iter()
                .map(|&(name, https, dot)| {
                    let pick = move |this: &mut Self, cx: &mut Context<Self>| {
                        let mut d = this.s.dns.clone();
                        if tls { d.tls = dot.into() } else { d.https = https.into() }
                        this.set_dns(d, cx);
                    };
                    (name.to_owned(), typed == if tls { dot } else { https }, Box::new(pick) as Pick)
                })
                .collect();
            rows.push(row("Provider", Some("Or type another server below."), page_room(cw, width), self.segmented("dns-preset", presets, cx), &t, window));
            let error = self.typed_dns(cx).err();
            let hint = match (&error, tls) {
                (Some(e), _) => format!("{e}."),
                (None, false) => "With an IP address in the URL no lookup is needed first.".to_owned(),
                (None, true) => "A host or host:port, port 853 if none is given.".to_owned(),
            };
            rows.push(row("Server", Some(&hint), page_room(cw, 260.), text_field("dns-server", &self.dns_input, 260., error.is_some(), &t, cx), &t, window));
        }
        let direct = dns.direct;
        rows.push(row(
            "Through the VPN",
            Some(if direct { "Queries leave from this computer directly." } else { "Queries go through the tunnel, unseen by your network." }),
            page_room(cw, 32.),
            switch("dns-direct", !direct, &t).on_click(cx.listener(move |this, _, _, cx| {
                this.s.dns.direct = !direct;
                cx.notify();
            })),
            &t,
            window,
        ));
        page_section("DNS", &t, rows)
    }

    /// Local proxy and DNS.
    fn network_sections(&self, cw: f32, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = self.theme(cx);
        let listen = self.s.proxy.listen.clone();
        let lan = self.s.lan.map(|a| a.to_string());
        // Loopback, the LAN address, a saved address from another network, every network.
        let mut choices: Vec<String> = vec![];
        for c in ["127.0.0.1".to_owned()].into_iter().chain(lan.clone()).chain([listen.clone(), "0.0.0.0".into()]) {
            if !choices.contains(&c) {
                choices.push(c);
            }
        }
        let width = segmented_width(choices.iter().map(String::as_str), window);
        let items = choices
            .into_iter()
            .map(|c| {
                let on = c == listen;
                let pick = c.clone();
                (c, on, Box::new(move |this: &mut Self, _: &mut Context<Self>| this.s.proxy.listen = pick.clone()) as Pick)
            })
            .collect();
        let listen_hint = match listen.as_str() {
            "127.0.0.1" => "Only apps on this computer can connect.".to_owned(),
            "0.0.0.0" => "Any network this computer is on. Anyone there can use your VPN.".to_owned(),
            ip if Some(ip) == lan.as_deref() => "Devices on your network can connect. Anyone there can use your VPN.".to_owned(),
            ip => format!("{ip} is from another network."),
        };
        let error = self.ports_error(cx);
        let socks_bad = crate::app::port(&self.socks_input, cx).is_none() || error.as_deref().is_some_and(|e| e.contains("different"));
        let http_bad = crate::app::port(&self.http_input, cx).is_none() || error.as_deref().is_some_and(|e| e.contains("different"));
        let caption = |s: &str| div().sm().text_color(t.muted_fg).child(s.to_owned());
        let ports = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(caption("SOCKS5"))
            .child(text_field("socks-port", &self.socks_input, 80., socks_bad, &t, cx))
            .child(caption("HTTP"))
            .child(text_field("http-port", &self.http_input, 80., http_bad, &t, cx));
        let ports_w = 3. * 8. + 2. * 80. + ["SOCKS5", "HTTP"].iter().map(|s| crate::ui::text_width(s, 14., gpui::FontWeight::NORMAL, window)).sum::<f32>();
        let p = &self.s.proxy;
        let ports_hint = match error {
            Some(e) => format!("{e}."),
            None if self.s.uses_tun() => "Not used while every app goes through the TUN.".to_owned(),
            None => format!("Apps connect to SOCKS5 {0}:{1} and HTTP {0}:{2}.", p.host(), p.socks, p.http),
        };
        vec![
            page_section(
                "Local proxy",
                &t,
                vec![
                    row("Listen on", Some(&listen_hint), page_room(cw, width), self.segmented("listen", items, cx), &t, window),
                    row("Ports", Some(&ports_hint), page_room(cw, ports_w), ports, &t, window),
                ],
            ),
            self.dns_section(cw, window, cx),
            div().px(px(4.)).sm().text_color(t.muted_fg).child("Changes apply on the next connection.").into_any_element(),
        ]
    }

    /// Detected country, how much of it is routed through the VPN, and a recheck button.
    fn region_row(&self, cw: f32, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let r = &self.s.region;
        let busy = r.status == crate::region::Status::Computing;
        let hint = match (&r.country, &r.status) {
            (Some(_), crate::region::Status::Failed(e)) if r.domains + r.subnets > 0 => format!("{} · {e}. The saved lists still apply.", r.place()),
            (Some(_), crate::region::Status::Failed(e)) => format!("{} · {e}.", r.place()),
            (Some(_), _) => format!("{} · {}", r.place(), r.line()),
            (None, _) => r.line(),
        };
        let label = if busy { "Checking…" } else { "Recheck" };
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
        row("Detected location", Some(&hint), page_room(cw, w), button, &t, window)
    }

    /// Windows and macOS: how system-wide mode works, the system proxy or a TUN.
    fn mode_row(&self, cw: f32, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let modes = [(false, "System proxy"), (true, "TUN")];
        let mut group = div().flex().bg(t.background).rounded(px(MD)).p(px(4.));
        for (i, (tun, label)) in modes.into_iter().enumerate() {
            let (muted, muted_fg) = (t.muted, t.muted_fg);
            group = group.child(
                div()
                    .id(SharedString::from(format!("mode-{label}")))
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
            (false, _) => "Browsers and most apps, no administrator rights.",
            (true, true) => "Every app and DNS. Asks for administrator rights.",
            (true, false) => "Every app and DNS. Asks for your password.",
        };
        row("Mode", Some(hint), page_room(cw, w), group, &t, window)
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
                        .child(div().sm().text_color(t.muted_fg).truncate().child(u.username.as_ref().map_or("Telegram account".into(), |n| format!("@{n}")))),
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
            .children(profile)
            .child(card)
            .child(section("Devices".into(), &t, vec![device]))
            .child(section(
                "Session".into(),
                &t,
                vec![row(
                    "Sign out",
                    Some(if self.s.user.is_some() {
                        "Signs this computer out of your Telegram account; the tunnel stops."
                    } else {
                        "Forgets the subscription link on this computer; the tunnel stops."
                    }),
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

    /// The full error the status line cuts off, the core's last lines, and a button copying both.
    fn log_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let lines = self.vpn.read(cx).log_lines();
        let error = self.s.error.clone();
        let text = error.iter().cloned().chain(lines.iter().cloned()).collect::<Vec<_>>().join("\n");
        let empty = lines.is_empty();
        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .children(error.map(|e| div().p(px(12.)).rounded(px(LG)).bg(t.destructive.opacity(0.1)).sm().text_color(t.destructive).child(e)))
            .child(
                div().flex().child(
                    button("log-copy", Variant::Outline, Size::Sm, true, &t)
                        .child(icon("copy", 16., t.foreground))
                        .child("Copy")
                        .on_click(move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()))),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .p(px(12.))
                    .rounded(px(LG))
                    .bg(t.muted.opacity(0.5))
                    .xs()
                    .text_color(t.muted_fg)
                    .when(empty, |d| d.child("No core output yet."))
                    .children(lines.into_iter().map(|l| div().child(l))),
            )
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

