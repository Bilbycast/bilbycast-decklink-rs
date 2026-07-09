// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0
//
// C ABI over the Blackmagic DeckLink SDK. See decklink_shim.h.

#include "decklink_shim.h"

#include "DeckLinkAPI.h"

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <deque>
#include <mutex>
#include <string>
#include <utility>

namespace {

// Presentation timescale we hand back to Rust. 90 kHz matches MPEG-TS.
constexpr BMDTimeScale kTimeScale = 90000;

// Bound the queue so a stalled consumer cannot exhaust memory. At 1080p50 each
// video frame is ~4 MB, so keep this small; dropping is preferable to OOM.
constexpr size_t kMaxQueued = 8;

// Default mode used to arm the input before hardware format detection kicks in.
constexpr BMDDisplayMode kDetectionSeedMode = bmdModeHD1080i50;

BMDPixelFormat to_bmd_pixfmt(int32_t pf) {
    return pf == DL_PIXFMT_V210 ? bmdFormat10BitYUV : bmdFormat8BitYUV;
}

// DeckLink display modes are literally FourCCs, so "Hp50" -> bmdModeHD1080p50.
bool mode_from_fourcc(const char *s, BMDDisplayMode *out) {
    if (!s || std::strlen(s) != 4)
        return false;
    *out = (BMDDisplayMode)(((uint32_t)(unsigned char)s[0] << 24) |
                            ((uint32_t)(unsigned char)s[1] << 16) |
                            ((uint32_t)(unsigned char)s[2] << 8) |
                            ((uint32_t)(unsigned char)s[3]));
    return true;
}

// Keeps the SDK objects alive so `dl_frame::data` can point straight into the
// SDK buffer with no copy.
//
// SDK 16 moved `GetBytes` off the frame onto `IDeckLinkVideoBuffer`, and the
// pointer is only valid between `StartAccess` and `EndAccess`. So for video we
// hold the buffer *in* the access window until Rust calls `dl_release_frame`.
struct FrameOwner {
    IUnknown *frame = nullptr;             // video frame or audio packet
    IDeckLinkVideoBuffer *vbuf = nullptr;  // video only; held in StartAccess
};

void release_owner(void *p) {
    auto *o = static_cast<FrameOwner *>(p);
    if (!o)
        return;
    if (o->vbuf) {
        o->vbuf->EndAccess(bmdBufferAccessRead);
        o->vbuf->Release();
    }
    if (o->frame)
        o->frame->Release();
    delete o;
}

// One queued frame. `meta._owner` is a heap `FrameOwner`.
struct QueuedFrame {
    dl_frame meta;
};

class Callback; // fwd

} // namespace

struct dl_capture {
    IDeckLink *device = nullptr;
    IDeckLinkInput *input = nullptr;
    Callback *callback = nullptr;

    std::mutex mu;
    std::condition_variable cv;
    std::deque<QueuedFrame> queue;
    bool stopped = false;
    std::atomic<uint64_t> dropped{0};

    // Current mode, updated by the format-detection callback.
    int64_t fr_num = 0;
    int64_t fr_den = 0;

    BMDPixelFormat pixel_format = bmdFormat8BitYUV;
    int32_t audio_channels = 0;
    bool detect_format = false;
};

namespace {

class Callback : public IDeckLinkInputCallback {
  public:
    explicit Callback(dl_capture *cap) : cap_(cap) {}

    // IUnknown — Linux COM emulation.
    HRESULT QueryInterface(REFIID, LPVOID *) override { return E_NOINTERFACE; }
    ULONG AddRef() override { return ++refs_; }
    ULONG Release() override {
        ULONG r = --refs_;
        if (r == 0)
            delete this;
        return r;
    }

    HRESULT VideoInputFormatChanged(BMDVideoInputFormatChangedEvents events,
                                    IDeckLinkDisplayMode *mode,
                                    BMDDetectedVideoInputFormatFlags) override {
        if (!mode || !(events & bmdVideoInputDisplayModeChanged))
            return S_OK;

        // Re-arm the input on the newly detected raster. Rust observes the
        // dimension change on the next frame and restarts its session.
        cap_->input->StopStreams();
        cap_->input->EnableVideoInput(mode->GetDisplayMode(), cap_->pixel_format,
                                      bmdVideoInputEnableFormatDetection);
        cap_->input->FlushStreams();
        cap_->input->StartStreams();

        BMDTimeValue dur = 0;
        BMDTimeScale scale = 0;
        if (mode->GetFrameRate(&dur, &scale) == S_OK && dur > 0) {
            std::lock_guard<std::mutex> lk(cap_->mu);
            cap_->fr_num = (int64_t)scale;
            cap_->fr_den = (int64_t)dur;
        }
        return S_OK;
    }

    HRESULT VideoInputFrameArrived(IDeckLinkVideoInputFrame *video,
                                   IDeckLinkAudioInputPacket *audio) override {
        if (video)
            push_video(video);
        if (audio)
            push_audio(audio);
        return S_OK;
    }

  private:
    void enqueue(QueuedFrame &&qf) {
        std::unique_lock<std::mutex> lk(cap_->mu);
        if (cap_->stopped) {
            release_owner(qf.meta._owner);
            return;
        }
        if (cap_->queue.size() >= kMaxQueued) {
            // Drop the oldest: a live capture must never block the SDK thread.
            release_owner(cap_->queue.front().meta._owner);
            cap_->queue.pop_front();
            cap_->dropped.fetch_add(1, std::memory_order_relaxed);
        }
        cap_->queue.push_back(std::move(qf));
        lk.unlock();
        cap_->cv.notify_one();
    }

    void push_video(IDeckLinkVideoInputFrame *v) {
        // SDK 16: bytes live behind IDeckLinkVideoBuffer, valid only while the
        // read-access window is open. Keep it open until Rust releases.
        IDeckLinkVideoBuffer *vbuf = nullptr;
        if (v->QueryInterface(IID_IDeckLinkVideoBuffer, (void **)&vbuf) != S_OK || !vbuf)
            return;
        if (vbuf->StartAccess(bmdBufferAccessRead) != S_OK) {
            vbuf->Release();
            return;
        }
        void *bytes = nullptr;
        if (vbuf->GetBytes(&bytes) != S_OK || !bytes) {
            vbuf->EndAccess(bmdBufferAccessRead);
            vbuf->Release();
            return;
        }

        BMDTimeValue t = 0, dur = 0;
        v->GetStreamTime(&t, &dur, kTimeScale);

        auto *owner = new FrameOwner();
        v->AddRef();
        owner->frame = v;
        owner->vbuf = vbuf;

        QueuedFrame qf{};
        qf.meta.kind = DL_FRAME_VIDEO;
        qf.meta.width = (int32_t)v->GetWidth();
        qf.meta.height = (int32_t)v->GetHeight();
        qf.meta.stride = (int32_t)v->GetRowBytes();
        // The whole point of using the SDK directly.
        qf.meta.no_signal = (v->GetFlags() & bmdFrameHasNoInputSource) ? 1 : 0;
        qf.meta.pts_90khz = (int64_t)t;
        qf.meta.data = (const uint8_t *)bytes;
        qf.meta.size = (size_t)v->GetRowBytes() * (size_t)v->GetHeight();
        {
            std::lock_guard<std::mutex> lk(cap_->mu);
            qf.meta.fr_num = cap_->fr_num;
            qf.meta.fr_den = cap_->fr_den;
        }
        qf.meta._owner = owner;
        enqueue(std::move(qf));
    }

    void push_audio(IDeckLinkAudioInputPacket *a) {
        void *bytes = nullptr;
        if (a->GetBytes(&bytes) != S_OK || !bytes)
            return;

        BMDTimeValue t = 0;
        a->GetPacketTime(&t, kTimeScale);

        const int32_t frames = (int32_t)a->GetSampleFrameCount();
        const int32_t ch = cap_->audio_channels;

        auto *owner = new FrameOwner();
        a->AddRef();
        owner->frame = a;

        QueuedFrame qf{};
        qf.meta.kind = DL_FRAME_AUDIO;
        qf.meta.channels = ch;
        qf.meta.sample_rate = 48000;
        qf.meta.sample_frames = frames;
        qf.meta.pts_90khz = (int64_t)t;
        qf.meta.data = (const uint8_t *)bytes;
        // 32-bit samples.
        qf.meta.size = (size_t)frames * (size_t)ch * 4u;
        qf.meta._owner = owner;
        enqueue(std::move(qf));
    }

    dl_capture *cap_;
    std::atomic<ULONG> refs_{1};
};

// Find a device by display name; NULL/empty matches the first device.
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

} // namespace

extern "C" {

int32_t dl_device_count(void) {
    IDeckLinkIterator *iter = CreateDeckLinkIteratorInstance();
    if (!iter)
        return 0;
    int32_t n = 0;
    IDeckLink *dev = nullptr;
    while (iter->Next(&dev) == S_OK) {
        ++n;
        dev->Release();
    }
    iter->Release();
    return n;
}

int32_t dl_device_name(int32_t index, char *buf, size_t buf_len) {
    if (!buf || buf_len == 0 || index < 0)
        return DL_ERR_PARAM;

    IDeckLinkIterator *iter = CreateDeckLinkIteratorInstance();
    if (!iter)
        return DL_ERR_NO_DEVICE;

    int32_t i = 0;
    int32_t rc = DL_ERR_NO_DEVICE;
    IDeckLink *dev = nullptr;
    while (iter->Next(&dev) == S_OK) {
        if (i == index) {
            const char *name = nullptr;
            if (dev->GetDisplayName(&name) == S_OK && name) {
                std::snprintf(buf, buf_len, "%s", name);
                free((void *)name);
                rc = DL_OK;
            }
            dev->Release();
            break;
        }
        dev->Release();
        ++i;
    }
    iter->Release();
    return rc;
}

int32_t dl_open(const char *device, const char *mode, int32_t pixel_format,
                int32_t audio_channels, dl_capture **out) {
    if (!out)
        return DL_ERR_PARAM;
    *out = nullptr;

    IDeckLink *dev = find_device(device);
    if (!dev)
        return DL_ERR_NO_DEVICE;

    IDeckLinkInput *input = nullptr;
    if (dev->QueryInterface(IID_IDeckLinkInput, (void **)&input) != S_OK) {
        dev->Release();
        return DL_ERR_UNSUPPORTED;
    }

    dl_capture *cap = new dl_capture();
    cap->device = dev;
    cap->input = input;
    cap->pixel_format = to_bmd_pixfmt(pixel_format);
    cap->audio_channels = audio_channels;

    const bool want_auto = !mode || !*mode || std::strcmp(mode, "auto") == 0;
    BMDDisplayMode display_mode = kDetectionSeedMode;
    if (!want_auto && !mode_from_fourcc(mode, &display_mode)) {
        dl_close(cap);
        return DL_ERR_PARAM;
    }

    BMDVideoInputFlags flags = bmdVideoInputFlagDefault;
    if (want_auto) {
        // Only advertise detection if the hardware supports it.
        IDeckLinkProfileAttributes *attrs = nullptr;
        bool supported = false;
        if (dev->QueryInterface(IID_IDeckLinkProfileAttributes, (void **)&attrs) == S_OK) {
            bool flag = false;
            if (attrs->GetFlag(BMDDeckLinkSupportsInputFormatDetection, &flag) == S_OK)
                supported = flag;
            attrs->Release();
        }
        if (!supported) {
            dl_close(cap);
            return DL_ERR_UNSUPPORTED;
        }
        flags = bmdVideoInputEnableFormatDetection;
        cap->detect_format = true;
    }

    cap->callback = new Callback(cap);
    if (input->SetCallback(cap->callback) != S_OK) {
        dl_close(cap);
        return DL_ERR_OPEN;
    }

    if (input->EnableVideoInput(display_mode, cap->pixel_format, flags) != S_OK) {
        dl_close(cap);
        return DL_ERR_OPEN;
    }

    // Seed the frame rate from the mode we armed with, so callers have a valid
    // rate before the format-detection callback (if any) fires.
    {
        IDeckLinkDisplayMode *dm = nullptr;
        if (input->GetDisplayMode(display_mode, &dm) == S_OK && dm) {
            BMDTimeValue dur = 0;
            BMDTimeScale scale = 0;
            if (dm->GetFrameRate(&dur, &scale) == S_OK && dur > 0) {
                cap->fr_num = (int64_t)scale;
                cap->fr_den = (int64_t)dur;
            }
            dm->Release();
        }
    }
    if (audio_channels > 0 &&
        input->EnableAudioInput(bmdAudioSampleRate48kHz, bmdAudioSampleType32bitInteger,
                                (uint32_t)audio_channels) != S_OK) {
        dl_close(cap);
        return DL_ERR_OPEN;
    }
    if (input->StartStreams() != S_OK) {
        dl_close(cap);
        return DL_ERR_OPEN;
    }

    *out = cap;
    return DL_OK;
}

int32_t dl_read_frame(dl_capture *cap, dl_frame *out, int32_t timeout_ms) {
    if (!cap || !out)
        return DL_ERR_PARAM;

    std::unique_lock<std::mutex> lk(cap->mu);
    if (!cap->cv.wait_for(lk, std::chrono::milliseconds(timeout_ms),
                          [&] { return cap->stopped || !cap->queue.empty(); }))
        return DL_TIMEOUT;

    if (cap->stopped && cap->queue.empty())
        return DL_ERR_STOPPED;

    QueuedFrame qf = std::move(cap->queue.front());
    cap->queue.pop_front();
    lk.unlock();

    *out = qf.meta;
    return DL_OK;
}

void dl_release_frame(dl_frame *f) {
    if (!f || !f->_owner)
        return;
    release_owner(f->_owner);
    f->_owner = nullptr;
    f->data = nullptr;
}

uint64_t dl_dropped_frames(const dl_capture *cap) {
    return cap ? cap->dropped.load(std::memory_order_relaxed) : 0;
}

void dl_close(dl_capture *cap) {
    if (!cap)
        return;

    if (cap->input) {
        cap->input->StopStreams();
        cap->input->DisableVideoInput();
        if (cap->audio_channels > 0)
            cap->input->DisableAudioInput();
        cap->input->SetCallback(nullptr);
    }

    {
        std::lock_guard<std::mutex> lk(cap->mu);
        cap->stopped = true;
        for (auto &qf : cap->queue)
            release_owner(qf.meta._owner);
        cap->queue.clear();
    }
    cap->cv.notify_all();

    if (cap->callback) {
        cap->callback->Release();
        cap->callback = nullptr;
    }
    if (cap->input) {
        cap->input->Release();
        cap->input = nullptr;
    }
    if (cap->device) {
        cap->device->Release();
        cap->device = nullptr;
    }
    delete cap;
}

} // extern "C"
