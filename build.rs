fn main() {
    // Per-monitor DPI awareness, common controls v6 and the app icon for hitori.exe.
    #[cfg(windows)]
    embed_resource::compile("packaging/windows/hitori.rc", embed_resource::NONE).manifest_required().unwrap();
}
