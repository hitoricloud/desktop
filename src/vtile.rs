//! CARTO vector tiles drawn with the label-free part of the dark-matter / positron styles.
use std::io::Read;

use std::path::PathBuf;
use std::sync::LazyLock;

use geo_types::{Geometry, LineString, Polygon};
use gpui::{Hsla, Rgba, rgb};
use tiny_skia as sk;
use mvt_reader::{Reader, feature::Value};

pub const URL: &str = "https://tiles.basemaps.cartocdn.com/v/20261026-01/vectortiles/carto.streets/v2";

type Line = Vec<(f32, f32)>;

/// One tile's geometry in tile-relative units (0..1, with the tile buffer outside).
#[derive(Default)]
pub struct VTile {
    water: Vec<Vec<Line>>,
    rivers: Vec<Line>,
    regions: Vec<Line>,
    countries: Vec<Line>,
    labels: Vec<Label>,
}

impl VTile {
    const EMPTY: VTile = VTile { water: vec![], rivers: vec![], regions: vec![], countries: vec![], labels: vec![] };
}

#[derive(Clone, Copy, PartialEq)]
enum LabelKind {
    Country,
    Continent,
}

/// A point from the `places` layer that one of the style's label layers shows.
struct Label {
    at: (f32, f32),
    kind: LabelKind,
    name: String,
    min_zoom: f64,
    rank: f64,
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Int(i) | Value::SInt(i) => Some(*i as f64),
        Value::UInt(u) => Some(*u as f64),
        Value::Float(f) => Some(*f as f64),
        Value::Double(d) => Some(*d),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn text(v: Option<&Value>) -> &str {
    match v {
        Some(Value::String(s)) => s,
        _ => "",
    }
}

fn line(l: &LineString<f32>, extent: f32) -> Line {
    l.0.iter().map(|c| (c.x / extent, c.y / extent)).collect()
}

fn rings(p: &Polygon<f32>, extent: f32) -> Vec<Line> {
    std::iter::once(p.exterior()).chain(p.interiors()).map(|r| line(r, extent)).collect()
}

fn lines(g: &Geometry<f32>, extent: f32) -> Vec<Line> {
    match g {
        Geometry::LineString(l) => vec![line(l, extent)],
        Geometry::MultiLineString(m) => m.0.iter().map(|l| line(l, extent)).collect(),
        _ => vec![],
    }
}

fn cache_path(z: u32, x: u32, y: u32) -> Option<PathBuf> {
    let version = URL.split('/').find(|p| p.starts_with("20")).unwrap_or("v");
    Some(crate::store::cache_dir().join("tiles").join(version).join(format!("{z}/{x}/{y}.mvt")))
}

/// Download (or read from the disk cache), gunzip and decode one tile; only what the visible layers need is kept.
pub fn load(z: u32, x: u32, y: u32) -> Option<VTile> {
    let path = cache_path(z, x, y);
    let gz = match path.as_ref().and_then(|p| std::fs::read(p).ok()) {
        Some(b) => b,
        None => {
            let b = crate::net::bytes(&format!("{URL}/{z}/{x}/{y}.mvt"))?;
            if let Some(p) = &path {
                let _ = std::fs::create_dir_all(p.parent().unwrap());
                let tmp = p.with_extension("part");
                if std::fs::write(&tmp, &b).is_ok() {
                    let _ = std::fs::rename(tmp, p);
                }
            }
            b
        }
    };
    let mut raw = Vec::new();
    if gz.starts_with(&[0x1f, 0x8b]) {
        flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut raw).ok()?;
    } else {
        raw = gz;
    }
    let reader = Reader::new(raw).ok()?;
    let mut tile = VTile::default();
    for layer in reader.get_layer_metadata().ok()? {
        let extent = layer.extent as f32;
        let features = match layer.name.as_str() {
            "water" | "boundaries" | "places" => reader.get_features(layer.layer_index).ok()?,
            _ => continue,
        };
        for f in features {
            let props = f.properties.unwrap_or_default();
            let kind = text(props.get("kind"));
            match (layer.name.as_str(), &f.geometry) {
                ("water", Geometry::Polygon(p)) => tile.water.push(rings(p, extent)),
                ("water", Geometry::MultiPolygon(m)) => tile.water.extend(m.0.iter().map(|p| rings(p, extent))),
                ("water", g) if kind == "river" => tile.rivers.extend(lines(g, extent)),
                ("places", g) if kind == "country" || kind == "continent" => {
                    let pt = match g {
                        Geometry::Point(p) => *p,
                        Geometry::MultiPoint(m) if !m.0.is_empty() => m.0[0],
                        _ => continue,
                    };
                    // The style's text-field: name:en, else name with its alternate-script parts.
                    let mut name = text(props.get("name:en")).to_owned();
                    if name.is_empty() {
                        name = ["name", "name2", "name3"].iter().map(|k| text(props.get(*k))).filter(|n| !n.is_empty()).collect::<Vec<_>>().join(" ");
                    }
                    if !name.is_empty() {
                        tile.labels.push(Label {
                            at: (pt.x() / extent, pt.y() / extent),
                            kind: if kind == "country" { LabelKind::Country } else { LabelKind::Continent },
                            name,
                            min_zoom: num(props.get("min_zoom")).unwrap_or(20.),
                            rank: num(props.get("population_rank")).unwrap_or(0.),
                        });
                    }
                }
                ("boundaries", g) => {
                    let detail = num(props.get("kind_detail"));
                    if detail == Some(2.) {
                        tile.countries.extend(lines(g, extent));
                    }
                    if kind == "macroregion" {
                        tile.regions.extend(lines(g, extent));
                    }
                }
                _ => {}
            }
        }
    }
    Some(tile)
}

/// MapLibre `stops` interpolation (base 1).
fn stops(z: f64, s: &[(f64, f64)]) -> f64 {
    if z <= s[0].0 {
        return s[0].1;
    }
    for w in s.windows(2) {
        if z <= w[1].0 {
            return w[0].1 + (w[1].1 - w[0].1) * (z - w[0].0) / (w[1].0 - w[0].0);
        }
    }
    s[s.len() - 1].1
}

fn mix(z: f64, s: &[(f64, u32)]) -> Hsla {
    let pick = |c: u32, k: u32| ((c >> k) & 0xff) as f64;
    if z <= s[0].0 {
        return rgb(s[0].1).into();
    }
    for w in s.windows(2) {
        if z <= w[1].0 {
            let t = (z - w[0].0) / (w[1].0 - w[0].0);
            let c = |k| (pick(w[0].1, k) + (pick(w[1].1, k) - pick(w[0].1, k)) * t).round() as u32;
            return rgb((c(16) << 16) | (c(8) << 8) | c(0)).into();
        }
    }
    rgb(s[s.len() - 1].1).into()
}

pub struct Style {
    pub background: u32,
    /// place_country_1, place_country_2 and place_continent text colours, and the text halo.
    country1: &'static [(f64, u32)],
    country2: &'static [(f64, u32)],
    continent: u32,
    halo: u32,
    water: u32,
    river: u32,
    region: u32,
    region_dash: bool,
    country: &'static [(f64, u32)],
}

pub const DARK: Style = Style {
    background: 0x0e0e0e,
    country1: &[(3., 0x9eb6bd), (5., 0x767e89), (6., 0x788d93)],
    country2: &[(3., 0x9d9d9d), (5., 0x727272), (6., 0x707070)],
    continent: 0x87a4b3,
    halo: 0x111111, water: 0x2c353c, river: 0x3f5a6d, region: 0x676772, region_dash: true, country: &[(4., 0x5c5e5e), (5., 0x606060), (6., 0x666666)] };
pub const LIGHT: Style = Style {
    background: 0xfafaf8,
    country1: &[(3., 0x8a99a4), (5., 0xa1adb6), (6., 0xb9c2c9)],
    country2: &[(3., 0x8a99a4), (5., 0xa1adb6), (6., 0xb9c2c9)],
    continent: 0x697b89,
    halo: 0xfafaf8, water: 0xd4dadc, river: 0xd1dbdf, region: 0xead5d7, region_dash: false, country: &[(4., 0xf2e6e7), (5., 0xebd6d8), (6., 0xebd6d8)] };

/// A tile placed in the raster: origin and size in device pixels.
pub struct Placed<'a> {
    pub tile: &'a VTile,
    pub x: f32,
    pub y: f32,
    pub size: f32,
    /// A parent or child tile standing in for one that is still loading.
    pub fallback: bool,
}

fn paint_of(c: Hsla) -> sk::Paint<'static> {
    let c = Rgba::from(c);
    let mut p = sk::Paint::default();
    p.set_color(sk::Color::from_rgba(c.r, c.g, c.b, c.a).unwrap_or(sk::Color::BLACK));
    p.anti_alias = true;
    p
}

fn path_of(ls: &[(f32, f32)], p: &Placed, close: bool, b: &mut sk::PathBuilder) {
    b.move_to(p.x + ls[0].0 * p.size, p.y + ls[0].1 * p.size);
    for c in &ls[1..] {
        b.line_to(p.x + c.0 * p.size, p.y + c.1 * p.size);
    }
    if close {
        b.close();
    }
}

/// Strokes polylines given in raster pixels.
pub fn stroke_lines(pix: &mut sk::Pixmap, lines: &[Vec<(f32, f32)>], width: f32, dash: Option<Vec<f32>>, round: bool, color: Hsla) {
    let placed = Placed { tile: &VTile::EMPTY, x: 0., y: 0., size: 1., fallback: false };
    let mut b = sk::PathBuilder::new();
    for l in lines.iter().filter(|l| l.len() > 1) {
        path_of(l, &placed, false, &mut b);
    }
    let Some(path) = b.finish() else { return };
    let mut st = sk::Stroke { width, line_join: sk::LineJoin::Round, ..Default::default() };
    if round {
        st.line_cap = sk::LineCap::Round;
    }
    st.dash = dash.and_then(|d| sk::StrokeDash::new(d, 0.));
    pix.stroke_path(&path, &paint_of(color), &st, sk::Transform::identity(), None);
}

fn stroke(pix: &mut sk::Pixmap, ls: &[Line], p: &Placed, width: f32, dash: Option<Vec<f32>>, color: Hsla) {
    let lines: Vec<Vec<(f32, f32)>> = ls.iter().map(|l| l.iter().map(|c| (p.x + c.0 * p.size, p.y + c.1 * p.size)).collect()).collect();
    stroke_lines(pix, &lines, width, dash, false, color);
}

/// Layer order and paint follow the style: background, water, waterway, boundary_state, boundary_country_inner,
/// then the country and continent labels. `scale` is the device pixel ratio; tile positions are in device pixels.
/// Stand-in tiles are drawn first and covered wherever a real tile is ready.
pub fn rasterize(pix: &mut sk::Pixmap, zoom: f64, scale: f32, style: &Style, tiles: &[Placed]) {
    let bg = Rgba::from(Hsla::from(rgb(style.background)));
    pix.fill(sk::Color::from_rgba(bg.r, bg.g, bg.b, 1.).unwrap());
    let (stand_in, ready): (Vec<&Placed>, Vec<&Placed>) = tiles.iter().partition(|p| p.fallback);
    layers(pix, zoom, scale, style, &stand_in);
    if !stand_in.is_empty() {
        let mut paint = paint_of(rgb(style.background).into());
        paint.anti_alias = false;
        for p in &ready {
            if let Some(r) = sk::Rect::from_xywh(p.x, p.y, p.size, p.size) {
                pix.fill_rect(r, &paint, sk::Transform::identity(), None);
            }
        }
    }
    layers(pix, zoom, scale, style, &ready);
    labels(pix, zoom, scale, style, tiles);
}

fn layers(pix: &mut sk::Pixmap, zoom: f64, scale: f32, style: &Style, tiles: &[&Placed]) {
    let water = paint_of(rgb(style.water).into());
    for p in tiles {
        for poly in &p.tile.water {
            let mut b = sk::PathBuilder::new();
            for r in poly.iter().filter(|r| r.len() > 2) {
                path_of(r, p, true, &mut b);
            }
            if let Some(path) = b.finish() {
                pix.fill_path(&path, &water, sk::FillRule::EvenOdd, sk::Transform::identity(), None);
            }
        }
    }
    for p in tiles {
        stroke(pix, &p.tile.rivers, p, 0.5 * scale, None, rgb(style.river).into());
    }
    if zoom >= 4. {
        let w = stops(zoom, &[(4., 0.5), (7., 1.)]) as f32 * scale;
        let dash = style.region_dash.then(|| [1., 2., 3., 1., 2., 3.].iter().map(|d| d * w).collect());
        for p in tiles {
            stroke(pix, &p.tile.regions, p, w, dash.clone(), rgb(style.region).into());
        }
    }
    let w = stops(zoom, &[(3., 1.), (6., 1.5)]) as f32 * scale;
    let color = mix(zoom, style.country);
    for p in tiles {
        stroke(pix, &p.tile.countries, p, w, None, color);
    }
}

/// The style's label font, as MapLibre would fetch it from CARTO's glyph server.
static FONT: LazyLock<Vec<u8>> = LazyLock::new(|| crate::Assets::get("fonts/Montserrat-SemiBold.ttf").map(|f| f.data.into_owned()).unwrap_or_default());

struct Outline<'a> {
    b: &'a mut sk::PathBuilder,
    x: f32,
    y: f32,
    k: f32,
}

impl ttf_parser::OutlineBuilder for Outline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.b.move_to(self.x + x * self.k, self.y - y * self.k);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.b.line_to(self.x + x * self.k, self.y - y * self.k);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.b.quad_to(self.x + x1 * self.k, self.y - y1 * self.k, self.x + x * self.k, self.y - y * self.k);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.b.cubic_to(self.x + x1 * self.k, self.y - y1 * self.k, self.x + x2 * self.k, self.y - y2 * self.k, self.x + x * self.k, self.y - y * self.k);
    }
    fn close(&mut self) {
        self.b.close();
    }
}

/// Splits words into lines no wider than `max`, balancing line widths like MapLibre's line breaker.
fn break_lines(words: &[(String, f32)], space: f32, max: f32) -> Vec<(String, f32)> {
    let total: f32 = words.iter().map(|w| w.1).sum::<f32>() + space * (words.len().saturating_sub(1)) as f32;
    if total <= max || words.len() < 2 {
        return vec![(words.iter().map(|w| w.0.as_str()).collect::<Vec<_>>().join(" "), total)];
    }
    let n = (total / max).ceil().max(2.) as usize;
    let target = total / n as f32;
    // Every way to cut the words into lines (labels have only a few words); keep the most even one.
    let cuts = words.len() - 1;
    let mut best: Option<(f32, Vec<(String, f32)>)> = None;
    for mask in 0u32..(1 << cuts.min(12)) {
        let mut lines: Vec<(Vec<&str>, f32)> = vec![(vec![], 0.)];
        for (i, w) in words.iter().enumerate() {
            let line = lines.last_mut().unwrap();
            if !line.0.is_empty() {
                line.1 += space;
            }
            line.0.push(&w.0);
            line.1 += w.1;
            if i < cuts && mask & (1 << i) != 0 {
                lines.push((vec![], 0.));
            }
        }
        let cost: f32 = lines.iter().map(|l| (l.1 - target).powi(2) + if l.1 > max { 1e6 } else { 0. }).sum::<f32>() + lines.len() as f32 * 1e3;
        if best.as_ref().is_none_or(|b| cost < b.0) {
            best = Some((cost, lines.into_iter().map(|l| (l.0.join(" "), l.1)).collect()));
        }
    }
    best.unwrap().1
}

/// place_continent (z < 2), place_country_1 (2 ≤ z < 7, min_zoom ≤ 3) and place_country_2 (3 ≤ z < 10, min_zoom ≥ 4):
/// upper-case Montserrat SemiBold with a 1px halo, placed in symbol-sort-key order, skipping any that would collide.
fn labels(pix: &mut sk::Pixmap, zoom: f64, scale: f32, style: &Style, tiles: &[Placed]) {
    let Ok(face) = ttf_parser::Face::parse(&FONT, 0) else { return };
    let tz = zoom.floor();
    let mut seen = std::collections::HashSet::new();
    let mut cands: Vec<(f64, &Label, f32, f32)> = vec![];
    for p in tiles {
        for l in &p.tile.labels {
            let shown = match l.kind {
                LabelKind::Continent => zoom < 2.,
                LabelKind::Country => (zoom >= 2. && zoom < 7. && l.min_zoom <= 3.) || (zoom >= 3. && zoom < 10. && l.min_zoom >= 4.),
            };
            if shown && seen.insert(&l.name) {
                cands.push((l.min_zoom * 100. - l.rank, l, p.x + l.at.0 * p.size, p.y + l.at.1 * p.size));
            }
        }
    }
    cands.sort_by(|a, b| a.0.total_cmp(&b.0));
    let upem = face.units_per_em() as f32;
    let cap = face.capital_height().unwrap_or((upem * 0.7) as i16) as f32 / upem;
    let mut boxes: Vec<sk::Rect> = vec![];
    let halo = paint_of(rgb(style.halo).into());
    for (_, l, x, y) in cands {
        let first = l.kind == LabelKind::Country && l.min_zoom <= 3.;
        let (size, max_em, spacing, color) = match l.kind {
            LabelKind::Continent => (14., 9., 0.1, rgb(style.continent).into()),
            LabelKind::Country if first => (stops(zoom, &[(3., 11.), (4., 12.), (5., 13.), (6., 14.)]), stops(tz, &[(2., 6.), (3., 6.), (4., 9.), (5., 12.)]), 0., mix(zoom, style.country1)),
            LabelKind::Country => (stops(zoom, &[(3., 10.), (5., 11.), (6., 12.), (7., 13.), (8., 14.)]), 10., 0., mix(zoom, style.country2)),
        };
        let px = size as f32 * scale;
        let k = px / upem;
        let track = spacing as f32 * px;
        let advance = |c: char| face.glyph_index(c).and_then(|g| face.glyph_hor_advance(g)).unwrap_or(0) as f32 * k + track;
        let upper = l.name.to_uppercase();
        let words: Vec<(String, f32)> = upper.split_whitespace().map(|w| (w.to_owned(), w.chars().map(advance).sum::<f32>() - track)).collect();
        let lines = break_lines(&words, advance(' '), max_em as f32 * px);
        let lh = 1.2 * px;
        let width = lines.iter().map(|l| l.1).fold(0., f32::max);
        let height = lh * lines.len() as f32;
        let pad = 2. * scale;
        let Some(bx) = sk::Rect::from_xywh(x - width / 2. - pad, y - height / 2. - pad, width + 2. * pad, height + 2. * pad) else { continue };
        if boxes.iter().any(|b| b.intersect(&bx).is_some()) {
            continue;
        }
        boxes.push(bx);
        let mut b = sk::PathBuilder::new();
        for (i, (line, w)) in lines.iter().enumerate() {
            let mut gx = x - w / 2.;
            let base = y - height / 2. + lh * (i as f32 + 0.5) + cap * px / 2.;
            for c in line.chars() {
                if let Some(g) = face.glyph_index(c) {
                    face.outline_glyph(g, &mut Outline { b: &mut b, x: gx, y: base, k });
                }
                gx += advance(c);
            }
        }
        let Some(path) = b.finish() else { continue };
        let st = sk::Stroke { width: 2. * scale, line_join: sk::LineJoin::Round, ..Default::default() };
        pix.stroke_path(&path, &halo, &st, sk::Transform::identity(), None);
        pix.fill_path(&path, &paint_of(color), sk::FillRule::Winding, sk::Transform::identity(), None);
    }
}
