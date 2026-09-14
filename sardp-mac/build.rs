//! Adds the Swift runtime rpath.
//!
//! `sck-capture-poc`'s build script links the Swift static shim and names
//! the Swift runtime libraries, and those directives (`cargo:rustc-link-lib`
//! / `-search`) do reach this crate's binaries transitively. The rpath does
//! not: `cargo:rustc-link-arg` applies only to the package whose build
//! script emitted it. Without it the Swift runtime is linked against the
//! SDK stubs, whose install names are `@rpath/libswift*.dylib`, and the
//! binary dies at launch with "Library not loaded ... no LC_RPATH's found".
//!
//! So every *binary* crate that ends up linking the shim needs this one
//! line -- `sardp-cli` will too when macOS is wired into it (3M-1-d).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
