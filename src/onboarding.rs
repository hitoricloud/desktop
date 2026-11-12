//! The only screen while signed out: sign in with Telegram (or a subscription link); and the account without a subscription.
use gpui::{AnyElement, Context, Div, Focusable, Window, div, prelude::*, px};

use crate::app::{Hitori, Variant};
use crate::theme::Theme;
use crate::ui::{LG, Size, Tw, Variant as Btn, XL, avatar, button, icon, spinner};
use crate::vpn::Telegram;

const BOT: &str = "https://t.me/hitorivpn";

impl Hitori {
    /// Signed out, or signed in without a subscription: no map, sidebar, inspector or palette.
    pub fn gated(&self) -> bool {
        !self.s.signed_in || self.s.no_subscription
    }

    pub fn telegram_sign_in(&mut self, cx: &mut Context<Self>) {
        self.with_vpn(cx, |v, cx| v.telegram_sign_in(cx));
    }

    pub fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        self.with_vpn(cx, |v, cx| v.cancel_sign_in(cx));
    }

    pub fn onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let content = if self.s.signed_in { self.no_subscription(&t, cx) } else { self.welcome(&t, window, cx) };
        if self.variant == Variant::Main {
            return div()
                .id("onboarding")
                .size_full()
                .overflow_y_scroll()
                .bg(t.background)
                .flex()
                .flex_col()
                .items_center()
                .child(div().flex_1().min_h(px(32.)))
                .child(div().flex_none().w(px(400.)).p(px(32.)).rounded(px(XL)).bg(t.card).shadow(t.panel_shadow()).child(content))
                .child(div().flex_1().min_h(px(32.)))
                .into_any_element();
        }
        // The tray popup's card, as in `tray()`.
        let md = self.popover || f32::from(window.viewport_size().width) >= 768.;
        let card = div()
            .flex()
            .flex_col()
            .justify_center()
            .overflow_hidden()
            .p(px(32.))
            .bg(t.card)
            .when(md, |d| d.w(px(400.)).h(px(720.)).max_h_full().rounded(px(XL)).shadow(t.panel_shadow()))
            .when(!md, |d| d.size_full())
            .child(content);
        if self.popover {
            return card.into_any_element();
        }
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(t.background)
            .child(div().size_full().flex().items_center().justify_center().bg(t.muted.opacity(0.4)).when(md, |d| d.p(px(32.))).child(card))
            .into_any_element()
    }

    /// Detected location and the resources blocked there, shown before sign-in (provided by the region-detection work).
    fn location_summary(&self, t: &Theme, _cx: &mut Context<Self>) -> Option<AnyElement> {
        let r = &self.s.region;
        let busy = r.status == crate::region::Status::Computing;
        let mark = if busy { spinner("onboard-region", 16., t.muted_fg).into_any_element() } else { icon("globe", 16., t.muted_fg).into_any_element() };
        Some(
            div()
                .flex()
                .items_start()
                .gap(px(12.))
                .p(px(12.))
                .rounded(px(LG))
                .bg(t.muted.opacity(0.5))
                .child(div().mt(px(2.)).flex_none().child(mark))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().sm().medium().child(if r.country.is_some() { r.place() } else { t!("onboarding.your_location").into() }))
                        .child(div().xs().text_color(t.muted_fg).child(r.line())),
                )
                .into_any_element(),
        )
    }

    fn welcome(&self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let header = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(16.))
            .child(div().size(px(56.)).rounded(px(XL)).bg(t.primary.opacity(0.1)).flex().items_center().justify_center().child(icon("shield-check", 28., t.primary)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.))
                    .child(div().text_px(24., 32.).semibold().tight().child("Hitori VPN"))
                    .child(div().sm().text_color(t.muted_fg).text_center().child(t!("onboarding.welcome.desc"))),
            );
        let error = self.s.error.clone().filter(|_| !self.paste_link && self.s.telegram == Telegram::Idle);
        let action = match &self.s.telegram {
            Telegram::Browser(url) => self.waiting(url.clone(), t, cx).into_any_element(),
            state => {
                let starting = *state == Telegram::Starting;
                button("tg-sign-in", Btn::Default, Size::Lg, true, t)
                    .w_full()
                    .h(px(44.))
                    .when(starting, |b| b.opacity(0.6))
                    .child(if starting { spinner("tg-start", 16., t.primary_fg).into_any_element() } else { icon("send", 16., t.primary_fg).into_any_element() })
                    .child(match (starting, error.is_some()) {
                        (true, _) => t!("onboarding.telegram.opening"),
                        (false, true) => t!("onboarding.telegram.try_again"),
                        (false, false) => t!("onboarding.telegram.sign_in"),
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.telegram_sign_in(cx)))
                    .into_any_element()
            }
        };
        let link = if self.paste_link {
            self.sign_in_body(cx)
        } else {
            button("use-link", Btn::Ghost, Size::Sm, true, t)
                .w_full()
                .fg(t.muted_fg, t.foreground)
                .child(icon("link", 16., t.muted_fg))
                .child(t!("onboarding.use_link"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.paste_link = true;
                    window.focus(&this.link.read(cx).focus_handle(cx));
                    cx.notify();
                }))
                .into_any_element()
        };
        let _ = window;
        div()
            .flex()
            .flex_col()
            .gap(px(24.))
            .child(header)
            .children(self.location_summary(t, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .children(error.map(|e| {
                        div()
                            .flex()
                            .items_start()
                            .gap(px(8.))
                            .p(px(12.))
                            .rounded(px(LG))
                            .bg(t.destructive.opacity(0.1))
                            .sm()
                            .text_color(t.destructive)
                            .child(div().h(px(20.)).flex().items_center().child(icon("shield-alert", 16., t.destructive)))
                            .child(div().flex_1().min_w_0().child(e))
                    }))
                    .child(action),
            )
            .child(divider(t!("onboarding.or"), t))
            .child(link)
            .child(self.bot_line(t!("onboarding.no_subscription_yet"), t!("onboarding.get_one"), t))
    }

    /// The browser is open at Telegram: reopen the page or give up.
    fn waiting(&self, url: String, t: &Theme, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(16.))
            .rounded(px(LG))
            .bg(t.muted.opacity(0.5))
            .child(
                div().flex().items_start().gap(px(12.)).child(div().h(px(20.)).flex().items_center().child(spinner("tg-wait", 16., t.primary))).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().sm().medium().child(t!("onboarding.telegram.waiting")))
                        .child(div().xs().text_color(t.muted_fg).child(t!("onboarding.telegram.waiting_desc"))),
                ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        button("tg-reopen", Btn::Outline, Size::Sm, true, t)
                            .flex_1()
                            .child(icon("globe", 16., t.foreground))
                            .child(t!("onboarding.telegram.open_again"))
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                    .child(button("tg-cancel", Btn::Ghost, Size::Sm, false, t).child(t!("onboarding.telegram.cancel")).on_click(cx.listener(|this, _, _, cx| this.cancel_sign_in(cx)))),
            )
    }

    /// Signed in with Telegram, but there is no subscription to load servers from.
    fn no_subscription(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let user = self.s.user.clone().unwrap_or_default();
        let busy = self.s.busy;
        div()
            .flex()
            .flex_col()
            .gap(px(24.))
            .child(
                div().flex().flex_col().items_center().gap(px(12.)).child(avatar(&self.initial(), self.s.avatar.clone(), 64., t)).child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(div().lg().semibold().child(user.name.clone()))
                        .children(user.username.map(|n| div().sm().text_color(t.muted_fg).child(format!("@{n}")))),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.))
                    .p(px(16.))
                    .rounded(px(LG))
                    .bg(t.muted.opacity(0.5))
                    .child(div().base().semibold().child(t!("onboarding.subscription.title")))
                    .child(div().sm().text_color(t.muted_fg).text_center().child(t!("onboarding.subscription.desc"))),
            )
            .children(self.s.error.clone().map(|e| div().sm().text_color(t.destructive).text_center().child(e)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        button("open-bot", Btn::Default, Size::Lg, true, t)
                            .w_full()
                            .h(px(44.))
                            .child(icon("send", 16., t.primary_fg))
                            .child(t!("onboarding.open_bot"))
                            .on_click(|_, _, cx| cx.open_url(BOT)),
                    )
                    .child(
                        button("check-again", Btn::Outline, Size::Lg, true, t)
                            .w_full()
                            .when(busy, |b| b.opacity(0.6))
                            .child(if busy { spinner("check-spin", 16., t.foreground).into_any_element() } else { icon("refresh-cw", 16., t.foreground).into_any_element() })
                            .child(if busy { t!("onboarding.subscription.checking") } else { t!("onboarding.subscription.check_again") })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.s.busy {
                                    this.with_vpn(cx, |v, cx| v.sync(cx));
                                }
                            })),
                    ),
            )
            .child(
                div().flex().justify_center().child(
                    button("nosub-signout", Btn::Ghost, Size::Sm, true, t)
                        .fg(t.muted_fg, t.foreground)
                        .child(icon("log-out", 16., t.muted_fg))
                        .child(t!("onboarding.subscription.sign_out"))
                        .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx))),
                ),
            )
    }

    fn bot_line(&self, lead: &str, action: &str, t: &Theme) -> Div {
        let primary = t.primary;
        div()
            .flex()
            .justify_center()
            .gap(px(4.))
            .xs()
            .text_color(t.muted_fg)
            .child(lead.to_owned())
            .child(div().id("bot-link").text_color(primary).medium().cursor_pointer().hover(|s| s.underline()).on_click(|_, _, cx| cx.open_url(BOT)).child(action.to_owned()))
    }
}

/// A rule with a word in the middle: `—— or ——`.
fn divider(word: &str, t: &Theme) -> Div {
    let line = || div().flex_1().h(px(1.)).bg(t.border);
    div().flex().items_center().gap(px(12.)).xs().text_color(t.muted_fg).child(line()).child(word.to_owned()).child(line())
}
