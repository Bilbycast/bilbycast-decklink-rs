// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Play 75% colour bars out of an SDI port.
//!
//! ```text
//! cargo run -p decklink-rs --example playout_bars -- "DeckLink Quad (2)" Hi50 [seconds]
//! ```
//!
//! Verifies the scheduled-playout path end to end without bilbycast-edge:
//! point the port at a monitor / multiviewer / another capture input and look.
//! A moving black bar sweeps across the frame so a frozen picture is
//! distinguishable from a live one.

use decklink_rs::{DecklinkPixelFormat, DecklinkPlayout, DecklinkPlayoutConfig};

/// BT.709 75% bars as (Y, Cb, Cr), left to right.
const BARS: [(u8, u8, u8); 8] = [
    (180, 128, 128), // white
    (168, 44, 136),  // yellow
    (145, 147, 44),  // cyan
    (133, 63, 52),   // green
    (63, 193, 204),  // magenta
    (51, 109, 212),  // red
    (28, 212, 120),  // blue
    (16, 128, 128),  // black
];

fn main() {
    let mut args = std::env::args().skip(1);
    let device = args.next().unwrap_or_default();
    let mode = args.next().unwrap_or_else(|| "Hi50".to_string());
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);

    let cfg = DecklinkPlayoutConfig {
        device: device.clone(),
        format: mode.clone(),
        width: 0, // take the mode's raster
        height: 0,
        frame_rate_num: 0,
        frame_rate_den: 0,
        pixel_format: DecklinkPixelFormat::Uyvy422,
        audio_channels: 0,
        audio_sample_rate: 48_000,
    };
    let mut po = match DecklinkPlayout::open(cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("open failed: {e}");
            std::process::exit(1);
        }
    };
    let (w, h) = po.video_dimensions();
    let (num, den) = po.video_frame_rate();
    let rb = po.row_bytes();
    let fps = num as u64 / den.max(1) as u64;
    let total = seconds * fps.max(1);
    println!("playing bars on '{device}' mode {mode}: {w}x{h} @ {num}/{den}, {total} frames");

    let mut frame = vec![0u8; rb * h as usize];
    for i in 0..total {
        // The sweeping bar: one bar-width column forced to black, advancing
        // one bar per second.
        let sweep = ((i / fps.max(1)) % 8) as u32;
        for row in 0..h as usize {
            let line = &mut frame[row * rb..row * rb + (w as usize) * 2];
            for px2 in 0..(w as usize) / 2 {
                let bar = (px2 as u32 * 2 * 8 / w) % 8;
                let (y, cb, cr) = if bar == sweep {
                    BARS[7]
                } else {
                    BARS[bar as usize]
                };
                let o = px2 * 4;
                line[o] = cb;
                line[o + 1] = y;
                line[o + 2] = cr;
                line[o + 3] = y;
            }
        }
        // Frame i's display time on the playout timeline, derived from the frame
        // index rather than accumulated so a fractional rate (59.94) does not
        // drift. A real caller passes `source_pts_90k - first_video_pts_90k`.
        let stream_time = i as i64 * 90_000 * den as i64 / num.max(1) as i64;
        if let Err(e) = po.write_video(&frame, stream_time) {
            eprintln!("write_video failed at frame {i}: {e}");
            std::process::exit(1);
        }
    }
    println!(
        "done: {total} frames, late={}, dropped={}",
        po.late_frames(),
        po.dropped_frames()
    );
}
