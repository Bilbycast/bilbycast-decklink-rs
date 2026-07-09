// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Print the hardware status of every DeckLink device on this host.
//!
//! Neither opens nor reserves any device, so it is safe to run against a card
//! that is mid-capture — useful for answering "which SDI ports have signal?"
//! without disturbing a live flow.
//!
//! ```text
//! cargo run -p decklink-rs --example device_status
//! ```

fn tri(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

fn opt(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("—")
}

fn main() {
    let devices = decklink_rs::enumerate_devices();
    if devices.is_empty() {
        println!("no DeckLink devices (is Desktop Video installed?)");
        return;
    }

    for d in &devices {
        match decklink_rs::device_status(d.index) {
            Ok(s) => {
                println!("[{}] {}", d.index, d.name);
                println!(
                    "     signal={:<7} reference={:<7} anc={:<7} busy={}",
                    tri(s.signal_locked),
                    tri(s.reference_locked),
                    tri(s.ancillary_locked),
                    tri(s.busy),
                );
                println!(
                    "     mode={:<6} colorspace={:<6} field={:<6} link={}",
                    opt(&s.detected_mode),
                    opt(&s.detected_colorspace),
                    opt(&s.detected_field_dominance),
                    opt(&s.sdi_link_config),
                );
                let pcie = match (s.pcie_link_speed, s.pcie_link_width) {
                    (Some(gen), Some(lanes)) => format!("gen{gen} x{lanes}"),
                    _ => "unknown".to_string(),
                };
                println!(
                    "     reference_mode={:<6} pcie={pcie}",
                    opt(&s.reference_mode)
                );
            }
            Err(e) => println!("[{}] {}: status unavailable: {e}", d.index, d.name),
        }
    }
}
