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
#define DL_ERR_TIME (-6)    /* playout: stream time did not advance */

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
/* Whether the DeckLink API itself is reachable — i.e. Desktop Video is
 * installed and DeckLinkAPIDispatch could dlopen libDeckLinkAPI.so. Distinct
 * from a device count of 0, which is a working API reporting no cards fitted:
 * a card can be plugged into the latter, never into the former. */
int32_t dl_api_available(void);
/* Unlike FFmpeg's avdevice enumeration, this releases the iterator cleanly and
 * does NOT wedge the device, so it is safe to call before capturing.
 * Returns 0 both when the API is unreachable and when no cards are fitted;
 * call dl_api_available() to tell those apart. */
int32_t dl_device_count(void);
/* Writes the device's display name (e.g. "DeckLink Quad (1)") into `buf`. */
int32_t dl_device_name(int32_t index, char *buf, size_t buf_len);

/* ── device status ────────────────────────────────────────────────────── */
/*
 * A read-only snapshot from `IDeckLinkStatus`. Queryable on a device this
 * process does NOT hold open, and concurrently with a capture in progress, so
 * a host can report signal presence on every SDI port at once.
 *
 * Fields the card declines to answer come back as the sentinels below rather
 * than a guess. On an unlocked port every `detected_*` field is unsupported —
 * the card genuinely does not know. (Note the SDK's *current* video input mode
 * is deliberately not exposed: on an unlocked port it returns a meaningless
 * 'ntsc' default rather than failing, so it cannot be told apart from a real
 * NTSC signal.)
 */
#define DL_STATUS_UNKNOWN_TRI (-1) /* int32_t tri-state: no answer */
#define DL_STATUS_UNKNOWN_FCC 0    /* int64_t FourCC: no answer */

typedef struct {
    /* tri-state: 0 = false, 1 = true, DL_STATUS_UNKNOWN_TRI = unsupported */
    int32_t signal_locked;    /* input locked to an SDI signal */
    int32_t reference_locked; /* locked to house reference (genlock) */
    int32_t ancillary_locked; /* ancillary data stream locked */
    int32_t busy;             /* device in use (by us or another process) */

    /* FourCC, or DL_STATUS_UNKNOWN_FCC */
    int64_t detected_mode;            /* e.g. 'Hi50' */
    int64_t detected_colorspace;      /* e.g. 'r709' */
    int64_t detected_field_dominance; /* e.g. 'uppr' */
    int64_t sdi_link_config;          /* e.g. 'lcsl' (single link) */
    int64_t reference_mode;           /* raster of the reference signal */

    /* DL_STATUS_UNKNOWN_TRI when unsupported */
    int32_t detected_dynamic_range; /* BMDDynamicRange; 0 == SDR */
    int32_t pcie_link_speed;        /* PCIe generation, e.g. 2 */
    int32_t pcie_link_width;        /* PCIe lanes, e.g. 4 */
} dl_device_status;

/* Fills `out` for the device at `index`. Does not open or reserve the device. */
int32_t dl_device_status_get(int32_t index, dl_device_status *out);

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

/* ── playout ──────────────────────────────────────────────────────────── */
/*
 * Scheduled video playout. The shim owns the SDK's completion callback and a
 * small in-flight window: `dl_playout_write_video` blocks while
 * DL_PLAYOUT_MAX_INFLIGHT frames are scheduled and un-completed, so the
 * caller's pace is governed by the card's clock. Playback starts
 * automatically once DL_PLAYOUT_PREROLL frames are queued.
 *
 * `mode` must be an explicit DeckLink 4CC ("Hi50", "Hp25", ...) — playout has
 * nothing to auto-detect from.
 *
 * Video and audio share ONE 90 kHz playout timeline whose origin is the start
 * of scheduled playback, and the caller owns it: both essences are scheduled at
 * `essence_pts_90k - first_video_pts_90k`. The shim never derives a display
 * time from how many writes have succeeded, so a write the caller skips or one
 * that fails (decode error, raster mismatch, DL_TIMEOUT, a keyframe wait after
 * a discontinuity) leaves a hole in the picture but does not shift the frames
 * after it — audio stays in sync.
 *
 * The SDK requires frames be scheduled in ascending display order, so a
 * `stream_time_90k` that does not advance is refused with DL_ERR_TIME rather
 * than handed to the SDK.
 *
 * Audio (optional): when `audio_channels` > 0 the device is opened with
 * timestamped scheduled audio at 48 kHz, 32-bit signed interleaved.
 */
#define DL_PLAYOUT_PREROLL 3
#define DL_PLAYOUT_MAX_INFLIGHT 8

typedef struct dl_playout dl_playout;

/* `audio_channels` - 0 disables audio; otherwise 2, 8 or 16. Audio is always
 * 48 kHz, 32-bit signed integer, interleaved. */
int32_t dl_playout_open(const char *device, const char *mode,
                        int32_t pixel_format, int32_t audio_channels,
                        dl_playout **out);

/* Frame geometry the card expects. `data` passed to write_video must be
 * exactly `row_bytes * height` bytes of the opened pixel format. */
int32_t dl_playout_info(const dl_playout *po, int32_t *width, int32_t *height,
                        int32_t *row_bytes, int64_t *fr_num, int64_t *fr_den);

/* Copies `size` bytes into a card frame and schedules it for display at
 * `stream_time_90k` on the shared 90 kHz playout timeline. Blocks (bounded)
 * while the in-flight window is full. DL_ERR_STOPPED after dl_playout_close,
 * DL_TIMEOUT if the card stops draining, DL_ERR_PARAM on a size mismatch,
 * DL_ERR_TIME if `stream_time_90k` does not advance past the last frame
 * scheduled. */
int32_t dl_playout_write_video(dl_playout *po, const uint8_t *data, size_t size,
                               int64_t stream_time_90k);

/* Schedule one block of interleaved 32-bit signed audio at `stream_time_90k`
 * on the shared 90 kHz playout timeline. `samples` holds
 * `sample_frames * channels` int32 values. Non-blocking. DL_ERR_UNSUPPORTED
 * if the device was opened with audio_channels == 0; DL_ERR_STOPPED after
 * close. */
int32_t dl_playout_write_audio(dl_playout *po, const int32_t *samples,
                               int32_t sample_frames, int64_t stream_time_90k);

/* Frames the card reported as displayed late (caller fell behind the SDI
 * cadence) / dropped. Cumulative. */
uint64_t dl_playout_late_frames(const dl_playout *po);
uint64_t dl_playout_dropped_frames(const dl_playout *po);

void dl_playout_close(dl_playout *po);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* BILBYCAST_DECKLINK_SHIM_H */
