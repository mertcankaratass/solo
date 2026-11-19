extern crate napi_build;

fn main() {
    napi_build::setup();

    // `ui_thread`: platforms where GPUI runs on a `solo-ui` thread that the
    // renderer drives through `UiCommand`s (everything but macOS).
    println!("cargo::rustc-check-cfg=cfg(ui_thread)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(os.as_str(), "windows" | "linux" | "freebsd") {
        println!("cargo::rustc-cfg=ui_thread");
    }

    // GPUI imports TaskDialogIndirect, which only comctl32 v6 exports. The
    // host (bun.exe) does not ask for v6, so without its own manifest the
    // addon resolves against comctl32 v5 and LoadLibrary fails with "The
    // specified procedure could not be found". A DLL's manifest resource 2
    // (ISOLATIONAWARE_MANIFEST_RESOURCE_ID) is the one the loader uses for
    // that DLL's own imports.
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" && env == "msvc" {
        println!("cargo::rustc-link-arg-cdylib=/MANIFEST:EMBED,ID=2");
        println!("cargo::rustc-link-arg-cdylib=/MANIFESTUAC:NO");
        println!(
            "cargo::rustc-link-arg-cdylib=/MANIFESTDEPENDENCY:type='win32' \
             name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
             processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
}
