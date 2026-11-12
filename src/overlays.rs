//! Sheet (Account, Settings, Routing rules) and the location palette.
use gpui::{AnyElement, Context, Div, Entity, Focusable, MouseButton, SharedString, Window, div, hsla, prelude::*, px};

use crate::app::{Hitori, Menu, Panel, Place, Protocol, SettingsTab};
use crate::data;
use crate::input::TextInput;
use crate::theme::{Theme, shadow_lg};
use crate::ui::{LG, MD, Size, Tw, Variant, XL, badge_secondary, button, icon, menu, menu_item, progress, switch, tip};
use crate::views::{Placement, popover};

type Pick = Box<dyn Fn(&mut Hitori, &mut Context<Hitori>)>;

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

/// Section heading above a bordered card whose rows are split by dividers.
fn page_section(title: &str, t: &Theme, rows: Vec<AnyElement>) -> AnyElement {
    let mut card = div().flex().flex_col().rounded(px(XL)).border_1().border_color(t.border).bg(t.muted.opacity(0.3));
    for (i, r) in rows.into_iter().enumerate() {
        card = card.child(div().when(i > 0, |d| d.border_t_1().border_color(t.border)).child(r));
    }
    div().flex().flex_col().gap(px(8.)).child(div().px(px(4.)).sm().semibold().child(title.to_owned())).child(card).into_any_element()
}

/// Settings row: the label with an ⓘ that explains it on hover, a note under it (status or error), the control on the right.
fn setting(id: &str, label: &str, info: Option<&str>, note: Option<(&str, gpui::Hsla)>, control: impl IntoElement, t: &Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(16.))
        .min_h(px(56.))
        .px(px(16.))
        .py(px(10.))
        .sm()
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(div().flex().items_center().gap(px(6.)).child(div().medium().child(label.to_owned())).children(info.map(|i| {
                    div().id(SharedString::from(format!("info-{id}"))).flex_none().child(icon("info", 14., t.muted_fg)).tooltip(tip(i.to_owned(), t))
                })))
                .children(note.map(|(n, color)| div().xs().text_color(color).line_clamp(2).child(n.to_owned()))),
        )
        .child(div().flex().flex_none().items_center().gap(px(8.)).child(control))
        .into_any_element()
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
            Panel::Log => (t!("overlays.log.title"), t!("overlays.log.desc").into()),
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
        let cw = (dw - side - 48.).min(640.);
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
                    .when(wide || on, |d| d.child(k.label())),
            );
        }
        let content = match tab {
            SettingsTab::Connection => self.connection_sections(window, cx),
            SettingsTab::Network => self.network_sections(window, cx),
            SettingsTab::Location => vec![page_section(t!("overlays.settings.location"), &t, vec![self.region_row(cx)])],
            SettingsTab::Appearance => {
                let dark = t.dark;
                let theme = setting("dark", t!("overlays.settings.dark"), None, None, switch("dark", dark, &t).on_click(cx.listener(move |this, _, _, cx| this.set_dark(!dark, cx))), &t);
                vec![page_section(SettingsTab::Appearance.label(), &t, vec![theme, self.language_row(window, cx)])]
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
                            div().w(px(cw)).mx_auto().py(px(20.)).flex().flex_col().gap(px(20.)).children(content),
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

    /// Select: a trigger with the current value; the options drop below it, the current one checked.
    fn select(&self, id: &'static str, items: Vec<(String, bool, Pick)>, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let widest = items.iter().map(|(l, ..)| crate::ui::text_width(l, 14., gpui::FontWeight::NORMAL, window)).fold(0., f32::max);
        let w = (widest + 2. + 24. + 8. + 16.).max(120.);
        let current = items.iter().find(|i| i.1).map_or(String::new(), |i| i.0.clone());
        let muted = t.muted;
        let trigger = div()
            .id(SharedString::from(format!("select-{id}")))
            .w(px(w))
            .h(px(36.))
            .px(px(12.))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .rounded(px(MD))
            .border_1()
            .border_color(t.border)
            .bg(t.background)
            .cursor_pointer()
            .hover(move |s| s.bg(muted))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_menu(Menu::Select(id), cx)))
            .child(div().min_w_0().truncate().child(current))
            .child(icon("chevron-down", 16., t.muted_fg));
        let popup = self.menu_phase(Menu::Select(id)).map(|ph| {
            let options = items
                .into_iter()
                .enumerate()
                .map(|(i, (label, on, pick))| {
                    menu_item(SharedString::from(format!("{id}-option-{i}")), &t, false)
                        .child(div().flex_1().child(label))
                        .when(on, |d| d.child(icon("check", 16., t.foreground)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            pick(this, cx);
                            this.menu = None;
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect();
            popover(Placement::BelowEnd, ph, menu(w.max(widest + 56.), &t, options))
        });
        div().relative().child(trigger).children(popup).into_any_element()
    }

    fn connection_sections(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = self.theme(cx);
        let protocols = Protocol::ALL.into_iter().map(|p| (p.label().to_owned(), self.s.protocol == p, Box::new(move |this: &mut Self, _: &mut Context<Self>| this.s.protocol = p) as Pick)).collect();
        let p = &self.s.proxy;
        let proxy_info = t!("overlays.connection.system_wide_off", host = p.host(), socks = p.socks, http = p.http);
        let system_info = if !self.s.system_wide {
            proxy_info.as_str()
        } else if crate::os::SYSTEM_PROXY {
            t!("overlays.connection.system_wide_proxy")
        } else {
            t!("overlays.connection.system_wide_tun")
        };
        let kill_info = if !self.s.system_wide {
            t!("overlays.connection.kill_switch_needs_system_wide")
        } else if crate::os::SYSTEM_PROXY && !self.s.tun {
            t!("overlays.connection.kill_switch_needs_tun")
        } else {
            t!("overlays.connection.kill_switch_hint")
        };
        let mut rows = vec![
            setting("protocol", t!("overlays.connection.protocol"), Some(t!("overlays.connection.protocol_hint")), None, self.select("protocol", protocols, window, cx), &t),
            setting(
                "system-wide",
                t!("overlays.connection.system_wide"),
                Some(system_info),
                None,
                switch("system-wide", self.s.system_wide, &t).on_click(cx.listener(|this, _, _, cx| {
                    this.s.system_wide = !this.s.system_wide;
                    cx.notify();
                })),
                &t,
            ),
        ];
        if crate::os::SYSTEM_PROXY && self.s.system_wide {
            rows.push(self.mode_row(window, cx));
        }
        rows.push(setting(
            "startup",
            t!("overlays.connection.startup"),
            None,
            None,
            switch("startup", self.s.auto_connect, &t).on_click(cx.listener(|this, _, _, cx| {
                this.s.auto_connect = !this.s.auto_connect;
                cx.notify();
            })),
            &t,
        ));
        rows.push(setting(
            "kill-switch",
            t!("overlays.connection.kill_switch"),
            Some(kill_info),
            None,
            switch("kill-switch", self.s.kill_switch, &t).on_click(cx.listener(|this, _, _, cx| {
                this.s.kill_switch = !this.s.kill_switch;
                cx.notify();
            })),
            &t,
        ));
        vec![page_section(t!("overlays.connection.title"), &t, rows)]
    }

    /// DNS: the subscription's servers, or one typed server (DoH URL or DoT host:port), through the tunnel or not.
    fn dns_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let custom = self.s.dns.mode != crate::xray::DnsMode::Subscription;
        let mut rows = vec![setting(
            "dns-custom",
            t!("overlays.dns.custom"),
            Some(t!("overlays.dns.custom_info")),
            None,
            switch("dns-custom", custom, &t).on_click(cx.listener(move |this, _, _, cx| {
                this.set_custom_dns(!custom, cx);
                cx.notify();
            })),
            &t,
        )];
        if custom {
            let error = self.typed_dns(cx).err();
            let field = text_field("dns-server", &self.dns_input, 240., error.is_some(), &t, cx);
            rows.push(setting("dns-server", t!("overlays.dns.server"), Some(t!("overlays.dns.server_info")), error.as_deref().map(|e| (e, t.destructive)), field, &t));
            let direct = self.s.dns.direct;
            rows.push(setting(
                "dns-direct",
                t!("overlays.dns.through_vpn"),
                Some(if direct { t!("overlays.dns.direct_hint") } else { t!("overlays.dns.tunnel_hint") }),
                None,
                switch("dns-direct", !direct, &t).on_click(cx.listener(move |this, _, _, cx| {
                    this.s.dns.direct = !direct;
                    cx.notify();
                })),
                &t,
            ));
        }
        page_section(t!("overlays.dns.title"), &t, rows)
    }

    /// Local proxy (who may connect, which ports) and DNS.
    fn network_sections(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = self.theme(cx);
        let proxy = &self.s.proxy;
        let (listen, shared) = (proxy.listen.clone(), proxy.lan());
        let lan = self.s.lan.map(|a| a.to_string());
        let mut rows = vec![setting(
            "lan-access",
            t!("overlays.network.lan_access"),
            Some(t!("overlays.network.lan_access_info")),
            None,
            switch("lan-access", shared, &t).on_click(cx.listener(move |this, _, _, cx| {
                this.s.proxy.listen = if shared { "127.0.0.1".into() } else { this.s.lan.map_or("0.0.0.0".into(), |a| a.to_string()) };
                cx.notify();
            })),
            &t,
        )];
        if shared {
            // The LAN address, a saved address from another network, every network.
            let mut choices: Vec<String> = vec![];
            for c in lan.clone().into_iter().chain([listen.clone(), "0.0.0.0".into()]) {
                if !choices.contains(&c) {
                    choices.push(c);
                }
            }
            let items = choices
                .into_iter()
                .map(|c| {
                    let on = c == listen;
                    let pick = c.clone();
                    (c, on, Box::new(move |this: &mut Self, _: &mut Context<Self>| this.s.proxy.listen = pick.clone()) as Pick)
                })
                .collect();
            let info = match listen.as_str() {
                "0.0.0.0" => t!("overlays.network.listen_any").to_owned(),
                ip if Some(ip) == lan.as_deref() => t!("overlays.network.listen_lan").to_owned(),
                ip => t!("overlays.network.listen_other", ip = ip),
            };
            rows.push(setting("listen", t!("overlays.network.listen_on"), Some(&info), None, self.select("listen", items, window, cx), &t));
        }
        let error = self.ports_error(cx);
        let (socks, http) = (crate::app::port(&self.socks_input, cx), crate::app::port(&self.http_input, cx));
        let same = socks.is_some() && socks == http;
        let caption = |s: &str| div().text_color(t.muted_fg).child(s.to_owned());
        let ports = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(caption("SOCKS5"))
            .child(text_field("socks-port", &self.socks_input, 80., socks.is_none() || same, &t, cx))
            .child(caption("HTTP"))
            .child(text_field("http-port", &self.http_input, 80., http.is_none() || same, &t, cx));
        let ports_info = if self.s.uses_tun() && !shared {
            t!("overlays.network.ports_tun").to_owned()
        } else {
            t!("overlays.network.ports_hint", host = proxy.host(), socks = proxy.socks, http = proxy.http)
        };
        rows.push(setting("ports", t!("overlays.network.ports"), Some(&ports_info), error.as_deref().map(|e| (e, t.destructive)), ports, &t));
        vec![
            page_section(t!("overlays.network.local_proxy"), &t, rows),
            self.dns_section(cx),
            div().px(px(4.)).xs().text_color(t.muted_fg).child(t!("overlays.network.apply")).into_any_element(),
        ]
    }

    /// Detected country, how much of it is routed through the VPN, and a recheck button.
    fn region_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let r = &self.s.region;
        let busy = r.status == crate::region::Status::Computing;
        let failed = matches!(r.status, crate::region::Status::Failed(_));
        let status = match (&r.country, &r.status) {
            (Some(_), crate::region::Status::Failed(e)) if r.domains + r.subnets > 0 => t!("overlays.region.failed_saved", place = r.place(), error = e),
            (Some(_), crate::region::Status::Failed(e)) => t!("overlays.region.failed", place = r.place(), error = e),
            (Some(_), _) => format!("{} · {}", r.place(), r.line()),
            (None, _) => r.line(),
        };
        let label = if busy { t!("overlays.region.checking") } else { t!("overlays.region.recheck") };
        let button = button("region-recheck", Variant::Outline, Size::Sm, true, &t)
            .when(busy, |b| b.opacity(0.5))
            .child(if busy { crate::ui::spinner("region-spin", 16., t.foreground).into_any_element() } else { icon("refresh-cw", 16., t.foreground).into_any_element() })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                if !busy {
                    this.recheck_region(cx);
                }
            }));
        setting("region", t!("overlays.region.detected"), None, Some((&status, if failed { t.destructive } else { t.muted_fg })), button, &t)
    }

    /// System, English, Русский: switches every window and the tray right away.
    fn language_row(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let items = std::iter::once((None, t!("overlays.settings.language_system")))
            .chain(crate::i18n::Lang::ALL.map(|l| (Some(l), l.name())))
            .map(|(choice, label)| (label.to_owned(), self.s.language == choice, Box::new(move |this: &mut Self, cx: &mut Context<Self>| this.set_language(choice, cx)) as Pick))
            .collect();
        setting("language", t!("overlays.settings.language_row"), None, None, self.select("language", items, window, cx), &t)
    }

    /// Windows and macOS: how system-wide mode works, the system proxy or a TUN.
    fn mode_row(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let items = [(false, t!("overlays.mode.system_proxy")), (true, "TUN")]
            .into_iter()
            .map(|(tun, label)| (label.to_owned(), self.s.tun == tun, Box::new(move |this: &mut Self, _: &mut Context<Self>| this.s.tun = tun) as Pick))
            .collect();
        let info = match (self.s.tun, cfg!(windows)) {
            (false, _) => t!("overlays.mode.proxy_hint"),
            (true, true) => t!("overlays.mode.tun_hint_windows"),
            (true, false) => t!("overlays.mode.tun_hint"),
        };
        setting("mode", t!("overlays.mode.title"), Some(info), None, self.select("mode", items, window, cx), &t)
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
                        .child(t!("overlays.log.copy"))
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
                    .when(empty, |d| d.child(t!("overlays.log.empty")))
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

