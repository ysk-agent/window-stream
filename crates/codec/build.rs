//! Build script: link the system libvpx and generate FFI bindings from the
//! installed headers.
//!
//! Why not the `libvpx-sys` crate? Its current crates.io release (1.4.2)
//! ships pre-generated bindings from the PNaCl era: its
//! `vpx_codec_enc_cfg_t` layout predates fields libvpx 1.14 has in the
//! middle of that struct (`rc_2pass_vbr_corpus_complexity`, the vizier
//! rate-control rationals, ...), so passing it to a 1.14 library would be
//! an ABI mismatch — `vpx_codec_enc_config_default` writes the *library's*
//! layout. Generating bindings from the headers that match the linked
//! library removes that whole class of bug. See the crate README.

use std::env;
use std::path::PathBuf;

fn main() {
    // Link libvpx via pkg-config (dynamic by default; the system package
    // also ships a static archive, but nothing here needs it).
    let lib = pkg_config::Config::new()
        .probe("vpx")
        .expect("libvpx not found via pkg-config: install libvpx-dev (see README)");

    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        // Only the libvpx public API; keeps the generated file small and
        // avoids pulling libc internals into the bindings.
        .allowlist_function("vpx_.*")
        .allowlist_type("vpx_.*")
        .allowlist_type("vp8_.*")
        .allowlist_type("vp9_.*")
        .allowlist_var("VPX_.*")
        .allowlist_var("VP8_.*")
        .allowlist_var("VP9_.*")
        .derive_debug(true)
        // No layout tests: bindgen's layout assertions compare the
        // generated struct layout against the *build-time* headers, but
        // several libvpx structs contain unions/bitfields whose layout
        // bindgen cannot reproduce exactly across versions, so the tests
        // would be brittle without catching a real mismatch — the FFI
        // safety here comes from generating bindings from the same
        // headers as the linked library (see the module docs above).
        .layout_tests(false);
    for path in &lib.include_paths {
        builder = builder.clang_arg(format!("-I{}", path.display()));
    }
    let bindings = builder
        .generate()
        .expect("bindgen failed to generate libvpx bindings");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("vpx_bindings.rs"))
        .expect("failed to write libvpx bindings");

    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");
}
