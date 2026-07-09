/* Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
 * SPDX-License-Identifier: MPL-2.0
 *
 * C ABI over the Blackmagic DeckLink SDK's COM-style C++ interfaces.
 *
 * The SDK delivers frames through `IDeckLinkInputCallback`, a C++ virtual
 * interface Rust cannot implement directly. This shim owns that callback,
 * pushes arriving frames onto a bounded queue, and exposes a blocking
 * `dl_read_frame` that Rust drives from a `spawn_blocking` thread.
 *
 * Everything the FFmpeg `decklink` avdevice hides is available here — most
 * importantly `bmdFrameHasNoInputSource`, which is the only reliable way to
 * detect a pulled cable.
 */

#ifndef BILBYCAST_DECKLINK_SHIM_H
#define BILBYCAST_DECKLINK_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── status codes ─────────────────────────────────────────────────────── */
#define DL_OK 0
#define DL_TIMEOUT 1        /* no frame within timeout_ms; not an error */
#define DL_ERR_PARAM (-1)
#define DL_ERR_NO_DEVICE (-2)
#define DL_ERR_OPEN (-3)
#define DL_ERR_UNSUPPORTED (-4)
#define DL_ERR_STOPPED (-5) /* capture closed while waiting */

/* ── pixel formats ────────────────────────────────────────────────────── */
#define DL_PIXFMT_UYVY422 0 /* bmdFormat8BitYUV  */
#define DL_PIXFMT_V210 1    /* bmdFormat10BitYUV */

/* ── frame kinds ──────────────────────────────────────────────────────── */
#define DL_FRAME_VIDEO 0
#define DL_FRAME_AUDIO 1

typedef struct dl_capture dl_capture;

/* One dequeued frame. `data` stays valid until `dl_release_frame`. */
typedef struct {
    int32_t kind; /* DL_FRAME_VIDEO | DL_FRAME_AUDIO */

    /* video */
    int32_t width;
    int32_t height;
    int32_t stride;    /* row bytes */
    int32_t no_signal; /* 1 == bmdFrameHasNoInputSource (cable out / no lock) */
    int64_t fr_num;    /* frame rate numerator   (e.g. 50)   */
    int64_t fr_den;    /* frame rate denominator (e.g. 1)    */

    /* audio */
    int32_t channels;
    int32_t sample_rate;
    int32_t sample_frames; /* frames (not samples) in this block */

    /* common */
    int64_t pts_90khz;
    const uint8_t *data;
    size_t size;

    void *_owner; /* opaque: the SDK frame to Release() */
} dl_frame;

/* ── enumeration ──────────────────────────────────────────────────────── */
/* Unlike FFmpeg's avdevice enumeration, this releases the iterator cleanly and
 * does NOT wedge the device, so it is safe to call before capturing. */
int32_t dl_device_count(void);
/* Writes the device's display name (e.g. "DeckLink Quad (1)") into `buf`. */
int32_t dl_device_name(int32_t index, char *buf, size_t buf_len);

/* ── capture ──────────────────────────────────────────────────────────── */
/*
 * `device`  - display name to match, or NULL for the first device.
 * `mode`    - NULL / "auto" to enable hardware input-format detection,
 *             otherwise a DeckLink 4CC mode string such as "Hp50" / "Hi50".
 * `pixel_format` - DL_PIXFMT_*.
 * `audio_channels` - 0 disables audio; otherwise 2, 8 or 16.
 */
int32_t dl_open(const char *device, const char *mode, int32_t pixel_format,
                int32_t audio_channels, dl_capture **out);

/* Blocks up to `timeout_ms` for the next frame. DL_TIMEOUT if none arrived. */
int32_t dl_read_frame(dl_capture *cap, dl_frame *out, int32_t timeout_ms);

/* Releases the SDK frame backing `f`. Must be called for every DL_OK frame. */
void dl_release_frame(dl_frame *f);

/* Number of frames dropped because the queue was full (slow consumer). */
uint64_t dl_dropped_frames(const dl_capture *cap);

void dl_close(dl_capture *cap);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* BILBYCAST_DECKLINK_SHIM_H */
