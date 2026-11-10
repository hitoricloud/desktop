fn main() {
    // Per-monitor DPI awareness, common controls v6 and the app icon for hitori.exe (also when cross-building).
    println!("cargo:rerun-if-changed=packaging/windows");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("packaging/windows/hitori.rc", embed_resource::NONE).manifest_required().unwrap();
    }
}
