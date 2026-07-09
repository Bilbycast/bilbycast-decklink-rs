// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Safe SDI capture/playout over Blackmagic DeckLink cards, via FFmpeg's
//! `decklink` avdevice.
//!
//! This crate is the DeckLink analogue of `video-engine` in
//! `bilbycast-ffmpeg-video-rs`: bilbycast-edge depends on it (behind the
//! off-by-default `sdi-decklink` feature) and drives it from
//! `engine::sdi_io`, mirroring how `engine::mxl_video_io` drives `mxl-rs`.
//!
//! # Model
//!
//! FFmpeg's `decklink` **input** device presents an [`AVFormatContext`] with a
//! raw-video stream (typically `uyvy422`, or `v210` for 10-bit) and, when
//! audio is enabled, a PCM stream (up to 16 channels of `pcm_s16le` /
//! `pcm_s32le`). Capture is a blocking `av_read_frame` loop yielding
//! [`CapturedFrame::Video`] / [`CapturedFrame::Audio`] — the caller runs it on
//! a `spawn_blocking` / `block_in_place` thread, exactly like the MXL grain
//! reader. The **output** device is the mirror: PCM + raw video AVPackets
//! written via `av_interleaved_write_frame`.
//!
//! # Design constraints (mirroring bilbycast-ffmpeg-video-rs)
//!
//! 1. **Send but not Sync** — handles move between threads but need `&mut`.
//! 2. **Blocking** — `read_frame` / `write_*` block on the SDI hardware
//!    cadence; always drive them off the Tokio reactor.
//! 3. **No libavcodec transcode here** — this crate only moves *raw* essence
//!    across the SDI boundary. Encode/decode stays in `video-engine` in
//!    bilbycast-edge, so the two FFmpeg builds never fight over codecs.
//!
//! # Status
//!
//! * [`enumerate_devices`] and [`DecklinkCapture`] are implemented and verified
//!   against real hardware (a DeckLink Quad 2 on bilby-z440).
//! * [`DecklinkPlayout`] is **not implemented**; its methods return
//!   [`Error::Unsupported`] rather than panicking.
//!
//! See the workspace `CLAUDE.md` for the FFmpeg-with-decklink build recipe and
//! the known-good bilbycast-edge config.

use std::fmt;

#[allow(unused_imports)]
use libdecklink_sys as sys;

/// Errors surfaced across the DeckLink boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No DeckLink device matched the requested name/index.
    #[error("decklink device not found: {0}")]
    DeviceNotFound(String),
    /// FFmpeg rejected the device open (bad format string, device busy,
    /// no signal, SDK not linked). Carries the decoded `av_strerror` text.
    #[error("decklink open failed: {0}")]
    OpenFailed(String),
    /// The SDI input reported no locked signal within the timeout.
    #[error("no SDI signal on {0}")]
    NoSignal(String),
    /// An `av_read_frame` / `av_*_write_frame` call failed mid-stream.
    #[error("decklink i/o error: {0}")]
    Io(String),
    /// A pixel/sample format the caller asked for isn't one this build maps.
    #[error("unsupported format: {0}")]
    Unsupported(String),
    /// End of stream (output closed, or capture cancelled).
    #[error("decklink stream ended")]
    Eof,
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// A DeckLink device as enumerated by FFmpeg's avdevice layer.
///
/// The `name` is the exact string FFmpeg's `decklink` device expects as its
/// URL (e.g. `"DeckLink Quad 2 (1)"`), i.e. what
/// `ffmpeg -f decklink -list_devices true -i ""` prints. bilbycast-edge stores
/// this verbatim in `SdiInputConfig::device` / `SdiOutputConfig::device`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecklinkDeviceInfo {
    /// Zero-based enumeration index within this host.
    pub index: u32,
    /// Human-friendly device name (FFmpeg's `device_description`, e.g.
    /// `"DeckLink Quad (3)"`). This is what bilbycast-edge stores in
    /// `SdiInputConfig::device` / `SdiOutputConfig::device` and what the
    /// manager UI shows — FFmpeg's `decklink` device accepts it verbatim as
    /// the `-i` argument.
    pub name: String,
    /// Stable persistent hardware ID (FFmpeg's `device_name`, e.g.
    /// `"80:3142d352:00000000"`). Survives card reordering / renaming, so it's
    /// the durable key; FFmpeg also accepts it as the device URL. Kept so the
    /// UI can pin a config to a physical connector if the operator wants.
    pub persistent_id: String,
    /// SDI sub-device / connector index parsed from the trailing `(N)` of
    /// [`Self::name`], if present (the Quad enumerates one device per SDI
    /// connector).
    pub sdi_channel: Option<u8>,
    /// Whether the device can capture (input) on this host.
    pub can_input: bool,
    /// Whether the device can play out (output) on this host.
    pub can_output: bool,
}

/// Pixel format of a captured / playout video frame. SDI is natively packed
/// 4:2:2; we surface both the common 8-bit `UYVY422` and 10-bit `V210` so the
/// bilbycast-edge side can unpack to the planar YUV its encoder wants (the same
/// V210→planar step `engine::mxl::video` already implements).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecklinkPixelFormat {
    /// 8-bit packed 4:2:2 (FFmpeg `uyvy422`). DeckLink default.
    Uyvy422,
    /// 10-bit packed 4:2:2 (FFmpeg `v210`). Preferred for 10-bit contribution.
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

/// Capture configuration. `format` follows FFmpeg's `decklink` naming
/// (`"auto"`, `"1080i50"`, `"1080p2997"`, …). `audio_channels` of 0 disables
/// audio capture; otherwise 2/8/16 per the SDI embedded-audio group count.
#[derive(Debug, Clone)]
pub struct DecklinkCaptureConfig {
    /// FFmpeg device name (see [`DecklinkDeviceInfo::name`]).
    pub device: String,
    /// SDI mode string, or `"auto"` to let the card detect the input raster.
    pub format: String,
    /// Requested pixel format. `V210` yields true 10-bit; `Uyvy422` is 8-bit.
    pub pixel_format: DecklinkPixelFormat,
    /// Embedded-audio channel count to demux (0 = video only).
    pub audio_channels: u8,
    /// Audio sample rate — SDI embedded audio is always 48 kHz.
    pub audio_sample_rate: u32,
}

/// Playout configuration. Mirror of [`DecklinkCaptureConfig`] for the output
/// device; `format` here is required (playout can't `"auto"`-detect).
#[derive(Debug, Clone)]
pub struct DecklinkPlayoutConfig {
    /// FFmpeg device name.
    pub device: String,
    /// SDI mode string (e.g. `"1080i50"`). Must be concrete.
    pub format: String,
    /// Output frame width in pixels.
    pub width: u32,
    /// Output frame height in pixels.
    pub height: u32,
    /// Frame-rate numerator / denominator (e.g. 25/1, 30000/1001).
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    /// Pixel format to hand the card.
    pub pixel_format: DecklinkPixelFormat,
    /// Embedded-audio channel count to mux (0 = video only).
    pub audio_channels: u8,
    /// Audio sample rate (48 kHz for SDI).
    pub audio_sample_rate: u32,
}

/// One packed 4:2:2 video frame off the SDI input.
#[derive(Debug, Clone)]
pub struct CapturedVideo {
    /// Presentation timestamp in the stream time base, rescaled to 90 kHz by
    /// the caller (bilbycast-edge anchors PTS on its master clock anyway).
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub pixel_format: DecklinkPixelFormat,
    /// Packed pixel bytes (single plane — UYVY422 / V210 are packed formats).
    pub data: Vec<u8>,
    /// Row stride in bytes (V210 rounds width up to a 48-pixel multiple).
    pub stride: usize,
}

/// One block of interleaved embedded PCM audio off the SDI input.
#[derive(Debug, Clone)]
pub struct CapturedAudio {
    /// Presentation timestamp in the stream time base.
    pub pts: i64,
    /// Channel count in this block.
    pub channels: u8,
    /// Sample rate (48 kHz).
    pub sample_rate: u32,
    /// Interleaved signed 32-bit PCM (DeckLink delivers 32-bit; the caller
    /// down-shifts to the width its audio encoder wants).
    pub samples: Vec<i32>,
}

/// A frame read from the SDI input — either video or audio essence. The
/// capture loop interleaves these in arrival order, matching how bilbycast-edge
/// feeds its per-flow video encode and audio bus.
#[derive(Debug, Clone)]
pub enum CapturedFrame {
    Video(CapturedVideo),
    Audio(CapturedAudio),
}

/// Decode an FFmpeg negative error code into human text via `av_strerror`.
fn av_err(code: i32) -> String {
    let mut buf = [0i8; 256];
    unsafe {
        sys::av_strerror(code, buf.as_mut_ptr(), buf.len());
        std::ffi::CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

/// FFmpeg's `raw_format` device-option value for a pixel format.
fn raw_format_opt(pf: DecklinkPixelFormat) -> &'static str {
    match pf {
        DecklinkPixelFormat::Uyvy422 => "uyvy422",
        DecklinkPixelFormat::V210 => "yuv422p10",
    }
}

/// Row stride in bytes for a packed 4:2:2 frame of the given width.
fn packed_stride(pf: DecklinkPixelFormat, width: u32) -> usize {
    let w = width as usize;
    match pf {
        // 2 bytes per pixel (Cb Y Cr Y over 2 px = 4 bytes).
        DecklinkPixelFormat::Uyvy422 => w * 2,
        // v210: 48 pixels → 128 bytes, width rounded up to a 48 multiple.
        DecklinkPixelFormat::V210 => w.div_ceil(48) * 128,
    }
}

/// Live SDI capture handle. `Send`, not `Sync`; drive `read_frame` on a
/// blocking thread. Closing is via `Drop`.
pub struct DecklinkCapture {
    fmt_ctx: *mut sys::AVFormatContext,
    pkt: *mut sys::AVPacket,
    video_idx: i32,
    audio_idx: i32,
    width: u32,
    height: u32,
    fr_num: u32,
    fr_den: u32,
    pixel_format: DecklinkPixelFormat,
    audio_channels: u8,
    audio_sample_rate: u32,
}

// The AVFormatContext is only touched behind `&mut self`, so the handle is safe
// to move between threads (e.g. onto a `spawn_blocking` worker) but not shared.
unsafe impl Send for DecklinkCapture {}

impl DecklinkCapture {
    /// Open the DeckLink input device described by `cfg`.
    ///
    /// Builds the FFmpeg option dict (`format_code`, `raw_format`, `channels`,
    /// `audio_depth`), calls `avformat_open_input` with
    /// `av_find_input_format("decklink")`, and probes the streams. Returns
    /// [`Error::NoSignal`] when no video stream materialises, or
    /// [`Error::OpenFailed`] (with decoded `av_strerror`) otherwise.
    pub fn open(cfg: DecklinkCaptureConfig) -> Result<Self> {
        use std::ffi::CString;
        use std::ptr;

        let dev = CString::new(cfg.device.as_str())
            .map_err(|_| Error::DeviceNotFound(cfg.device.clone()))?;
        let kind = CString::new("decklink").unwrap();

        unsafe {
            sys::avdevice_register_all();
            let ifmt = sys::av_find_input_format(kind.as_ptr());
            if ifmt.is_null() {
                return Err(Error::OpenFailed(
                    "decklink input format missing (FFmpeg lacks --enable-decklink)".into(),
                ));
            }

            // ── device options ──
            let mut opts: *mut sys::AVDictionary = ptr::null_mut();
            let set = |opts: &mut *mut sys::AVDictionary, k: &str, v: &str| {
                let ck = CString::new(k).unwrap();
                let cv = CString::new(v).unwrap();
                sys::av_dict_set(opts, ck.as_ptr(), cv.as_ptr(), 0);
            };
            let set_int = |opts: &mut *mut sys::AVDictionary, k: &str, v: i64| {
                let ck = CString::new(k).unwrap();
                sys::av_dict_set_int(opts, ck.as_ptr(), v, 0);
            };
            // "auto" relies on the card's input format detection; a concrete
            // value (e.g. "Hi50") pins the raster.
            if cfg.format != "auto" {
                set(&mut opts, "format_code", &cfg.format);
            }
            set(&mut opts, "raw_format", raw_format_opt(cfg.pixel_format));
            let ch = if cfg.audio_channels == 0 {
                2
            } else {
                cfg.audio_channels as i64
            };
            set_int(&mut opts, "channels", ch);
            // 32-bit PCM so the CapturedAudio i32 sample contract is exact.
            set_int(&mut opts, "audio_depth", 32);

            let mut fmt_ctx: *mut sys::AVFormatContext = ptr::null_mut();
            let ret =
                sys::avformat_open_input(&mut fmt_ctx, dev.as_ptr(), ifmt as *const _, &mut opts);
            sys::av_dict_free(&mut opts);
            if ret < 0 {
                return Err(Error::OpenFailed(av_err(ret)));
            }

            let ret = sys::avformat_find_stream_info(fmt_ctx, ptr::null_mut());
            if ret < 0 {
                sys::avformat_close_input(&mut fmt_ctx);
                return Err(Error::OpenFailed(format!(
                    "find_stream_info: {}",
                    av_err(ret)
                )));
            }

            // ── locate the video + audio streams ──
            let streams =
                std::slice::from_raw_parts((*fmt_ctx).streams, (*fmt_ctx).nb_streams as usize);
            let (mut video_idx, mut audio_idx) = (-1i32, -1i32);
            let (mut width, mut height, mut sr, mut ach) =
                (0u32, 0u32, cfg.audio_sample_rate, ch as u8);
            let (mut fr_num, mut fr_den) = (25u32, 1u32);
            for (i, &s) in streams.iter().enumerate() {
                let par = (*s).codecpar;
                if par.is_null() {
                    continue;
                }
                let ct = (*par).codec_type;
                if ct == sys::AVMediaType_AVMEDIA_TYPE_VIDEO {
                    video_idx = i as i32;
                    width = (*par).width as u32;
                    height = (*par).height as u32;
                    // Real (constant) frame rate of the SDI raster.
                    let r = (*s).r_frame_rate;
                    if r.num > 0 && r.den > 0 {
                        fr_num = r.num as u32;
                        fr_den = r.den as u32;
                    }
                } else if ct == sys::AVMediaType_AVMEDIA_TYPE_AUDIO {
                    audio_idx = i as i32;
                    sr = (*par).sample_rate as u32;
                    ach = (*par).ch_layout.nb_channels as u8;
                }
            }
            if video_idx < 0 {
                sys::avformat_close_input(&mut fmt_ctx);
                return Err(Error::NoSignal(cfg.device.clone()));
            }

            let pkt = sys::av_packet_alloc();
            Ok(DecklinkCapture {
                fmt_ctx,
                pkt,
                video_idx,
                audio_idx,
                width,
                height,
                fr_num,
                fr_den,
                pixel_format: cfg.pixel_format,
                audio_channels: ach,
                audio_sample_rate: sr,
            })
        }
    }

    /// Detected video raster (width, height) after `open`.
    pub fn video_dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Detected constant frame rate (numerator, denominator) after `open`,
    /// e.g. `(25, 1)` for 1080i50, `(30000, 1001)` for 1080p29.97.
    pub fn video_frame_rate(&self) -> (u32, u32) {
        (self.fr_num, self.fr_den)
    }

    /// Detected embedded-audio channel count after `open`.
    pub fn audio_channels(&self) -> u8 {
        self.audio_channels
    }

    /// Detected audio sample rate (Hz) after `open`.
    pub fn audio_sample_rate(&self) -> u32 {
        self.audio_sample_rate
    }

    /// Block until the next video or audio frame is available, returning it.
    ///
    /// Wraps `av_read_frame`; demultiplexes by stream index into
    /// [`CapturedFrame`]. Returns [`Error::Eof`] when the device closes / errors.
    pub fn read_frame(&mut self) -> Result<CapturedFrame> {
        unsafe {
            loop {
                sys::av_packet_unref(self.pkt);
                let ret = sys::av_read_frame(self.fmt_ctx, self.pkt);
                if ret < 0 {
                    return Err(Error::Eof);
                }
                let idx = (*self.pkt).stream_index;
                let size = (*self.pkt).size as usize;
                let bytes = std::slice::from_raw_parts((*self.pkt).data, size);
                if idx == self.video_idx {
                    return Ok(CapturedFrame::Video(CapturedVideo {
                        pts: (*self.pkt).pts,
                        width: self.width,
                        height: self.height,
                        pixel_format: self.pixel_format,
                        data: bytes.to_vec(),
                        stride: packed_stride(self.pixel_format, self.width),
                    }));
                } else if idx == self.audio_idx {
                    // interleaved s32le (audio_depth=32)
                    let mut samples = Vec::with_capacity(size / 4);
                    for c in bytes.chunks_exact(4) {
                        samples.push(i32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                    }
                    return Ok(CapturedFrame::Audio(CapturedAudio {
                        pts: (*self.pkt).pts,
                        channels: self.audio_channels,
                        sample_rate: self.audio_sample_rate,
                        samples,
                    }));
                }
                // Unknown stream — keep reading.
            }
        }
    }
}

impl Drop for DecklinkCapture {
    fn drop(&mut self) {
        unsafe {
            if !self.pkt.is_null() {
                sys::av_packet_free(&mut self.pkt);
            }
            if !self.fmt_ctx.is_null() {
                sys::avformat_close_input(&mut self.fmt_ctx);
            }
        }
    }
}

/// Live SDI playout handle. `Send`, not `Sync`; drive the `write_*` calls on a
/// blocking thread. Closing (write trailer) is via `Drop`.
///
/// **Not implemented yet** — every constructor returns [`Error::Unsupported`],
/// so this type is never instantiated today.
#[allow(dead_code)]
pub struct DecklinkPlayout {
    _cfg: DecklinkPlayoutConfig,
}

/// Error text returned by every not-yet-implemented [`DecklinkPlayout`] method.
const PLAYOUT_UNIMPLEMENTED: &str =
    "SDI playout (DecklinkPlayout) is not implemented yet; only capture is supported";

impl DecklinkPlayout {
    /// Open the DeckLink output device described by `cfg`.
    ///
    /// **Not implemented yet** — returns [`Error::Unsupported`]. When it lands:
    /// `avformat_alloc_output_context2(NULL, "decklink", device)`, add a raw
    /// video stream (+ optional PCM audio stream), set the mode via the option
    /// dict, and `avformat_write_header`.
    ///
    /// Deliberately an `Err` rather than `todo!()`: this crate is linked into a
    /// long-running broadcast binary, where a panic would take down the process.
    pub fn open(_cfg: DecklinkPlayoutConfig) -> Result<Self> {
        Err(Error::Unsupported(PLAYOUT_UNIMPLEMENTED.to_string()))
    }

    /// Submit one packed 4:2:2 video frame for playout at `pts`.
    ///
    /// **Not implemented yet** — returns [`Error::Unsupported`].
    pub fn write_video(&mut self, _data: &[u8], _pts: i64) -> Result<()> {
        Err(Error::Unsupported(PLAYOUT_UNIMPLEMENTED.to_string()))
    }

    /// Submit one block of interleaved 32-bit PCM for playout at `pts`.
    ///
    /// **Not implemented yet** — returns [`Error::Unsupported`].
    pub fn write_audio(&mut self, _samples: &[i32], _pts: i64) -> Result<()> {
        Err(Error::Unsupported(PLAYOUT_UNIMPLEMENTED.to_string()))
    }
}

/// Enumerate DeckLink devices visible to FFmpeg's avdevice layer on this host.
///
/// Calls `avdevice_register_all` then `avdevice_list_input_sources` /
/// `avdevice_list_output_sinks` against the `decklink` device (FFmpeg's
/// `ff_decklink_list_devices`). Returns an empty vec on a host without a
/// DeckLink card or without an `--enable-decklink` FFmpeg — bilbycast-edge's
/// `hardware_probe` treats empty as "SDI unavailable" and simply doesn't
/// advertise the `sdi-decklink` capability, exactly like the MXL probe gates
/// its capability bits.
///
/// Deliberately infallible (returns empty, never panics) so a probe on a
/// non-SDI host is a clean no-op.
///
/// # Warning — wedges the device (`decklink-enumerate-wedge`)
///
/// FFmpeg's decklink discovery is **not** released by
/// `avdevice_free_list_devices`, so after calling this a later
/// [`DecklinkCapture::open`] on the same device fails with `EIO` for the rest of
/// the process lifetime. Do **not** call this in a process that will also
/// capture. bilbycast-edge's boot probe deliberately skips it for this reason.
pub fn enumerate_devices() -> Vec<DecklinkDeviceInfo> {
    use std::collections::BTreeMap;
    use std::ffi::{CStr, CString};

    // persistent_id -> (friendly_name, can_input, can_output). Keyed on the
    // stable device_name (persistent hardware ID) so the same physical
    // connector merges its input + output capability into one entry. BTreeMap
    // keeps a deterministic ordering across probes.
    let mut merged: BTreeMap<String, (String, bool, bool)> = BTreeMap::new();

    // Fold one AVDeviceInfoList into `merged`, setting the input or output bit.
    // Safety: `list` is a valid, non-null pointer returned by an
    // `avdevice_list_*` call; entries and their C strings live until
    // `avdevice_free_list_devices`.
    unsafe fn absorb(
        list: *mut sys::AVDeviceInfoList,
        is_input: bool,
        merged: &mut BTreeMap<String, (String, bool, bool)>,
    ) {
        let devs = std::slice::from_raw_parts((*list).devices, (*list).nb_devices as usize);
        for &d in devs {
            if d.is_null() || (*d).device_name.is_null() {
                continue;
            }
            let Ok(id) = CStr::from_ptr((*d).device_name).to_str() else {
                continue;
            };
            let friendly = if (*d).device_description.is_null() {
                id.to_string()
            } else {
                CStr::from_ptr((*d).device_description)
                    .to_str()
                    .unwrap_or(id)
                    .to_string()
            };
            let entry = merged
                .entry(id.to_string())
                .or_insert((friendly.clone(), false, false));
            // Prefer a non-empty friendly name if a later list has one.
            if entry.0.is_empty() && !friendly.is_empty() {
                entry.0 = friendly;
            }
            if is_input {
                entry.1 = true;
            } else {
                entry.2 = true;
            }
        }
    }

    let Ok(dev_kind) = CString::new("decklink") else {
        return Vec::new();
    };

    unsafe {
        sys::avdevice_register_all();

        // ── input sources ──
        let ifmt = sys::av_find_input_format(dev_kind.as_ptr());
        if !ifmt.is_null() {
            let mut list: *mut sys::AVDeviceInfoList = std::ptr::null_mut();
            let n = sys::avdevice_list_input_sources(
                ifmt,
                std::ptr::null(),
                std::ptr::null_mut(),
                &mut list,
            );
            if n >= 0 && !list.is_null() {
                absorb(list, true, &mut merged);
            }
            if !list.is_null() {
                sys::avdevice_free_list_devices(&mut list);
            }
        }

        // ── output sinks ──
        let ofmt = sys::av_guess_format(dev_kind.as_ptr(), std::ptr::null(), std::ptr::null());
        if !ofmt.is_null() {
            let mut list: *mut sys::AVDeviceInfoList = std::ptr::null_mut();
            let n = sys::avdevice_list_output_sinks(
                ofmt,
                std::ptr::null(),
                std::ptr::null_mut(),
                &mut list,
            );
            if n >= 0 && !list.is_null() {
                absorb(list, false, &mut merged);
            }
            if !list.is_null() {
                sys::avdevice_free_list_devices(&mut list);
            }
        }
    }

    merged
        .into_iter()
        .enumerate()
        .map(
            |(index, (persistent_id, (name, can_input, can_output)))| DecklinkDeviceInfo {
                index: index as u32,
                sdi_channel: parse_sdi_channel(&name),
                name,
                persistent_id,
                can_input,
                can_output,
            },
        )
        .collect()
}

/// Parse the trailing `(N)` sub-device index from a DeckLink device name,
/// e.g. `"DeckLink Quad 2 (3)"` → `Some(3)`. `None` when absent.
fn parse_sdi_channel(name: &str) -> Option<u8> {
    let close = name.rfind(')')?;
    let open = name[..close].rfind('(')?;
    name[open + 1..close].trim().parse().ok()
}
