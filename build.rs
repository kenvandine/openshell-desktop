//! Generates Rust bindings for libghostty-vt and links it.
//!
//! libghostty-vt is found through pkg-config; build it with
//! `scripts/build-libghostty-vt.sh` and point PKG_CONFIG_PATH at its prefix.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    let library = pkg_config::Config::new()
        .probe("libghostty-vt")
        .unwrap_or_else(|e| {
            panic!(
                "libghostty-vt not found ({e}).\n\
                 Build it with scripts/build-libghostty-vt.sh and set\n\
                 PKG_CONFIG_PATH=$PWD/vendor/ghostty-vt/share/pkgconfig"
            )
        });

    let mut builder = bindgen::Builder::default()
        .header_contents("wrapper.h", "#include <ghostty/vt.h>")
        .allowlist_function("ghostty_.*")
        .allowlist_type("Ghostty.*")
        .allowlist_var("GHOSTTY_.*")
        .prepend_enum_name(false)
        .derive_default(true)
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for path in &library.include_paths {
        builder = builder.clang_arg(format!("-I{}", path.display()));
    }

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("ghostty_vt.rs");
    builder
        .generate()
        .expect("failed to generate libghostty-vt bindings")
        .write_to_file(out)
        .expect("failed to write libghostty-vt bindings");
}
