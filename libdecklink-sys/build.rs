// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0
//
//! Build script for libdecklink-sys.
//!
//! Unlike `libffmpeg-video-sys` (which vendors a MINIMAL FFmpeg with
//! `--disable-avdevice --disable-avformat` and no proprietary linkage),
//! this crate needs an FFmpeg that was configured with:
//!
//!   `--enable-avdevice --enable-avformat --enable-decklink --enable-nonfree`
//!   `--extra-cflags=-I$DECKLINK_SDK_DIR --extra-ldflags=-L$DECKLINK_SDK_DIR`
//!
//! against the Blackmagic **DeckLink SDK** headers (`DeckLinkAPI.h` et al.,
//! shipped in the SDK zip, NOT in the Desktop Video runtime deb). FFmpeg's
//! `decklink` in/out device is `--enable-nonfree` because it links the
//! proprietary SDK — so a binary linking it is non-redistributable, which is
//! fine for our private on-prem edge deployment but is why this stays behind
//! bilbycast-edge's off-by-default `sdi-decklink` feature.
//!
//! Two ways to satisfy the link:
//!
//!  1. `LIBDECKLINK_FFMPEG_DIR=/path/to/ffmpeg-install` — point at a
//!     from-source FFmpeg prefix built with the flags above. (Recommended
//!     for bilby-z440: build once, cache the prefix.)
//!  2. `--features system-ffmpeg` — pkg-config a system FFmpeg that already
//!     has `--enable-decklink`. Most distro packages do NOT, so this only
//!     works with a hand-built or Blackmagic-provided ffmpeg on
//!     `PKG_CONFIG_PATH`.
//!
//! A fully-vendored build (fetch FFmpeg source + configure with decklink) is
//! deliberately NOT implemented here yet — it requires the operator to accept
//! the SDK EULA and stage `DECKLINK_SDK_DIR`, so we make that an explicit,
//! documented prerequisite (see README.md / CLAUDE.md) rather than an
//! implicit source pull. Revisit if CI needs a hermetic build.

use std::env;
use std::path::PathBuf;

// FFmpeg libs we consume, in link order (dependents before dependencies).
const FFMPEG_LIBS: &[&str] = &["avdevice", "avformat", "avcodec", "avutil"];

fn main() {
    println!("cargo:rerun-if-env-changed=LIBDECKLINK_FFMPEG_DIR");
    println!("cargo:rerun-if-changed=wrapper.h");

    let include_path = if let Ok(dir) = env::var("LIBDECKLINK_FFMPEG_DIR") {
        // Explicit prefix built with --enable-decklink.
        let prefix = PathBuf::from(&dir);
        println!(
            "cargo:rustc-link-search=native={}",
            prefix.join("lib").display()
        );
        link_ffmpeg_dynamic();
        prefix.join("include")
    } else if cfg!(feature = "system-ffmpeg") {
        // System FFmpeg via pkg-config — must already have decklink enabled.
        let mut include = None;
        for lib in FFMPEG_LIBS {
            let probed = pkg_config::Config::new()
                .probe(&format!("lib{lib}"))
                .unwrap_or_else(|e| {
                    panic!(
                        "pkg-config: lib{lib} not found ({e}). Install an FFmpeg \
                         built with --enable-decklink, or set \
                         LIBDECKLINK_FFMPEG_DIR to its install prefix."
                    )
                });
            if include.is_none() {
                include = probed.include_paths.first().cloned();
            }
        }
        include.expect("no include path returned by pkg-config for FFmpeg")
    } else {
        panic!(
            "libdecklink-sys: no FFmpeg-with-decklink located. Set \
             LIBDECKLINK_FFMPEG_DIR=/path/to/ffmpeg-install (built with \
             --enable-decklink against the Blackmagic DeckLink SDK), or enable \
             the `system-ffmpeg` feature to pkg-config a system build that \
             already has it. See CLAUDE.md for the FFmpeg configure recipe."
        );
    };

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", include_path.display()))
        // ── avdevice: registration + device enumeration ──
        .allowlist_function("avdevice_register_all")
        .allowlist_function("avdevice_list_input_sources")
        .allowlist_function("avdevice_list_output_sinks")
        .allowlist_function("avdevice_free_list_devices")
        .allowlist_type("AVDeviceInfoList")
        .allowlist_type("AVDeviceInfo")
        // ── avformat: open the decklink demuxer/muxer, read/write frames ──
        .allowlist_function("avformat_alloc_context")
        .allowlist_function("avformat_open_input")
        .allowlist_function("avformat_close_input")
        .allowlist_function("avformat_find_stream_info")
        .allowlist_function("avformat_alloc_output_context2")
        .allowlist_function("avformat_free_context")
        .allowlist_function("avformat_new_stream")
        .allowlist_function("avformat_write_header")
        .allowlist_function("av_read_frame")
        .allowlist_function("av_write_frame")
        .allowlist_function("av_interleaved_write_frame")
        .allowlist_function("av_write_trailer")
        .allowlist_function("av_find_input_format")
        .allowlist_function("av_guess_format")
        .allowlist_type("AVFormatContext")
        .allowlist_type("AVInputFormat")
        .allowlist_type("AVOutputFormat")
        .allowlist_type("AVStream")
        // ── avcodec: packet lifecycle (raw video / PCM ride in AVPackets) ──
        .allowlist_function("av_packet_alloc")
        .allowlist_function("av_packet_free")
        .allowlist_function("av_packet_unref")
        .allowlist_function("av_new_packet")
        .allowlist_type("AVPacket")
        .allowlist_type("AVCodecParameters")
        .allowlist_type("AVCodecID")
        // ── avutil: dict (device options), frames, image/sample geometry ──
        .allowlist_function("av_dict_set")
        .allowlist_function("av_dict_set_int")
        .allowlist_function("av_dict_free")
        .allowlist_function("av_frame_alloc")
        .allowlist_function("av_frame_free")
        .allowlist_function("av_frame_get_buffer")
        .allowlist_function("av_image_get_buffer_size")
        .allowlist_function("av_image_fill_arrays")
        .allowlist_function("av_samples_get_buffer_size")
        .allowlist_function("av_get_pix_fmt")
        .allowlist_function("av_get_pix_fmt_name")
        .allowlist_function("av_log_set_level")
        .allowlist_function("av_strerror")
        .allowlist_type("AVDictionary")
        .allowlist_type("AVFrame")
        .allowlist_type("AVRational")
        .allowlist_type("AVMediaType")
        .allowlist_type("AVPixelFormat")
        .allowlist_type("AVSampleFormat")
        // ── constants ──
        .allowlist_var("AVMEDIA_TYPE_.*")
        .allowlist_var("AV_PIX_FMT_.*")
        .allowlist_var("AV_SAMPLE_FMT_.*")
        .allowlist_var("AV_LOG_.*")
        .allowlist_var("AVERROR.*")
        .allowlist_var("AVFMT_.*")
        .allowlist_var("AV_TIME_BASE.*")
        .derive_debug(true)
        .derive_copy(true)
        .derive_default(true)
        .generate()
        .expect("bindgen failed to generate DeckLink/FFmpeg bindings");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("failed to write bindings.rs");
}

/// Emit dynamic link directives for the FFmpeg libs plus the platform system
/// libs the DeckLink avdevice pulls in. We link FFmpeg dynamically here
/// (unlike libffmpeg-video-sys's static vendored build) because the
/// `--enable-decklink --enable-nonfree` FFmpeg is expected to be an external
/// prefix the operator manages, not a static archive we vendor.
fn link_ffmpeg_dynamic() {
    for lib in FFMPEG_LIBS {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "linux" {
        // FFmpeg + the Blackmagic SDK's C++ runtime dependencies.
        println!("cargo:rustc-link-lib=m");
        println!("cargo:rustc-link-lib=pthread");
        println!("cargo:rustc-link-lib=dl");
        // libavdevice's decklink indev/outdev is C++ (the BMD SDK is C++),
        // so the final link needs the C++ standard library.
        println!("cargo:rustc-link-lib=stdc++");
    }
}
