#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#[macro_use]
mod i18n;
mod app;
mod apps;
mod auth;
mod data;
#[cfg(not(target_os = "linux"))]
mod elevated;
// The macOS helper's text handling, tested everywhere.
#[cfg(all(test, not(target_os = "macos")))]
#[path = "elevated/darwin.rs"]
mod darwin;
mod helper;
mod store;
mod sub;
mod xray;
mod fx;
mod input;
mod ipc;
mod live;
mod map;
mod net;
mod onboarding;
mod overlays;
mod palette;
mod region;
mod score;
mod suggest;
mod os;
mod sysproxy;
mod theme;
mod tray;
#[cfg(target_os = "linux")]
mod tun;
mod ui;
mod views;
mod vpn;
mod vtile;

use std::borrow::Cow;

use gpui::{App, Application, AssetSource, Bounds, SharedString, WindowBounds, WindowOptions, prelude::*, px, size};
use rust_embed::RustEmbed;

use app::Variant;

#[derive(RustEmbed)]
#[folder = "assets"]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(Self::get(path).map(|f| f.data))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Self::iter().filter(|p| p.starts_with(path)).map(|p| p.to_string().into()).collect())
    }
}

#[cfg(target_os = "macos")]
gpui::actions!(hitori, [Quit]);

/// Text calibration: the same strings the web test page draws, white on black, 48px.
struct TextTest;
pub const TEST_LINES: [&str; 4] = ["Exit Estonia", "qbittorrent", "203.0.113.66", "Hitori VPN"];
impl gpui::Render for TextTest {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        use gpui::{div, prelude::*};
        let feats = std::env::var("HITORI_TNUM").is_ok();
        div().size_full().bg(gpui::black()).text_color(gpui::white()).font(gpui::Font {
            family: "Google Sans".into(),
            features: gpui::FontFeatures(std::sync::Arc::new(if feats { vec![("tnum".into(), 1)] } else { vec![] })),
            fallbacks: None,
            weight: gpui::FontWeight::NORMAL,
            style: gpui::FontStyle::Normal,
        }).text_size(px(std::env::var("HITORI_TEXT_PX").ok().and_then(|v| v.parse().ok()).unwrap_or(48.))).line_height(px(80.)).pl(px(20.)).pt(px(20.)).children(TEST_LINES.map(|l| div().child(l)))
    }
}

/// `--tray` opens the 400×720 tray popup; `HITORI_SIZE=WxH` overrides the window size (screenshots).
/// The Vulkan loader initialises every installed driver while enumerating GPUs; Mesa's software one (lavapipe) alone
/// maps ~100 MB of LLVM. Skip it when a hardware GPU exists and nobody chose drivers explicitly.
#[cfg(target_os = "linux")]
fn skip_software_vulkan() {
    let chosen = ["VK_ICD_FILENAMES", "VK_DRIVER_FILES", "VK_LOADER_DRIVERS_SELECT", "VK_LOADER_DRIVERS_DISABLE"].iter().any(|v| std::env::var_os(v).is_some());
    let hardware = std::path::Path::new("/dev/nvidia0").exists()
        || std::fs::read_dir("/dev/dri").is_ok_and(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("renderD")));
    if !chosen && hardware {
        // SAFETY: called first thing in main, before any other thread exists.
        unsafe { std::env::set_var("VK_LOADER_DRIVERS_DISABLE", "*lvp*") };
    }
}

/// `--tray`: only the tray popup window (screenshots). `--background`: start with just the tray icon.
/// `--smoke`: open the main window, draw for a few seconds and exit 0 (CI on real Windows and macOS machines).
/// `--tun-selftest` (Windows, macOS; already elevated in CI): TUN mode end to end, see `helper::selftest`.
/// `status|connect|disconnect|servers|select <id>|watch`: control the running instance, see docs/ipc.md.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--tun-helper") {
        #[cfg(target_os = "linux")]
        std::process::exit(tun::helper(&args[2..]));
        #[cfg(not(target_os = "linux"))]
        std::process::exit(elevated::helper(&args[2..]));
    }
    #[cfg(not(target_os = "linux"))]
    if args.get(1).map(String::as_str) == Some("--tun-selftest") {
        std::process::exit(helper::selftest());
    }
    if let Some(code) = ipc::cli(args.get(1..).unwrap_or_default()) {
        std::process::exit(code);
    }
    #[cfg(target_os = "linux")]
    skip_software_vulkan();
    let tray_only = args.iter().any(|a| a == "--tray");
    let background = args.iter().any(|a| a == "--background");
    let smoke = args.iter().any(|a| a == "--smoke");
    // One instance per user: a second launch shows the running one instead. `--smoke` may run beside it.
    let control = if smoke { None } else { ipc::claim() };
    // A previous run that crashed may have left the system proxy pointing at it, or (macOS) its core running.
    sysproxy::restore();
    os::reap_orphans();
    Application::new().with_assets(Assets).run(move |cx: &mut App| {
        let fonts = ["Regular", "Medium", "SemiBold", "Bold"]
            .iter()
            .filter_map(|w| Assets::get(&format!("fonts/GoogleSans-{w}.ttf")).map(|f| f.data))
            .collect();
        cx.text_system().add_fonts(fonts).expect("fonts");
        let settings = store::load::<store::Settings>(&store::Settings::path()).unwrap_or_default();
        i18n::set(i18n::resolve(i18n::Lang::parse(&settings.language)));
        let dark = settings.dark;
        cx.set_global(if dark { theme::Theme::dark() } else { theme::Theme::light() });
        cx.bind_keys(app::bindings());
        cx.bind_keys(input::bindings());
        if std::env::var("HITORI_TEXTTEST").is_ok() {
            let bounds = Bounds::centered(None, size(px(800.), px(400.)), cx);
            cx.open_window(WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() }, |_, cx| cx.new(|_| TextTest)).expect("window");
            return;
        }
        let vpn = cx.new(app::Vpn::new);
        cx.set_global(tray::Windows { vpn: Some(vpn.clone()), ..Default::default() });
        if let Some(listener) = control {
            ipc::serve(listener, &vpn, cx);
        }
        let has_tray = !tray_only && tray::start(&vpn, cx);
        // With a tray icon, closing every window leaves the app (and the tunnel) running.
        gpui::KEEP_ALIVE_WITHOUT_WINDOWS.store(has_tray, std::sync::atomic::Ordering::Relaxed);
        // macOS apps outlive their windows by default; without a tray icon there is no way back in.
        #[cfg(target_os = "macos")]
        {
            cx.on_window_closed(move |cx| {
                if !has_tray && cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([gpui::KeyBinding::new("cmd-q", Quit, None)]);
            set_menus(cx);
        }
        if tray_only {
            tray::open_window(Variant::Tray, None, cx);
        } else if !(background && has_tray) {
            let main = tray::open_window(Variant::Main, None, cx);
            cx.global_mut::<tray::Windows>().main = main;
        }
        cx.activate(true);
        if smoke {
            smoke_test(cx);
        }
    });
}

#[cfg(target_os = "macos")]
pub fn set_menus(cx: &mut App) {
    cx.set_menus(vec![gpui::Menu { name: t!("app.title").into(), items: vec![gpui::MenuItem::action(t!("app.quit_app"), Quit)] }]);
}

/// The main window opened and keeps drawing frames: exit 0 after a few seconds, 2 if it never opened or closed early.
fn smoke_test(cx: &mut App) {
    let Some(main) = cx.global::<tray::Windows>().main else {
        eprintln!("hitori --smoke: no main window");
        std::process::exit(2);
    };
    cx.spawn(async move |cx| {
        for _ in 0..50 {
            cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
            let drawn = cx.update(|cx| main.update(cx, |_, window, _| window.refresh()).is_ok()).unwrap_or(false);
            if !drawn {
                eprintln!("hitori --smoke: the main window closed");
                std::process::exit(2);
            }
        }
        println!("hitori --smoke: ok, {} servers", data::servers().len());
        std::process::exit(0);
    })
    .detach();
}
