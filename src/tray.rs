//! System tray icon (StatusNotifierItem over D-Bus) and the popup window it opens.
use std::time::{Duration, Instant};

use gpui::{App, AppContext, Bounds, Entity, Global, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowOptions, px, size};
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};

use crate::app::{Conn, Hitori, Shell, Variant, Vpn};
use crate::data::{self, server};

pub const POPUP: (f32, f32) = (400., 720.);

/// Tray events, sent from ksni's D-Bus thread to the GPUI main thread.
pub enum Msg {
    /// Icon clicked; screen position of the click when the tray host reports one.
    TogglePopup(i32, i32),
    OpenMain,
    Power,
    Go(&'static str),
    Quit,
}

pub struct TrayIcon {
    tx: async_channel::Sender<Msg>,
    conn: Conn,
    server_id: &'static str,
    icons: [Vec<ksni::Icon>; 3],
}

/// Lucide shield (as in the app) rasterised into SNI ARGB32 pixmaps: blue protected, red unprotected, grey connecting.
fn icons() -> [Vec<ksni::Icon>; 3] {
    let shapes = [
        ("shield-check", "#51a2ff"),
        ("shield-alert", "#ff6467"),
        ("shield-check", "#9f9fa9"),
    ];
    shapes.map(|(name, color)| {
        let svg = crate::Assets::get(&format!("icons/{name}.svg")).map(|f| String::from_utf8_lossy(&f.data).replace("stroke=\"black\"", &format!("stroke=\"{color}\""))).unwrap_or_default();
        let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()).expect("icon svg");
        [22u32, 32, 48, 64]
            .into_iter()
            .map(|n| {
                let mut pix = tiny_skia::Pixmap::new(n, n).unwrap();
                let k = n as f32 / 24.;
                resvg::render(&tree, tiny_skia::Transform::from_scale(k, k), &mut pix.as_mut());
                let mut data = Vec::with_capacity((n * n * 4) as usize);
                for p in pix.pixels() {
                    let c = p.demultiply();
                    data.extend_from_slice(&[c.alpha(), c.red(), c.green(), c.blue()]);
                }
                ksni::Icon { width: n as i32, height: n as i32, data }
            })
            .collect()
    })
}

impl ksni::Tray for TrayIcon {
    fn id(&self) -> String {
        "hitori".into()
    }
    fn title(&self) -> String {
        "Hitori VPN".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons[match self.conn {
            Conn::On => 0,
            Conn::Off => 1,
            Conn::Connecting => 2,
        }]
        .clone()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "Hitori VPN".into(), description: self.status(), ..Default::default() }
    }
    fn activate(&mut self, x: i32, y: i32) {
        self.tx.send_blocking(Msg::TogglePopup(x, y)).ok();
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let send = |m: fn() -> Msg| Box::new(move |t: &mut Self| drop(t.tx.send_blocking(m())));
        vec![
            StandardItem { label: self.status(), enabled: false, ..Default::default() }.into(),
            StandardItem { label: if self.conn == Conn::Off { "Connect".into() } else { "Disconnect".into() }, activate: send(|| Msg::Power), ..Default::default() }.into(),
            SubMenu {
                label: "Location".into(),
                submenu: vec![
                    RadioGroup {
                        selected: data::servers().iter().position(|s| s.id == self.server_id).unwrap_or(0),
                        select: Box::new(|t: &mut Self, i| {
                            if let Some(s) = data::servers().get(i) {
                                drop(t.tx.send_blocking(Msg::Go(s.id)));
                            }
                        }),
                        options: data::servers().iter().map(|s| RadioItem { label: format!("{} · {}", s.country, s.city), ..Default::default() }).collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem { label: "Open Hitori".into(), activate: send(|| Msg::OpenMain), ..Default::default() }.into(),
            StandardItem { label: "Quit".into(), activate: send(|| Msg::Quit), ..Default::default() }.into(),
        ]
    }
}

impl TrayIcon {
    fn status(&self) -> String {
        let s = server(self.server_id);
        match self.conn {
            Conn::On => format!("Protected · {}", s.country),
            Conn::Connecting => format!("Connecting to {}…", s.country),
            Conn::Off => "Unprotected".into(),
        }
    }
}

/// Open windows, so the tray can toggle the popup and bring the main window back.
#[derive(Default)]
pub struct Windows {
    pub vpn: Option<Entity<Vpn>>,
    pub main: Option<WindowHandle<Shell>>,
    pub popup: Option<WindowHandle<Shell>>,
    /// Clicking the tray icon first unfocuses (and closes) the popup; don't reopen it from that same click.
    pub popup_closed: Option<Instant>,
}

impl Global for Windows {}

/// Where the tray popover goes: the overlay window's size and the card's position inside it.
pub struct Popover {
    size: (f32, f32),
    card: (f32, f32),
}

pub fn open_window(variant: Variant, popover: Option<Popover>, cx: &mut App) -> Option<WindowHandle<Shell>> {
    let vpn = cx.global::<Windows>().vpn.clone()?;
    let forced = std::env::var("HITORI_SIZE").ok().and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
    });
    let tray = variant == Variant::Tray;
    let (w, h) = popover.as_ref().map(|p| p.size).or(forced).unwrap_or(if tray { POPUP } else { (1440., 900.) });
    let card = popover.map(|p| p.card);
    let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions { title: Some("Hitori VPN".into()), ..Default::default() }),
            app_id: Some(if tray { "hitori-tray" } else { "hitori" }.into()),
            window_background: if card.is_some() { WindowBackgroundAppearance::Transparent } else { WindowBackgroundAppearance::Opaque },
            ..Default::default()
        },
        move |window, cx| {
            let hitori = cx.new(|cx| {
                let mut h = Hitori::new(variant, vpn, window, cx);
                h.popover = card.is_some();
                h
            });
            cx.new(|cx| {
                if tray {
                    // A tray popup closes when it loses focus.
                    cx.observe_window_activation(window, |_, window, cx| {
                        if !window.is_window_active() {
                            cx.global_mut::<Windows>().popup_closed = Some(Instant::now());
                            window.remove_window();
                        }
                    })
                    .detach();
                }
                Shell { hitori, card }
            })
        },
    )
    .ok()
}

/// Hyprland has no way for a client to place its window, so ask the compositor: a borderless transparent window
/// over the monitor's work area, with the card next to the click, against the bar. Covering the work area keeps
/// focus-follows-mouse from moving focus away while the pointer travels. Other compositors get a normal window.
fn place_popup(click: (i32, i32)) -> Option<Popover> {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let run = |args: &[&str]| std::process::Command::new("hyprctl").args(args).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let (cx, cy) = if click != (0, 0) {
        (click.0 as f32, click.1 as f32)
    } else {
        let pos = run(&["cursorpos"])?;
        let mut it = pos.split(',').filter_map(|v| v.trim().parse::<f32>().ok());
        let (Some(x), Some(y)) = (it.next(), it.next()) else { return None };
        (x, y)
    };
    let monitors: serde_json::Value = run(&["monitors", "-j"]).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
    let m = monitors.as_array().and_then(|ms| ms.iter().find(|m| m["focused"].as_bool() == Some(true)).or(ms.first())).cloned()?;
    let f = |k: &str| m[k].as_f64().unwrap_or(0.) as f32;
    let scale = f("scale").max(1.);
    let (mx, my, mw, mh) = (f("x"), f("y"), f("width") / scale, f("height") / scale);
    let reserved: Vec<f32> = m["reserved"].as_array().map(|r| r.iter().map(|v| v.as_f64().unwrap_or(0.) as f32).collect()).unwrap_or_default();
    let (rt, rb) = (reserved.get(1).copied().unwrap_or(0.), reserved.get(3).copied().unwrap_or(0.));
    let (w, h) = POPUP;
    let (aw, ah) = (mw, mh - rt - rb);
    let x = (cx - mx - w / 2.).clamp(8., (mw - w - 8.).max(8.));
    let y = if cy - my < mh / 2. { 8. } else { ah - h - 8. };
    let rule = format!(
        "if _G.hitori_tray then _G.hitori_tray:set_enabled(false) end; _G.hitori_tray = hl.window_rule({{ name = \"hitori-tray\", match = {{ class = \"^hitori-tray$\" }}, float = true, pin = true, no_anim = true, no_shadow = true, no_blur = true, border_size = 0, rounding = 0, size = \"{aw} {ah}\", move = \"{} {}\" }})",
        mx.round(),
        (my + rt).round()
    );
    run(&["eval", &rule])?;
    Some(Popover { size: (aw, ah), card: (x.round(), y.round()) })
}

fn handle(msg: Msg, cx: &mut App) {
    let vpn = cx.global::<Windows>().vpn.clone();
    match msg {
        Msg::TogglePopup(x, y) => {
            let w = cx.global::<Windows>();
            let open = w.popup.and_then(|p| p.update(cx, |_, _, _| ()).ok()).is_some();
            if open {
                if let Some(p) = cx.global::<Windows>().popup {
                    p.update(cx, |_, window, _| window.remove_window()).ok();
                }
            } else if !cx.global::<Windows>().popup_closed.is_some_and(|t| t.elapsed() < Duration::from_millis(400)) {
                let popover = place_popup((x, y));
                let handle = open_window(Variant::Tray, popover, cx);
                cx.global_mut::<Windows>().popup = handle;
            }
        }
        Msg::OpenMain => {
            let shown = cx.global::<Windows>().main.and_then(|m| m.update(cx, |_, window, _| window.activate_window()).ok()).is_some();
            if !shown {
                let handle = open_window(Variant::Main, None, cx);
                cx.global_mut::<Windows>().main = handle;
            }
        }
        Msg::Power => {
            if let Some(v) = vpn {
                v.update(cx, |v, cx| v.toggle(cx));
            }
        }
        Msg::Go(id) => {
            if let Some(v) = vpn {
                v.update(cx, |v, cx| v.go(id, cx));
            }
        }
        Msg::Quit => cx.quit(),
    }
}

/// Start the tray icon; returns false when there is no StatusNotifier host (then the app quits with its last window).
pub fn start(vpn: &Entity<Vpn>, cx: &mut App) -> bool {
    let (tx, rx) = async_channel::unbounded();
    let s = vpn.read(cx).s.clone();
    let icon = TrayIcon { tx, conn: s.conn, server_id: s.server_id, icons: icons() };
    let tray = match icon.spawn() {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("hitori: no system tray ({e})");
            return false;
        }
    };
    cx.observe(vpn, move |vpn, cx| {
        let s = vpn.read(cx).s.clone();
        tray.update(|t| {
            t.conn = s.conn;
            t.server_id = s.server_id;
        });
    })
    .detach();
    cx.spawn(async move |cx| {
        while let Ok(msg) = rx.recv().await {
            if cx.update(|cx| handle(msg, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
    true
}
