// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0
//
//! Capture frames from a DeckLink input and report what arrived, including
//! whether the card reports a locked input signal.
//!
//!   DECKLINK_SDK_DIR=~/decklink-sdk-include \
//!   cargo run -p decklink-rs --example capture_probe -- "DeckLink Quad (1)" auto

use decklink_rs::{CapturedFrame, DecklinkCapture, DecklinkCaptureConfig, DecklinkPixelFormat};

fn main() {
    let mut args = std::env::args().skip(1);
    let device = args.next().unwrap_or_else(|| "DeckLink Quad (1)".into());
    let format = args.next().unwrap_or_else(|| "auto".into());

    let cfg = DecklinkCaptureConfig {
        device: device.clone(),
        format,
        pixel_format: DecklinkPixelFormat::Uyvy422,
        audio_channels: 2,
        audio_sample_rate: 48_000,
    };

    let mut cap = match DecklinkCapture::open(cfg) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("open {device:?} failed: {e}");
            std::process::exit(1);
        }
    };
    let (w, h) = cap.video_dimensions();
    let (n, d) = cap.video_frame_rate();
    println!("opened {device:?}: detected {w}x{h} @ {n}/{d}");

    let (mut vframes, mut aframes, mut with_signal, mut no_signal) = (0u64, 0u64, 0u64, 0u64);
    let target = 50u64;
    while vframes < target {
        match cap.read_frame() {
            Ok(CapturedFrame::Video(v)) => {
                vframes += 1;
                if v.signal_present {
                    with_signal += 1
                } else {
                    no_signal += 1
                }
                if vframes == 1 {
                    println!(
                        "first video frame: {}x{} {} stride={} bytes={} signal_present={}",
                        v.width,
                        v.height,
                        v.pixel_format,
                        v.stride,
                        v.data.len(),
                        v.signal_present
                    );
                }
            }
            Ok(CapturedFrame::Audio(_)) => aframes += 1,
            Err(e) => {
                eprintln!("read_frame ended: {e}");
                break;
            }
        }
    }

    println!(
        "captured {vframes} video ({with_signal} with signal, {no_signal} NO SIGNAL), \
         {aframes} audio blocks; shim dropped {}",
        cap.dropped_frames()
    );
}
