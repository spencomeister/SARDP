//! Compiles the Swift ScreenCaptureKit shim (`shim/*.swift`) into a static
//! library and links it, the Swift runtime and the Apple frameworks into
//! this binary. Needs only the Command Line Tools (`swiftc`), not Xcode.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        println!("cargo:warning=sck-capture-poc only builds on macOS; skipping the Swift shim");
        return;
    }
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let shim_dir = manifest_dir.join("shim");
    println!("cargo:rerun-if-changed={}", shim_dir.display());

    let sdk = String::from_utf8(
        Command::new("xcrun")
            .args(["--show-sdk-path"])
            .output()
            .expect("xcrun --show-sdk-path")
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    let mut sources: Vec<PathBuf> = std::fs::read_dir(&shim_dir)
        .expect("shim dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "swift"))
        .collect();
    sources.sort();
    for s in &sources {
        println!("cargo:rerun-if-changed={}", s.display());
    }

    let lib = out_dir.join("libSardpSckShim.a");
    let status = Command::new("swiftc")
        .args(["-O", "-parse-as-library", "-emit-library", "-static"])
        .args(["-module-name", "SardpSckShim"])
        .args(["-sdk", &sdk])
        .args([
            "-target",
            &format!(
                "{}-apple-macosx14.0",
                env::var("CARGO_CFG_TARGET_ARCH")
                    .unwrap()
                    .replace("aarch64", "arm64")
            ),
        ])
        .arg("-o")
        .arg(&lib)
        .args(&sources)
        .status()
        .expect("run swiftc");
    assert!(status.success(), "swiftc failed");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=SardpSckShim");
    // Swift runtime: the .tbd stubs are in the SDK, the dylibs in /usr/lib/swift.
    println!("cargo:rustc-link-search=native={sdk}/usr/lib/swift");
    // NOTE: `-l`/`-L` directives reach dependent crates' binaries, but a
    // link *arg* does not -- it applies only to this package. A binary in
    // another crate that links this shim therefore needs its own
    // `-Wl,-rpath,/usr/lib/swift` (see `sardp-mac/build.rs`), or it dies
    // at launch with "Library not loaded: @rpath/libswift*.dylib".
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    for lib in [
        "swiftCore",
        "swiftFoundation",
        "swiftDispatch",
        "swiftObjectiveC",
        "swiftCoreFoundation",
        "swiftDarwin",
        "swiftCoreGraphics",
        "swiftCoreMedia",
        "swiftVideoToolbox",
        "swiftCoreImage",
        "swiftXPC",
        "swiftIOKit",
        "swiftos",
        "swift_Concurrency",
    ] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    for fw in [
        "ScreenCaptureKit",
        "VideoToolbox",
        "CoreMedia",
        "CoreVideo",
        "CoreGraphics",
        "Foundation",
        "ApplicationServices",
    ] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}
