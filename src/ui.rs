//! shadcn/ui (new-york-v4) primitives, on a 4px grid.
use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, Context, Div, ElementId, FontFeatures, FontWeight, Hsla, ImageSource, Img, IntoElement, ParentElement, PathBuilder,
    Render, Resource, SharedString, Stateful, Styled, Svg, Transformation, Window, canvas, div, img, percentage, point, prelude::*, px, svg,
};

use crate::fx::{Fx, fx};
use crate::theme::{Theme, shadow_xs};

pub trait Tw: Styled + Sized {
    fn text_px(self, size: f32, line: f32) -> Self {
        self.text_size(px(size)).line_height(px(line))
    }
    fn xs(self) -> Self {
        self.text_px(12., 16.)
    }
    fn sm(self) -> Self {
        self.text_px(14., 20.)
    }
    fn base(self) -> Self {
        self.text_px(16., 24.)
    }
    fn lg(self) -> Self {
        self.text_px(18., 28.)
    }
    fn xl(self) -> Self {
        self.text_px(20., 28.)
    }
    fn normal(self) -> Self {
        self.font_weight(FontWeight::NORMAL)
    }
    fn medium(self) -> Self {
        self.font_weight(FontWeight::MEDIUM)
    }
    fn semibold(self) -> Self {
        self.font_weight(FontWeight::SEMIBOLD)
    }
    fn bold(self) -> Self {
        self.font_weight(FontWeight::BOLD)
    }
    /// Body default: `font-variant-numeric: tabular-nums`.
    fn tnum(mut self) -> Self {
        self.text_style().get_or_insert_with(Default::default).font_features = Some(features(0));
        self
    }
    /// `tracking-tight` (-0.025em), through the vendored GPUI's private `hTRK` feature.
    fn tight(mut self) -> Self {
        self.text_style().get_or_insert_with(Default::default).font_features = Some(features(-25));
        self
    }
}

pub fn features(tracking_milli_em: i32) -> FontFeatures {
    let mut v = vec![("tnum".to_owned(), 1)];
    if tracking_milli_em != 0 {
        v.push(("hTRK".to_owned(), (1000 + tracking_milli_em) as u32));
    }
    FontFeatures(std::sync::Arc::new(v))
}
impl<T: Styled> Tw for T {}

/// Radii: sm 4, md 8, lg 12, xl 16.
pub const SM: f32 = 4.;
pub const MD: f32 = 8.;
pub const LG: f32 = 12.;
pub const XL: f32 = 16.;

pub fn icon(name: &str, size: f32, color: Hsla) -> Svg {
    svg().path(SharedString::from(format!("icons/{name}.svg"))).size(px(size)).flex_none().text_color(color)
}

/// Spinning `Loader2`, 1s linear like `animate-spin`.
pub fn spinner(id: impl Into<ElementId>, size: f32, color: Hsla) -> impl IntoElement {
    icon("loader-circle", size, color).with_animation(id, Animation::new(Duration::from_secs(1)).repeat(), |svg, delta| {
        svg.with_transformation(Transformation::rotate(percentage(delta)))
    })
}

/// Flag glyphs pre-rendered from the web font (24px list boxes, 16px map chips).
pub fn flag(code: &str, big: bool, window: &Window) -> Img {
    let size = if big { 24 } else { 16 };
    let res = if window.scale_factor() > 1. { 2 } else { 1 };
    let path = format!("flags/{}-{size}@{res}x.png", code.to_lowercase());
    img(ImageSource::Resource(Resource::Embedded(path.into()))).size(px(size as f32)).flex_none()
}

/// `inset-ring`: a 1px outline drawn over the box, taking no layout. Parent must be `relative`.
pub fn ring(color: Hsla, radius: f32) -> Div {
    div().absolute().inset_0().rounded(px(radius)).border_1().border_color(color)
}

/// Load ring: a track with the filled share drawn clockwise from the top.
pub fn meter(fraction: f32, size: f32, width: f32, fill: Hsla, track: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |b, _, window, _| {
            let c = b.center();
            let r = (size - width) / 2.;
            let at = |a: f32| point(c.x + px(r * a.sin()), c.y - px(r * a.cos()));
            let mut arc = |from: f32, to: f32, color: Hsla| {
                if to - from < 1e-3 {
                    return;
                }
                let mut p = PathBuilder::stroke(px(width));
                p.move_to(at(from));
                let steps = ((to - from) / std::f32::consts::FRAC_PI_2).ceil();
                for i in 1..=steps as usize {
                    p.arc_to(point(px(r), px(r)), px(0.), false, true, at(from + (to - from) * i as f32 / steps));
                }
                if let Ok(path) = p.build() {
                    window.paint_path(path, color);
                }
            };
            let end = std::f32::consts::TAU * fraction.clamp(0., 1.);
            arc(end, std::f32::consts::TAU, track);
            arc(0., end, fill);
        },
    )
    .size(px(size))
    .flex_none()
}

/// `animate-pulse`: opacity 1 → .5 → 1 over 2s.
pub fn pulse<E: IntoElement + Styled + 'static>(id: impl Into<ElementId>, el: E) -> impl IntoElement
where
    gpui::AnimationElement<E>: IntoElement,
{
    el.with_animation(id, Animation::new(Duration::from_secs(2)).repeat(), |el, d| {
        let t = if d < 0.5 { d * 2. } else { (1. - d) * 2. };
        let eased = t * t * (3. - 2. * t);
        el.opacity(1. - 0.5 * eased)
    })
}

#[derive(Clone, Copy, PartialEq)]
pub enum Variant {
    Default,
    Secondary,
    Ghost,
    Outline,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Size {
    Default,
    Xs,
    Sm,
    Lg,
    Icon,
    IconXs,
    IconSm,
    IconLg,
}

/// `<Button>`: callers add children and `on_click`. `icon` mirrors Tailwind's `has-[>svg]` padding.
/// Colours transition like `transition-all`; set a state background with `.rest(..)`, not `.bg(..)`.
pub fn button(id: impl Into<SharedString>, variant: Variant, size: Size, icon: bool, t: &Theme) -> Fx {
    let t = *t;
    let (rest, hover, fg) = match variant {
        Variant::Default => (t.primary, t.primary.opacity(0.9), t.primary_fg),
        Variant::Secondary => (t.secondary, t.secondary.opacity(0.8), t.secondary_fg),
        Variant::Ghost => (gpui::transparent_black(), t.ghost_hover(), t.foreground),
        Variant::Outline => {
            if t.dark { (t.input.opacity(0.3), t.input.opacity(0.5), t.foreground) } else { (t.background, t.accent, t.foreground) }
        }
    };
    let mut d = fx(id, rest, Some(hover)).fg(fg, fg).flex().flex_none().items_center().justify_center().gap(px(8.)).rounded(px(MD)).sm().medium().whitespace_nowrap().cursor_pointer();
    d = match size {
        Size::Default => d.h(px(36.)).px(px(if icon { 12. } else { 16. })),
        Size::Xs => d.h(px(24.)).gap(px(4.)).px(px(8.)).xs(),
        Size::Sm => d.h(px(32.)).px(px(12.)),
        Size::Lg => d.h(px(40.)).px(px(if icon { 16. } else { 24. })),
        Size::Icon => d.size(px(36.)),
        Size::IconXs => d.size(px(24.)),
        Size::IconSm => d.size(px(32.)),
        Size::IconLg => d.size(px(40.)),
    };
    if variant == Variant::Outline {
        d = d.border_1().border_color(if t.dark { t.input } else { t.border }).shadow(shadow_xs());
    }
    d
}

/// Icon size inside a button of this size.
pub fn button_icon(size: Size) -> f32 {
    if matches!(size, Size::Xs | Size::IconXs) { 12. } else { 16. }
}

/// `<Switch>`: the track colour and thumb transition over 150ms (`transition-all` / `transition-transform`).
#[derive(IntoElement)]
pub struct Switch {
    inner: Stateful<Div>,
    key: SharedString,
    on: bool,
    theme: Theme,
}

pub fn switch(id: impl Into<SharedString>, on: bool, t: &Theme) -> Switch {
    let key: SharedString = id.into();
    Switch { inner: div().id(ElementId::Name(key.clone())), key, on, theme: *t }
}

impl gpui::InteractiveElement for Switch {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.inner.interactivity()
    }
}
impl gpui::StatefulInteractiveElement for Switch {}

impl gpui::RenderOnce for Switch {
    fn render(self, window: &mut Window, cx: &mut gpui::App) -> impl IntoElement {
        let t = self.theme;
        let track = if self.on { t.primary } else if t.dark { t.input.opacity(0.8) } else { t.input };
        let thumb = if t.dark {
            if self.on { t.primary_fg } else { t.foreground }
        } else {
            t.background
        };
        let track = crate::fx::color(format!("{}/track", self.key), track, 150., window, cx);
        let thumb = crate::fx::color(format!("{}/thumb", self.key), thumb, 150., window, cx);
        let x = crate::fx::tween(format!("{}/x", self.key), if self.on { 15. } else { 1. }, 150., crate::fx::ease_in_out, window, cx);
        self.inner
            .relative()
            .flex_none()
            .w(px(32.))
            .h(px(20.))
            .rounded_full()
            .bg(track)
            .shadow(shadow_xs())
            .cursor_pointer()
            .child(div().absolute().top(px(2.)).left(px(x)).size(px(16.)).rounded_full().bg(thumb))
    }
}

pub fn avatar(letter: &str, t: &Theme) -> Div {
    div().flex_none().size(px(32.)).rounded_full().overflow_hidden().bg(t.muted).flex().items_center().justify_center().sm().text_color(t.muted_fg).child(letter.to_owned())
}

pub fn skeleton(id: impl Into<ElementId>, w: f32, h: f32, t: &Theme) -> impl IntoElement {
    pulse(id, div().flex_none().w(px(w)).h(px(h)).rounded(px(MD)).bg(t.accent))
}

pub fn badge_secondary(text: &str, t: &Theme) -> Div {
    div().flex().flex_none().items_center().justify_center().h(px(24.)).px(px(8.)).rounded_full().bg(t.secondary).text_color(t.secondary_fg).xs().medium().whitespace_nowrap().child(text.to_owned())
}

pub fn progress(fraction: f32, t: &Theme) -> Div {
    div().relative().h(px(8.)).w_full().overflow_hidden().rounded_full().bg(t.primary.opacity(0.2)).child(div().absolute().top_0().left_0().h_full().w(gpui::relative(fraction)).bg(t.primary))
}

/// Tooltip bubble: `bg-foreground text-background px-3 py-2 text-xs rounded-md`, fading in like
/// tw-animate's `fade-in-0 slide-in-from-bottom-2` (150ms, ease).
pub struct Tip {
    text: SharedString,
    theme: Theme,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .font_family("Google Sans")
            .tnum()
            .relative()
            .px(px(12.))
            .py(px(8.))
            .rounded(px(MD))
            .bg(self.theme.foreground)
            .text_color(self.theme.background)
            .xs()
            .child(self.text.clone())
            .with_animation("tip-in", Animation::new(Duration::from_millis(150)).with_easing(crate::fx::ease), |d, p| d.opacity(p).top(px(8. * (1. - p))))
    }
}

pub fn tip(text: impl Into<SharedString>, t: &Theme) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView + 'static {
    let text = text.into();
    let theme = *t;
    move |_, cx| cx.new(|_| Tip { text: text.clone(), theme }).into()
}

// ---- dropdown menu pieces ----

/// `DropdownMenuContent`: p-1 rounded-md bg-popover ring shadow-md.
pub fn menu(width: f32, t: &Theme, children: Vec<AnyElement>) -> Stateful<Div> {
    div()
        .id("menu")
        .occlude()
        .relative()
        .w(px(width))
        .p(px(4.))
        .rounded(px(MD))
        .bg(t.popover)
        .text_color(t.foreground)
        .shadow(crate::theme::shadow_md())
        .flex()
        .flex_col()
        .font_family("Google Sans")
        .tnum()
        .base()
        .children(children)
        .child(ring(t.border, MD))
}

pub fn menu_label(t: &Theme) -> Div {
    let _ = t;
    div().px(px(8.)).py(px(8.)).sm().medium()
}

pub fn menu_separator(t: &Theme) -> Div {
    div().relative().mx(px(-4.)).h(px(8.)).child(div().absolute().left_0().right_0().top(px(4.)).h(px(1.)).bg(t.border))
}

/// `DropdownMenuItem`: px-2 py-2 gap-2 text-sm, icons 16px muted.
pub fn menu_item(id: impl Into<ElementId>, t: &Theme, destructive: bool) -> Stateful<Div> {
    let hover = if destructive { t.destructive.opacity(if t.dark { 0.2 } else { 0.1 }) } else { t.accent };
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.))
        .rounded(px(SM))
        .px(px(8.))
        .py(px(8.))
        .sm()
        .cursor_pointer()
        .text_color(if destructive { t.destructive } else { t.foreground })
        .hover(move |s| s.bg(hover))
}

/// Width of a single line of text, laid out the way the vendored GPUI paints it.
pub fn text_width(text: &str, size: f32, weight: FontWeight, window: &Window) -> f32 {
    let font = gpui::Font { family: "Google Sans".into(), features: features(0), fallbacks: None, weight, style: gpui::FontStyle::Normal };
    let run = gpui::TextRun { len: text.len(), font, color: gpui::black(), background_color: None, underline: None, strikethrough: None };
    f32::from(window.text_system().shape_line(text.to_owned().into(), px(size), &[run], None).width)
}

/// CSS `text-wrap: balance` (shadcn's ItemDescription): the narrowest width that keeps the greedy line count.
pub fn balanced_width(text: &str, size: f32, available: f32, window: &Window) -> Option<f32> {
    let font = gpui::Font { family: "Google Sans".into(), features: features(0), fallbacks: None, weight: FontWeight::NORMAL, style: gpui::FontStyle::Normal };
    let run = gpui::TextRun { len: text.len(), font, color: gpui::black(), background_color: None, underline: None, strikethrough: None };
    let line = window.text_system().shape_line(text.to_owned().into(), px(size), &[run], None);
    let mut words = vec![];
    let mut at = 0;
    for w in text.split(' ') {
        words.push((f32::from(line.x_for_index(at)), f32::from(line.x_for_index(at + w.len()))));
        at += w.len() + 1;
    }
    let lines = |width: f32| {
        let (mut n, mut start) = (1, words[0].0);
        for &(s, e) in &words[1..] {
            if e - start > width {
                n += 1;
                start = s;
            }
        }
        n
    };
    let n = lines(available);
    if n < 2 {
        return None;
    }
    let (mut lo, mut hi) = (0, available.floor() as i32);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if lines(mid as f32) <= n { hi = mid } else { lo = mid + 1 }
    }
    // GPUI's wrapper sums per-character advances rather than the shaped line, so leave it a space's width of room.
    Some((hi as f32 + size * 0.25).ceil().min(available))
}
