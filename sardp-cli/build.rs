//! Two things the binaries need that only the build script can provide.
//!
//! 1. **The Swift runtime rpath on macOS.** These binaries link the Swift
//!    shim transitively (`sardp-mac` -> `sck-capture-poc`). That crate's
//!    build script names the Swift runtime libraries, and `-l`/`-L`
//!    directives do reach here -- but a link *arg* does not, so the rpath
//!    has to be repeated. Without it the binary dies at launch with
//!    "Library not loaded: @rpath/libswift_Concurrency.dylib"
//!    (KNOWN_ISSUES #25).
//!
//! 2. **A `desktop_capture` cfg.** `--capture desktop` and input injection
//!    exist on the platforms that have an OS-integration crate. Naming
//!    that condition once, here, keeps the server from spelling
//!    `any(windows, target_os = "macos")` at a dozen sites and means
//!    adding Linux (3G-1) is a one-line change.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if os == "macos" {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }

    println!("cargo::rustc-check-cfg=cfg(desktop_capture)");
    if matches!(os.as_str(), "windows" | "macos") {
        println!("cargo::rustc-cfg=desktop_capture");
    }
}
