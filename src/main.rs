mod app;
mod data;
mod fx;
mod input;
mod map;
mod net;
mod overlays;
mod palette;
mod score;
mod theme;
mod tray;
mod ui;
mod views;
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
fn main() {
    skip_software_vulkan();
    let args: Vec<String> = std::env::args().collect();
    let tray_only = args.iter().any(|a| a == "--tray");
    let background = args.iter().any(|a| a == "--background");
    Application::new().with_assets(Assets).run(move |cx: &mut App| {
        let fonts = ["Regular", "Medium", "SemiBold", "Bold"]
            .iter()
            .filter_map(|w| Assets::get(&format!("fonts/GoogleSans-{w}.ttf")).map(|f| f.data))
            .collect();
        cx.text_system().add_fonts(fonts).expect("fonts");
        cx.set_global(theme::Theme::dark());
        cx.bind_keys(app::bindings());
        cx.bind_keys(input::bindings());
        if std::env::var("HITORI_TEXTTEST").is_ok() {
            let bounds = Bounds::centered(None, size(px(800.), px(400.)), cx);
            cx.open_window(WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() }, |_, cx| cx.new(|_| TextTest)).expect("window");
            return;
        }
        let vpn = cx.new(app::Vpn::new);
        cx.set_global(tray::Windows { vpn: Some(vpn.clone()), ..Default::default() });
        let has_tray = !tray_only && tray::start(&vpn, cx);
        // With a tray icon, closing every window leaves the app (and the tunnel) running.
        gpui::KEEP_ALIVE_WITHOUT_WINDOWS.store(has_tray, std::sync::atomic::Ordering::Relaxed);
        if tray_only {
            tray::open_window(Variant::Tray, None, cx);
        } else if !(background && has_tray) {
            let main = tray::open_window(Variant::Main, None, cx);
            cx.global_mut::<tray::Windows>().main = main;
        }
        cx.activate(true);
    });
}
