// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0
//
//! Smoke test: enumerate DeckLink devices via FFmpeg's avdevice layer.
//!
//! Run on a host with a DeckLink card and an `--enable-decklink` FFmpeg:
//!   LIBDECKLINK_FFMPEG_DIR=~/ffmpeg-decklink \
//!   LD_LIBRARY_PATH=~/ffmpeg-decklink/lib \
//!   cargo run -p decklink-rs --example list_devices

fn main() {
    let devices = decklink_rs::enumerate_devices();
    if devices.is_empty() {
        println!("no DeckLink devices found (no card, or FFmpeg lacks --enable-decklink)");
        return;
    }
    println!("found {} DeckLink device(s):", devices.len());
    for d in &devices {
        println!(
            "  [{}] {:?}  id={:?}  sdi_channel={:?}  input={} output={}",
            d.index, d.name, d.persistent_id, d.sdi_channel, d.can_input, d.can_output
        );
    }
}
