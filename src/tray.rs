//! System tray icon and the popup window it opens: StatusNotifierItem over D-Bus on Linux, the native tray (Windows
//! notification area, macOS menu bar) elsewhere.
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext, Bounds, Entity, Global, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, point, px, size,
};

use crate::app::{Conn, Hitori, Shell, Variant, Vpn};
use crate::data::server;
use crate::views::MIN_SIZE;

#[cfg(target_os = "linux")]
pub use linux::start;
#[cfg(not(target_os = "linux"))]
pub use native::start;

pub const POPUP: (f32, f32) = (400., 720.);

/// Tray events, sent from the tray's thread to the GPUI main thread.
#[derive(Clone)]
pub enum Msg {
    /// Icon clicked; screen position of the click when the tray host reports one.
    TogglePopup(i32, i32),
    OpenMain,
    Power,
    Go(&'static str),
    Quit,
}

/// The shield as in the app, rasterised: blue protected, red unprotected, grey connecting; straight RGBA per size.
fn icons(sizes: &[u32]) -> [Vec<(u32, Vec<u8>)>; 3] {
    let shapes = [("shield-check", "#51a2ff"), ("shield-alert", "#ff6467"), ("shield-check", "#9f9fa9")];
    shapes.map(|(name, color)| {
        let svg = crate::Assets::get(&format!("icons/{name}.svg")).map(|f| String::from_utf8_lossy(&f.data).replace("stroke=\"black\"", &format!("stroke=\"{color}\""))).unwrap_or_default();
        let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()).expect("icon svg");
        sizes
            .iter()
            .map(|&n| {
                let mut pix = tiny_skia::Pixmap::new(n, n).unwrap();
                let k = n as f32 / 24.;
                resvg::render(&tree, tiny_skia::Transform::from_scale(k, k), &mut pix.as_mut());
                let rgba = pix.pixels().iter().flat_map(|p| {
                    let c = p.demultiply();
                    [c.red(), c.green(), c.blue(), c.alpha()]
                });
                (n, rgba.collect())
            })
            .collect()
    })
}

fn icon_index(conn: Conn) -> usize {
    match conn {
        Conn::On => 0,
        Conn::Off => 1,
        Conn::Connecting => 2,
    }
}

fn status(conn: Conn, server_id: &str) -> String {
    let s = server(server_id);
    match conn {
        Conn::On => format!("Protected · {}", s.country),
        Conn::Connecting => format!("Connecting to {}…", s.country),
        Conn::Off => "Unprotected".into(),
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

/// Where the tray popover goes: the overlay window's size and the card's position inside it, and where the window goes
/// when the app may place it (Windows, macOS).
pub struct Popover {
    size: (f32, f32),
    card: Card,
    origin: Option<(f32, f32)>,
}

/// The card in a popover window: `x` from the left, `gap` from the top edge, or from the bottom one against a bottom bar.
#[derive(Clone, Copy)]
pub struct Card {
    x: f32,
    gap: f32,
    bottom: bool,
}

impl Card {
    /// The card's bounds (x, y, w, h) in a window of this size: the popup's size, shrunk and moved to stay inside.
    pub fn place(self, (vw, vh): (f32, f32)) -> (f32, f32, f32, f32) {
        let (w, h) = (POPUP.0.min(vw - 2. * self.gap).max(0.), POPUP.1.min(vh - 2. * self.gap).max(0.));
        let y = if self.bottom { vh - h - self.gap } else { self.gap };
        (self.x.min(vw - w - self.gap).max(0.), y, w, h)
    }
}

/// The display's logical size. Wayland has no primary display: there the smallest output, so the window fits on any.
fn screen(cx: &App) -> Option<(f32, f32)> {
    let size = |b: Bounds<gpui::Pixels>| (f32::from(b.size.width), f32::from(b.size.height));
    cx.primary_display()
        .map(|d| size(d.bounds()))
        .or_else(|| cx.displays().iter().map(|d| size(d.bounds())).min_by(|a, b| (a.0 * a.1).total_cmp(&(b.0 * b.1))))
}

/// Windows and macOS: a borderless card at the tray. The taskbar or menu bar side is guessed from where the click was.
#[cfg(not(target_os = "linux"))]
fn native_popup(click: (i32, i32), cx: &App) -> Option<Popover> {
    let display = cx.primary_display()?.bounds();
    let (dw, dh) = (f32::from(display.size.width), f32::from(display.size.height));
    // Short screens get a shorter card (its rule list scrolls) between the menu bar or taskbar and the other edge.
    let (w, h) = (POPUP.0, POPUP.1.min(dh - 72.));
    // The click is in physical pixels; without the display's scale only which half of the screen it was in is used.
    let low = click.1 as f32 > dh / 2. || cfg!(windows);
    let x = (dw - w - 12.).max(0.);
    let y = if low { (dh - h - 60.).max(0.) } else { 32. };
    let card = Card { x: 0., gap: 0., bottom: false };
    Some(Popover { size: (w, h), card, origin: Some((f32::from(display.origin.x) + x, f32::from(display.origin.y) + y)) })
}

pub fn open_window(variant: Variant, popover: Option<Popover>, cx: &mut App) -> Option<WindowHandle<Shell>> {
    let vpn = cx.global::<Windows>().vpn.clone()?;
    let forced = std::env::var("HITORI_SIZE").ok().and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
    });
    let tray = variant == Variant::Tray;
    let screen = screen(cx);
    // The main window's minimum, unless the screen itself is smaller.
    let min = screen.map_or(MIN_SIZE, |(sw, sh)| (MIN_SIZE.0.min(sw), MIN_SIZE.1.min(sh)));
    let (w, h) = popover.as_ref().map(|p| p.size).or(forced).unwrap_or_else(|| {
        // Small screens: windows fit inside 90% of the display (the cockpit narrows below 1024 px, the tray's list scrolls).
        let (w, h) = if tray { POPUP } else { (1440., 900.) };
        let (w, h) = screen.map_or((w, h), |(sw, sh)| (w.min(sw * 0.9).floor(), h.min(sh * 0.9).floor()));
        if tray { (w, h) } else { (w.max(min.0), h.max(min.1)) }
    });
    // HITORI_SIZE may ask for less (screenshots).
    let min = (min.0.min(w), min.1.min(h));
    let origin = popover.as_ref().and_then(|p| p.origin);
    let card = popover.map(|p| p.card);
    let bounds = match origin {
        Some((x, y)) => Bounds { origin: point(px(x), px(y)), size: size(px(w), px(h)) },
        None => Bounds::centered(None, size(px(w), px(h)), cx),
    };
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // A placed popup is just the card: no title bar, above other windows.
            titlebar: if origin.is_some() { None } else { Some(TitlebarOptions { title: Some("Hitori VPN".into()), ..Default::default() }) },
            kind: if origin.is_some() { WindowKind::PopUp } else { WindowKind::Normal },
            // Tray windows keep their size: the popup is a fixed card, the popover overlay covers the work area.
            is_resizable: !tray,
            window_min_size: (!tray).then(|| size(px(min.0), px(min.1))),
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
    .inspect_err(|e| eprintln!("hitori: cannot open a window: {e:#}"))
    .ok()
}

/// Hyprland has no way for a client to place its window, so ask the compositor: a borderless transparent window
/// over the monitor's work area, with the card next to the click, against the bar. Covering the work area keeps
/// focus-follows-mouse from moving focus away while the pointer travels. Other compositors get a normal window.
#[cfg(target_os = "linux")]
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
    let w = POPUP.0;
    // Whole logical pixels: the window rule and the fixed size (min == max) must agree.
    let (aw, ah) = (mw.floor(), (mh - rt - rb).floor());
    let x = (cx - mx - w / 2.).clamp(8., (mw - w - 8.).max(8.));
    let bottom = cy - my >= mh / 2.;
    let rule = format!(
        "if _G.hitori_tray then _G.hitori_tray:set_enabled(false) end; _G.hitori_tray = hl.window_rule({{ name = \"hitori-tray\", match = {{ class = \"^hitori-tray$\" }}, float = true, pin = true, no_anim = true, no_shadow = true, no_blur = true, border_size = 0, rounding = 0, size = \"{aw} {ah}\", move = \"{} {}\" }})",
        mx.round(),
        (my + rt).round()
    );
    run(&["eval", &rule])?;
    Some(Popover { size: (aw, ah), card: Card { x: x.round(), gap: 8., bottom }, origin: None })
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
                #[cfg(target_os = "linux")]
                let popover = place_popup((x, y));
                #[cfg(not(target_os = "linux"))]
                let popover = native_popup((x, y), cx);
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

/// Run tray messages on the GPUI main thread.
fn listen(rx: async_channel::Receiver<Msg>, cx: &mut App) {
    cx.spawn(async move |cx| {
        while let Ok(msg) = rx.recv().await {
            if cx.update(|cx| handle(msg, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

#[cfg(target_os = "linux")]
mod linux {
    use gpui::{App, Entity};
    use ksni::blocking::TrayMethods;
    use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};

    use super::{Msg, icon_index, icons, listen, status};
    use crate::app::{Conn, Vpn};
    use crate::data;

pub struct TrayIcon {
    tx: async_channel::Sender<Msg>,
    conn: Conn,
    server_id: &'static str,
    icons: [Vec<ksni::Icon>; 3],
}

/// SNI wants ARGB32 pixmaps.
fn sni_icons() -> [Vec<ksni::Icon>; 3] {
    icons(&[22, 32, 48, 64]).map(|sizes| {
        sizes
            .into_iter()
            .map(|(n, rgba)| ksni::Icon { width: n as i32, height: n as i32, data: rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect() })
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
        self.icons[icon_index(self.conn)].clone()
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
        status(self.conn, self.server_id)
    }
}

/// Start the tray icon; returns false when there is no StatusNotifier host (then the app quits with its last window).
pub fn start(vpn: &Entity<Vpn>, cx: &mut App) -> bool {
    let (tx, rx) = async_channel::unbounded();
    let s = vpn.read(cx).s.clone();
    let icon = TrayIcon { tx, conn: s.conn, server_id: s.server_id, icons: sni_icons() };
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
    listen(rx, cx);
    true
}
}

#[cfg(not(target_os = "linux"))]
mod native {
    //! `tray-icon`: left click toggles the popup, right click (Windows) or any click on the menu (macOS) shows the menu.
    use std::collections::HashMap;

    use gpui::{App, Entity, Global};
    use tray_icon::menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use super::{Msg, icon_index, icons, listen, status};
    use crate::app::{Conn, Vpn};
    use crate::data;

    /// The icon and what each menu item does; rebuilt when the state it shows changes.
    struct Tray {
        icon: TrayIcon,
        icons: [Icon; 3],
        actions: HashMap<MenuId, Msg>,
        shown: (Conn, &'static str, usize),
    }

    impl Global for Tray {}

    fn menu(conn: Conn, server_id: &'static str) -> (Menu, HashMap<MenuId, Msg>) {
        let mut actions = HashMap::new();
        let menu = Menu::new();
        let item = |text: &str, msg: Option<Msg>, actions: &mut HashMap<MenuId, Msg>| {
            let item = MenuItem::new(text, msg.is_some(), None);
            if let Some(m) = msg {
                actions.insert(item.id().clone(), m);
            }
            item
        };
        let _ = menu.append(&item(&status(conn, server_id), None, &mut actions));
        let _ = menu.append(&item(if conn == Conn::Off { "Connect" } else { "Disconnect" }, Some(Msg::Power), &mut actions));
        let places: Vec<CheckMenuItem> = data::servers()
            .into_iter()
            .map(|s| {
                let item = CheckMenuItem::new(format!("{} · {}", s.country, s.city), true, s.id == server_id, None);
                actions.insert(item.id().clone(), Msg::Go(s.id));
                item
            })
            .collect();
        let refs: Vec<&dyn IsMenuItem> = places.iter().map(|p| p as &dyn IsMenuItem).collect();
        if let Ok(sub) = Submenu::with_items("Location", !places.is_empty(), &refs) {
            let _ = menu.append(&sub);
        }
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&item("Open Hitori", Some(Msg::OpenMain), &mut actions));
        let _ = menu.append(&item("Quit", Some(Msg::Quit), &mut actions));
        (menu, actions)
    }

    /// Start the tray icon; returns false when the platform has none (the app then quits with its last window).
    pub fn start(vpn: &Entity<Vpn>, cx: &mut App) -> bool {
        let icons = icons(&[32]).map(|mut v| {
            let (n, rgba) = v.remove(0);
            Icon::from_rgba(rgba, n, n).expect("tray icon")
        });
        let s = vpn.read(cx).s.clone();
        let (m, actions) = menu(s.conn, s.server_id);
        let icon = match TrayIconBuilder::new()
            .with_icon(icons[icon_index(s.conn)].clone())
            .with_tooltip(status(s.conn, s.server_id))
            .with_menu(Box::new(m))
            .with_menu_on_left_click(false)
            .build()
        {
            Ok(icon) => icon,
            Err(e) => {
                eprintln!("hitori: no system tray ({e})");
                return false;
            }
        };
        cx.set_global(Tray { icon, icons, actions, shown: (s.conn, s.server_id, data::servers().len()) });

        let (tx, rx) = async_channel::unbounded();
        let (menu_tx, menu_rx) = async_channel::unbounded::<MenuId>();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, position, .. } = e {
                tx.send_blocking(Msg::TogglePopup(position.x as i32, position.y as i32)).ok();
            }
        }));
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| drop(menu_tx.send_blocking(e.id))));
        listen(rx, cx);
        cx.spawn(async move |cx| {
            while let Ok(id) = menu_rx.recv().await {
                let gone = cx
                    .update(|cx| {
                        let msg = cx.global::<Tray>().actions.get(&id).cloned();
                        if let Some(msg) = msg {
                            super::handle(msg, cx);
                        }
                    })
                    .is_err();
                if gone {
                    break;
                }
            }
        })
        .detach();
        cx.observe(vpn, |vpn, cx| {
            let s = vpn.read(cx).s.clone();
            let now = (s.conn, s.server_id, data::servers().len());
            let tray = cx.global_mut::<Tray>();
            if tray.shown == now {
                return;
            }
            if tray.shown.0 != s.conn {
                let _ = tray.icon.set_icon(Some(tray.icons[icon_index(s.conn)].clone()));
            }
            let _ = tray.icon.set_tooltip(Some(status(s.conn, s.server_id)));
            let (m, actions) = menu(s.conn, s.server_id);
            tray.icon.set_menu(Some(Box::new(m)));
            tray.actions = actions;
            tray.shown = now;
        })
        .detach();
        true
    }
}
