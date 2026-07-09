// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0
//
//! Capture a fixed number of frames from a DeckLink input and report what
//! arrived. Proves `DecklinkCapture` end-to-end against a live SDI source.
//!
//!   LIBDECKLINK_FFMPEG_DIR=~/ffmpeg-decklink LD_LIBRARY_PATH=~/ffmpeg-decklink/lib:/lib \
//!   cargo run -p decklink-rs --example capture_probe -- "DeckLink Quad (1)" Hi50

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
    println!("opened {device:?}: detected {w}x{h}");

    let (mut vframes, mut aframes, mut vbytes, mut abytes) = (0u64, 0u64, 0u64, 0u64);
    let mut first_audio_chans = 0u8;
    // ~2 seconds of a 25/50 fps signal plus its audio blocks.
    let target_video = 50u64;
    while vframes < target_video {
        match cap.read_frame() {
            Ok(CapturedFrame::Video(v)) => {
                vframes += 1;
                vbytes += v.data.len() as u64;
                if vframes == 1 {
                    println!(
                        "first video frame: {}x{} {} stride={} bytes={}",
                        v.width,
                        v.height,
                        v.pixel_format,
                        v.stride,
                        v.data.len()
                    );
                }
            }
            Ok(CapturedFrame::Audio(a)) => {
                aframes += 1;
                abytes += (a.samples.len() * 4) as u64;
                first_audio_chans = a.channels;
            }
            Err(e) => {
                eprintln!("read_frame ended: {e}");
                break;
            }
        }
    }

    println!(
        "captured: {vframes} video frames ({vbytes} B), {aframes} audio blocks ({abytes} B, {first_audio_chans} ch)"
    );
}
