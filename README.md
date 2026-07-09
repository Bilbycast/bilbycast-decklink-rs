# bilbycast-decklink-rs

Safe Rust SDI capture and playout for **Blackmagic DeckLink** cards, via
FFmpeg's `decklink` avdevice. The SDI I/O backend for
[bilbycast-edge](https://github.com/Bilbycast/bilbycast-edge) (feature
`sdi-decklink`, off by default), targeting upstream issue
[#19](https://github.com/Bilbycast/bilbycast-edge/issues/19).

```
bilbycast-decklink-rs/
├── libdecklink-sys/   raw FFI (bindgen) over FFmpeg libavdevice/libavformat/libavcodec/libavutil
└── decklink-rs/       safe wrapper: DecklinkCapture, DecklinkPlayout, enumerate_devices
```

## Why a separate crate from bilbycast-ffmpeg-video-rs?

That crate ships a minimal, LGPL-clean FFmpeg with `--disable-avdevice
--disable-avformat`. DeckLink needs both of those libraries **plus** the
proprietary Blackmagic DeckLink SDK and `--enable-nonfree`. Keeping it separate
keeps the codec crate clean and confines the non-free linkage to one opt-in
feature.

## Quick start (on a host with a DeckLink card)

```bash
# Build an FFmpeg with the decklink device (see CLAUDE.md for the SDK setup):
export LIBDECKLINK_FFMPEG_DIR="$HOME/ffmpeg-decklink"
cargo build
cargo test        # runs enumerate_devices() against the real card
```

See [CLAUDE.md](CLAUDE.md) for the full FFmpeg-with-DeckLink build recipe,
prerequisites, and design notes.

## License

MPL-2.0 for the wrapper code. Note: a binary linking FFmpeg's `--enable-nonfree
--enable-decklink` device is **not redistributable** — intended for private
on-prem edge deployment only (which is why the feature is opt-in).
