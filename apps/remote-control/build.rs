fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-env-changed=RC_BUILD_VERSION");
    let version = std::env::var("RC_BUILD_VERSION")
        .ok()
        .map(|s| s.trim().trim_start_matches('v').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=RC_APP_VERSION={version}");
    let mut res = winresource::WindowsResource::new();
    // Id 1: the windows load it by this id (theme::icon).
    res.set_icon_with_id("assets/icon.ico", "1");
    res.set("ProductVersion", &version);
    res.set("FileVersion", &version);
    res.compile().expect("compiling the icon resource");
}
