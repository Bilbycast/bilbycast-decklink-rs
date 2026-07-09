// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Raw FFI to the C ABI shim over the Blackmagic DeckLink SDK.
//!
//! The SDK is COM-style C++ and delivers frames through a virtual
//! `IDeckLinkInputCallback`, which Rust cannot implement. `shim/` wraps that in
//! a small C API; this crate is the generated binding for it.
//!
//! Everything here is `unsafe` FFI — the safe surface lives in `decklink-rs`.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
