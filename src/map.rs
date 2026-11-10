//! A small slippy map with MapLibre's camera maths, drawing CARTO's label-free raster tiles.
use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt, AnyElement, Context, Hsla, ImageSource, MouseButton, Pixels, Point, RenderImage, ScrollDelta, Window, div, img,
    prelude::*, px,
};

use crate::app::{Conn, Hitori, Ring};
use crate::data::{self, RuleMode, Server, SiteRule, server};
use crate::theme::Theme;
use crate::ui::{MD, Tw, flag, tip};
use crate::views::{app_badge, site_badge};
use crate::vtile::{self, VTile};

const FIT_MS: f64 = 900.;
const MAX_ZOOM: f64 = 5.;
const MIN_ZOOM: f64 = 1.;

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

pub struct MapState {
    center: (f64, f64),
    zoom: f64,
    ease: Option<Ease>,
    framed: String,
    size: (f32, f32),
    tiles: HashMap<(u32, u32, u32), Tile>,
    /// Bumped when a tile arrives, so the raster key changes.
    tiles_gen: u64,
    /// The basemap, labels and route for one camera, rasterized off the main thread and drawn as one image.
    /// While the camera moves, the newest raster is scaled and shifted on the GPU until the next one is ready.
    raster: Option<Raster>,
    job: Option<gpui::Task<()>>,
    /// Replaced rasters, freed a few frames later: the GPU may still be drawing them.
    retired: Vec<(Arc<RenderImage>, u8)>,
    inflight: usize,
    drag: Option<(Point<Pixels>, (f64, f64))>,
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
const OVERSCAN: f32 = 128.;

fn unit(lat: f64, lon: f64) -> (f64, f64) {
    let s = (lat.clamp(-85.05, 85.05) * PI / 180.).sin();
    ((lon + 180.) / 360., 0.5 - ((1. + s) / (1. - s)).ln() / (4. * PI))
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
        Self { center: unit(52., 30.), zoom: 3., ease: None, framed: String::new(), size: (0., 0.), tiles: HashMap::new(), tiles_gen: 0, raster: None, job: None, retired: vec![], inflight: 0, drag: None }
    }

    /// Theme changed: tiles of the other style are no longer drawn.
    pub fn restyle(&mut self) {}

    pub fn refit(&mut self) {
        self.framed.clear();
    }

    fn world(&self) -> f64 {
        512. * 2f64.powf(self.zoom)
    }

    fn screen(&self, u: (f64, f64)) -> (f32, f32) {
        let world = self.world();
        let mut dx = u.0 - self.center.0;
        dx -= dx.round();
        ((dx * world + self.size.0 as f64 / 2.) as f32, ((u.1 - self.center.1) * world + self.size.1 as f64 / 2.) as f32)
    }

    /// `fitBounds(bounds, { padding, maxZoom: 5 })`.
    fn fit(&self, points: &[(f64, f64)], pad: &Padding) -> ((f64, f64), f64) {
        let us: Vec<_> = points.iter().map(|p| unit(p.0, p.1)).collect();
        let (x0, x1) = us.iter().fold((f64::MAX, f64::MIN), |(a, b), u| (a.min(u.0), b.max(u.0)));
        let (y0, y1) = us.iter().fold((f64::MAX, f64::MIN), |(a, b), u| (a.min(u.1), b.max(u.1)));
        let (w, h) = (self.size.0 as f64, self.size.1 as f64);
        let sx = (w - (pad.left + pad.right) as f64) / ((x1 - x0) * 512.);
        let sy = (h - (pad.top + pad.bottom) as f64) / ((y1 - y0) * 512.);
        let zoom = sx.min(sy).log2().clamp(MIN_ZOOM, MAX_ZOOM);
        let world = 512. * 2f64.powf(zoom);
        let cx = (x0 + x1) / 2. + ((pad.right - pad.left) as f64 / 2.) / world;
        let cy = (y0 + y1) / 2. + ((pad.bottom - pad.top) as f64 / 2.) / world;
        ((cx, cy), zoom)
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
        let mut r = Routes { paths: vec![Path { hops: main.clone(), main: true, riders: vec![] }], direct: vec![], blocked: vec![] };
        if !s.all_routes {
            return r;
        }
        let put = |r: &mut Routes, hops: Vec<&'static str>, who: Rider| match r.paths.iter_mut().find(|p| p.hops == hops) {
            Some(p) => p.riders.push(who),
            None => r.paths.push(Path { hops, main: false, riders: vec![who] }),
        };
        for (a, rule) in &s.app_routes {
            if !data::active(a) {
                continue;
            }
            match rule.mode {
                RuleMode::Block => r.blocked.push(Rider::App(a)),
                RuleMode::Direct => r.direct.push(Rider::App(a)),
                RuleMode::Vpn | RuleMode::Auto => {
                    let exit = rule.exit.filter(|e| data::has_server(e)).unwrap_or(s.server_id);
                    let via = rule.via.or(if rule.exit.is_some() { None } else { entry }).filter(|v| *v != exit && data::has_server(v));
                    put(&mut r, via.into_iter().chain([exit]).collect(), Rider::App(a));
                }
            }
        }
        for (site, rule) in &s.sites {
            match rule {
                SiteRule::Vpn => put(&mut r, main.clone(), Rider::Site(site)),
                SiteRule::Direct => r.direct.push(Rider::Site(site)),
            }
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
            let to = m.fit(&points, &pad);
            m.ease = Some(Ease { from: (m.center, m.zoom), to, start: Instant::now() });
        }
        if let Some(e) = &m.ease {
            let p = (e.start.elapsed().as_secs_f64() * 1000. / FIT_MS).min(1.);
            let k = ease(p);
            m.center = (e.from.0.0 + (e.to.0.0 - e.from.0.0) * k, e.from.0.1 + (e.to.0.1 - e.from.0.1) * k);
            m.zoom = e.from.1 + (e.to.1 - e.from.1) * k;
            if p >= 1. {
                m.ease = None;
            } else {
                window.request_animation_frame();
            }
        }

        let basemap = self.basemap(&t, scale, window, cx);
        let markers = self.markers(&t, window, cx);
        div()
            .id("map")
            .absolute()
            .inset_0()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &gpui::MouseDownEvent, _, _| {
                this.map.ease = None;
                this.map.drag = Some((e.position, this.map.center));
            }))
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                if let (Some((start, c)), true) = (this.map.drag, e.dragging()) {
                    let world = this.map.world();
                    let d = e.position - start;
                    this.map.center = (c.0 - f64::from(f32::from(d.x)) / world, (c.1 - f64::from(f32::from(d.y)) / world).clamp(0., 1.));
                    cx.notify();
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.map.drag = None))
            .on_scroll_wheel(cx.listener(|this, e: &gpui::ScrollWheelEvent, _, cx| {
                let dy = match e.delta {
                    ScrollDelta::Pixels(p) => f32::from(p.y) as f64 / 100.,
                    ScrollDelta::Lines(l) => l.y as f64 / 2.,
                };
                this.map.ease = None;
                this.map.zoom = (this.map.zoom + dy).clamp(MIN_ZOOM, 18.);
                cx.notify();
            }))
            .child(basemap)
            .children(markers)
            .child(attribution(&t))
            .into_any_element()
    }

    /// Vector tiles at floor(zoom) (512px each), labels and the route, as one image (see `Raster`).
    fn basemap(&mut self, t: &Theme, scale: f32, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let paths = Routes::of(self).lines(me(self));
        let on = self.s.conn == Conn::On;
        let route_color = if on { t.primary } else { t.muted_fg };
        let dark = t.dark;
        let m = &mut self.map;
        let tz = m.zoom.floor().clamp(0., 14.) as u32;
        let n = 1i64 << tz;
        let world = m.world();
        let full = (m.size.0 + 2. * OVERSCAN, m.size.1 + 2. * OVERSCAN);
        let left = m.center.0 - full.0 as f64 / 2. / world;
        let top = m.center.1 - full.1 as f64 / 2. / world;
        let (x0, x1) = ((left * n as f64).floor() as i64, ((left + full.0 as f64 / world) * n as f64).floor() as i64);
        let (y0, y1) = (((top * n as f64).floor() as i64).max(0), (((top + full.1 as f64 / world) * n as f64).floor() as i64).min(n - 1));
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
                // Until it arrives: the nearest loaded ancestor, else whichever children are loaded.
                let parent = (1..=tz.min(4)).find_map(|k| {
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
        // Load what the view needs, nearest the centre first.
        let mid = (((m.center.0 * n as f64).floor() as i64).rem_euclid(n), (m.center.1 * n as f64).floor() as i64);
        wanted.sort_by_key(|k| (k.1 as i64 - mid.0).abs().min(n - (k.1 as i64 - mid.0).abs()) + (k.2 as i64 - mid.1).abs());
        for key in wanted {
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
        let key = format!("{:.6}/{:.9}/{:.9}/{w}x{h}/{dark}/{}/{on}/{paths:?}", m.zoom, m.center.0, m.center.1, m.tiles_gen);
        if m.job.is_none() && m.raster.as_ref().map(|r| &r.key) != Some(&key) {
            let (center, zoom) = (m.center, m.zoom);
            let legs: Vec<_> = paths.iter().map(|(stops, main)| (route_legs(stops, center, world, full), *main)).collect();
            m.job = Some(cx.spawn(async move |this, cx| {
                let image = cx
                    .background_executor()
                    .spawn(async move {
                        let mut pix = tiny_skia::Pixmap::new(w, h).expect("map size");
                        let tiles: Vec<vtile::Placed> =
                            placed.iter().map(|(d, x, y, sz, fb)| vtile::Placed { tile: d, x: x * scale, y: y * scale, size: sz * scale, fallback: *fb }).collect();
                        vtile::rasterize(&mut pix, zoom, scale, if dark { &vtile::DARK } else { &vtile::LIGHT }, &tiles);
                        // MapLibre: line-width 2, line-cap round, dasharray [2, 2] (in line widths) while off.
                        // Other routes first and fainter, the main connection on top.
                        for main in [false, true] {
                            let lines: Vec<Vec<(f32, f32)>> =
                                legs.iter().filter(|l| l.1 == main).flat_map(|l| l.0.iter()).map(|l| l.iter().map(|p| (p.0 * scale, p.1 * scale)).collect()).collect();
                            let color = if main { route_color } else { route_color.opacity(0.5) };
                            vtile::stroke_lines(&mut pix, &lines, 2. * scale, (!on).then(|| vec![4. * scale, 4. * scale]), true, color);
                        }
                        let mut rgba = pix.take();
                        for px in rgba.chunks_exact_mut(4) {
                            px.swap(0, 2);
                        }
                        Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::from_raw(w, h, rgba).expect("raster"))]))
                    })
                    .await;
                this.update(cx, |this, cx| {
                    let m = &mut this.map;
                    m.job = None;
                    if let Some(old) = m.raster.replace(Raster { key, image, center, zoom, size: full }) {
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
        let bg = gpui::rgb(if dark { vtile::DARK.background } else { vtile::LIGHT.background });
        let Some(r) = &m.raster else { return div().absolute().inset_0().bg(bg).into_any_element() };
        // Where the raster's top-left corner and size land under the current camera.
        let rw = 512. * 2f64.powf(r.zoom);
        let k = world / rw;
        let mut ux = r.center.0 - r.size.0 as f64 / 2. / rw;
        ux -= (r.center.0 - m.center.0).round();
        let uy = r.center.1 - r.size.1 as f64 / 2. / rw;
        let (mut x, mut y) = (((ux - m.center.0) * world) as f32 + m.size.0 / 2., ((uy - m.center.1) * world) as f32 + m.size.1 / 2.);
        if (k - 1.).abs() < 1e-9 {
            (x, y) = ((x * scale).round() / scale, (y * scale).round() / scale);
        }
        div()
            .absolute()
            .inset_0()
            .bg(bg)
            .child(img(ImageSource::Render(r.image.clone())).absolute().left(px(x)).top(px(y)).w(px(r.size.0 * k as f32)).h(px(r.size.1 * k as f32)))
            .into_any_element()
    }

    fn markers(&self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let routes = Routes::of(self);
        let mut out: Vec<AnyElement> = data::servers().into_iter().map(|s| self.server_marker(s, &routes.riders_at(s.id), t, window, cx)).collect();
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
                            .child("You")
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
        } else if entry || exit {
            (16., 12., t.foreground, t.background)
        } else {
            (16., 12., t.background, t.muted_fg)
        };
        let hover = t.foreground;
        let plain = !selected && !entry && !exit;
        dot = dot.child(
            div().size(px(outer)).rounded_full().bg(ring).flex().items_center().justify_center().child(
                div().size(px(inner)).rounded_full().bg(fill).when(plain, |d| d.group_hover("marker", move |s| s.bg(hover))),
            ),
        );
        let chip = (selected || entry || exit).then(|| {
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
                .child(if entry { format!("via {}", s.city) } else { s.city.to_owned() })
                .when(!riders.is_empty(), |d| d.child(badges(riders, s.id, &t).ml(px(2.))))
        });
        let id = s.id;
        let anchored_left = chip.is_some();
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
                    .when(flip, |d| d.left(px(12.)).top(px(-14.)))
                    .when(anchored_left && !flip, |d| d.left(px(-12.)).top(px(-14.)))
                    .when(!anchored_left, |d| d.left(px(-12.)).top(px(-12.)))
                    .on_click(cx.listener(move |this, _, window, cx| this.go(id, window, cx)))
                    .child(dot)
                    .children(chip),
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

/// You → entry → exit as great-circle polylines, in logical px of a raster of `size` centred on `center`.
fn route_legs(stops: &[(f64, f64)], center: (f64, f64), world: f64, size: (f32, f32)) -> Vec<Vec<(f32, f32)>> {
    stops
        .windows(2)
        .map(|w| {
            let pts: Vec<(f64, f64)> = arc(w[0], w[1], 32).into_iter().map(|(la, lo)| unit(la, lo)).collect();
            let mut prev = pts[0].0;
            pts.into_iter()
                .map(|mut u| {
                    u.0 -= (u.0 - prev).round();
                    prev = u.0;
                    u
                })
                .map(|u| {
                    let mut dx = u.0 - center.0;
                    if (dx - dx.round()).abs() < dx.abs() && dx.abs() > 0.5 {
                        dx -= dx.round();
                    }
                    ((dx * world + size.0 as f64 / 2.) as f32, ((u.1 - center.1) * world + size.1 as f64 / 2.) as f32)
                })
                .collect()
        })
        .collect()
}
