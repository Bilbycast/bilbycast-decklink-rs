// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0
//
//! Enumerate DeckLink devices via the SDK.
//!
//!   DECKLINK_SDK_DIR=~/decklink-sdk-include \
//!   cargo run -p decklink-rs --example list_devices

fn main() {
    let devices = decklink_rs::enumerate_devices();
    if devices.is_empty() {
        println!("no DeckLink devices found (no card, or Desktop Video not installed)");
        return;
    }
    println!("found {} DeckLink device(s):", devices.len());
    for d in &devices {
        println!(
            "  [{}] {:?}  sdi_channel={:?}",
            d.index, d.name, d.sdi_channel
        );
    }
}
