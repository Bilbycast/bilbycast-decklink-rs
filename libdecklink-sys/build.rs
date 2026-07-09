// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0
//
//! Build script for libdecklink-sys.
//!
//! Compiles our C++ shim (`shim/decklink_shim.cpp`) together with the
//! Blackmagic SDK's `DeckLinkAPIDispatch.cpp`, then generates Rust bindings for
//! the shim's C header.
//!
//! **No FFmpeg.** We talk to the DeckLink SDK directly, which means:
//!   * no `--enable-decklink --enable-nonfree` FFmpeg build,
//!   * no FFmpeg >= 8 requirement,
//!   * no duplicate `libav*` symbols in a binary that already links FFmpeg,
//!   * and access to `bmdFrameHasNoInputSource`, which the avdevice hides.
//!
//! `DeckLinkAPIDispatch.cpp` `dlopen`s `libDeckLinkAPI.so` at runtime, so the
//! only build-time requirement is the SDK **headers**:
//!
//! ```text
//! export DECKLINK_SDK_DIR=/path/to/Blackmagic_DeckLink_SDK/Linux/include
//! ```
//!
//! At runtime the host needs Blackmagic Desktop Video installed (it ships
//! `libDeckLinkAPI.so` and the kernel driver).

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=DECKLINK_SDK_DIR");
    println!("cargo:rerun-if-changed=shim/decklink_shim.cpp");
    println!("cargo:rerun-if-changed=shim/decklink_shim.h");

    let sdk = env::var("DECKLINK_SDK_DIR").unwrap_or_else(|_| {
        panic!(
            "DECKLINK_SDK_DIR is not set. Point it at the Blackmagic DeckLink SDK's \
             Linux/include directory (the one containing DeckLinkAPI.h and \
             DeckLinkAPIDispatch.cpp). See CLAUDE.md."
        )
    });
    let sdk = PathBuf::from(sdk);

    let api_h = sdk.join("DeckLinkAPI.h");
    let dispatch = sdk.join("DeckLinkAPIDispatch.cpp");
    for f in [&api_h, &dispatch] {
        assert!(
            f.is_file(),
            "missing {} — DECKLINK_SDK_DIR does not look like the SDK's Linux/include",
            f.display()
        );
    }

    // The SDK is C++ and its dispatch file dlopen()s libDeckLinkAPI.so.
    cc::Build::new()
        .cpp(true)
        .std("c++14")
        .include(&sdk)
        .include("shim")
        .file("shim/decklink_shim.cpp")
        .file(&dispatch)
        // The SDK headers trip these; they are not our code to fix.
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-multichar")
        .compile("decklink_shim");

    // `DeckLinkAPIDispatch.cpp` resolves libDeckLinkAPI.so via dlopen.
    println!("cargo:rustc-link-lib=dl");
    println!("cargo:rustc-link-lib=pthread");

    // Bindings for our own C header only — never the C++ SDK headers, which
    // bindgen cannot represent (COM-style pure-virtual interfaces).
    let bindings = bindgen::Builder::default()
        .header("shim/decklink_shim.h")
        .allowlist_function("dl_.*")
        .allowlist_type("dl_.*")
        .allowlist_var("DL_.*")
        .derive_debug(true)
        .derive_copy(true)
        .derive_default(true)
        .generate()
        .expect("bindgen failed to generate DeckLink shim bindings");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("failed to write bindings.rs");
}
