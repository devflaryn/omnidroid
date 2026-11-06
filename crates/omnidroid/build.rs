//! Gives the program its icon on Windows, where an icon is a resource in the executable: Explorer,
//! the taskbar and the window's title bar all read it from there. The resource is prebuilt
//! (`tools/make_icon.py`), so a build needs no resource compiler; other platforms set theirs when
//! the window opens (`omni-platform`'s window seam).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let var = |name| std::env::var(name).unwrap_or_default();
    if var("CARGO_CFG_TARGET_OS") == "windows" && var("CARGO_CFG_TARGET_ENV") == "msvc" {
        let res = std::path::Path::new(&var("CARGO_MANIFEST_DIR")).join("../omni-platform/icon/omnidroid.res");
        println!("cargo:rerun-if-changed={}", res.display());
        println!("cargo:rustc-link-arg-bins={}", res.display());
    }
}
