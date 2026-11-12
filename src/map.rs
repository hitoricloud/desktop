//! A small slippy map with MapLibre's camera maths, drawing CARTO's label-free raster tiles.
use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt, AnyElement, Context, Hsla, ImageSource, MouseButton, Pixels, Point, RenderImage, ScrollDelta, Window, div, img,
    SharedString, prelude::*, px,
};

use crate::app::{Conn, Hitori, Ring};
use crate::data::{self, AppRule, RuleMode, Server, server};
use crate::theme::Theme;
use crate::ui::{MD, Size, Tw, Variant, button, button_icon, flag, icon, tip};
use crate::views::{app_badge, site_badge};
use crate::vtile::{self, VTile};

const FIT_MS: f64 = 900.;
const MAX_ZOOM: f64 = 5.;
const MAX_USER_ZOOM: f64 = 18.;
/// Zoom levels per wheel line (a notch is 3 lines on Linux and Windows) and per touchpad pixel.
const ZOOM_PER_LINE: f64 = 1. / 6.;
const ZOOM_PER_PX: f64 = 1. / 300.;
/// Time constants (s) of the exponential zoom easing: wheel notches glide, touchpad deltas follow closely.
const WHEEL_TAU: f64 = 0.09;
const TOUCH_TAU: f64 = 0.04;
/// The whole world at this zoom, drawn under everything so no part of the view is ever blank.
const BACKDROP_Z: u32 = 2;

pub struct Padding {
    pub top: f32,
    pub bottom: f32,
    pub left: f32,
    pub right: f32,
}

enum Tile {
    Loading,
    Ready(Arc<VTile>),
    Failed,
}

struct Ease {
    from: ((f64, f64), f64),
    to: ((f64, f64), f64),
    start: Instant,
}

/// A wheel or touchpad zoom in progress: the unit point `anchor` stays under the view point `at`.
struct Zoom {
    to: f64,
    at: (f32, f32),
    anchor: (f64, f64),
    tau: f64,
}

pub struct MapState {
    center: (f64, f64),
    zoom: f64,
    ease: Option<Ease>,
    zooming: Option<Zoom>,
    /// The last camera step, for frame-rate independent easing.
    tick: Instant,
    framed: String,
    size: (f32, f32),
    tiles: HashMap<(u32, u32, u32), Tile>,
    /// Bumped when a tile arrives, so the raster key changes.
    tiles_gen: u64,
    /// The basemap, labels and route for one camera, rasterized off the main thread and drawn as one image.
    /// While the camera moves, the newest raster is scaled and shifted on the GPU until the next one is ready.
    raster: Option<Raster>,
    /// The raster before `raster`, still drawn beneath it where the newer one does not reach.
    under: Option<Raster>,
    /// The whole world without labels (see `BACKDROP_Z`).
    backdrop: Option<Raster>,
    job: Option<gpui::Task<()>>,
    backdrop_job: Option<gpui::Task<()>>,
    /// Replaced rasters, freed a few frames later: the GPU may still be drawing them.
    retired: Vec<(Arc<RenderImage>, u8)>,
    inflight: usize,
    drag: Option<(Point<Pixels>, (f64, f64))>,
    /// The server clicked on the map: its card asks to confirm before the connection moves there.
    pub pick: Option<&'static str>,
}

/// Web Mercator in [0, 1] world units.
struct Raster {
    key: String,
    image: Arc<RenderImage>,
    center: (f64, f64),
    zoom: f64,
    /// Logical size, overscan included.
    size: (f32, f32),
}

/// Extra map rasterized around the view, so panning shows map rather than an empty edge before the next raster.
const OVERSCAN: (f32, f32) = (256., 128.);

fn unit(lat: f64, lon: f64) -> (f64, f64) {
    let s = (lat.clamp(-85.05, 85.05) * PI / 180.).sin();
    ((lon + 180.) / 360., 0.5 - ((1. + s) / (1. - s)).ln() / (4. * PI))
}

fn world_px(zoom: f64) -> f64 {
    512. * 2f64.powf(zoom)
}

/// The shortest signed horizontal distance on the wrapped world.
fn wrap(dx: f64) -> f64 {
    dx - dx.round()
}

/// The smallest zoom at which the world covers the whole view.
fn min_zoom(size: (f32, f32)) -> f64 {
    (size.0.max(size.1) as f64 / 512.).log2().max(0.)
}

/// Keeps the poles at or beyond the view's edges and the centre's x in [0, 1).
fn constrain(center: (f64, f64), zoom: f64, size: (f32, f32)) -> (f64, f64) {
    let half = size.1 as f64 / 2. / world_px(zoom);
    (center.0.rem_euclid(1.), if half >= 0.5 { 0.5 } else { center.1.clamp(half, 1. - half) })
}

/// The camera centre that puts unit point `u` at view point `at`.
fn anchored(u: (f64, f64), at: (f32, f32), zoom: f64, size: (f32, f32)) -> (f64, f64) {
    let world = world_px(zoom);
    (u.0 - (at.0 - size.0 / 2.) as f64 / world, u.1 - (at.1 - size.1 / 2.) as f64 / world)
}

/// Whole-world shifts that bring the x range [a, b] into [c - half, c + half].
fn copies(a: f64, b: f64, c: f64, half: f64) -> std::ops::RangeInclusive<i64> {
    (c - half - b).ceil() as i64..=(c + half - a).floor() as i64
}

/// The shortest x range on the wrapped world holding every x; the end may pass 1.
fn span(xs: &[f64]) -> (f64, f64) {
    let mut xs: Vec<f64> = xs.iter().map(|x| x.rem_euclid(1.)).collect();
    xs.sort_by(f64::total_cmp);
    // Leave out the widest gap between neighbours, the one across the seam included.
    let gap = |i: usize| if i + 1 < xs.len() { xs[i + 1] - xs[i] } else { xs[0] + 1. - xs[i] };
    let i = (0..xs.len()).max_by(|a, b| gap(*a).total_cmp(&gap(*b))).unwrap_or(0);
    if i + 1 == xs.len() { (xs[0], xs[i]) } else { (xs[i + 1], xs[i] + 1.) }
}

/// MapLibre's default easing, cubic-bezier(0.25, 0.1, 0.25, 1).
fn ease(t: f64) -> f64 {
    let (x1, y1, x2, y2) = (0.25, 0.1, 0.25, 1.0);
    let bez = |a: f64, b: f64, s: f64| 3. * a * s * (1. - s).powi(2) + 3. * b * s * s * (1. - s) + s.powi(3);
    let (mut lo, mut hi) = (0., 1.);
    for _ in 0..30 {
        let mid = (lo + hi) / 2.;
        if bez(x1, x2, mid) < t { lo = mid } else { hi = mid }
    }
    bez(y1, y2, (lo + hi) / 2.)
}

/// Great-circle points between two places, as (lat, lon).
fn arc(a: (f64, f64), b: (f64, f64), steps: usize) -> Vec<(f64, f64)> {
    let r = PI / 180.;
    let v = |p: (f64, f64)| [(p.0 * r).cos() * (p.1 * r).cos(), (p.0 * r).cos() * (p.1 * r).sin(), (p.0 * r).sin()];
    let (p, q) = (v(a), v(b));
    let angle = (p[0] * q[0] + p[1] * q[1] + p[2] * q[2]).min(1.).acos();
    if angle < 1e-6 {
        return vec![a, b];
    }
    (0..=steps)
        .map(|i| {
            let t = i as f64 / steps as f64;
            let (s1, s2) = (((1. - t) * angle).sin() / angle.sin(), (t * angle).sin() / angle.sin());
            let c: Vec<f64> = (0..3).map(|k| s1 * p[k] + s2 * q[k]).collect();
            (c[2].atan2((c[0] * c[0] + c[1] * c[1]).sqrt()) / r, c[1].atan2(c[0]) / r)
        })
        .collect()
}

impl MapState {
    pub fn new() -> Self {
        Self {
            center: unit(52., 30.),
            zoom: 3.,
            ease: None,
            zooming: None,
            tick: Instant::now(),
            framed: String::new(),
            size: (0., 0.),
            tiles: HashMap::new(),
            tiles_gen: 0,
            raster: None,
            under: None,
            backdrop: None,
            job: None,
            backdrop_job: None,
            retired: vec![],
            inflight: 0,
            drag: None,
            pick: None,
        }
    }

    /// HITORI_STATE scripting: `zoom:<levels>@<x>;<y>` around a view point, `pan:<dx>;<dy>` as a drag.
    pub fn script(&mut self, key: &str, value: &str) {
        let num = |s: &str| s.trim().parse::<f32>().unwrap_or(0.);
        let (a, at) = value.split_once('@').unwrap_or((value, ""));
        let (x, y) = at.split_once(';').or(a.split_once(';')).map_or((self.size.0 / 2., self.size.1 / 2.), |(x, y)| (num(x), num(y)));
        match key {
            "zoom" => self.zoom_by(num(a) as f64, (x, y), WHEEL_TAU),
            _ => {
                self.ease = None;
                self.pan((gpui::point(px(0.), px(0.)), self.center), gpui::point(px(x), px(y)));
            }
        }
    }

    /// Theme changed: tiles of the other style are no longer drawn.
    pub fn restyle(&mut self) {}

    pub fn refit(&mut self) {
        self.framed.clear();
    }

    fn world(&self) -> f64 {
        world_px(self.zoom)
    }

    /// A unit point in view pixels, at the world copy nearest the view's centre.
    fn screen(&self, u: (f64, f64)) -> (f32, f32) {
        let world = self.world();
        ((wrap(u.0 - self.center.0) * world + self.size.0 as f64 / 2.) as f32, ((u.1 - self.center.1) * world + self.size.1 as f64 / 2.) as f32)
    }

    /// The unit point under a view point.
    fn unproject(&self, at: (f32, f32)) -> (f64, f64) {
        let world = self.world();
        (self.center.0 + (at.0 - self.size.0 / 2.) as f64 / world, self.center.1 + (at.1 - self.size.1 / 2.) as f64 / world)
    }

    /// Eases the zoom by `dz` levels around the view point `at`; further deltas add to the target.
    fn zoom_by(&mut self, dz: f64, at: (f32, f32), tau: f64) {
        self.ease = None;
        if self.zooming.is_none() {
            self.tick = Instant::now();
        }
        let to = (self.zooming.as_ref().map_or(self.zoom, |z| z.to) + dz).clamp(min_zoom(self.size), MAX_USER_ZOOM);
        self.zooming = Some(Zoom { to, at, anchor: self.unproject(at), tau });
    }

    /// Advances the fit and zoom animations to `now`; true while either still runs.
    fn step(&mut self, now: Instant) -> bool {
        let dt = now.saturating_duration_since(self.tick).as_secs_f64().min(0.05);
        self.tick = now;
        let mut moving = false;
        if let Some(e) = &self.ease {
            let p = (now.saturating_duration_since(e.start).as_secs_f64() * 1000. / FIT_MS).min(1.);
            let k = ease(p);
            self.center = (e.from.0.0 + (e.to.0.0 - e.from.0.0) * k, e.from.0.1 + (e.to.0.1 - e.from.0.1) * k);
            self.zoom = e.from.1 + (e.to.1 - e.from.1) * k;
            if p >= 1. { self.ease = None } else { moving = true }
        }
        if let Some(z) = &self.zooming {
            self.zoom += (z.to - self.zoom) * (1. - (-dt / z.tau).exp());
            if (z.to - self.zoom).abs() < 1e-3 {
                self.zoom = z.to;
            }
            self.center = anchored(z.anchor, z.at, self.zoom, self.size);
            if self.zoom == z.to { self.zooming = None } else { moving = true }
        }
        self.zoom = self.zoom.clamp(min_zoom(self.size), MAX_USER_ZOOM);
        self.center = constrain(self.center, self.zoom, self.size);
        moving
    }

    /// Drag from `start` (the view point and centre at mouse-down) to `at`.
    fn pan(&mut self, start: (Point<Pixels>, (f64, f64)), at: Point<Pixels>) {
        let (world, d) = (self.world(), at - start.0);
        self.center = constrain((start.1.0 - f64::from(f32::from(d.x)) / world, start.1.1 - f64::from(f32::from(d.y)) / world), self.zoom, self.size);
    }

    /// `fitBounds(bounds, { padding, maxZoom: 5 })`, the short way round, at no less than `min_zoom`.
    fn fit(&self, points: &[(f64, f64)], pad: &Padding) -> ((f64, f64), f64) {
        let us: Vec<_> = points.iter().map(|p| unit(p.0, p.1)).collect();
        let (x0, x1) = span(&us.iter().map(|u| u.0).collect::<Vec<_>>());
        let (y0, y1) = us.iter().fold((f64::MAX, f64::MIN), |(a, b), u| (a.min(u.1), b.max(u.1)));
        let (w, h) = (self.size.0 as f64, self.size.1 as f64);
        let sx = (w - (pad.left + pad.right) as f64) / ((x1 - x0) * 512.);
        let sy = (h - (pad.top + pad.bottom) as f64) / ((y1 - y0) * 512.);
        let zoom = sx.min(sy).log2().min(MAX_ZOOM).max(min_zoom(self.size));
        let world = world_px(zoom);
        let cx = (x0 + x1) / 2. + ((pad.right - pad.left) as f64 / 2.) / world;
        let cy = (y0 + y1) / 2. + ((pad.bottom - pad.top) as f64 / 2.) / world;
        (constrain((cx, cy), zoom, self.size), zoom)
    }
}

/// What travels a route: shown as badges on its exit's label.
#[derive(Clone, Copy, PartialEq)]
enum Rider {
    App(&'static str),
    Site(&'static str),
}

/// One path on the map: servers ending at the exit, and what uses it.
struct Path {
    hops: Vec<&'static str>,
    main: bool,
    riders: Vec<Rider>,
}

/// Everything in use: the main connection, per-app exits and cascades, and what stays local or is blocked.
struct Routes {
    paths: Vec<Path>,
    direct: Vec<Rider>,
    blocked: Vec<Rider>,
}

impl Routes {
    fn of(this: &Hitori) -> Routes {
        let s = &this.s;
        let entry = s.entry_id.filter(|e| *e != s.server_id);
        let main: Vec<&'static str> = entry.into_iter().chain([s.server_id]).collect();
        let mut r = Routes { paths: vec![Path { hops: main, main: true, riders: vec![] }], direct: vec![], blocked: vec![] };
        let mut put = |who: Rider, rule: &AppRule| match rule.mode {
            RuleMode::Block => r.blocked.push(who),
            RuleMode::Direct => r.direct.push(who),
            RuleMode::Vpn | RuleMode::Auto => {
                let exit = rule.exit.filter(|e| data::has_server(e)).unwrap_or(s.server_id);
                let via = rule.via.or(if rule.exit.is_some() { None } else { entry }).filter(|v| *v != exit && data::has_server(v));
                let hops: Vec<&'static str> = via.into_iter().chain([exit]).collect();
                match r.paths.iter_mut().find(|p| p.hops == hops) {
                    Some(p) => p.riders.push(who),
                    None => r.paths.push(Path { hops, main: false, riders: vec![who] }),
                }
            }
        };
        for (a, rule) in s.app_routes.iter().filter(|(a, _)| data::active(a)) {
            put(Rider::App(a), rule);
        }
        for (site, rule) in &s.sites {
            put(Rider::Site(site), rule);
        }
        r
    }

    /// Each path as places from you through its hops, and whether it is the main connection.
    fn lines(&self, me: Option<(f64, f64)>) -> Vec<(Vec<(f64, f64)>, bool)> {
        self.paths
            .iter()
            .map(|p| (me.into_iter().chain(p.hops.iter().map(|h| (server(h).lat, server(h).lon))).collect(), p.main))
            .collect()
    }

    /// Who leaves the tunnel at this server.
    fn uses(&self, id: &str) -> bool {
        self.paths.iter().any(|p| p.hops.contains(&id))
    }

    fn riders_at(&self, id: &str) -> Vec<Rider> {
        self.paths.iter().filter(|p| p.hops.last() == Some(&id)).flat_map(|p| p.riders.iter().copied()).collect()
    }
}

fn me(this: &Hitori) -> Option<(f64, f64)> {
    this.s.me.as_ref().and_then(|m| m.geo.as_ref()).map(|g| (g.lat, g.lon))
}

/// Up to six badges, then a count; each names its app or site on hover.
fn badges(riders: &[Rider], key: &str, t: &Theme) -> gpui::Div {
    let mut row = div().flex().items_center().gap(px(2.));
    for (i, r) in riders.iter().take(6).enumerate() {
        let (badge, name) = match r {
            Rider::App(a) => (app_badge(data::app(a)), data::app(a).name.to_owned()),
            Rider::Site(s) => (site_badge(t), (*s).to_owned()),
        };
        row = row.child(div().id(gpui::SharedString::from(format!("rb-{key}-{i}"))).tooltip(tip(name, t)).child(badge));
    }
    if riders.len() > 6 {
        row = row.child(div().pl(px(2.)).xs().text_color(t.muted_fg).child(format!("+{}", riders.len() - 6)));
    }
    row
}

impl Hitori {
    pub fn map_view(&mut self, size: (f32, f32), mut pad: Padding, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme(cx);
        let scale = window.scale_factor();
        let routes = Routes::of(self);
        // Your label is centred on you and grows with what stays local; keep it inside the map.
        let local = routes.direct.len().min(6) + routes.blocked.len().min(6);
        let half = (40. + 22. * local as f32 + 56. * (!routes.direct.is_empty() as u8 + !routes.blocked.is_empty() as u8) as f32) / 2.;
        pad.left = pad.left.max(half + 16.);
        pad.right = pad.right.max(half + 16.);
        let lines = routes.lines(me(self));
        let frame = format!("{lines:?}");
        let m = &mut self.map;
        if m.size != size {
            m.size = size;
            m.framed.clear();
        }
        let key = frame;
        if key != m.framed {
            m.framed = key;
            let points: Vec<(f64, f64)> = lines.iter().flat_map(|(l, _)| l.iter().copied()).collect();
            let (c, z) = m.fit(&points, &pad);
            // Towards the copy of the target nearest the current view, not across the whole world.
            let to = ((m.center.0 + wrap(c.0 - m.center.0), c.1), z);
            m.zooming = None;
            m.ease = Some(Ease { from: (m.center, m.zoom), to, start: Instant::now() });
        }
        if m.step(Instant::now()) {
            window.request_animation_frame();
        }

        let basemap = self.basemap(&t, scale, window, cx);
        let markers = self.markers(&t, window, cx);
        div()
            .id("map")
            .absolute()
            .inset_0()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                if this.map.pick.take().is_some() {
                    cx.notify();
                }
                this.map.ease = None;
                this.map.zooming = None;
                this.map.drag = Some((e.position, this.map.center));
            }))
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                if let (Some(start), true) = (this.map.drag, e.dragging()) {
                    this.map.pan(start, e.position);
                    cx.notify();
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.map.drag = None))
            // Wheel notches and touchpad scrolls zoom around the cursor (Windows reports touchpad pinches as ctrl+wheel).
            .on_scroll_wheel(cx.listener(|this, e: &gpui::ScrollWheelEvent, _, cx| {
                let (dz, tau) = match e.delta {
                    ScrollDelta::Pixels(p) => (f32::from(p.y) as f64 * ZOOM_PER_PX, TOUCH_TAU),
                    ScrollDelta::Lines(l) => (l.y as f64 * ZOOM_PER_LINE, WHEEL_TAU),
                };
                if dz != 0. {
                    let (ox, oy, _, _) = this.ring_clip.get();
                    this.map.zoom_by(dz, (f32::from(e.position.x) - ox, f32::from(e.position.y) - oy), tau);
                    cx.notify();
                }
            }))
            .child(basemap)
            .children(markers)
            .child(attribution(&t))
            .into_any_element()
    }

    /// Vector tiles at floor(zoom) (512px each), labels and the route as one image (see `Raster`), over `under` and the backdrop.
    fn basemap(&mut self, t: &Theme, scale: f32, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let paths = Routes::of(self).lines(me(self));
        let on = self.s.conn == Conn::On;
        let route_color = if on { t.primary } else { t.muted_fg };
        let dark = t.dark;
        let style = if dark { &vtile::DARK } else { &vtile::LIGHT };
        let m = &mut self.map;
        let tz = m.zoom.floor().clamp(0., 14.) as u32;
        let n = 1i64 << tz;
        let world = m.world();
        let center = m.center;
        let full = (m.size.0 + 2. * OVERSCAN.0, m.size.1 + 2. * OVERSCAN.1);
        let left = center.0 - full.0 as f64 / 2. / world;
        let top = center.1 - full.1 as f64 / 2. / world;
        // Tile columns and rows at zoom z under the raster grown by `margin` px on each side.
        let range = |z: u32, margin: (f32, f32)| {
            let k = (1i64 << z) as f64;
            let (l, t) = (left - margin.0 as f64 / world, top - margin.1 as f64 / world);
            let (r, b) = (l + (full.0 + 2. * margin.0) as f64 / world, t + (full.1 + 2. * margin.1) as f64 / world);
            ((l * k).floor() as i64, (r * k).floor() as i64, ((t * k).floor() as i64).max(0), ((b * k).floor() as i64).min(k as i64 - 1))
        };
        let (x0, x1, y0, y1) = range(tz, (0., 0.));
        // (tile, x, y, size) in logical px from the raster's top-left, and whether it stands in for a missing tile.
        let mut placed: Vec<(Arc<VTile>, f32, f32, f32, bool)> = vec![];
        let mut stand_ins = std::collections::HashSet::new();
        let at = |z: u32, x: i64, y: i64| {
            let k = (1i64 << z) as f64;
            (((x as f64 / k - left) * world) as f32, ((y as f64 / k - top) * world) as f32, (world / k) as f32)
        };
        let mut wanted = vec![];
        for y in y0..=y1 {
            for x in x0..=x1 {
                let key = (tz, x.rem_euclid(n) as u32, y as u32);
                if let Some(Tile::Ready(d)) = m.tiles.get(&key) {
                    let (sx, sy, sz) = at(tz, x, y);
                    placed.push((d.clone(), sx, sy, sz, false));
                    continue;
                }
                wanted.push(key);
                // Until it arrives: the nearest loaded ancestor (the backdrop's tiles always are), else loaded children.
                let parent = (1..=tz).find_map(|k| {
                    let key = (tz - k, (x.rem_euclid(n) >> k) as u32, (y >> k) as u32);
                    match m.tiles.get(&key) {
                        Some(Tile::Ready(d)) => Some((k, d.clone())),
                        _ => None,
                    }
                });
                if let Some((k, d)) = parent {
                    let (px, py) = (x.div_euclid(1 << k), y >> k);
                    if stand_ins.insert((tz - k, px, py)) {
                        let (sx, sy, sz) = at(tz - k, px, py);
                        placed.push((d, sx, sy, sz, true));
                    }
                } else if tz < 14 {
                    for (cx2, cy2) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let key = (tz + 1, (x.rem_euclid(n) * 2 + cx2) as u32, (y * 2 + cy2) as u32);
                        if let Some(Tile::Ready(d)) = m.tiles.get(&key) {
                            let (sx, sy, sz) = at(tz + 1, x * 2 + cx2, y * 2 + cy2);
                            placed.push((d.clone(), sx, sy, sz, true));
                        }
                    }
                }
            }
        }
        let around = |z: u32, margin: (f32, f32)| {
            let (a, b, c, d) = range(z, margin);
            let k = 1i64 << z;
            let mid = (((center.0 * k as f64).floor() as i64).rem_euclid(k), (center.1 * k as f64).floor() as i64);
            let mut keys: Vec<(u32, u32, u32)> = (c..=d).flat_map(|y| (a..=b).map(move |x| (z, x.rem_euclid(k) as u32, y as u32))).collect();
            keys.sort_by_key(|t| (t.1 as i64 - mid.0).abs().min(k - (t.1 as i64 - mid.0).abs()) + (t.2 as i64 - mid.1).abs());
            keys
        };
        // The backdrop, the view (nearest the centre first), then what panning and zooming out will need next.
        let bn = 1u32 << BACKDROP_Z;
        let world_keys = (0..bn).flat_map(|y| (0..bn).map(move |x| (BACKDROP_Z, x, y)));
        let next = around(tz, (m.size.0 / 2., m.size.1 / 2.)).into_iter().chain(if tz > 0 { around(tz - 1, (0., 0.)) } else { vec![] });
        let mid = (((center.0 * n as f64).floor() as i64).rem_euclid(n), (center.1 * n as f64).floor() as i64);
        wanted.sort_by_key(|k| (k.1 as i64 - mid.0).abs().min(n - (k.1 as i64 - mid.0).abs()) + (k.2 as i64 - mid.1).abs());
        for key in world_keys.chain(wanted).chain(next) {
            if m.inflight >= 8 {
                break;
            }
            if m.tiles.contains_key(&key) {
                continue;
            }
            m.inflight += 1;
            m.tiles.insert(key, Tile::Loading);
            cx.spawn(async move |this, cx| {
                let data = cx.background_executor().spawn(async move { vtile::load(key.0, key.1, key.2) }).await;
                this.update(cx, |this, cx| {
                    this.map.inflight -= 1;
                    this.map.tiles_gen += 1;
                    this.map.tiles.insert(key, data.map(|d| Tile::Ready(Arc::new(d))).unwrap_or(Tile::Failed));
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }

        let (w, h) = ((full.0 * scale).round().max(1.) as u32, (full.1 * scale).round().max(1.) as u32);
        let key = format!("{:.6}/{:.9}/{:.9}/{w}x{h}/{dark}/{}/{on}/{paths:?}", m.zoom, center.0, center.1, m.tiles_gen);
        if m.job.is_none() && m.raster.as_ref().map(|r| &r.key) != Some(&key) {
            let zoom = m.zoom;
            let legs: Vec<_> = paths.iter().map(|(stops, main)| (route_legs(stops, center, world, full), *main)).collect();
            m.job = Some(cx.spawn(async move |this, cx| {
                let image = cx
                    .background_executor()
                    .spawn(async move {
                        let mut pix = tiny_skia::Pixmap::new(w, h).expect("map size");
                        let tiles: Vec<vtile::Placed> =
                            placed.iter().map(|(d, x, y, sz, fb)| vtile::Placed { tile: d, x: x * scale, y: y * scale, size: sz * scale, fallback: *fb }).collect();
                        vtile::rasterize(&mut pix, zoom, scale, style, &tiles, true);
                        // MapLibre: line-width 2, line-cap round, dasharray [2, 2] (in line widths) while off.
                        // Other routes first and fainter, the main connection on top.
                        for main in [false, true] {
                            let lines: Vec<Vec<(f32, f32)>> =
                                legs.iter().filter(|l| l.1 == main).flat_map(|l| l.0.iter()).map(|l| l.iter().map(|p| (p.0 * scale, p.1 * scale)).collect()).collect();
                            let color = if main { route_color } else { route_color.opacity(0.5) };
                            vtile::stroke_lines(&mut pix, &lines, 2. * scale, (!on).then(|| vec![4. * scale, 4. * scale]), true, color);
                        }
                        render_image(pix)
                    })
                    .await;
                this.update(cx, |this, cx| {
                    let m = &mut this.map;
                    m.job = None;
                    if let Some(old) = m.raster.replace(Raster { key, image, center, zoom, size: full })
                        && let Some(older) = m.under.replace(old)
                    {
                        m.retired.push((older.image, 0));
                    }
                    cx.notify();
                })
                .ok();
            }));
        }
        let world_tiles: Vec<(Arc<VTile>, u32, u32)> = (0..bn)
            .flat_map(|y| (0..bn).map(move |x| (x, y)))
            .filter_map(|(x, y)| match m.tiles.get(&(BACKDROP_Z, x, y)) {
                Some(Tile::Ready(d)) => Some((d.clone(), x, y)),
                _ => None,
            })
            .collect();
        let backdrop_key = format!("{dark}/{}", world_tiles.len());
        if m.backdrop_job.is_none() && !world_tiles.is_empty() && m.backdrop.as_ref().map(|r| &r.key) != Some(&backdrop_key) {
            let side = 512 * bn;
            m.backdrop_job = Some(cx.spawn(async move |this, cx| {
                let image = cx
                    .background_executor()
                    .spawn(async move {
                        let mut pix = tiny_skia::Pixmap::new(side, side).expect("backdrop size");
                        let tiles: Vec<vtile::Placed> =
                            world_tiles.iter().map(|(d, x, y)| vtile::Placed { tile: d, x: (x * 512) as f32, y: (y * 512) as f32, size: 512., fallback: false }).collect();
                        vtile::rasterize(&mut pix, BACKDROP_Z as f64, 1., style, &tiles, false);
                        render_image(pix)
                    })
                    .await;
                this.update(cx, |this, cx| {
                    let m = &mut this.map;
                    m.backdrop_job = None;
                    let size = (side as f32, side as f32);
                    if let Some(old) = m.backdrop.replace(Raster { key: backdrop_key, image, center: (0.5, 0.5), zoom: BACKDROP_Z as f64, size }) {
                        m.retired.push((old.image, 0));
                    }
                    cx.notify();
                })
                .ok();
            }));
        }
        for r in &mut m.retired {
            r.1 += 1;
        }
        for (old, _) in m.retired.extract_if(.., |r| r.1 > 3) {
            window.drop_image(old).ok();
        }
        if !m.retired.is_empty() {
            window.request_animation_frame();
        }
        // Where a raster's top-left corner and size land under the current camera, at the copy nearest the view.
        let place = |r: &Raster| {
            let rw = world_px(r.zoom);
            let k = world / rw;
            let ux = r.center.0 - r.size.0 as f64 / 2. / rw - (r.center.0 - center.0).round();
            let uy = r.center.1 - r.size.1 as f64 / 2. / rw;
            let (mut x, mut y) = (((ux - center.0) * world) as f32 + m.size.0 / 2., ((uy - center.1) * world) as f32 + m.size.1 / 2.);
            if (k - 1.).abs() < 1e-9 {
                (x, y) = ((x * scale).round() / scale, (y * scale).round() / scale);
            }
            (x, y, r.size.0 * k as f32, r.size.1 * k as f32)
        };
        let image = |r: &Raster, (x, y, w, h): (f32, f32, f32, f32)| img(ImageSource::Render(r.image.clone())).absolute().left(px(x)).top(px(y)).w(px(w)).h(px(h));
        let mut layers = div().absolute().inset_0().bg(gpui::rgb(style.background));
        // Far from its own zoom a raster is only a blur, and float precision suffers.
        if let Some(b) = &m.backdrop
            && m.zoom < 9.
        {
            let (x, y, w, h) = place(b);
            for j in -1..=1 {
                let x = x + j as f32 * w;
                if x < m.size.0 && x + w > 0. {
                    layers = layers.child(image(b, (x, y, w, h)));
                }
            }
        }
        if let Some(u) = m.under.as_ref().filter(|u| (u.zoom - m.zoom).abs() < 2.) {
            layers = layers.child(image(u, place(u)));
        }
        if let Some(r) = &m.raster {
            layers = layers.child(image(r, place(r)));
        }
        layers.into_any_element()
    }

    fn markers(&self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let routes = Routes::of(self);
        // The picked server last, so its card is above the other markers.
        // Servers without the chosen transport are hidden unless a route still goes through them.
        let offered = data::offered(self.s.protocol);
        let mut servers: Vec<_> = data::servers().into_iter().filter(|s| offered.iter().any(|o| o.id == s.id) || routes.uses(s.id)).collect();
        servers.sort_by_key(|s| self.map.pick == Some(s.id));
        let mut out: Vec<AnyElement> = servers.into_iter().map(|s| self.server_marker(s, &routes.riders_at(s.id), t, window, cx)).collect();
        if let Some(g) = self.s.me.as_ref().and_then(|m| m.geo.as_ref()) {
            let (x, y) = self.map.screen(unit(g.lat, g.lon));
            let local = |label: &str, color: Hsla, riders: &[Rider]| {
                (!riders.is_empty()).then(|| {
                    div().flex().items_center().gap(px(4.)).pl(px(4.)).border_l_1().border_color(t.border).child(div().text_color(color).child(label.to_owned())).child(badges(riders, label, t))
                })
            };
            out.push(
                div()
                    .absolute()
                    .left(px(x - 200.))
                    .top(px(y - 12.))
                    .w(px(400.))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h(px(24.))
                            .px(px(8.))
                            .rounded(px(MD))
                            .bg(t.card)
                            .shadow(t.panel_shadow())
                            .text_color(t.muted_fg)
                            .xs()
                            .whitespace_nowrap()
                            .gap(px(4.))
                            .child(t!("map.you"))
                            .children(local("direct", t.muted_fg, &routes.direct))
                            .children(local("blocked", t.destructive, &routes.blocked)),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    fn server_marker(&self, s: &'static Server, riders: &[Rider], t: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let selected = s.id == self.s.server_id;
        let entry = self.s.entry_id == Some(s.id);
        let picked = self.map.pick == Some(s.id);
        // Exit of another route: an app or site leaves the tunnel here.
        let exit = !selected && !riders.is_empty();
        let (x, y) = self.map.screen(unit(s.lat, s.lon));
        let t = *t;
        let mut dot = div().relative().size(px(24.)).flex_none().flex().items_center().justify_center();
        if selected && self.s.conn != Conn::Off {
            let on = self.s.conn == Conn::On;
            // The ring animates forever while connected. When nothing can cover it, the window's Shell draws it in its own
            // layer, so its frames do not re-render (and re-layout) the rest of the app.
            let (ox, oy, w, h) = self.ring_clip.get();
            let near = |r: &[f32; 4]| x + 24. > r[0] && x - 24. < r[0] + r[2] && y + 24. > r[1] && y - 24. < r[1] + r[3];
            if self.ring_isolated.get() && !self.keepout.borrow().iter().any(near) {
                self.ring.set(Some(Ring { at: (ox + x, oy + y), clip: (ox, oy, w, h), on }));
            } else {
                dot = dot.child(ping_ring(on, &t));
            }
        }
        let (outer, inner, ring, fill) = if selected {
            (20., 16., t.background, if self.s.conn == Conn::On { t.primary } else { t.foreground })
        } else if entry || exit || picked {
            (16., 12., t.foreground, t.background)
        } else {
            (16., 12., t.background, t.muted_fg)
        };
        let hover = t.foreground;
        let plain = !selected && !entry && !exit && !picked;
        dot = dot.child(
            div().size(px(outer)).rounded_full().bg(ring).flex().items_center().justify_center().child(
                div().size(px(inner)).rounded_full().bg(fill).when(plain, |d| d.group_hover("marker", move |s| s.bg(hover))),
            ),
        );
        let chip = (!picked && (selected || entry || exit)).then(|| {
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .h(px(28.))
                .px(px(8.))
                .rounded(px(MD))
                .bg(t.card)
                .shadow(t.panel_shadow())
                .xs()
                .whitespace_nowrap()
                .text_color(if selected { t.foreground } else { t.muted_fg })
                .when(selected, |d| d.medium())
                .child(flag(s.code, false, window))
                .child(if entry { t!("map.via", city = s.city) } else { s.city.to_owned() })
                .when(!riders.is_empty(), |d| d.child(badges(riders, s.id, &t).ml(px(2.))))
        });
        let id = s.id;
        let card = if picked { Some(self.pick_card(s, &t, window, cx)) } else { None };
        let anchored_left = chip.is_some() || card.is_some();
        // Labels in the right part of the map open to the left of their dot, so they stay on screen.
        let flip = anchored_left && x > self.map.size.0 * 0.6;
        div()
            .absolute()
            .top(px(y))
            .when(flip, |d| d.right(px(self.map.size.0 - x)))
            .when(!flip, |d| d.left(px(x)))
            .child(
                div()
                    .id(id)
                    .group("marker")
                    .relative()
                    .flex()
                    .when(flip, |d| d.flex_row_reverse())
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .when(flip, |d| d.left(px(12.)).top(px(if picked { -18. } else { -14. })))
                    .when(anchored_left && !flip, |d| d.left(px(-12.)).top(px(if picked { -18. } else { -14. })))
                    .when(!anchored_left, |d| d.left(px(-12.)).top(px(-12.)))
                    // Not a drag of the map, and the card stays open.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| this.pick(id, cx)))
                    .child(dot)
                    .children(chip)
                    .children(card),
            )
            .into_any_element()
    }
}

impl Hitori {
    /// A click on a marker: the card to confirm moving there; the live exit itself needs none.
    fn pick(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let live = id == self.s.server_id && self.s.conn != Conn::Off;
        self.map.pick = if live || self.map.pick == Some(id) { None } else { Some(id) };
        cx.notify();
    }

    /// Flag, city and latency of the picked server, with Connect (or Switch, while connected) and close.
    fn pick_card(&self, s: &'static Server, t: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let id = s.id;
        let ping = match data::ping(id) {
            Some(ms) => Some(format!("{ms} ms")),
            None if data::timed_out(id) => Some(t!("views.servers.offline").to_owned()),
            None => None,
        };
        let label = if self.s.conn == Conn::Off { t!("map.connect") } else { t!("map.switch") };
        div()
            .id(SharedString::from(format!("pick-{id}")))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(36.))
            .pl(px(8.))
            .pr(px(4.))
            .rounded(px(MD))
            .bg(t.card)
            .shadow(t.panel_shadow())
            .xs()
            .whitespace_nowrap()
            .cursor_default()
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(flag(s.code, false, window))
            .child(div().medium().text_color(t.foreground).child(s.city.to_owned()))
            .children(ping.map(|p| div().text_color(t.muted_fg).child(p)))
            .child(
                button(SharedString::from(format!("pick-go-{id}")), Variant::Default, Size::Xs, false, t)
                    .ml(px(4.))
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.go(id, window, cx);
                    })),
            )
            .child(
                button(SharedString::from(format!("pick-x-{id}")), Variant::Ghost, Size::IconXs, true, t)
                    .tooltip(tip(t!("map.cancel"), t))
                    .child(icon("x", button_icon(Size::IconXs), t.muted_fg))
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.map.pick = None;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
}

/// `animate-ping` (connected) or `animate-pulse` (connecting) around the selected marker's dot.
pub fn ping_ring(on: bool, t: &Theme) -> impl IntoElement {
    div().absolute().inset_0().rounded_full().bg(t.primary.opacity(0.3)).with_animation(
        ("ring", on as usize),
        Animation::new(Duration::from_secs(if on { 1 } else { 2 })).repeat(),
        move |d, p| {
            if on {
                // animate-ping: scale 1 → 2 and fade out by 75%, cubic-bezier(0, 0, .2, 1).
                let q = (p / 0.75).min(1.);
                let k = 1. - (1. - q).powi(3);
                let grow = 12. * k;
                d.top(px(-grow)).left(px(-grow)).right(px(-grow)).bottom(px(-grow)).opacity(1. - k)
            } else {
                let tri = if p < 0.5 { p * 2. } else { (1. - p) * 2. };
                d.opacity(1. - 0.5 * tri)
            }
        },
    )
}

/// The selected marker's dot, as `server_marker` draws it.
pub fn selected_dot(on: bool, t: &Theme) -> impl IntoElement {
    div().size(px(20.)).rounded_full().bg(t.background).flex().items_center().justify_center().child(div().size(px(16.)).rounded_full().bg(if on { t.primary } else { t.foreground }))
}

/// MapLibre's compact attribution: links in muted text, the rest in the control's (black) text colour.
fn attribution(t: &Theme) -> impl IntoElement {
    let link = t.muted_fg;
    let plain: Hsla = gpui::black();
    div()
        .absolute()
        .right(px(10.))
        .bottom(px(10.))
        .h(px(24.))
        .flex()
        .items_center()
        .text_px(10., 20.)
        .child(div().flex().px(px(4.)).child(div().text_color(plain).child("© ")).child(div().text_color(link).child("OpenStreetMap")).child(div().text_color(plain).child(" contributors © ")).child(div().text_color(link).child("CARTO")))
        .child(div().size(px(24.)).rounded_full().bg(gpui::black().opacity(0.05)).flex().items_center().justify_center().child(div().text_px(12., 12.).bold().text_color(plain).child("i")))
}

/// You → entry → exit as great-circle polylines in px of a raster of `size` centred on `center`, at every world copy it shows.
fn route_legs(stops: &[(f64, f64)], center: (f64, f64), world: f64, size: (f32, f32)) -> Vec<Vec<(f32, f32)>> {
    let half = size.0 as f64 / 2. / world;
    stops
        .windows(2)
        .flat_map(|w| {
            let mut prev = unit(w[0].0, w[0].1).0;
            let pts: Vec<(f64, f64)> = arc(w[0], w[1], 32)
                .into_iter()
                .map(|(la, lo)| {
                    let mut u = unit(la, lo);
                    u.0 -= (u.0 - prev).round();
                    prev = u.0;
                    u
                })
                .collect();
            let (a, b) = pts.iter().fold((f64::MAX, f64::MIN), |(a, b), u| (a.min(u.0), b.max(u.0)));
            copies(a, b, center.0, half)
                .map(|k| pts.iter().map(|u| (((u.0 + k as f64 - center.0) * world + size.0 as f64 / 2.) as f32, ((u.1 - center.1) * world + size.1 as f64 / 2.) as f32)).collect())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A finished pixmap as an image GPUI can draw (BGRA).
fn render_image(pix: tiny_skia::Pixmap) -> Arc<RenderImage> {
    let (w, h) = (pix.width(), pix.height());
    let mut rgba = pix.take();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::from_raw(w, h, rgba).expect("raster"))]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(size: (f32, f32), center: (f64, f64), zoom: f64) -> MapState {
        MapState { size, center, zoom, ..MapState::new() }
    }

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01
    }

    #[test]
    fn wheel_zoom_keeps_the_point_under_the_cursor() {
        let mut m = map((1000., 800.), (0.55, 0.4), 3.);
        let at = (150., 620.);
        let u = m.unproject(at);
        m.zoom_by(1., at, WHEEL_TAU);
        let t0 = m.tick;
        for i in 1..=60 {
            m.step(t0 + Duration::from_millis(16 * i));
            assert!(close(m.screen(u), at), "frame {i}: {:?} drifted from {at:?}", m.screen(u));
        }
        assert_eq!(m.zoom, 4.);
        assert!(m.zooming.is_none());
    }

    #[test]
    fn zoom_easing_ignores_frame_rate() {
        let run = |frame_ms: u64| {
            let mut m = map((1000., 800.), (0.5, 0.5), 3.);
            m.zoom_by(1.5, (500., 400.), WHEEL_TAU);
            let t0 = m.tick;
            for i in 1..=(96 / frame_ms) {
                m.step(t0 + Duration::from_millis(frame_ms * i));
            }
            m.zoom
        };
        let (fast, slow) = (run(8), run(32));
        assert!((fast - slow).abs() < 1e-9, "{fast} vs {slow}");
        assert!(fast > 3. && fast < 4.5);
    }

    #[test]
    fn zoom_is_clamped_so_the_world_fills_the_view() {
        let size = (1400., 900.);
        let mut m = map(size, (0.5, 0.5), 3.);
        m.zoom_by(-20., (700., 450.), WHEEL_TAU);
        assert_eq!(m.zooming.as_ref().unwrap().to, min_zoom(size));
        while m.step(m.tick + Duration::from_millis(16)) {}
        let world = m.world();
        assert!(world >= 1400. - 1e-6);
        assert!(m.center.1 - 450. / world >= -1e-9 && m.center.1 + 450. / world <= 1. + 1e-9);
        m.zoom_by(100., (700., 450.), WHEEL_TAU);
        assert_eq!(m.zooming.as_ref().unwrap().to, MAX_USER_ZOOM);
    }

    #[test]
    fn camera_stays_inside_the_poles_and_wraps_sideways() {
        let size = (1024., 600.);
        assert_eq!(min_zoom(size), 1.);
        assert_eq!(constrain((0.3, 0.), 1., size), (0.3, 300. / 1024.));
        assert_eq!(constrain((0.3, 1.), 1., size), (0.3, 1. - 300. / 1024.));
        assert_eq!(constrain((1.25, 0.5), 1., size).0, 0.25);
        assert_eq!(constrain((-0.25, 0.5), 1., size).0, 0.75);
        let mut m = map(size, (0.98, 0.5), 2.);
        let start = (gpui::point(px(0.), px(0.)), m.center);
        m.pan(start, gpui::point(px(-100.), px(-5000.)));
        assert!((m.center.0 - (0.98 + 100. / 2048. - 1.)).abs() < 1e-12);
        assert_eq!(m.center.1, 1. - 300. / 2048.);
    }

    #[test]
    fn points_use_the_copy_nearest_the_view() {
        let m = map((1000., 800.), (0.95, 0.5), 2.);
        let (x, _) = m.screen((0.05, 0.5));
        assert!((x - (500. + 0.1 * 2048.)).abs() < 0.01);
        let (x, _) = m.screen((0.85, 0.5));
        assert!((x - (500. - 0.1 * 2048.)).abs() < 0.01);
    }

    #[test]
    fn copies_cover_the_view() {
        assert_eq!(copies(0.9, 1.1, 0., 0.2), -1..=-1);
        assert_eq!(copies(0.9, 1.1, 0.5, 0.6), -1..=0);
        assert!(copies(0.2, 0.3, 0.5, 0.1).is_empty());
    }

    #[test]
    fn spans_take_the_short_way_round() {
        let s = span(&[0.9, 0.05]);
        assert!((s.0 - 0.9).abs() < 1e-12 && (s.1 - 1.05).abs() < 1e-12);
        assert_eq!(span(&[0.2, 0.4, 0.3]), (0.2, 0.4));
        assert_eq!(span(&[0.5]), (0.5, 0.5));
    }

    #[test]
    fn routes_cross_the_antimeridian_unbroken() {
        let (tokyo, la) = ((35.7, 139.7), (34.05, -118.2));
        let world = world_px(2.);
        let size = (1500., 1100.);
        for center in [(0., 0.4), (0.5, 0.4), (0.8, 0.4)] {
            for leg in route_legs(&[tokyo, la], center, world, size) {
                assert!(leg.windows(2).all(|w| (w[1].0 - w[0].0).abs() < 0.05 * world as f32), "a leg jumps across the map at {center:?}");
                assert!(leg.iter().any(|p| p.0 > -0.25 * world as f32 && p.0 < size.0 + 0.25 * world as f32));
            }
        }
        let m = map((1200., 800.), (0.5, 0.5), 3.);
        let ((cx, _), zoom) = m.fit(&[tokyo, la], &Padding { top: 64., bottom: 64., left: 64., right: 64. });
        assert!(wrap(cx).abs() < 0.1, "fit centred at {cx}, not over the Pacific");
        assert!(zoom >= min_zoom(m.size));
    }
}
