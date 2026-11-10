//! Animations in the style of Tailwind/CSS: Tailwind transitions (150ms, cubic-bezier(.4,0,.2,1)) and
//! tw-animate-css enter/exit (150ms, `ease`).
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Div, ElementId, Hsla, InteractiveElement, Interactivity, IntoElement, ParentElement, RenderOnce, Rgba, SharedString,
    Stateful, StatefulInteractiveElement, StyleRefinement, Styled, Window, div,
};

fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let b = |a: f32, b: f32, s: f32| 3. * a * s * (1. - s).powi(2) + 3. * b * s * s * (1. - s) + s.powi(3);
    let (mut lo, mut hi) = (0f32, 1f32);
    for _ in 0..24 {
        let mid = (lo + hi) / 2.;
        if b(x1, x2, mid) < t { lo = mid } else { hi = mid }
    }
    b(y1, y2, (lo + hi) / 2.)
}

pub type Ease = fn(f32) -> f32;
/// CSS `ease`: tw-animate-css enter/exit.
pub fn ease(t: f32) -> f32 {
    bezier(0.25, 0.1, 0.25, 1., t)
}
/// Tailwind's default transition timing and `ease-in-out`.
pub fn ease_in_out(t: f32) -> f32 {
    bezier(0.4, 0., 0.2, 1., t)
}
pub fn linear(t: f32) -> f32 {
    t
}

pub trait Lerp: Copy + PartialEq + 'static {
    fn lerp(a: Self, b: Self, t: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(a: Self, b: Self, t: f32) -> Self {
        a + (b - a) * t
    }
}

/// Colours interpolate in premultiplied sRGB, as CSS transitions do.
impl Lerp for Rgba {
    fn lerp(a: Self, b: Self, t: f32) -> Self {
        let alpha = a.a + (b.a - a.a) * t;
        let ch = |x: f32, y: f32| if alpha <= 0. { 0. } else { (x * a.a + (y * b.a - x * a.a) * t) / alpha };
        Rgba { r: ch(a.r, b.r), g: ch(a.g, b.g), b: ch(a.b, b.b), a: alpha }
    }
}

struct Tween<T> {
    from: T,
    to: T,
    start: Instant,
    ms: f32,
    ease: Ease,
}

impl<T: Lerp> Tween<T> {
    fn progress(&self) -> f32 {
        if self.ms <= 0. { 1. } else { (self.start.elapsed().as_secs_f32() * 1000. / self.ms).min(1.) }
    }
    fn value(&self) -> T {
        T::lerp(self.from, self.to, (self.ease)(self.progress()))
    }
}

/// A value that eases toward `target` whenever the target changes; keeps the view redrawing until it settles.
pub fn tween<T: Lerp>(key: impl Into<SharedString>, target: T, ms: f32, ease: Ease, window: &mut Window, cx: &mut App) -> T {
    let key: SharedString = key.into();
    let state = window.use_keyed_state(ElementId::Name(key), cx, |_, _| Tween { from: target, to: target, start: Instant::now() - Duration::from_secs(1), ms, ease });
    let (value, moving) = state.update(cx, |t, _| {
        if t.to != target {
            t.from = t.value();
            t.to = target;
            t.start = Instant::now();
            t.ms = ms;
            t.ease = ease;
        }
        (t.value(), t.progress() < 1.)
    });
    if moving {
        window.request_animation_frame();
    }
    value
}

pub fn color(key: impl Into<SharedString>, target: Hsla, ms: f32, window: &mut Window, cx: &mut App) -> Hsla {
    tween(key, Rgba::from(target), ms, ease_in_out, window, cx).into()
}

/// An element whose background (and optionally text colour) transitions between rest and hover,
/// like `transition-colors` / `transition-all` with a `hover:` colour.
#[derive(IntoElement)]
pub struct Fx {
    inner: Stateful<Div>,
    key: SharedString,
    rest: Hsla,
    hover: Option<Hsla>,
    fg: Option<(Hsla, Hsla)>,
    ms: f32,
}

pub fn fx(id: impl Into<SharedString>, rest: Hsla, hover: Option<Hsla>) -> Fx {
    let key: SharedString = id.into();
    Fx { inner: div().id(ElementId::Name(key.clone())), key, rest, hover, fg: None, ms: 150. }
}

impl Fx {
    /// Background when not hovered (state colour: open menu, connected, …).
    pub fn rest(mut self, c: Hsla) -> Self {
        self.rest = c;
        self
    }
    /// Text colour at rest and on hover, transitioned with the background.
    pub fn fg(mut self, rest: Hsla, hover: Hsla) -> Self {
        self.fg = Some((rest, hover));
        self
    }
}

impl Styled for Fx {
    fn style(&mut self) -> &mut StyleRefinement {
        self.inner.style()
    }
}

impl ParentElement for Fx {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.inner.extend(elements)
    }
}

impl InteractiveElement for Fx {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.inner.interactivity()
    }
}

impl StatefulInteractiveElement for Fx {}

impl RenderOnce for Fx {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let hovered = window.use_keyed_state(ElementId::Name(format!("{}/hover", self.key).into()), cx, |_, _| false);
        let on = *hovered.read(cx);
        let bg = color(format!("{}/bg", self.key), if on { self.hover.unwrap_or(self.rest) } else { self.rest }, self.ms, window, cx);
        let mut inner = self.inner.bg(bg).on_hover(move |h, _, cx| {
            hovered.update(cx, |s, cx| {
                if *s != *h {
                    *s = *h;
                    cx.notify();
                }
            })
        });
        if let Some((rest, hover)) = self.fg {
            inner = inner.text_color(color(format!("{}/fg", self.key), if on { hover } else { rest }, self.ms, window, cx));
        }
        inner
    }
}

/// Enter/exit bookkeeping for overlays that animate out after they are closed.
pub struct Presence<T: Copy + PartialEq> {
    last: Option<T>,
    opened: Instant,
    closing: Option<(T, Instant)>,
}

impl<T: Copy + PartialEq> Presence<T> {
    pub fn new() -> Self {
        Self { last: None, opened: Instant::now(), closing: None }
    }

    /// Call each frame with the logical open item.
    pub fn sync(&mut self, now: Option<T>) {
        if now != self.last {
            if let Some(old) = self.last {
                self.closing = Some((old, Instant::now()));
            }
            if now.is_some() {
                self.opened = Instant::now();
            }
            self.last = now;
        }
    }

    /// Eased visibility 0..1 for `which` while open or animating out; `entering` tells the two phases apart.
    pub fn phase(&self, which: T, in_ms: f32, out_ms: f32, ease: Ease) -> Option<(f32, bool)> {
        if self.last == Some(which) {
            let p = (self.opened.elapsed().as_secs_f32() * 1000. / in_ms).min(1.);
            return Some((ease(p), true));
        }
        match self.closing {
            Some((w, at)) if w == which => {
                let p = at.elapsed().as_secs_f32() * 1000. / out_ms;
                (p < 1.).then(|| (1. - ease(p), false))
            }
            _ => None,
        }
    }

    /// The item being shown, open or closing.
    pub fn shown(&self, out_ms: f32) -> Option<T> {
        self.last.or_else(|| self.closing.filter(|(_, at)| at.elapsed().as_secs_f32() * 1000. < out_ms).map(|(w, _)| w))
    }

    pub fn animating(&self, in_ms: f32, out_ms: f32) -> bool {
        (self.last.is_some() && self.opened.elapsed().as_secs_f32() * 1000. < in_ms)
            || self.closing.is_some_and(|(_, at)| at.elapsed().as_secs_f32() * 1000. < out_ms)
    }
}
