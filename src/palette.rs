//! The command palette (shadcn `CommandDialog` + cmdk): location search and per-app route settings share it.
use gpui::{AnyElement, Context, Hsla, MouseButton, SharedString, Window, div, hsla, prelude::*, px};

use crate::app::{Conn, Hitori, Mode};
use crate::data::{self, AppRule, Protocol, RuleMode, Scope, Server, app, server};
use crate::overlays::close_button;
use crate::score::command_score;
use crate::theme::shadow_lg;
use crate::ui::{LG, SM, Tw, flag, icon, ring, switch};
use crate::views::app_badge;

const PALETTE_W: f32 = 512.;

/// What picking a row does.
#[derive(Clone, Copy, PartialEq)]
pub enum Act {
    Go(&'static str),
    Via(&'static str),
    Mode(RuleMode),
    Exit(Option<&'static str>),
    Through(Option<&'static str>),
    Proto(Option<Protocol>),
    Kill,
    Remove,
}

#[derive(Clone)]
pub enum Look {
    /// `chosen: Some(..)` shows ping and a check instead of the live connection state.
    Country { s: &'static Server, chosen: Option<bool> },
    Choice { icon: &'static str, label: String, hint: Option<String>, hint_primary: bool, checked: bool },
    Toggle { label: &'static str, sub: String, on: bool },
    Remove,
}

#[derive(Clone)]
pub struct Row {
    /// cmdk `value` plus `keywords`, matched by command-score.
    pub text: String,
    pub look: Look,
    pub disabled: bool,
    pub act: Act,
}

pub type Group = (String, Vec<Row>);

fn country(prefix: &str, s: &'static Server, chosen: Option<bool>, disabled: bool, act: Act) -> Row {
    Row { text: format!("{prefix}{} {} {}", if prefix.is_empty() { format!("{} {}", s.country, s.id) } else { s.country.to_owned() }, s.city, s.code), look: Look::Country { s, chosen }, disabled, act }
}

fn choice(text: &str, icon: &'static str, label: &str, hint: Option<String>, checked: bool, disabled: bool, act: Act) -> Row {
    Row { text: text.to_owned(), look: Look::Choice { icon, label: label.to_owned(), hint, hint_primary: false, checked }, disabled, act }
}

/// cmdk with a search: items scored and sorted (stable) within groups, groups ordered by their best item.
fn arrange(groups: Vec<Group>, q: &str) -> Vec<Group> {
    if q.is_empty() {
        return groups.into_iter().filter(|g| !g.1.is_empty()).collect();
    }
    let mut out: Vec<(f64, Group)> = groups
        .into_iter()
        .filter_map(|(heading, rows)| {
            let mut scored: Vec<(f64, Row)> = rows.into_iter().map(|r| (command_score(&r.text, q), r)).filter(|(s, _)| *s > 0.).collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
            let best = scored.first()?.0;
            Some((best, (heading, scored.into_iter().map(|(_, r)| r).collect())))
        })
        .collect();
    out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    out.into_iter().map(|(_, g)| g).collect()
}

impl Hitori {
    /// Groups the palette shows for its mode and the current search.
    pub fn palette_groups(&self, mode: Mode, cx: &gpui::App) -> Vec<Group> {
        let q = self.search.read(cx).content.trim().to_owned();
        let groups = match mode {
            Mode::Exit | Mode::Entry => {
                let act = |id| if mode == Mode::Entry { Act::Via(id) } else { Act::Go(id) };
                let servers: Vec<&'static Server> = data::servers().into_iter().filter(|s| !(mode == Mode::Entry && s.id == self.s.server_id)).collect();
                let mut g = vec![];
                if mode == Mode::Exit && !self.s.recents.is_empty() {
                    let rows = self.s.recents.iter().map(|id| {
                        let s = server(id);
                        Row { text: format!("recent {} {} {}", s.id, s.city, s.code), look: Look::Country { s, chosen: None }, disabled: false, act: act(s.id) }
                    });
                    g.push(("Recents".to_owned(), rows.collect()));
                }
                g.push((format!("All countries ({})", servers.len()), servers.into_iter().map(|s| country("", s, None, false, act(s.id))).collect()));
                g
            }
            Mode::Rule(app_id) => self.rule_groups(app_id),
        };
        arrange(groups, &q)
    }

    /// Route settings for one app: route mode, exit, route through, protocol, options.
    fn rule_groups(&self, app_id: &'static str) -> Vec<Group> {
        let rule = self.app_route(app_id).cloned().unwrap_or(AppRule::AUTO);
        let main_exit = server(self.s.server_id);
        let off = matches!(rule.mode, RuleMode::Direct | RuleMode::Block);
        let exit_id = rule.exit.unwrap_or(main_exit.id);
        let suggested = rule.mode != RuleMode::Vpn && self.s.suggestions.iter().any(|s| s.app_id == app_id && s.scope == Scope::App);
        let modes = [
            (RuleMode::Auto, "Auto", "Direct first, VPN when it fails", "wand-sparkles"),
            (RuleMode::Vpn, "Always VPN", "Never direct", "shield-check"),
            (RuleMode::Direct, "Direct", "Bypasses VPN", "globe"),
            (RuleMode::Block, "Block", "No network", "ban"),
        ];
        let route = modes
            .iter()
            .map(|(m, label, hint, icon)| {
                let suggest = *m == RuleMode::Vpn && suggested;
                Row {
                    text: format!("route {label}"),
                    look: Look::Choice { icon, label: (*label).to_owned(), hint: Some(if suggest { "Suggested".into() } else { (*hint).to_owned() }), hint_primary: suggest, checked: rule.mode == *m },
                    disabled: false,
                    act: Act::Mode(*m),
                }
            })
            .collect();
        let mut exit = vec![choice("exit same as connection", "shield-check", "Same as connection", Some(main_exit.country.into()), rule.exit.is_none(), off, Act::Exit(None))];
        exit.extend(data::servers().into_iter().map(|s| country("exit ", s, Some(rule.exit == Some(s.id)), off, Act::Exit(Some(s.id)))));
        let mut through = vec![choice("through straight to exit", "move-right", "Straight to exit", Some(server(exit_id).country.into()), rule.via.is_none(), off, Act::Through(None))];
        through.extend(data::servers().into_iter().filter(|s| s.id != exit_id).map(|s| country("through ", s, Some(rule.via == Some(s.id)), off, Act::Through(Some(s.id)))));
        let mut proto = vec![choice("protocol same as connection", "zap", "Same as connection", Some(self.s.protocol.label().into()), rule.protocol.is_none(), off, Act::Proto(None))];
        proto.extend(Protocol::ALL.iter().map(|p| choice(&format!("protocol {}", p.label()), "zap", p.label(), None, rule.protocol == Some(*p), off, Act::Proto(Some(*p)))));
        let options = vec![
            Row { text: "option kill switch".into(), look: Look::Toggle { label: "Kill switch", sub: format!("Block {} while the tunnel is down", app(app_id).name), on: rule.kill }, disabled: rule.mode == RuleMode::Block, act: Act::Kill },
            Row { text: "option remove rule".into(), look: Look::Remove, disabled: false, act: Act::Remove },
        ];
        vec![("Route".into(), route), ("Exit".into(), exit), ("Route through".into(), through), ("Protocol".into(), proto), ("Options".into(), options)]
    }

    pub fn palette_rows(&self, mode: Mode, cx: &gpui::App) -> Vec<Row> {
        self.palette_groups(mode, cx).into_iter().flat_map(|g| g.1).collect()
    }

    /// cmdk keeps the selection on an enabled item: the first one after the list changes, then wherever arrows move it.
    pub fn palette_fix_selection(&mut self, cx: &gpui::App) {
        let Some(mode) = self.palette else { return };
        let rows = self.palette_rows(mode, cx);
        if rows.get(self.palette_index).is_none_or(|r| r.disabled) {
            self.palette_index = rows.iter().position(|r| !r.disabled).unwrap_or(0);
        }
    }

    /// Arrow keys: next or previous enabled item, without wrapping (cmdk `loop` is off).
    pub fn palette_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(mode) = self.palette else { return };
        let rows = self.palette_rows(mode, cx);
        let mut i = self.palette_index as isize;
        loop {
            i += delta;
            match rows.get(i as usize) {
                _ if i < 0 => break,
                None => break,
                Some(r) if !r.disabled => {
                    self.palette_index = i as usize;
                    break;
                }
                Some(_) => {}
            }
        }
        cx.notify();
    }

    pub fn palette_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mode) = self.palette else { return };
        if let Some(row) = self.palette_rows(mode, cx).get(self.palette_index).filter(|r| !r.disabled) {
            self.act(row.act, window, cx);
        }
    }

    fn patch_rule(&mut self, app_id: &'static str, change: impl FnOnce(&mut AppRule), cx: &mut Context<Self>) {
        let mut rule = self.app_route(app_id).cloned().unwrap_or(AppRule::AUTO);
        change(&mut rule);
        if rule.is_default() {
            self.s.app_routes.retain(|(a, _)| *a != app_id);
        } else if let Some(slot) = self.s.app_routes.iter_mut().find(|(a, _)| *a == app_id) {
            slot.1 = rule;
        } else {
            self.s.app_routes.push((app_id, rule));
        }
        cx.notify();
    }

    pub fn act(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        let main_exit = self.s.server_id;
        let rule_app = match self.palette {
            Some(Mode::Rule(a)) => Some(a),
            _ => None,
        };
        match (act, rule_app) {
            (Act::Go(id), _) => self.go(id, window, cx),
            (Act::Via(id), _) => self.via(Some(id), window, cx),
            (Act::Remove, Some(a)) => {
                self.patch_rule(a, |r| *r = AppRule::AUTO, cx);
                self.close_palette(window, cx);
            }
            (_, None) => {}
            (Act::Mode(m), Some(a)) => self.patch_rule(a, |r| r.mode = m, cx),
            (Act::Exit(x), Some(a)) => self.patch_rule(
                a,
                |r| {
                    let clash = x.unwrap_or(main_exit);
                    r.exit = x;
                    if r.via == Some(clash) {
                        r.via = None;
                    }
                },
                cx,
            ),
            (Act::Through(x), Some(a)) => self.patch_rule(a, |r| r.via = x, cx),
            (Act::Proto(p), Some(a)) => self.patch_rule(a, |r| r.protocol = p, cx),
            (Act::Kill, Some(a)) => self.patch_rule(a, |r| r.kill = !r.kill, cx),
        }
    }

    pub fn palette_dialog(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.palette_fix_selection(cx);
        let t = self.theme(cx);
        let vp = window.viewport_size();
        let (w, h) = (f32::from(vp.width), f32::from(vp.height));
        let groups = self.palette_groups(mode, cx);
        let rule = matches!(mode, Mode::Rule(_));
        // Dialog: overlay `fade-in-0` 150ms; content `fade-in-0 zoom-in-95 duration-200` (no subtree scaling in GPUI).
        let dim = self.palette_fx.phase(mode, 150., 150., crate::fx::ease).map_or(0., |p| p.0);
        let fade = self.palette_fx.phase(mode, 200., 200., crate::fx::ease).map_or(0., |p| p.0);
        // cmdk layout: each group p-1 (later groups pt-0), heading 32px, then the items.
        let mut list_h = 0.;
        let mut rows: Vec<AnyElement> = vec![];
        let mut index = 0;
        for (gi, (heading, items)) in groups.into_iter().enumerate() {
            let first = gi == 0;
            list_h += if first { 4. } else { 0. } + 32. + 4.;
            let mut g = div().px(px(8.)).pb(px(4.)).when(first, |d| d.pt(px(4.))).child(div().h(px(32.)).px(px(8.)).flex().items_center().xs().medium().text_color(t.muted_fg).child(heading));
            for row in items {
                list_h += row_height(&row);
                g = g.child(self.palette_row(&row, index, window, cx));
                index += 1;
            }
            rows.push(g.into_any_element());
        }
        if rows.is_empty() {
            list_h = 68.;
            rows.push(div().py(px(24.)).flex().justify_center().sm().child(if rule { "No settings match." } else { "No countries match." }).into_any_element());
        }
        let list_h = list_h.min(if rule { 400. } else { 300. });
        let dialog_h = 48. + list_h;
        let dw = PALETTE_W.min(w - 32.);
        let lead = match mode {
            Mode::Rule(a) => app_badge(app(a)).into_any_element(),
            _ => icon("search", 16., t.foreground.opacity(0.5)).into_any_element(),
        };
        div()
            .absolute()
            .inset_0()
            .when(dim > 0., |d| {
                d.child(div().id("palette-overlay").absolute().inset_0().bg(hsla(0., 0., 0., 0.5)).opacity(dim).occlude().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.close_palette(window, cx)),
                ))
            })
            .child(
                div()
                    .id("palette")
                    .key_context("Palette")
                    .opacity(fade)
                    .occlude()
                    .absolute()
                    .left(px(((w - dw) / 2.).floor()))
                    .top(px(((h - dialog_h) / 2.).floor()))
                    .w(px(dw))
                    .h(px(dialog_h))
                    .rounded(px(LG))
                    .overflow_hidden()
                    .bg(t.popover)
                    .shadow(shadow_lg())
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(48.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(12.))
                            .border_b_1()
                            .border_color(t.border)
                            .sm()
                            .child(lead)
                            .child(div().flex_1().min_w_0().child(self.search.clone())),
                    )
                    .child(div().id("palette-list").h(px(list_h)).overflow_y_scroll().children(rows))
                    .child(ring(t.border, LG))
                    .child(close_button("palette-close", &t).on_click(cx.listener(|this, _, window, cx| this.close_palette(window, cx)))),
            )
            .into_any_element()
    }

    fn palette_row(&self, row: &Row, index: usize, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let selected = index == self.palette_index && !row.disabled;
        let destructive = matches!(row.look, Look::Remove);
        let sel_bg: Hsla = if destructive { t.destructive.opacity(if t.dark { 0.2 } else { 0.1 }) } else { t.accent };
        let fg = if destructive { t.destructive } else { t.foreground };
        let check = |on: bool| div().size(px(20.)).flex_none().when(on, |d| d.child(icon("check", 20., t.foreground)));
        let base = div()
            .id(SharedString::from(format!("pal-{index}")))
            .h(px(row_height(row)))
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(8.))
            .rounded(px(SM))
            .sm()
            .text_color(fg)
            .when(selected, |d| d.bg(sel_bg))
            .when(row.disabled, |d| d.opacity(0.5));
        let body: Vec<AnyElement> = match &row.look {
            Look::Country { s, chosen } => {
                let live = chosen.is_none() && self.s.server_id == s.id && self.s.conn != Conn::Off;
                let entry = chosen.is_none() && self.s.entry_id == Some(s.id);
                let ping = || div().xs().text_color(t.muted_fg).child(data::ping(s.id).map_or("—".to_owned(), |ms| format!("{ms} ms"))).into_any_element();
                let mut v = vec![
                    flag(s.code, true, window).into_any_element(),
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().text_px(14., 20.).medium().child(s.country))
                        .child(div().truncate().xs().text_color(t.muted_fg).child(s.city))
                        .into_any_element(),
                ];
                if let Some(on) = chosen {
                    v.push(ping());
                    v.push(check(*on).into_any_element());
                } else if live {
                    v.push(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .xs()
                            .medium()
                            .text_color(t.primary)
                            .child(div().size(px(8.)).rounded_full().bg(t.primary))
                            .child(if self.s.conn == Conn::On { "Connected" } else { "Connecting" })
                            .into_any_element(),
                    );
                } else if entry {
                    v.push(div().xs().text_color(t.muted_fg).child("Entry").into_any_element());
                } else {
                    v.push(ping());
                }
                v
            }
            Look::Choice { icon: name, label, hint, hint_primary, checked } => vec![
                icon(name, 20., t.muted_fg).into_any_element(),
                div().flex_1().min_w_0().truncate().child(label.clone()).into_any_element(),
                div().xs().text_color(if *hint_primary { t.primary } else { t.muted_fg }).children(hint.clone()).into_any_element(),
                check(*checked).into_any_element(),
            ],
            Look::Toggle { label, sub, on } => vec![
                icon("shield-x", 20., t.muted_fg).into_any_element(),
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().child(*label))
                    .child(div().truncate().xs().text_color(t.muted_fg).child(sub.clone()))
                    .into_any_element(),
                switch(SharedString::from(format!("pal-switch-{index}")), *on, &t).into_any_element(),
            ],
            Look::Remove => vec![
                icon("trash-2", 20., t.destructive).into_any_element(),
                div().flex_1().child("Remove rule").into_any_element(),
                div().xs().text_color(t.muted_fg).child("Back to autorouting").into_any_element(),
            ],
        };
        let act = row.act;
        let mut el = base.children(body);
        if !row.disabled {
            el = el
                .cursor_pointer()
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if this.palette_index != index {
                        this.palette_index = index;
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _, window, cx| this.act(act, window, cx)));
        }
        el.into_any_element()
    }
}

/// `h-12` country rows; other items `py-3` around their text (CommandDialog).
fn row_height(row: &Row) -> f32 {
    match row.look {
        Look::Country { .. } => 48.,
        Look::Toggle { .. } => 60.,
        _ => 44.,
    }
}
