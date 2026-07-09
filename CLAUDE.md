# CLAUDE.md — bilbycast-decklink-rs

## What Is This

Rust wrapper around FFmpeg's **libavdevice / libavformat** `decklink` device
for Blackmagic **DeckLink** SDI capture and playout, for bilbycast-edge. It is
the SDI sibling of `bilbycast-ffmpeg-video-rs` (codec layer) and
`bilbycast-mxl-rs` (MXL shared-memory layer), used only when bilbycast-edge is
built with `--features sdi-decklink` (default **off**).

Deliberately a **separate crate** from `bilbycast-ffmpeg-video-rs`: that crate
builds a minimal FFmpeg with `--disable-avdevice --disable-avformat` and zero
proprietary linkage (LGPL-clean, redistributable). DeckLink needs *both* of
those libs plus the proprietary **Blackmagic DeckLink SDK** headers and
`--enable-nonfree` at FFmpeg-configure time. Isolating that here keeps the
codec crate clean and keeps the non-free linkage behind one opt-in feature.

## Projects

| Crate | Role |
|-------|------|
| **libdecklink-sys** | Raw FFI (bindgen) over libavdevice/libavformat/libavcodec/libavutil. `links = "avdevice"`. |
| **decklink-rs** | Safe wrapper — `DecklinkCapture`, `DecklinkPlayout`, `enumerate_devices`, and the `Captured*` / `Decklink*Config` types. The crate bilbycast-edge depends on. |

## The FFmpeg-with-DeckLink prerequisite (the crux)

`-f decklink` only exists in an FFmpeg configured against the DeckLink SDK:

```bash
# 1. Get the Blackmagic DeckLink SDK (accept the EULA) — Linux/include only:
#    DeckLinkAPI.h, DeckLinkAPIVersion.h etc. The Desktop Video *runtime* deb
#    is NOT enough (it ships libDeckLinkAPI.so but no headers).
export DECKLINK_SDK_DIR="$HOME/decklink-sdk-include"   # the SDK's Linux/include

# 2. Build FFmpeg with avdevice + avformat + the decklink device.
#    IMPORTANT — version + flags, both learned the hard way on bilby-z440:
#    - Use FFmpeg **>= 8.0** (n8.1.2 verified). FFmpeg 7.1.x's DeckLink code
#      predates SDK 16.0's IDeckLinkVideoInputFrame / memory-allocator API and
#      will NOT compile against it.
#    - Pass the SDK include via BOTH --extra-cflags AND **--extra-cxxflags**.
#      FFmpeg 8.x compiles the decklink_*.cpp device files as C++ and no longer
#      folds --extra-cflags into the C++ compile, so cxxflags is required or
#      DeckLinkAPIVersion.h is "not found".
git clone --depth 1 --branch n8.1.2 https://github.com/FFmpeg/FFmpeg.git ffmpeg-src
cd ffmpeg-src
./configure --prefix="$HOME/ffmpeg-decklink" \
  --enable-avdevice --enable-avformat --enable-avcodec --enable-avutil \
  --enable-swscale --enable-swresample \
  --enable-decklink --enable-nonfree \
  --extra-cflags="-I$DECKLINK_SDK_DIR" \
  --extra-cxxflags="-I$DECKLINK_SDK_DIR" \
  --extra-ldflags="-L$DECKLINK_SDK_DIR" \
  --enable-shared --disable-static \
  --disable-programs --enable-ffmpeg --disable-doc
make -j"$(nproc)" && make install

# 3. Point this crate at that prefix (runtime needs LD_LIBRARY_PATH too):
export LIBDECKLINK_FFMPEG_DIR="$HOME/ffmpeg-decklink"
export LD_LIBRARY_PATH="$HOME/ffmpeg-decklink/lib:/lib"
export PKG_CONFIG_PATH="$HOME/ffmpeg-decklink/lib/pkgconfig"
cargo run -p decklink-rs --example list_devices   # enumerate the live card
```

Alternatively `--features system-ffmpeg` pkg-configs a system FFmpeg that
already has `--enable-decklink` (rare on distro packages).

The build script does **not** vendor+build FFmpeg itself: the SDK EULA and the
non-free flag mean the operator must stage the SDK explicitly. See
`libdecklink-sys/build.rs`.

## Build & Test

```bash
# On bilby-z440 (the SDI test host — DeckLink Quad 2, 8 channels):
export LIBDECKLINK_FFMPEG_DIR="$HOME/ffmpeg-decklink"
cargo build
cargo test                       # enumerate_devices() against real hardware
```

### Prerequisites

- **Clang/LLVM** (bindgen), **pkg-config**.
- An FFmpeg built with `--enable-decklink` (see above) reachable via
  `LIBDECKLINK_FFMPEG_DIR` or `--features system-ffmpeg`.
- **Blackmagic Desktop Video** runtime installed (kernel module + `libDeckLinkAPI.so`).
  On bilby-z440: Desktop Video 16.0.1a2, firmware 0x128, `/dev/blackmagic/io0`–`io7`.

## Architecture

- FFmpeg's `decklink` **indev** yields an `AVFormatContext` with a raw-video
  stream (`uyvy422` 8-bit or `v210` 10-bit) plus, optionally, a PCM stream
  (up to 16 ch `pcm_s16le`/`pcm_s32le`). `DecklinkCapture::read_frame` is a
  blocking `av_read_frame` loop demuxing into `CapturedFrame::{Video,Audio}`.
- The `decklink` **outdev** is the mirror: raw video + PCM AVPackets via
  `av_interleaved_write_frame` (`DecklinkPlayout`).
- **No codec work here.** Only raw essence crosses the SDI boundary. Encode
  (SDI-in → H.264/HEVC → TS) and decode (TS → YUV → SDI-out) stay in
  bilbycast-edge's `video-engine`, so the two FFmpeg builds never overlap on
  codecs. This is the same split MXL uses (V210 grains in/out; encode in edge).

## Key Design Constraints

1. **Send but not Sync** — handles move between threads, need `&mut`.
2. **Blocking API** — `read_frame` / `write_*` block on the SDI cadence; the
   bilbycast-edge side drives them under `spawn_blocking` / `block_in_place`,
   exactly like the MXL grain reader/writer.
3. **Feature-gated off in bilbycast-edge** (`sdi-decklink`). Never default-on —
   both the build prereq footprint and the non-free FFmpeg linkage warrant it.
4. **Infallible enumeration** — `enumerate_devices()` returns empty (never
   panics) on non-SDI hosts, so the edge `hardware_probe` degrades cleanly.

## Integration with bilbycast-edge

Path dependency, gated by the `sdi-decklink` feature:

```toml
[dependencies]
decklink-rs = { path = "../bilbycast-decklink-rs/decklink-rs", optional = true }

[features]
sdi-decklink = ["dep:decklink-rs"]
```

Driven from `engine::sdi_io` (capture → `video-engine` encode → `TsMuxer` →
broadcast; broadcast → `TsDemuxer` → `video-engine` decode → playout),
mirroring `engine::mxl_video_io`. Targets upstream issue
[bilbycast-edge#19](https://github.com/Bilbycast/bilbycast-edge/issues/19).

## Known-good bilbycast-edge SDI config

Verified end-to-end on bilby-z440 (live 1080p50 source → NVENC → SRT, correct
colours, audio, no freezing). Run the edge with `BILBYCAST_PROBE_SESSION_LIMITS=0`.

```json
{ "id": "sdi1", "name": "SDI 1", "type": "sdi",
  "device": "DeckLink Quad (1)",
  "format": "auto",
  "pixel_format": "uyvy422",
  "audio_channels": 2,
  "video_encode": {
    "codec": "h264_nvenc", "chroma": "yuv420p",
    "tune": "", "preset": "fast", "rate_control": "cbr",
    "bitrate_kbps": 10000, "gop_size": 50
  }
}
```

Gotchas the hard way:

* **`format` must be `"auto"`.** A forced `format_code` (e.g. `Hp50`) makes the
  card emit its internal **no-signal colour bars** even when a valid signal is
  present.
* **`tune` must be `""` for NVENC.** The edge defaults `tune` to
  `"zerolatency"`, an x264-only tune — NVENC rejects it and `avcodec_open2`
  fails with `EINVAL` (`-22`).
* **`chroma` must be `"yuv420p"` for `h264_nvenc`** (h264 NVENC has no 4:2:2
  path; only `hevc_nvenc` does).
* **Bitrate**: 25 Mbps overran the SRT egress here (subscriber lagged, ~0.4 %
  TS packet loss → visible freezing). 10 Mbps is clean.
* **Always benchmark the `--release` build** — debug is ~4× the CPU.

## Upstream bilbycast-edge bugs found during bring-up

Worth reporting alongside the SDI PR — none are SDI-specific:

1. `video_encode_util::build_encoder_config` defaults `tune = "zerolatency"`,
   which is invalid for every NVENC backend ⇒ `avcodec_open2` EINVAL.
2. `video_encode_util::try_build_scaler` skips conversion when
   `dims_match && is_planar_yuv(src)` — it never checks the planar layout
   matches the **encoder's** chroma. A planar 4:2:2 source with a 4:2:0
   encoder target is fed through unconverted, so the encoder reads chroma from
   the wrong rows (perfect luma, ghosted/smeared chroma). Also affects
   ST 2110-20 ingest with a 4:2:2 source.
3. Ingress encoder failures were reported only via `event_sender.emit`
   (manager events), so a standalone edge fails silently. `sdi_io.rs` now also
   logs them via `tracing`.

## Status

- ✅ **`enumerate_devices()` — verified on bilby-z440** (2026-07-01).
  Builds against FFmpeg n8.1.2 + DeckLink SDK 16.0 and enumerates the live
  DeckLink Quad: 8 devices `"DeckLink Quad (1)"`..`"(8)"`, persistent IDs
  `80:3142d35X:00000000`, `sdi_channel` 1..8, all input+output capable.
- ✅ **`DecklinkCapture` (open + read_frame) — verified against a live SDI
  source** on port 1 (2026-07-01). 1080i50 auto/`Hi50`, UYVY422 8-bit video
  (stride 3840, 4,147,200 B/frame) + 48 kHz stereo s32 PCM, at realtime.
  Example: `cargo run -p decklink-rs --example capture_probe -- "DeckLink Quad (1)" Hi50`.
- ⏳ `DecklinkPlayout` FFI still `todo!()` — next bring-up target
  (`avformat_alloc_output_context2` → raw video + PCM streams →
  `av_interleaved_write_frame`); needs the io0→io1 loopback to verify.

The card reports as **"DeckLink Quad"** (not "Quad 2") — use that exact string
in bilbycast-edge `SdiInputConfig::device`.
