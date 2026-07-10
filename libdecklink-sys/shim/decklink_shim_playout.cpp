// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0
//
// Scheduled SDI video playout. See decklink_shim.h for the contract.
//
// A separate translation unit from the capture shim; the two small helpers
// (mode parsing, device lookup) are duplicated here rather than exported,
// keeping both files' internals in anonymous namespaces.

#include "decklink_shim.h"

#include "DeckLinkAPI.h"

#include <atomic>
#include <condition_variable>
#include <cstdlib>
#include <cstring>
#include <mutex>

namespace {

// Schedule timescale. 90 kHz matches MPEG-TS, so the edge's timestamps map
// onto the card's clock without rescaling surprises.
constexpr BMDTimeScale kTimeScale = 90000;

BMDPixelFormat to_bmd_pixfmt(int32_t pf) {
    return pf == DL_PIXFMT_V210 ? bmdFormat10BitYUV : bmdFormat8BitYUV;
}

bool mode_from_fourcc(const char *s, BMDDisplayMode *out) {
    if (!s || std::strlen(s) != 4)
        return false;
    *out = (BMDDisplayMode)(((uint32_t)(unsigned char)s[0] << 24) |
                            ((uint32_t)(unsigned char)s[1] << 16) |
                            ((uint32_t)(unsigned char)s[2] << 8) |
                            ((uint32_t)(unsigned char)s[3]));
    return true;
}

IDeckLink *find_device(const char *want) {
    IDeckLinkIterator *iter = CreateDeckLinkIteratorInstance();
    if (!iter)
        return nullptr;
    IDeckLink *dev = nullptr;
    IDeckLink *found = nullptr;
    while (iter->Next(&dev) == S_OK) {
        if (!want || !*want) {
            found = dev;
            break;
        }
        const char *name = nullptr;
        if (dev->GetDisplayName(&name) == S_OK && name) {
            const bool hit = std::strcmp(name, want) == 0;
            free((void *)name);
            if (hit) {
                found = dev;
                break;
            }
        }
        dev->Release();
    }
    iter->Release();
    return found;
}

class PlayoutCallback;

} // namespace

struct dl_playout {
    IDeckLink *device = nullptr;
    IDeckLinkOutput *output = nullptr;
    PlayoutCallback *callback = nullptr;

    int32_t width = 0;
    int32_t height = 0;
    int32_t row_bytes = 0;
    BMDPixelFormat pixfmt = bmdFormat8BitYUV;

    // Schedule clock: each frame plays for `frame_duration` kTimeScale ticks.
    BMDTimeValue frame_duration = 0;
    BMDTimeValue next_time = 0;
    int64_t fr_num = 0;
    int64_t fr_den = 0;

    // Backpressure: frames scheduled but not yet completed by the card.
    std::mutex mu;
    std::condition_variable cv;
    int32_t in_flight = 0;
    bool started = false;
    bool stopping = false;

    std::atomic<uint64_t> late{0};
    std::atomic<uint64_t> dropped{0};
};

namespace {

// Owns the SDK's completion notifications. `dl_playout_close` clears the
// card's callback pointer before releasing, so this never outlives its use.
class PlayoutCallback : public IDeckLinkVideoOutputCallback {
public:
    explicit PlayoutCallback(dl_playout *po) : po_(po) {}

    HRESULT ScheduledFrameCompleted(IDeckLinkVideoFrame * /*frame*/,
                                    BMDOutputFrameCompletionResult result) override {
        if (result == bmdOutputFrameDisplayedLate)
            po_->late.fetch_add(1, std::memory_order_relaxed);
        else if (result == bmdOutputFrameDropped)
            po_->dropped.fetch_add(1, std::memory_order_relaxed);
        {
            std::lock_guard<std::mutex> lk(po_->mu);
            po_->in_flight--;
        }
        po_->cv.notify_all();
        return S_OK;
    }

    HRESULT ScheduledPlaybackHasStopped() override {
        {
            std::lock_guard<std::mutex> lk(po_->mu);
            po_->stopping = true;
        }
        po_->cv.notify_all();
        return S_OK;
    }

    HRESULT QueryInterface(REFIID, void **out) override {
        *out = nullptr;
        return E_NOINTERFACE;
    }
    ULONG AddRef() override { return ++ref_; }
    ULONG Release() override {
        ULONG r = --ref_;
        if (r == 0)
            delete this;
        return r;
    }

private:
    virtual ~PlayoutCallback() = default;
    dl_playout *po_;
    std::atomic<ULONG> ref_{1};
};

} // namespace

extern "C" {

int32_t dl_playout_open(const char *device, const char *mode,
                        int32_t pixel_format, dl_playout **out) {
    if (!out || !mode)
        return DL_ERR_PARAM;
    *out = nullptr;

    BMDDisplayMode display_mode;
    if (!mode_from_fourcc(mode, &display_mode))
        return DL_ERR_PARAM; // playout has nothing to auto-detect from

    IDeckLink *dev = find_device(device);
    if (!dev)
        return DL_ERR_NO_DEVICE;

    IDeckLinkOutput *output = nullptr;
    if (dev->QueryInterface(IID_IDeckLinkOutput, (void **)&output) != S_OK) {
        dev->Release();
        return DL_ERR_UNSUPPORTED;
    }

    const BMDPixelFormat pixfmt = to_bmd_pixfmt(pixel_format);
    bool supported = false;
    BMDDisplayMode actual = display_mode;
    if (output->DoesSupportVideoMode(bmdVideoConnectionUnspecified, display_mode,
                                     pixfmt, bmdNoVideoOutputConversion,
                                     bmdSupportedVideoModeDefault, &actual,
                                     &supported) != S_OK ||
        !supported) {
        output->Release();
        dev->Release();
        return DL_ERR_UNSUPPORTED;
    }

    auto *po = new dl_playout();
    po->device = dev;
    po->output = output;
    po->pixfmt = pixfmt;

    IDeckLinkDisplayMode *dm = nullptr;
    if (output->GetDisplayMode(display_mode, &dm) != S_OK || !dm) {
        dl_playout_close(po);
        return DL_ERR_OPEN;
    }
    po->width = (int32_t)dm->GetWidth();
    po->height = (int32_t)dm->GetHeight();
    BMDTimeValue dur = 0;
    BMDTimeScale scale = 0;
    if (dm->GetFrameRate(&dur, &scale) == S_OK && dur > 0 && scale > 0) {
        po->fr_num = (int64_t)scale;
        po->fr_den = (int64_t)dur;
        // dur/scale seconds per frame, expressed in kTimeScale ticks.
        po->frame_duration = (BMDTimeValue)((dur * kTimeScale) / scale);
    }
    dm->Release();
    if (po->frame_duration <= 0) {
        dl_playout_close(po);
        return DL_ERR_OPEN;
    }
    if (output->RowBytesForPixelFormat(pixfmt, po->width, &po->row_bytes) != S_OK ||
        po->row_bytes <= 0) {
        dl_playout_close(po);
        return DL_ERR_OPEN;
    }

    po->callback = new PlayoutCallback(po);
    if (output->SetScheduledFrameCompletionCallback(po->callback) != S_OK) {
        dl_playout_close(po);
        return DL_ERR_OPEN;
    }
    if (output->EnableVideoOutput(display_mode, bmdVideoOutputFlagDefault) != S_OK) {
        dl_playout_close(po);
        return DL_ERR_OPEN;
    }

    *out = po;
    return DL_OK;
}

int32_t dl_playout_info(const dl_playout *po, int32_t *width, int32_t *height,
                        int32_t *row_bytes, int64_t *fr_num, int64_t *fr_den) {
    if (!po)
        return DL_ERR_PARAM;
    if (width)
        *width = po->width;
    if (height)
        *height = po->height;
    if (row_bytes)
        *row_bytes = po->row_bytes;
    if (fr_num)
        *fr_num = po->fr_num;
    if (fr_den)
        *fr_den = po->fr_den;
    return DL_OK;
}

int32_t dl_playout_write_video(dl_playout *po, const uint8_t *data, size_t size) {
    if (!po || !data)
        return DL_ERR_PARAM;
    if (size != (size_t)po->row_bytes * (size_t)po->height)
        return DL_ERR_PARAM;

    // Backpressure: the card's completion callbacks pace the writer.
    {
        std::unique_lock<std::mutex> lk(po->mu);
        po->cv.wait(lk, [po] {
            return po->stopping || po->in_flight < DL_PLAYOUT_MAX_INFLIGHT;
        });
        if (po->stopping)
            return DL_ERR_STOPPED;
        po->in_flight++;
    }

    IDeckLinkMutableVideoFrame *frame = nullptr;
    if (po->output->CreateVideoFrame(po->width, po->height, po->row_bytes,
                                     po->pixfmt, bmdFrameFlagDefault,
                                     &frame) != S_OK ||
        !frame) {
        std::lock_guard<std::mutex> lk(po->mu);
        po->in_flight--;
        return DL_ERR_OPEN;
    }

    // SDK 16: pixel access goes through IDeckLinkVideoBuffer within an explicit
    // access window — IDeckLinkVideoFrame::GetBytes is gone. Same trap as
    // capture, write access this time.
    IDeckLinkVideoBuffer *vbuf = nullptr;
    void *bytes = nullptr;
    if (frame->QueryInterface(IID_IDeckLinkVideoBuffer, (void **)&vbuf) != S_OK ||
        !vbuf || vbuf->StartAccess(bmdBufferAccessWrite) != S_OK) {
        if (vbuf)
            vbuf->Release();
        frame->Release();
        std::lock_guard<std::mutex> lk(po->mu);
        po->in_flight--;
        return DL_ERR_OPEN;
    }
    vbuf->GetBytes(&bytes);
    std::memcpy(bytes, data, size);
    vbuf->EndAccess(bmdBufferAccessWrite);
    vbuf->Release();

    const HRESULT hr = po->output->ScheduleVideoFrame(frame, po->next_time,
                                                      po->frame_duration,
                                                      kTimeScale);
    // The card holds its own reference until completion.
    frame->Release();
    if (hr != S_OK) {
        std::lock_guard<std::mutex> lk(po->mu);
        po->in_flight--;
        return DL_ERR_OPEN;
    }
    po->next_time += po->frame_duration;

    // Start the clock once the pre-roll is queued, so the first frames do not
    // play into an unprimed pipeline.
    bool start = false;
    {
        std::lock_guard<std::mutex> lk(po->mu);
        if (!po->started && po->in_flight >= DL_PLAYOUT_PREROLL) {
            po->started = true;
            start = true;
        }
    }
    if (start && po->output->StartScheduledPlayback(0, kTimeScale, 1.0) != S_OK)
        return DL_ERR_OPEN;
    return DL_OK;
}

uint64_t dl_playout_late_frames(const dl_playout *po) {
    return po ? po->late.load(std::memory_order_relaxed) : 0;
}

uint64_t dl_playout_dropped_frames(const dl_playout *po) {
    return po ? po->dropped.load(std::memory_order_relaxed) : 0;
}

void dl_playout_close(dl_playout *po) {
    if (!po)
        return;
    {
        std::lock_guard<std::mutex> lk(po->mu);
        po->stopping = true;
    }
    po->cv.notify_all();
    if (po->output) {
        if (po->started) {
            BMDTimeValue actual_stop = 0;
            po->output->StopScheduledPlayback(0, &actual_stop, kTimeScale);
        }
        po->output->SetScheduledFrameCompletionCallback(nullptr);
        po->output->DisableVideoOutput();
        po->output->Release();
        po->output = nullptr;
    }
    if (po->callback) {
        po->callback->Release();
        po->callback = nullptr;
    }
    if (po->device) {
        po->device->Release();
        po->device = nullptr;
    }
    delete po;
}

} // extern "C"
