fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    let mut res = winresource::WindowsResource::new();
    // Id 1: the windows load it by this id (theme::icon).
    res.set_icon_with_id("assets/icon.ico", "1");
    res.compile().expect("compiling the icon resource");
}
