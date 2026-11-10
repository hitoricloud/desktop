//! Routing-first layouts: the routing workspace takes the map's place, as a list of rows or a board of route columns.
use gpui::{AnyElement, Context, Div, Focusable, MouseButton, SharedString, Window, div, prelude::*, px};

use crate::app::{Hitori, Mode};
use crate::data::{self, AppInfo, AppRule, RuleMode, SiteRule, ago, app, server};
use crate::fx::fx;
use crate::theme::{Theme, shadow_xs, theme};
use crate::ui::{LG, MD, Size, Tw, Variant, XL, button, button_icon, flag, icon, tip};
use crate::views::{app_badge, site_badge};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Layout {
    Map,
    List,
    Board,
}

impl Layout {
    pub const ALL: [Layout; 3] = [Layout::Map, Layout::List, Layout::Board];

    pub fn key(self) -> &'static str {
        match self {
            Layout::Map => "map",
            Layout::List => "list",
            Layout::Board => "board",
        }
    }

    pub fn parse(key: &str) -> Self {
        Layout::ALL.into_iter().find(|l| l.key() == key).unwrap_or(Layout::Map)
    }

    fn label(self) -> &'static str {
        match self {
            Layout::Map => "Map",
            Layout::List => "List",
            Layout::Board => "Board",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Layout::Map => "map",
            Layout::List => "list",
            Layout::Board => "square-kanban",
        }
    }
}

/// Something a rule can be about; what the board drags between columns.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Item {
    App(&'static str),
    Site(&'static str),
}

/// The card under the pointer while dragging.
struct DragCard {
    item: Item,
}

impl Render for DragCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let (badge, name) = match self.item {
            Item::App(a) => (app_badge(app(a)), app(a).name.to_owned()),
            Item::Site(s) => (site_badge(&t), s.to_owned()),
        };
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .w(px(220.))
            .p(px(12.))
            .rounded(px(LG))
            .bg(t.card)
            .border_1()
            .border_color(t.primary)
            .shadow(t.panel_shadow())
            .text_color(t.foreground)
            .sm()
            .medium()
            .child(badge)
            .child(div().truncate().child(name))
    }
}

/// `https://www.Example.com/path` → `example.com`; `None` unless it looks like a domain.
pub fn domain(input: &str) -> Option<String> {
    let s = input.trim().to_lowercase();
    let s = s.split_once("://").map_or(s.as_str(), |(_, r)| r);
    let host = s.split(['/', '?', '#', ':']).next()?.trim_start_matches("www.").trim_end_matches('.');
    let ok = host.contains('.')
        && host.len() <= 253
        && host.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-'))
        && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    ok.then(|| host.to_owned())
}

impl Hitori {
    /// Where an app's or a site's traffic goes now, and whether a rule (rather than the default) decides it.
    pub fn item_mode(&self, item: Item) -> (RuleMode, bool) {
        match item {
            Item::App(a) => self.app_route(a).map_or((self.s.default_route, false), |r| (r.mode, true)),
            Item::Site(s) => match self.site_rule(s) {
                Some(SiteRule::Vpn) => (RuleMode::Vpn, true),
                Some(SiteRule::Direct) => (RuleMode::Direct, true),
                None => (self.s.default_route, false),
            },
        }
    }

    /// Choosing the default route's mode drops the rule; sites can only be VPN or direct.
    pub fn set_item_mode(&mut self, item: Item, mode: RuleMode, cx: &mut Context<Self>) {
        match item {
            Item::App(a) => self.patch_rule(a, |r| r.mode = mode, cx),
            Item::Site(s) => {
                let rule = match mode {
                    m if m == self.s.default_route => None,
                    RuleMode::Vpn => Some(SiteRule::Vpn),
                    RuleMode::Direct => Some(SiteRule::Direct),
                    _ => return,
                };
                self.site(s, rule, cx);
            }
        }
    }

    fn can_take(&self, item: Item, mode: RuleMode) -> bool {
        matches!(item, Item::App(_)) || matches!(mode, RuleMode::Vpn | RuleMode::Direct) || mode == self.s.default_route
    }

    /// Apps with a rule, then apps seen this session; by name, so rows stay put while apps come and go.
    fn workspace_apps(&self) -> Vec<&'static AppInfo> {
        let mut apps: Vec<&'static AppInfo> = self.s.app_routes.iter().map(|(a, _)| app(a)).collect();
        for a in data::running() {
            if !apps.iter().any(|x| x.id == a.id) {
                apps.push(a);
            }
        }
        apps.sort_by_key(|a| (self.app_route(a.id).is_none(), a.name.to_lowercase()));
        apps
    }

    /// Where an item ends up, for its row or card; muted when it just follows the connection.
    fn item_destination(&self, item: Item) -> (String, bool) {
        let rule = match item {
            Item::App(a) => self.app_route(a).cloned().unwrap_or(AppRule::mode(self.s.default_route)),
            Item::Site(_) => AppRule::mode(self.item_mode(item).0),
        };
        self.destination(&rule)
    }

    pub fn layout_switch(&self, cx: &mut Context<Self>) -> Div {
        let t = self.theme(cx);
        let mut seg = div().flex().p(px(2.)).gap(px(2.)).rounded(px(MD)).bg(t.muted).shadow(t.panel_shadow());
        for layout in Layout::ALL {
            let on = layout == self.s.layout;
            seg = seg.child(
                fx(SharedString::from(format!("layout-{}", layout.key())), if on { t.background } else { t.card }, Some(if on { t.background } else { t.accent }))
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .when(on, |d| d.shadow(shadow_xs()))
                    .cursor_pointer()
                    .tooltip(tip(layout.label(), &t))
                    .child(icon(layout.icon(), 16., if on { t.foreground } else { t.muted_fg }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.s.layout = layout;
                        this.menu = None;
                        cx.notify();
                    })),
            );
        }
        seg
    }

    /// Route buttons for one item; the current one is filled, or outlined when it only follows the default.
    fn mode_buttons(&self, item: Item, modes: &[RuleMode], t: &Theme, cx: &mut Context<Self>) -> Div {
        let (current, ruled) = self.item_mode(item);
        let key = match item {
            Item::App(a) => format!("a-{a}"),
            Item::Site(s) => format!("s-{s}"),
        };
        let mut seg = div().flex().p(px(2.)).gap(px(2.)).rounded(px(MD)).bg(t.muted.opacity(0.6));
        for &mode in modes {
            let on = mode == current;
            let fg = match (on, mode) {
                (true, RuleMode::Block) => t.destructive,
                (true, _) if ruled => t.foreground,
                _ => t.muted_fg,
            };
            let rest = if on && ruled { t.background } else { gpui::transparent_black() };
            seg = seg.child(
                fx(SharedString::from(format!("m-{key}-{}", mode.key())), rest, Some(if on && ruled { t.background } else { t.ghost_hover() }))
                    .h(px(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .rounded(px(6.))
                    .when(on && ruled, |d| d.shadow(shadow_xs()))
                    .when(on && !ruled, |d| d.border_1().border_color(t.border))
                    .when(!(on && !ruled), |d| d.border_1().border_color(gpui::transparent_black()))
                    .xs()
                    .medium()
                    .text_color(fg)
                    .cursor_pointer()
                    .child(icon(mode.icon(), 13., fg))
                    .child(mode.label())
                    .on_click(cx.listener(move |this, _, _, cx| this.set_item_mode(item, mode, cx))),
            );
        }
        seg
    }

    /// Centre of the main window in the list and board layouts.
    pub fn workspace(&mut self, wide: bool, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let narrow = (!wide).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .p(px(16.))
                .rounded(px(XL))
                .bg(t.card)
                .child(self.status(cx))
                .child(self.route_field(false, window, cx))
                .child(self.power_button(cx))
        });
        let body = match self.s.layout {
            Layout::Board => self.board(cx).into_any_element(),
            _ => self.list(window, cx).into_any_element(),
        };
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(t.background)
            .text_color(t.foreground)
            .child(
                div()
                    .flex_none()
                    .h(px(64.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(24.))
                    .child(self.brand(cx))
                    .child(div().flex_1())
                    .child(self.layout_switch(cx))
                    .child(self.account_menu(window, cx)),
            )
            .child(
                div()
                    .id("workspace")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(24.))
                    .pb(px(24.))
                    .flex()
                    .flex_col()
                    .gap(px(20.))
                    .children(narrow)
                    .child(self.workspace_head(cx))
                    .children(self.suggestion_strip(cx))
                    .child(body),
            )
            .into_any_element()
    }

    /// The default route, large: what happens to everything without a rule.
    fn workspace_head(&self, cx: &mut Context<Self>) -> Div {
        let t = self.theme(cx);
        let d = self.s.default_route;
        let exit = server(self.s.server_id);
        let detail = match d {
            RuleMode::Auto => format!("Sites on the blocked lists go through {}, everything else uses your connection.", exit.country),
            RuleMode::Vpn => format!("Everything goes through {} unless a rule says otherwise.", exit.country),
            RuleMode::Direct => format!("Everything uses your connection; only your rules go through {}.", exit.country),
            RuleMode::Block => "Nothing reaches the internet unless a rule lets it through.".into(),
        };
        let mut seg = div().flex().p(px(3.)).gap(px(3.)).rounded(px(LG)).bg(t.muted);
        for mode in RuleMode::ALL {
            let on = mode == d;
            let fg = if on { if mode == RuleMode::Block { t.destructive } else { t.foreground } } else { t.muted_fg };
            seg = seg.child(
                fx(SharedString::from(format!("head-{}", mode.key())), if on { t.background } else { gpui::transparent_black() }, Some(if on { t.background } else { t.ghost_hover() }))
                    .flex_1()
                    .h(px(52.))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .px(px(12.))
                    .rounded(px(MD))
                    .when(on, |x| x.shadow(shadow_xs()))
                    .cursor_pointer()
                    .child(div().flex().items_center().gap(px(6.)).sm().medium().text_color(fg).child(icon(mode.icon(), 15., fg)).child(mode.label()))
                    .child(div().truncate().xs().text_color(t.muted_fg).child(mode.default_hint()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.s.default_route = mode;
                        cx.notify();
                    })),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_size(px(20.)).semibold().child("Default route"))
                    .child(div().sm().text_color(t.muted_fg).child(detail)),
            )
            .child(seg)
    }

    fn suggestion_strip(&self, cx: &mut Context<Self>) -> Option<Div> {
        let t = self.theme(cx);
        let list = self.s.suggestions.clone();
        if list.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(section_title(&format!("Needs VPN · {}", list.len()), &t))
                .child(div().flex().flex_wrap().gap(px(8.)).children(list.iter().map(|s| div().w(px(300.)).child(self.suggestion_card(s, false, cx))))),
        )
    }

    // ---------- list ----------

    fn list(&mut self, window: &Window, cx: &mut Context<Self>) -> Div {
        let t = self.theme(cx);
        let apps = self.workspace_apps();
        let ruled = apps.iter().filter(|a| self.app_route(a.id).is_some()).count();
        let mut rows = div().flex().flex_col().rounded(px(LG)).border_1().border_color(t.border).overflow_hidden();
        for (i, a) in apps.iter().enumerate() {
            rows = rows.child(self.app_row(a, i > 0, window, cx));
        }
        if apps.is_empty() {
            rows = rows.child(div().p(px(16.)).sm().text_color(t.muted_fg).child("Apps appear here once they use the network."));
        }
        let mut sites = div().flex().flex_col().rounded(px(LG)).border_1().border_color(t.border).overflow_hidden();
        for (i, (target, _)) in self.s.sites.clone().into_iter().enumerate() {
            sites = sites.child(self.site_row(target, i > 0, window, cx));
        }
        sites = sites.child(self.add_site(!self.s.sites.is_empty(), cx));
        div()
            .flex()
            .flex_col()
            .gap(px(20.))
            .child(div().flex().flex_col().gap(px(8.)).child(section_title(&format!("Websites · {}", self.s.sites.len()), &t)).child(sites))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(section_title(&format!("Apps · {ruled} with rules, {} following the default", apps.len() - ruled), &t))
                    .child(rows),
            )
    }

    fn row_shell(&self, id: SharedString, divider: bool, t: &Theme) -> gpui::Stateful<Div> {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(12.))
            .h(px(56.))
            .px(px(12.))
            .when(divider, |d| d.border_t_1().border_color(t.border))
            .hover(|s| s.bg(t.muted.opacity(0.3)))
    }

    fn destination_cell(&self, item: Item, t: &Theme, window: &Window) -> Div {
        let (mode, _) = self.item_mode(item);
        let (text, muted) = self.item_destination(item);
        let exit = match item {
            Item::App(a) => self.app_route(a).and_then(|r| r.exit),
            Item::Site(_) => None,
        };
        let code = server(exit.unwrap_or(self.s.server_id)).code;
        let color = match mode {
            RuleMode::Block => t.destructive,
            RuleMode::Direct => t.muted_fg,
            _ if muted => t.muted_fg,
            _ => t.foreground,
        };
        div()
            .w(px(200.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .sm()
            .text_color(color)
            .child(match mode {
                RuleMode::Vpn | RuleMode::Auto => div().flex_none().child(flag(code, false, window)).into_any_element(),
                m => icon(m.icon(), 16., color).into_any_element(),
            })
            .child(div().truncate().child(text))
    }

    fn app_row(&self, a: &'static AppInfo, divider: bool, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let item = Item::App(a.id);
        let rule = self.app_route(a.id).cloned();
        let mut sub = vec![a.process.to_owned()];
        if let Some(m) = data::seen_min(a.id) {
            sub.push(ago(m));
        }
        if let Some(r) = &rule {
            if let Some(p) = r.protocol {
                sub.push(p.label().to_owned());
            }
            if r.kill {
                sub.push("kill switch".into());
            }
        } else {
            sub.push("default".into());
        }
        let id = a.id;
        self.row_shell(SharedString::from(format!("row-{id}")), divider, &t)
            .child(app_badge(a))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().truncate().sm().medium().child(a.name))
                    .child(div().truncate().xs().text_color(t.muted_fg).child(sub.join(" · "))),
            )
            .child(self.mode_buttons(item, &RuleMode::ALL, &t, cx))
            .child(self.destination_cell(item, &t, window))
            .child(
                button(SharedString::from(format!("more-{id}")), Variant::Ghost, Size::IconSm, true, &t)
                    .tooltip(tip("Exit, cascade, protocol", &t))
                    .child(icon("ellipsis", button_icon(Size::IconSm), t.foreground))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_palette(Mode::Rule(id), window, cx))),
            )
    }

    fn site_row(&self, target: &'static str, divider: bool, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let item = Item::Site(target);
        let mut modes = vec![RuleMode::Vpn, RuleMode::Direct];
        if !modes.contains(&self.s.default_route) {
            modes.insert(0, self.s.default_route);
        }
        self.row_shell(SharedString::from(format!("row-site-{target}")), divider, &t)
            .child(site_badge(&t))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().truncate().sm().medium().child(target))
                    .child(div().xs().text_color(t.muted_fg).child("website and its subdomains")),
            )
            .child(self.mode_buttons(item, &modes, &t, cx))
            .child(self.destination_cell(item, &t, window))
            .child(
                button(SharedString::from(format!("rm-{target}")), Variant::Ghost, Size::IconSm, true, &t)
                    .tooltip(tip("Remove rule", &t))
                    .child(icon("x", button_icon(Size::IconSm), t.foreground))
                    .on_click(cx.listener(move |this, _, _, cx| this.site(target, None, cx))),
            )
    }

    fn add_site(&self, divider: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let focus = self.site_input.read(cx).focus_handle(cx);
        let typed = self.site_input.read(cx).content.to_string();
        let valid = domain(&typed).is_some();
        let add = |label: &'static str, rule: SiteRule, variant: Variant| {
            button(SharedString::from(format!("add-site-{label}")), variant, Size::Sm, false, &t)
                .when(!valid, |b| b.opacity(0.5))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.add_site_rule(rule, cx)))
        };
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(56.))
            .px(px(12.))
            .when(divider, |d| d.border_t_1().border_color(t.border))
            .child(icon("plus", 16., t.muted_fg))
            .child(
                div()
                    .id("site-field")
                    .flex_1()
                    .min_w_0()
                    .h(px(32.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .rounded(px(MD))
                    .border_1()
                    .border_color(t.border)
                    .bg(t.background)
                    .sm()
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, move |_, window, _| window.focus(&focus))
                    .child(div().flex_1().min_w_0().child(self.site_input.clone())),
            )
            .child(add("Always VPN", SiteRule::Vpn, Variant::Default))
            .child(add("Always direct", SiteRule::Direct, Variant::Secondary))
    }

    pub fn add_site_rule(&mut self, rule: SiteRule, cx: &mut Context<Self>) {
        let typed = self.site_input.read(cx).content.to_string();
        let Some(host) = domain(&typed) else { return };
        self.site(data::intern(&host), Some(rule), cx);
        self.site_input.update(cx, |input, _| input.reset());
    }

    // ---------- board ----------

    fn board(&mut self, cx: &mut Context<Self>) -> Div {
        // Columns stretch to the tallest, so the space under a short one still takes drops.
        let mut cols = div().flex().gap(px(12.)).min_h(px(420.));
        for mode in RuleMode::ALL {
            cols = cols.child(self.column(mode, cx));
        }
        cols
    }

    fn column(&self, mode: RuleMode, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let default = mode == self.s.default_route;
        let mut items: Vec<Item> = vec![];
        for a in self.workspace_apps() {
            if self.item_mode(Item::App(a.id)).0 == mode {
                items.push(Item::App(a.id));
            }
        }
        for (s, _) in &self.s.sites {
            if self.item_mode(Item::Site(s)).0 == mode {
                items.push(Item::Site(s));
            }
        }
        let exit = server(self.s.server_id);
        let detail = match mode {
            RuleMode::Auto => format!("Lists via {}, rest direct", exit.country),
            RuleMode::Vpn => format!("Through {}", exit.country),
            RuleMode::Direct => "Your own connection".into(),
            RuleMode::Block => "No network".into(),
        };
        let fg = if mode == RuleMode::Block { t.destructive } else { t.foreground };
        let primary = t.primary;
        let mut body = div().flex().flex_col().gap(px(8.));
        for item in &items {
            body = body.child(self.board_card(*item, cx));
        }
        if items.is_empty() {
            body = body.child(
                div().h(px(64.)).flex().items_center().justify_center().rounded(px(LG)).border_1().border_color(t.border).xs().text_color(t.muted_fg).child("Drop apps or sites here"),
            );
        }
        div()
            .id(SharedString::from(format!("col-{}", mode.key())))
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(12.))
            .rounded(px(XL))
            .bg(t.card)
            .border_1()
            .border_color(if default { t.primary.opacity(0.4) } else { gpui::transparent_black() })
            .drag_over::<Item>(move |s, _, _, _| s.bg(primary.opacity(0.08)).border_color(primary))
            .can_drop({
                let mode_ok = self.can_take_fn(mode);
                move |v, _, _| mode_ok(v)
            })
            .on_drop(cx.listener(move |this, item: &Item, _, cx| {
                if this.can_take(*item, mode) {
                    this.set_item_mode(*item, mode, cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(icon(mode.icon(), 16., fg))
                            .child(div().sm().semibold().text_color(fg).child(mode.label()))
                            .child(div().xs().text_color(t.muted_fg).child(items.len().to_string()))
                            .child(div().flex_1())
                            .when(default, |d| d.child(div().px(px(6.)).py(px(1.)).rounded(px(6.)).bg(t.primary.opacity(0.15)).xs().medium().text_color(t.primary).child("Default"))),
                    )
                    .child(div().truncate().xs().text_color(t.muted_fg).child(detail)),
            )
            .child(body)
    }

    /// Drop targets refuse what they cannot hold (sites are only VPN or direct).
    fn can_take_fn(&self, mode: RuleMode) -> impl Fn(&dyn std::any::Any) -> bool + 'static {
        let default = self.s.default_route;
        move |v| v.downcast_ref::<Item>().is_some_and(|i| matches!(i, Item::App(_)) || matches!(mode, RuleMode::Vpn | RuleMode::Direct) || mode == default)
    }

    fn board_card(&self, item: Item, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme(cx);
        let (_, ruled) = self.item_mode(item);
        let (badge, name, key) = match item {
            Item::App(a) => (app_badge(app(a)), app(a).name.to_owned(), format!("card-{a}")),
            Item::Site(s) => (site_badge(&t), s.to_owned(), format!("card-site-{s}")),
        };
        let sub = match item {
            Item::App(a) => match self.app_route(a) {
                None => "default".to_owned(),
                Some(r) => {
                    let mut parts = vec![];
                    if let Some(e) = r.exit {
                        parts.push(format!("exit {}", server(e).country));
                    }
                    if let Some(v) = r.via {
                        parts.push(format!("via {}", server(v).country));
                    }
                    if let Some(p) = r.protocol {
                        parts.push(p.label().to_owned());
                    }
                    if r.kill {
                        parts.push("kill switch".into());
                    }
                    if parts.is_empty() { data::seen_min(a).map(ago).unwrap_or_else(|| "your rule".into()) } else { parts.join(" · ") }
                }
            },
            Item::Site(_) => "website".to_owned(),
        };
        let card = fx(SharedString::from(key), if ruled { t.muted.opacity(0.6) } else { gpui::transparent_black() }, Some(t.accent))
            .flex()
            .items_center()
            .gap(px(8.))
            .p(px(10.))
            .rounded(px(LG))
            .border_1()
            .border_color(if ruled { gpui::transparent_black() } else { t.border })
            .cursor_grab()
            .child(badge)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().truncate().sm().medium().text_color(if ruled { t.foreground } else { t.muted_fg }).child(name))
                    .child(div().truncate().xs().text_color(t.muted_fg).child(sub)),
            )
            .child(icon("grip-vertical", 14., t.muted_fg.opacity(0.6)))
            .on_drag(item, |item: &Item, _, _, cx| cx.new(|_| DragCard { item: *item }));
        match item {
            Item::App(a) => card.on_click(cx.listener(move |this, _, window, cx| this.open_palette(Mode::Rule(a), window, cx))).into_any_element(),
            Item::Site(_) => card.into_any_element(),
        }
    }
}

fn section_title(text: &str, t: &Theme) -> Div {
    div().xs().medium().text_color(t.muted_fg).child(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::domain;

    #[test]
    fn domains_from_what_people_paste() {
        assert_eq!(domain("https://www.YouTube.com/watch?v=1").as_deref(), Some("youtube.com"));
        assert_eq!(domain("chatgpt.com").as_deref(), Some("chatgpt.com"));
        assert_eq!(domain("sub.example.org:8443/x").as_deref(), Some("sub.example.org"));
        for bad in ["", "localhost", "exa mple.com", "a..b", "-x.com", "x\".com", "домен.рф"] {
            assert_eq!(domain(bad), None, "{bad}");
        }
    }
}
