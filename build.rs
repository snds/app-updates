//! On Windows, a Tauri window needs Common Controls 6. An app gets it from the manifest
//! `tauri-build` embeds; this library's test binary has none, so Tauri's mock runtime stopped at
//! launch (`STATUS_ENTRYPOINT_NOT_FOUND`). The same manifest dependency is linked into the test
//! binaries here. A library isn't linked, so nothing an app builds carries it.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" && env == "msvc" && std::env::var_os("CARGO_FEATURE_TAURI").is_some() {
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
}
