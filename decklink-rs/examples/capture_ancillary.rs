// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use decklink_rs::{CapturedFrame, DecklinkCapture, DecklinkCaptureConfig, DecklinkPixelFormat};

fn main() {
    let device = std::env::args().nth(1).unwrap_or_else(|| "DeckLink Quad (2)".into());
    let mut cap = DecklinkCapture::open(DecklinkCaptureConfig {
        device, format: "auto".into(), pixel_format: DecklinkPixelFormat::Uyvy422,
        audio_channels: 0, audio_sample_rate: 48_000,
    }).expect("open capture");
    for _ in 0..300 {
        if let CapturedFrame::Video(v) = cap.read_frame().expect("capture frame") {
            for p in v.ancillary {
                if p.did == 0x41 && p.sdid == 0x07 {
                    println!("SCTE104 line={} len={} data={:02x?}", p.line_number, p.data.len(), p.data);
                    if p.data.get(13..17) == Some(&[0x12,0x34,0x56,0x78]) { return; }
                }
            }
        }
    }
    panic!("known SCTE-104 packet not captured");
}
