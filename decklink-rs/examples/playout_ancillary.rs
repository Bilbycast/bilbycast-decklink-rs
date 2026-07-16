// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use decklink_rs::{CapturedAncillaryPacket, DecklinkPixelFormat, DecklinkPlayout, DecklinkPlayoutConfig};

fn main() {
    let device = std::env::args().nth(1).unwrap_or_else(|| "DeckLink Quad (1)".into());
    let mut po = DecklinkPlayout::open(DecklinkPlayoutConfig {
        device, format: "Hi50".into(), width: 0, height: 0,
        frame_rate_num: 0, frame_rate_den: 0, pixel_format: DecklinkPixelFormat::Uyvy422,
        audio_channels: 0, audio_sample_rate: 48_000,
    }).expect("open playout");
    let (_, h) = po.video_dimensions();
    let frame = vec![0x80; po.row_bytes() * h as usize];
    // SCTE-104 start_normal, event 0x12345678, 500 ms pre-roll, 30 s auto-return.
    let payload = vec![0,25,0,0,0,0,0,0,1,1,0,13,1,0x12,0x34,0x56,0x78,0,1,0xf4,1,0x2c,0,0,1];
    let anc = [CapturedAncillaryPacket { did: 0x41, sdid: 0x07, line_number: 0, data: payload }];
    let (num, den) = po.video_frame_rate();
    for i in 0..500i64 {
        // Derived from the frame index rather than accumulated, so a fractional
        // rate does not drift. A real caller passes
        // `source_pts_90k - first_video_pts_90k`.
        let stream_time = i * 90_000 * den as i64 / num.max(1) as i64;
        po.write_video_with_ancillary(&frame, stream_time, &anc)
            .expect("schedule frame with ANC");
    }
    println!("scheduled 500 SCTE-104 ANC frames; late={} dropped={}", po.late_frames(), po.dropped_frames());
}
