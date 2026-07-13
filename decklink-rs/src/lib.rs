// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Safe SDI capture over Blackmagic DeckLink cards, via the DeckLink SDK.
//!
//! bilbycast-edge depends on this crate (behind the off-by-default
//! `sdi-decklink` feature) and drives it from `engine::sdi_io`, mirroring how
//! `engine::mxl_video_io` drives `mxl-rs`.
//!
//! # Why the SDK and not FFmpeg's `decklink` avdevice
//!
//! The avdevice route works, but it hides the one thing a broadcast plant needs
//! most: `bmdFrameHasNoInputSource`. On signal loss FFmpeg silently substitutes
//! colour bars, so a pulled cable is indistinguishable from a healthy feed.
//!
//! Talking to the SDK directly also removes a lot of incidental pain:
//!
//! * no `--enable-decklink --enable-nonfree` FFmpeg build (the edge binary stays
//!   redistributable),
//! * no FFmpeg >= 8 requirement,
//! * no duplicate `libav*` symbols in a binary that already links FFmpeg,
//! * device enumeration that does not wedge the card.
//!
//! # Model
//!
//! The SDK delivers frames on its own thread through a C++ callback. A small
//! C ABI shim (`libdecklink-sys`) owns that callback and pushes frames onto a
//! bounded queue; [`DecklinkCapture::read_frame`] blocks on that queue. Drive it
//! from a `spawn_blocking` / `block_in_place` thread.
//!
//! Frames are zero-copy inside the shim, but this crate copies each frame into
//! an owned buffer so the SDK's frame can be released promptly — a stalled
//! consumer must never starve the card of buffers.
//!
//! # Design constraints
//!
//! 1. **Send but not Sync** — handles move between threads but need `&mut`.
//! 2. **Blocking** — `read_frame` blocks on the SDI cadence.
//! 3. **No codec work here** — only raw essence crosses the SDI boundary.
//!
//! # Status
//!
//! Capture is implemented and verified against real hardware. Playout
//! ([`DecklinkPlayout`]) is not implemented; its methods return
//! [`Error::Unsupported`] rather than panicking.

use std::ffi::{CStr, CString};
use std::fmt;
use std::time::{Duration, Instant};

use libdecklink_sys as sys;

/// Blocking wait per `dl_read_frame` call.
const READ_TIMEOUT_MS: i32 = 1_000;
/// If no frame arrives for this long, treat the device as gone so the caller can
/// re-open it. A live SDI input delivers frames continuously — even with no
/// signal, the card emits frames flagged `no_signal`. Silence means the device
/// itself went away.
const NO_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `open` waits for the first video frame before giving up.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// After the first video frame, keep draining briefly so the card's hardware
/// input-format detection can fire and correct the raster / frame rate. We arm
/// the input on a seed mode, so the very first frame may still carry the seed's
/// rate rather than the source's.
const FORMAT_SETTLE: Duration = Duration::from_millis(400);

/// Errors surfaced across the DeckLink boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No DeckLink device matched the requested name.
    #[error("decklink device not found: {0}")]
    DeviceNotFound(String),
    /// The SDK refused to open or configure the input.
    #[error("decklink open failed: {0}")]
    OpenFailed(String),
    /// The device delivered no frames at all (unplugged card, driver fault).
    #[error("no SDI frames from {0}")]
    NoSignal(String),
    /// A capture-time failure.
    #[error("decklink i/o error: {0}")]
    Io(String),
    /// A requested pixel format / mode this build does not map.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Playout backpressure: the card's scheduled-frame queue is full and did
    /// not drain within the write timeout (the card is behind the SDI cadence,
    /// or wedged with playback running but not draining). The frame was not
    /// scheduled. Callers should drop the frame and retry — re-checking their
    /// own cancellation — rather than treat the device as failed.
    #[error("playout busy: scheduled-frame queue full")]
    Busy,
    /// The capture was closed.
    #[error("decklink stream ended")]
    Eof,
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// A DeckLink device as reported by the SDK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecklinkDeviceInfo {
    /// Zero-based enumeration index.
    pub index: u32,
    /// Display name, e.g. `"DeckLink Quad (1)"`. This is what bilbycast-edge
    /// stores in `SdiInputConfig::device`.
    pub name: String,
    /// SDI connector index parsed from a trailing `(N)`, when present.
    pub sdi_channel: Option<u8>,
}

/// Pixel format of a captured frame. SDI is natively packed 4:2:2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecklinkPixelFormat {
    /// 8-bit packed 4:2:2 (`bmdFormat8BitYUV`). DeckLink default.
    Uyvy422,
    /// 10-bit packed 4:2:2 (`bmdFormat10BitYUV`).
    V210,
}

impl fmt::Display for DecklinkPixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecklinkPixelFormat::Uyvy422 => write!(f, "uyvy422"),
            DecklinkPixelFormat::V210 => write!(f, "v210"),
        }
    }
}

impl DecklinkPixelFormat {
    fn as_raw(self) -> i32 {
        match self {
            DecklinkPixelFormat::Uyvy422 => sys::DL_PIXFMT_UYVY422 as i32,
            DecklinkPixelFormat::V210 => sys::DL_PIXFMT_V210 as i32,
        }
    }
}

/// Capture configuration.
#[derive(Debug, Clone)]
pub struct DecklinkCaptureConfig {
    /// Device display name (see [`DecklinkDeviceInfo::name`]).
    pub device: String,
    /// `"auto"` to use the card's hardware input-format detection, otherwise a
    /// DeckLink mode FourCC such as `"Hp50"` / `"Hi50"`.
    pub format: String,
    /// Requested pixel format.
    pub pixel_format: DecklinkPixelFormat,
    /// Embedded-audio channel count (0 disables audio; otherwise 2, 8 or 16).
    pub audio_channels: u8,
    /// Audio sample rate. SDI embedded audio is always 48 kHz.
    pub audio_sample_rate: u32,
}

/// One packed 4:2:2 video frame off the SDI input.
#[derive(Debug, Clone)]
pub struct CapturedVideo {
    /// Presentation timestamp in 90 kHz ticks (the card's stream time).
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub pixel_format: DecklinkPixelFormat,
    /// Packed pixel bytes (UYVY422 / V210 are single-plane packed formats).
    pub data: Vec<u8>,
    /// Row stride in bytes.
    pub stride: usize,
    /// `false` when the card reports `bmdFrameHasNoInputSource` — no cable, or
    /// no lock. The frame still contains pixels (the card substitutes bars or
    /// black), so callers may keep encoding to hold the transport stream up,
    /// but they must raise an alarm.
    pub signal_present: bool,
}

/// One block of interleaved embedded PCM audio off the SDI input.
#[derive(Debug, Clone)]
pub struct CapturedAudio {
    /// Presentation timestamp in 90 kHz ticks.
    pub pts: i64,
    pub channels: u8,
    pub sample_rate: u32,
    /// Interleaved signed 32-bit PCM.
    pub samples: Vec<i32>,
}

/// A frame read from the SDI input.
#[derive(Debug, Clone)]
pub enum CapturedFrame {
    Video(CapturedVideo),
    Audio(CapturedAudio),
}

/// Parse the trailing `(N)` connector index from a device name,
/// e.g. `"DeckLink Quad (3)"` → `Some(3)`.
fn parse_sdi_channel(name: &str) -> Option<u8> {
    let close = name.rfind(')')?;
    let open = name[..close].rfind('(')?;
    name[open + 1..close].trim().parse().ok()
}

/// `bmdModeUnknown` — the SDK's "I have nothing to report" raster FourCC
/// (`'iunk'`), returned in place of an error by several status fields.
const BMD_MODE_UNKNOWN: i64 = 0x6975_6E6B;

/// Decode a DeckLink FourCC status value (e.g. `0x48693530` → `"Hi50"`).
///
/// Returns `None` for the zero sentinel and for any value that is not four
/// printable ASCII bytes — better to report nothing than to hand the operator
/// mojibake.
fn fourcc_to_string(v: i64) -> Option<String> {
    if v == 0 {
        return None;
    }
    // `bmdModeUnknown`. The card returns this rather than failing when it has
    // nothing to report (e.g. `reference_mode` with no reference connected),
    // so map it to `None` instead of handing the operator a fake raster.
    if v == BMD_MODE_UNKNOWN {
        return None;
    }
    let b = [
        ((v >> 24) & 0xFF) as u8,
        ((v >> 16) & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        (v & 0xFF) as u8,
    ];
    if b.iter().any(|c| !c.is_ascii_graphic() && *c != b' ') {
        return None;
    }
    Some(String::from_utf8_lossy(&b).trim().to_string())
}

/// Convert the shim's tri-state (`-1` unknown) into an `Option<bool>`.
fn tri_to_bool(v: i32) -> Option<bool> {
    match v {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// A read-only snapshot of one device's hardware status.
///
/// Every field is `Option` because the card answers per-field: an unlocked
/// input genuinely does not know its colorspace or raster, and older/simpler
/// models decline fields entirely. `None` means "the card did not say", never
/// "no" — conflating those is precisely the failure that motivated moving off
/// FFmpeg's avdevice.
///
/// Cheap and non-invasive: this neither opens nor reserves the device, and can
/// be read while another process (or this one) is capturing from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceStatus {
    /// Input is locked to an SDI signal.
    pub signal_locked: Option<bool>,
    /// Locked to house reference (genlock).
    pub reference_locked: Option<bool>,
    /// Ancillary data stream is locked.
    pub ancillary_locked: Option<bool>,
    /// Device is held open by some process — including this one.
    pub busy: Option<bool>,
    /// Detected input raster as a DeckLink mode FourCC, e.g. `"Hi50"`.
    /// `None` on an unlocked input.
    pub detected_mode: Option<String>,
    /// Detected colorimetry, e.g. `"r709"`.
    pub detected_colorspace: Option<String>,
    /// Detected field dominance, e.g. `"uppr"` (upper field first).
    pub detected_field_dominance: Option<String>,
    /// Detected SDI link configuration, e.g. `"lcsl"` (single link).
    pub sdi_link_config: Option<String>,
    /// Raster of the reference signal. `None` when no reference is connected
    /// (the card reports `bmdModeUnknown` there, which is mapped away).
    pub reference_mode: Option<String>,
    /// `BMDDynamicRange`; `0` is SDR.
    pub detected_dynamic_range: Option<i32>,
    /// PCIe generation the card negotiated, e.g. `2`.
    pub pcie_link_speed: Option<i32>,
    /// PCIe lanes the card negotiated, e.g. `4`.
    pub pcie_link_width: Option<i32>,
}

/// Read the hardware status of the device at `index`.
///
/// Does not open or reserve the device, so it is safe to call for every port on
/// a card while flows are running — that is what makes a whole-card "which
/// inputs have signal?" view possible.
pub fn device_status(index: u32) -> Result<DeviceStatus> {
    let mut raw = sys::dl_device_status::default();
    let rc = unsafe { sys::dl_device_status_get(index as i32, &mut raw) };
    match rc {
        x if x == sys::DL_OK as i32 => {}
        x if x == sys::DL_ERR_NO_DEVICE => {
            return Err(Error::DeviceNotFound(format!("device index {index}")));
        }
        x if x == sys::DL_ERR_UNSUPPORTED => {
            return Err(Error::Unsupported(
                "device does not implement IDeckLinkStatus".into(),
            ));
        }
        other => return Err(Error::Io(format!("dl_device_status_get failed: {other}"))),
    }

    Ok(DeviceStatus {
        signal_locked: tri_to_bool(raw.signal_locked),
        reference_locked: tri_to_bool(raw.reference_locked),
        ancillary_locked: tri_to_bool(raw.ancillary_locked),
        busy: tri_to_bool(raw.busy),
        detected_mode: fourcc_to_string(raw.detected_mode),
        detected_colorspace: fourcc_to_string(raw.detected_colorspace),
        detected_field_dominance: fourcc_to_string(raw.detected_field_dominance),
        sdi_link_config: fourcc_to_string(raw.sdi_link_config),
        reference_mode: fourcc_to_string(raw.reference_mode),
        detected_dynamic_range: (raw.detected_dynamic_range >= 0)
            .then_some(raw.detected_dynamic_range),
        pcie_link_speed: (raw.pcie_link_speed >= 0).then_some(raw.pcie_link_speed),
        pcie_link_width: (raw.pcie_link_width >= 0).then_some(raw.pcie_link_width),
    })
}

/// Enumerate the DeckLink devices on this host.
///
/// Returns an empty vec when Desktop Video is not installed or no card is
/// present, so a probe on a non-SDI host is a clean no-op.
///
/// Unlike FFmpeg's avdevice enumeration, this releases the SDK iterator cleanly
/// and does **not** wedge the device, so it is safe to call before capturing.
pub fn enumerate_devices() -> Vec<DecklinkDeviceInfo> {
    let count = unsafe { sys::dl_device_count() };
    if count <= 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(count as usize);
    let mut buf = [0i8; 256];
    for index in 0..count {
        let rc = unsafe { sys::dl_device_name(index, buf.as_mut_ptr(), buf.len()) };
        if rc != sys::DL_OK as i32 {
            continue;
        }
        let name = unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        out.push(DecklinkDeviceInfo {
            index: index as u32,
            sdi_channel: parse_sdi_channel(&name),
            name,
        });
    }
    out
}

/// Live SDI capture handle. `Send`, not `Sync`; drive `read_frame` on a
/// blocking thread. Closing is via `Drop`.
pub struct DecklinkCapture {
    cap: *mut sys::dl_capture,
    device: String,
    pixel_format: DecklinkPixelFormat,
    width: u32,
    height: u32,
    fr_num: u32,
    fr_den: u32,
    audio_channels: u8,
    audio_sample_rate: u32,
    /// The first video frame, consumed by `open` to learn the raster.
    pending: Option<CapturedVideo>,
}

// The handle is only touched behind `&mut self`, so it may move between threads
// (e.g. onto a `spawn_blocking` worker) but never be shared.
unsafe impl Send for DecklinkCapture {}

impl DecklinkCapture {
    /// Open the DeckLink input described by `cfg`.
    ///
    /// Blocks until the first video frame arrives, so the detected raster and
    /// frame rate are known on return. A card with no signal still delivers
    /// frames (flagged `no_signal`), so this succeeds with the cable out.
    pub fn open(cfg: DecklinkCaptureConfig) -> Result<Self> {
        let device = CString::new(cfg.device.as_str())
            .map_err(|_| Error::DeviceNotFound(cfg.device.clone()))?;
        let mode = CString::new(cfg.format.as_str())
            .map_err(|_| Error::Unsupported(format!("bad format {:?}", cfg.format)))?;

        let mut raw: *mut sys::dl_capture = std::ptr::null_mut();
        let rc = unsafe {
            sys::dl_open(
                device.as_ptr(),
                mode.as_ptr(),
                cfg.pixel_format.as_raw(),
                cfg.audio_channels as i32,
                &mut raw,
            )
        };
        if rc != sys::DL_OK as i32 || raw.is_null() {
            return Err(match rc {
                x if x == sys::DL_ERR_NO_DEVICE => Error::DeviceNotFound(cfg.device.clone()),
                x if x == sys::DL_ERR_UNSUPPORTED => Error::Unsupported(format!(
                    "device {:?} does not support the requested mode/detection",
                    cfg.device
                )),
                x if x == sys::DL_ERR_PARAM => {
                    Error::Unsupported(format!("bad mode {:?}", cfg.format))
                }
                other => Error::OpenFailed(format!("dl_open failed (code {other})")),
            });
        }

        let mut me = DecklinkCapture {
            cap: raw,
            device: cfg.device.clone(),
            pixel_format: cfg.pixel_format,
            width: 0,
            height: 0,
            fr_num: 0,
            fr_den: 1,
            audio_channels: cfg.audio_channels,
            audio_sample_rate: cfg.audio_sample_rate,
            pending: None,
        };

        // Wait for the first video frame so the caller knows the raster.
        let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
        let mut latest = loop {
            if Instant::now() >= deadline {
                return Err(Error::NoSignal(cfg.device.clone()));
            }
            match me.next_raw()? {
                Some(CapturedFrame::Video(v)) => break v,
                // Audio before the first video frame: discard, we need the raster.
                Some(CapturedFrame::Audio(_)) | None => continue,
            }
        };

        // Then let hardware format detection settle. We armed on a seed mode, so
        // the first frame can still report the seed's raster/rate; the detection
        // callback re-arms the input and later frames carry the truth.
        let settle = Instant::now() + FORMAT_SETTLE;
        while Instant::now() < settle {
            match me.next_raw() {
                Ok(Some(CapturedFrame::Video(v))) => latest = v,
                Ok(_) => continue,
                // A transient during re-arm must not fail the open.
                Err(_) => break,
            }
        }

        me.width = latest.width;
        me.height = latest.height;
        me.pending = Some(latest);
        Ok(me)
    }

    /// Detected video raster (width, height).
    pub fn video_dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Detected frame rate (numerator, denominator), e.g. `(50, 1)`.
    pub fn video_frame_rate(&self) -> (u32, u32) {
        (self.fr_num.max(1), self.fr_den.max(1))
    }

    /// Configured embedded-audio channel count.
    pub fn audio_channels(&self) -> u8 {
        self.audio_channels
    }

    /// Audio sample rate (Hz).
    pub fn audio_sample_rate(&self) -> u32 {
        self.audio_sample_rate
    }

    /// Frames dropped inside the shim because the consumer fell behind.
    pub fn dropped_frames(&self) -> u64 {
        unsafe { sys::dl_dropped_frames(self.cap) }
    }

    /// Block until the next video or audio frame is available.
    ///
    /// Returns [`Error::NoSignal`] if the device stops delivering frames
    /// entirely (see [`NO_FRAME_TIMEOUT`]) — that means the *device* went away,
    /// not the signal — or [`Error::Eof`] once the capture is closed.
    ///
    /// Note that **signal loss does not end the stream**: the card keeps
    /// delivering frames with [`CapturedVideo::signal_present`] set to `false`.
    pub fn read_frame(&mut self) -> Result<CapturedFrame> {
        if let Some(v) = self.pending.take() {
            return Ok(CapturedFrame::Video(v));
        }
        let deadline = Instant::now() + NO_FRAME_TIMEOUT;
        loop {
            match self.next_raw()? {
                Some(f) => return Ok(f),
                None => {
                    if Instant::now() >= deadline {
                        return Err(Error::NoSignal(self.device.clone()));
                    }
                }
            }
        }
    }

    /// One `dl_read_frame` attempt. `Ok(None)` == timed out, try again.
    fn next_raw(&mut self) -> Result<Option<CapturedFrame>> {
        let mut f: sys::dl_frame = unsafe { std::mem::zeroed() };
        let rc = unsafe { sys::dl_read_frame(self.cap, &mut f, READ_TIMEOUT_MS) };

        if rc == sys::DL_TIMEOUT as i32 {
            return Ok(None);
        }
        if rc == sys::DL_ERR_STOPPED {
            return Err(Error::Eof);
        }
        if rc != sys::DL_OK as i32 {
            return Err(Error::Io(format!("dl_read_frame failed (code {rc})")));
        }

        // Copy out of the SDK buffer, then release it immediately: holding SDK
        // frames starves the card of capture buffers.
        let out = if f.kind == sys::DL_FRAME_VIDEO as i32 {
            if f.fr_num > 0 && f.fr_den > 0 {
                self.fr_num = f.fr_num as u32;
                self.fr_den = f.fr_den as u32;
            }
            let data = unsafe { std::slice::from_raw_parts(f.data, f.size) }.to_vec();
            CapturedFrame::Video(CapturedVideo {
                pts: f.pts_90khz,
                width: f.width as u32,
                height: f.height as u32,
                pixel_format: self.pixel_format,
                data,
                stride: f.stride as usize,
                signal_present: f.no_signal == 0,
            })
        } else {
            let n = f.size / 4;
            let bytes = unsafe { std::slice::from_raw_parts(f.data, f.size) };
            let mut samples = Vec::with_capacity(n);
            for c in bytes.chunks_exact(4) {
                samples.push(i32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
            CapturedFrame::Audio(CapturedAudio {
                pts: f.pts_90khz,
                channels: f.channels as u8,
                sample_rate: f.sample_rate as u32,
                samples,
            })
        };

        unsafe { sys::dl_release_frame(&mut f) };
        Ok(Some(out))
    }
}

impl Drop for DecklinkCapture {
    fn drop(&mut self) {
        if !self.cap.is_null() {
            unsafe { sys::dl_close(self.cap) };
            self.cap = std::ptr::null_mut();
        }
    }
}

/// Playout configuration. Mirror of [`DecklinkCaptureConfig`] for the output.
#[derive(Debug, Clone)]
pub struct DecklinkPlayoutConfig {
    pub device: String,
    /// SDI mode FourCC (e.g. `"Hi50"`). Playout cannot auto-detect.
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub pixel_format: DecklinkPixelFormat,
    pub audio_channels: u8,
    pub audio_sample_rate: u32,
}

/// Live SDI playout handle (video only today).
///
/// Frames are *scheduled* against the card's clock: `write_video` copies the
/// frame into card memory and queues it after the previous one; playback
/// starts automatically once a small pre-roll is queued, and the call blocks
/// while the in-flight window is full — so the card's cadence paces the
/// caller. Drive it from a blocking thread, exactly like
/// [`DecklinkCapture::read_frame`].
pub struct DecklinkPlayout {
    po: *mut sys::dl_playout,
    device: String,
    width: u32,
    height: u32,
    row_bytes: usize,
    fr_num: u32,
    fr_den: u32,
}

// SAFETY: the shim guards its internal state with a mutex; the handle is
// moved between threads but only used behind `&mut` (Send, not Sync) — the
// same contract as `DecklinkCapture`.
unsafe impl Send for DecklinkPlayout {}

impl DecklinkPlayout {
    /// Open the device for scheduled playout at `cfg.format` (an explicit
    /// DeckLink mode FourCC such as `"Hi50"` — playout has nothing to
    /// auto-detect from).
    ///
    /// When `cfg.width` / `cfg.height` are non-zero they are validated against
    /// the mode's raster, so a caller that already decoded video learns about
    /// a mode/content mismatch here rather than as a garbled picture.
    pub fn open(cfg: DecklinkPlayoutConfig) -> Result<Self> {
        if cfg.pixel_format == DecklinkPixelFormat::V210 {
            return Err(Error::Unsupported(
                "v210 (10-bit) playout is not implemented yet; use uyvy422".into(),
            ));
        }
        let device_c = CString::new(cfg.device.as_str())
            .map_err(|_| Error::OpenFailed("device name contains NUL".into()))?;
        let mode_c = CString::new(cfg.format.as_str())
            .map_err(|_| Error::OpenFailed("mode contains NUL".into()))?;

        let mut po: *mut sys::dl_playout = std::ptr::null_mut();
        let rc = unsafe {
            sys::dl_playout_open(
                device_c.as_ptr(),
                mode_c.as_ptr(),
                sys::DL_PIXFMT_UYVY422 as i32,
                &mut po,
            )
        };
        match rc {
            x if x == sys::DL_OK as i32 => {}
            x if x == sys::DL_ERR_NO_DEVICE => {
                return Err(Error::DeviceNotFound(cfg.device.clone()));
            }
            x if x == sys::DL_ERR_UNSUPPORTED => {
                return Err(Error::Unsupported(format!(
                    "device '{}' does not support playout of mode '{}' at uyvy422",
                    cfg.device, cfg.format
                )));
            }
            x if x == sys::DL_ERR_PARAM => {
                return Err(Error::OpenFailed(format!(
                    "invalid playout mode '{}' (must be a 4-char DeckLink mode FourCC)",
                    cfg.format
                )));
            }
            other => {
                return Err(Error::OpenFailed(format!(
                    "dl_playout_open failed: {other}"
                )));
            }
        }

        let (mut w, mut h, mut rb) = (0i32, 0i32, 0i32);
        let (mut num, mut den) = (0i64, 0i64);
        unsafe { sys::dl_playout_info(po, &mut w, &mut h, &mut rb, &mut num, &mut den) };

        let out = Self {
            po,
            device: cfg.device.clone(),
            width: w as u32,
            height: h as u32,
            row_bytes: rb as usize,
            fr_num: num as u32,
            fr_den: den.max(1) as u32,
        };

        if (cfg.width != 0 && cfg.width != out.width)
            || (cfg.height != 0 && cfg.height != out.height)
        {
            return Err(Error::Unsupported(format!(
                "mode '{}' is {}x{} but the caller's video is {}x{} — pick the \
                 mode matching the content",
                cfg.format, out.width, out.height, cfg.width, cfg.height
            )));
        }
        Ok(out)
    }

    /// Raster of the opened mode.
    pub fn video_dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Frame rate of the opened mode, e.g. `(25000, 1000)`.
    pub fn video_frame_rate(&self) -> (u32, u32) {
        (self.fr_num, self.fr_den)
    }

    /// Bytes per row the card expects; a frame is `row_bytes() * height`.
    pub fn row_bytes(&self) -> usize {
        self.row_bytes
    }

    /// Schedule one UYVY422 frame. `data` must be exactly
    /// `row_bytes() * height` bytes. Blocks while the in-flight window is
    /// full — the card's clock paces the caller — but only up to a bounded
    /// timeout: if the card stops draining it returns [`Error::Busy`] instead
    /// of blocking forever, so a caller can re-check its own cancellation.
    pub fn write_video(&mut self, data: &[u8]) -> Result<()> {
        let rc = unsafe { sys::dl_playout_write_video(self.po, data.as_ptr(), data.len()) };
        match rc {
            x if x == sys::DL_OK as i32 => Ok(()),
            x if x == sys::DL_TIMEOUT as i32 => Err(Error::Busy),
            x if x == sys::DL_ERR_STOPPED => Err(Error::Eof),
            x if x == sys::DL_ERR_PARAM => Err(Error::Io(format!(
                "frame size mismatch: got {} bytes, mode wants {} ({}x{} rows)",
                data.len(),
                self.row_bytes * self.height as usize,
                self.row_bytes,
                self.height,
            ))),
            other => Err(Error::Io(format!(
                "playout write failed on '{}': {other}",
                self.device
            ))),
        }
    }

    /// Frames the card reported as displayed late — the caller fell behind
    /// the SDI cadence. Cumulative.
    pub fn late_frames(&self) -> u64 {
        unsafe { sys::dl_playout_late_frames(self.po) }
    }

    /// Frames the card dropped outright. Cumulative.
    pub fn dropped_frames(&self) -> u64 {
        unsafe { sys::dl_playout_dropped_frames(self.po) }
    }

    /// **Not implemented yet** — returns [`Error::Unsupported`]. Video-only
    /// playout ships first; audio scheduling joins once the video path is
    /// loopback-verified. Deliberately an `Err` rather than `todo!()`: this
    /// crate links into a long-running broadcast binary.
    pub fn write_audio(&mut self, _samples: &[i32], _pts: i64) -> Result<()> {
        Err(Error::Unsupported(
            "SDI playout audio is not implemented yet; video-only".into(),
        ))
    }
}

impl Drop for DecklinkPlayout {
    fn drop(&mut self) {
        if !self.po.is_null() {
            unsafe { sys::dl_playout_close(self.po) };
            self.po = std::ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fourcc_to_string, parse_sdi_channel, tri_to_bool};

    #[test]
    fn parses_connector_index() {
        assert_eq!(parse_sdi_channel("DeckLink Quad (3)"), Some(3));
        assert_eq!(parse_sdi_channel("DeckLink Duo 2"), None);
        assert_eq!(parse_sdi_channel("Weird (x)"), None);
    }

    #[test]
    fn decodes_status_fourcc() {
        // Values observed from a DeckLink Quad 2 on a live 1080i50 source.
        assert_eq!(fourcc_to_string(1_214_854_448).as_deref(), Some("Hi50"));
        assert_eq!(fourcc_to_string(1_916_219_449).as_deref(), Some("r709"));
        assert_eq!(fourcc_to_string(1_818_456_940).as_deref(), Some("lcsl"));
        // Zero is the "card declined to answer" sentinel, not a mode.
        assert_eq!(fourcc_to_string(0), None);
        // Non-printable bytes must not reach the operator as mojibake.
        assert_eq!(fourcc_to_string(1), None);
        // Modes shorter than 4 chars are space-padded by the SDK ("pal ").
        assert_eq!(fourcc_to_string(0x7061_6C20).as_deref(), Some("pal"));
        // `bmdModeUnknown` ('iunk') is the card saying nothing, not a raster.
        assert_eq!(fourcc_to_string(0x6975_6E6B), None);
    }

    #[test]
    fn tri_state_never_confuses_unknown_with_false() {
        assert_eq!(tri_to_bool(1), Some(true));
        assert_eq!(tri_to_bool(0), Some(false));
        assert_eq!(tri_to_bool(-1), None);
    }
}
