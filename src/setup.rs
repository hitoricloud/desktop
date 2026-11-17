#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Offline GPUI installer. The main application and core are embedded only in release setup builds.
#![allow(dead_code)]
mod install;
mod release;
mod theme;
#[cfg(windows)]
mod setup_windows;
include!(concat!(env!("OUT_DIR"), "/payload.rs"));

use std::path::PathBuf;
use gpui::{Application, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*, px, size};

struct Setup {
    uninstall: bool,
    destination: PathBuf,
    busy: bool,
    done: bool,
    error: Option<String>,
}

impl Setup {
    fn act(&mut self, cx: &mut Context<Self>) {
        if self.busy { return; }
        if self.done {
            if !self.uninstall { let _ = std::process::Command::new(self.destination.join("hitori.exe")).spawn(); }
            cx.quit();
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();
        let destination = self.destination.clone();
        let uninstall = self.uninstall;
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { perform(destination, uninstall) }).await;
            let quit = uninstall && result.is_ok();
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result { Ok(()) => this.done = true, Err(e) => this.error = Some(e) }
                cx.notify();
            });
            if quit { let _ = cx.update(|cx| cx.quit()); }
        }).detach();
    }
}

fn perform(destination: PathBuf, uninstall: bool) -> Result<(), String> {
    #[cfg(windows)]
    { setup_windows::perform(destination, uninstall, PAYLOAD) }
    #[cfg(not(windows))]
    { let _ = (destination, uninstall); Err("This installer is for Windows. Use the Linux package or macOS DMG on this system.".into()) }
}

impl gpui::Render for Setup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = match window.appearance() {
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark => theme::Theme::dark(),
            _ => theme::Theme::light(),
        };
        let action = if self.busy { "Please wait…" } else if self.done { "Launch Hitori" } else if self.uninstall { "Uninstall" } else { "Install" };
        let text = if self.done { "Hitori VPN is installed." } else if self.uninstall { "Close Hitori before uninstalling. Your account and settings will be kept." }
            else { "Install for this Windows account. No administrator password is needed. Close Hitori before installing." };
        div().size_full().bg(t.background).text_color(t.foreground).font_family("Google Sans").p(px(32.)).flex().flex_col().gap(px(16.))
            .child(div().text_size(px(24.)).font_weight(gpui::FontWeight::SEMIBOLD).child(if self.uninstall { "Uninstall Hitori VPN" } else { "Hitori VPN" }))
            .child(div().text_size(px(14.)).text_color(t.muted_fg).child(format!("Version {}", release::VERSION)))
            .child(div().text_size(px(14.)).child(text))
            .child(div().text_size(px(12.)).text_color(t.muted_fg).child(self.destination.to_string_lossy().into_owned()))
            .child(div().flex_1())
            .children(self.error.as_ref().map(|e| div().text_size(px(12.)).text_color(t.destructive).child(e.clone())))
            .child(div().flex().gap(px(12.))
                .child(div().id("cancel").flex().items_center().justify_center().h(px(48.)).px(px(24.)).rounded(px(8.)).border_1().border_color(t.border).cursor_pointer()
                    .child("Close").when(!self.busy, |d| d.on_click(|_, _, cx| cx.quit())))
                .child(div().id("install").flex_1().flex().items_center().justify_center().h(px(48.)).rounded(px(8.)).bg(t.primary).text_color(t.primary_fg)
                    .when(self.busy, |d| d.opacity(0.5)).cursor_pointer().child(action)
                    .when(!self.busy, |d| d.on_click(cx.listener(|this, _, _, cx| this.act(cx))))))
    }
}

fn main() {
    #[cfg(windows)]
    if let Some(result) = setup_windows::worker_args() {
        if let Err(e) = result { setup_windows::show_error(&e); std::process::exit(1); }
        return;
    }
    let uninstall = std::env::args().any(|a| a == "--uninstall");
    Application::new().run(move |cx| {
        let fonts = ["Regular", "Medium", "SemiBold", "Bold"].map(|weight| {
            // Font embedding follows the host application; no browser or system font dependency.
            match weight {
                "Regular" => include_bytes!("../assets/fonts/GoogleSans-Regular.ttf").as_slice(),
                "Medium" => include_bytes!("../assets/fonts/GoogleSans-Medium.ttf").as_slice(),
                "SemiBold" => include_bytes!("../assets/fonts/GoogleSans-SemiBold.ttf").as_slice(),
                _ => include_bytes!("../assets/fonts/GoogleSans-Bold.ttf").as_slice(),
            }.into()
        }).into();
        cx.text_system().add_fonts(fonts).expect("installer fonts");
        #[cfg(windows)]
        let destination = setup_windows::destination().unwrap_or_default();
        #[cfg(not(windows))]
        let destination = PathBuf::from("Windows per-user installation");
        let bounds = Bounds::centered(None, size(px(480.), px(384.)), cx);
        cx.open_window(WindowOptions { titlebar: Some(gpui::TitlebarOptions { title: Some("Hitori VPN Setup".into()), ..Default::default() }), window_bounds: Some(WindowBounds::Windowed(bounds)), window_min_size: Some(size(px(480.), px(384.))), ..Default::default() }, |_, cx| {
            cx.new(|_| Setup { uninstall, destination, busy: false, done: false, error: None })
        }).expect("installer window");
        cx.on_window_closed(|cx| { if cx.windows().is_empty() { cx.quit(); } }).detach();
        cx.activate(true);
    });
}
