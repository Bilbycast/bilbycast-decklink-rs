# CLAUDE.md — bilbycast-decklink-rs

## What Is This

Safe Rust **SDI capture** for Blackmagic **DeckLink** cards, talking to the
Blackmagic DeckLink SDK directly. Used only when bilbycast-edge is built with
`--features sdi-decklink` (default **off**).

## Projects

| Crate | Role |
|-------|------|
| **libdecklink-sys** | `shim/decklink_shim.{h,cpp}` — a C++ shim exposing a C ABI over the SDK — plus bindgen FFI for it. Compiled with `cc` together with the SDK's `DeckLinkAPIDispatch.cpp`. |
| **decklink-rs** | Safe wrapper: `enumerate_devices`, `device_status`, `DecklinkCapture`, and the `Captured*` / `Decklink*Config` types. The crate bilbycast-edge depends on. |

## Why the SDK, not FFmpeg's `decklink` avdevice

The original implementation went through `libavdevice`. It worked, but it hides
`bmdFrameHasNoInputSource`: on signal loss FFmpeg silently substitutes colour
bars, so **a pulled cable is indistinguishable from a healthy feed**. That was
proven on hardware — pulling the SDI cable produced no error, no event, and a
perfectly "healthy" 10 Mbps stream.

Going straight to the SDK also removed a lot of incidental pain:

* No `--enable-decklink --enable-nonfree` FFmpeg build. The edge binary is no
  longer non-redistributable.
* No FFmpeg >= 8 requirement (DeckLink SDK 16 only compiles into FFmpeg 8+).
* No duplicate `libav*` symbols. The edge already statically links a vendored
  FFmpeg via `video-engine`; adding a second FFmpeg for avdevice caused a symbol
  clash that had to be solved by unifying both on one shared build.
* Device enumeration no longer wedges the card (see below).

## Why a C++ shim

The DeckLink API is COM-style C++: frames arrive via `IDeckLinkInputCallback`, a
pure-virtual interface Rust cannot implement. The shim owns that callback, pushes
frames onto a bounded queue, and exposes a blocking `dl_read_frame`.

Frames are zero-copy inside the shim; `decklink-rs` copies each frame into an
owned buffer and releases the SDK frame immediately, because holding SDK frames
starves the card of capture buffers.

## Build & Test

Only the SDK **headers** are needed at build time — `DeckLinkAPIDispatch.cpp`
`dlopen`s `libDeckLinkAPI.so` at runtime.

```bash
# Blackmagic "Desktop Video SDK" (accept the EULA) -> Linux/include
export DECKLINK_SDK_DIR=$HOME/decklink-sdk-include
cargo build

# On a host with a card + Desktop Video installed:
cargo run -p decklink-rs --example list_devices
cargo run -p decklink-rs --example capture_probe -- "DeckLink Quad (1)" auto
```

Prereqs: a C++ toolchain, `clang` (bindgen), and Blackmagic **Desktop Video** at
runtime (kernel driver + `libDeckLinkAPI.so`).

## SDK gotchas (learned the hard way)

* **SDK 16 moved `GetBytes`.** It is no longer on `IDeckLinkVideoInputFrame`;
  `QueryInterface(IID_IDeckLinkVideoBuffer)` then `StartAccess(bmdBufferAccessRead)`
  / `GetBytes` / `EndAccess`. The pointer is only valid inside that window, so the
  shim holds the buffer in the access state until the frame is released. (This is
  also why FFmpeg 7.1 will not compile against SDK 16.)
* **Format detection needs a settle window.** The input is armed on a *seed*
  mode, so the very first frame can still report the seed's raster/rate. `open`
  drains for ~400 ms so `VideoInputFormatChanged` can fire.
* **DeckLink display modes are literally FourCCs** — `"Hp50"` is
  `bmdModeHD1080p50`, so a mode string maps to `BMDDisplayMode` directly.
* **Signal loss does not stop the stream.** The card keeps delivering frames with
  `bmdFrameHasNoInputSource` set. Callers should keep encoding (holding the
  transport stream up is what downstream wants) and raise an alarm.
* **`IDeckLinkStatus` needs no open handle.** `device_status(index)` neither
  opens nor reserves the device, and reads correctly while another process is
  capturing from it (`busy` reports `true`). That is what makes a whole-card
  "which ports have signal?" view possible without disturbing live flows.
* **The card answers per-field, and says "unknown" a lot.** On an unlocked input
  every `Detected*` field returns `E_FAIL`, so each maps to `Option`. Never
  render a missing answer as `false`.
* **Two traps in the status API.** `CurrentVideoInputMode` returns a bogus
  `'ntsc'` default on an unlocked port instead of failing, so it is deliberately
  *not* exposed — `DetectedVideoInputMode` is the honest one. And several
  fields return `bmdModeUnknown` (`'iunk'`) rather than an error;
  `fourcc_to_string` maps that to `None`.

Observed on a DeckLink Quad 2 (locked 1080i50 on port 1, ports 2–8 idle):

```
[0] signal=yes reference=no anc=yes busy=no
    mode=Hi50 colorspace=r709 field=uppr link=lcsl  pcie=gen2 x4
[1] signal=no  reference=no anc=no  busy=no
    mode=—    colorspace=—    field=—    link=—     pcie=gen2 x4
```

`reference=no` on every port: no house reference is patched to this card.
Ethernet/HDMI status IDs belong to the IP and HDMI models and are not exposed.

## Physical connector ↔ software device mapping (8-port Quad cards)

Software numbering **interleaves** across the physical connectors. Counting
physical SDI ports from the REF/genlock BNC outward (per Blackmagic's Desktop
Video Utility diagram, verified empirically on this card):

| physical | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
|----------|---|---|---|---|---|---|---|---|
| software | 1 | 5 | 2 | 6 | 3 | 7 | 4 | 8 |

Sub-devices pair as (1,5), (2,6), (3,7), (4,8) — each pair shares two
*adjacent* physical connectors. The pairing is live routing, not trivia:
**when a sub-device plays out while its own connector is carrying an input,
the output emerges on its pair partner's connector.** Verified: playout on
"DeckLink Quad (1)" while its connector held a live input emerged on physical
port 2 (= software 5's connector) — a loop from physical 2 to physical 3 then
arrives at "DeckLink Quad (2)".

Also observed: **idle outputs emit NTSC black** — a looped-back input shows
`signal=yes, mode=ntsc` with nothing deliberately playing. Do not read a
looped port's bare `signal_locked` as proof your playout works; check
`detected_mode` matches the mode you scheduled.

Playout itself was verified photographically: bars scheduled on one
sub-device, captured on the looped pair connector, correct colours and a
moving element confirming live video (`examples/playout_bars.rs`).

## Known-good bilbycast-edge SDI config

Verified on bilby-z440 (live 1080i50 source → NVENC → SRT: correct colours,
audio, no freezing). Run the edge with `BILBYCAST_PROBE_SESSION_LIMITS=0`.

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

Gotchas:

* **`format` must be `"auto"`.** A forced mode that does not match the source
  makes the card report no signal and emit bars. (Confirmed: forcing `Hp50` on a
  1080i50 source gives `signal_present = false` on every frame.)
* **`tune` must be `""` for NVENC.** The edge defaults `tune` to `"zerolatency"`,
  an x264-only tune — NVENC rejects it and `avcodec_open2` fails with `EINVAL`.
* **`chroma` must be `"yuv420p"` for `h264_nvenc`** (h264 NVENC has no 4:2:2
  path; only `hevc_nvenc` does).
* **Bitrate**: 25 Mbps overran SRT egress here (~0.4 % TS packet loss → visible
  freezing). 10 Mbps is clean.
* **Always benchmark the `--release` build** — debug is ~4x the CPU.

## Upstream bilbycast-edge bugs found during bring-up

None are SDI-specific; worth reporting alongside the SDI PR:

1. `video_encode_util::build_encoder_config` defaults `tune = "zerolatency"`,
   which is invalid for every NVENC backend ⇒ `avcodec_open2` EINVAL.
2. `video_encode_util::try_build_scaler` skips conversion when
   `dims_match && is_planar_yuv(src)` — it never checks the planar layout matches
   the **encoder's** chroma. A planar 4:2:2 source with a 4:2:0 encoder target is
   fed through unconverted, so the encoder reads chroma from the wrong rows
   (perfect luma, ghosted/smeared chroma). Also affects ST 2110-20 ingest.
3. Ingress encoder failures were reported only via `event_sender.emit` (manager
   events), so a standalone edge fails silently. `sdi_io.rs` now also logs them.

## Key Design Constraints

1. **Send but not Sync** — handles move between threads, need `&mut`.
2. **Blocking API** — `read_frame` blocks on the SDI cadence; drive it under
   `spawn_blocking` / `block_in_place`.
3. **No codec work here** — only raw essence crosses the SDI boundary. Encode
   stays in bilbycast-edge's `video-engine`.
4. **Feature-gated off in bilbycast-edge** (`sdi-decklink`). Never default-on.
5. **Never panic** — this crate is linked into a long-running broadcast binary.
   Unimplemented playout returns `Error::Unsupported`, not `todo!()`.

## Historical note: the enumerate wedge

The FFmpeg-based implementation had a nasty bug: `avdevice_list_input_sources` on
the `decklink` device left FFmpeg's decklink discovery un-released, which wedged
the card for the rest of the process — every later `DecklinkCapture::open`
returned `EIO`. The edge's boot probe had to skip enumeration entirely.

The SDK path releases its iterator cleanly, so enumeration is safe again;
`enumerate_devices` followed immediately by a capture is verified working.
