// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0
//
// bindgen entry header for libdecklink-sys. We wrap only the FFmpeg
// libav* surface needed to drive the `decklink` avdevice — enumeration,
// demux (capture), and mux (playout). The Blackmagic DeckLink SDK itself
// is NOT included here: FFmpeg's libavdevice already links it, and we talk
// to the card exclusively through the FFmpeg abstraction.

#include <libavdevice/avdevice.h>
#include <libavformat/avformat.h>
#include <libavcodec/avcodec.h>
#include <libavutil/avutil.h>
#include <libavutil/frame.h>
#include <libavutil/dict.h>
#include <libavutil/opt.h>
#include <libavutil/imgutils.h>
#include <libavutil/samplefmt.h>
#include <libavutil/pixfmt.h>
