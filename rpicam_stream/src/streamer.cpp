/**
 * streamer.cpp
 *
 * Raspberry Pi H264 RTP streamer with dynamic bitrate control.
 * Replaces: rpicam-vid | gst-launch-1.0 fdsrc ! h264parse ! rtph264pay ! udpsink
 *
 * Pipeline:
 *   libcamera (YUV420) → appsrc → v4l2h264enc → h264parse
 *   → rtph264pay → rtpulpfecenc → udpsink
 *
 * Bitrate control: write integer to stdin e.g. `echo 500000`
 *
 * Build:
 *   g++ streamer.cpp -o streamer \
 *     $(pkg-config --cflags --libs gstreamer-1.0 gstreamer-app-1.0) \
 *     $(pkg-config --cflags --libs libcamera) \
 *     -lpthread -std=c++17
 *
 * Run:
 *   ./streamer --host 37.59.106.4 --port 9600
 *   # Then from another terminal or your Rust ABR controller:
 *   echo 300000 > /proc/$(pidof streamer)/fd/0
 */

#include <gst/gst.h>
#include <gst/app/gstappsrc.h>

#include <libcamera/libcamera.h>

#include <sys/mman.h>   // mmap, munmap, PROT_READ, MAP_SHARED, MAP_FAILED

#include <atomic>
#include <chrono>
#include <csignal>
#include <cstring>
#include <iostream>
#include <memory>
#include <mutex>
#include <queue>
#include <string>
#include <thread>
#include <condition_variable>

using namespace libcamera;

// ─── Global state ────────────────────────────────────────────────────────────

static std::atomic<bool>     g_running{true};
static std::atomic<uint32_t> g_bitrate{1000000};   // current target bitrate (bps)
static std::atomic<bool>     g_bitrate_changed{false};

// Frame queue between libcamera callback and GStreamer push thread
struct Frame {
    std::vector<uint8_t> data;
    uint32_t             width;
    uint32_t             height;
};

static std::queue<Frame>        g_frame_queue;
static std::mutex               g_queue_mutex;
static std::condition_variable  g_queue_cv;
static constexpr size_t         MAX_QUEUE = 4;

// GStreamer elements we need to reach at runtime
static GstElement *g_pipeline  = nullptr;
static GstElement *g_appsrc    = nullptr;
static GstElement *g_encoder   = nullptr;  // v4l2h264enc or x264enc
static GstElement *g_fecenc    = nullptr;
static Camera     *s_cam       = nullptr;

// ─── Signal handler ──────────────────────────────────────────────────────────

static void on_signal(int) {
    g_running = false;
    g_queue_cv.notify_all();
    if (g_pipeline)
        gst_element_send_event(g_pipeline, gst_event_new_eos());
}

// ─── stdin bitrate reader thread ─────────────────────────────────────────────

static void stdin_reader_thread() {
    std::string line;
    while (g_running && std::getline(std::cin, line)) {
        if (line.empty()) continue;
        try {
            uint32_t bps = static_cast<uint32_t>(std::stoul(line));
            bps = std::clamp(bps, 100'000u, 8'000'000u);
            g_bitrate.store(bps);
            g_bitrate_changed.store(true);
            std::cerr << "[ABR] New bitrate: " << bps / 1000 << " kbps\n";
        } catch (...) {
            std::cerr << "[ABR] Invalid bitrate value: " << line << "\n";
        }
    }
    g_running = false;
    g_queue_cv.notify_all();
}

// ─── Apply bitrate to encoder ────────────────────────────────────────────────

static void apply_bitrate(uint32_t bps) {
    if (!g_encoder) return;

    std::string enc_name;
    {
        gchar *name = gst_element_get_name(g_encoder);
        enc_name = name;
        g_free(name);
    }

    if (enc_name.find("v4l2h264enc") != std::string::npos) {
        // v4l2h264enc uses "extra-controls" for bitrate
        GstStructure *ctrls = gst_structure_new(
            "extra-controls",
            "video_bitrate", G_TYPE_INT, (gint)bps,
            nullptr
        );
        g_object_set(G_OBJECT(g_encoder), "extra-controls", ctrls, nullptr);
        gst_structure_free(ctrls);
    } else {
        // x264enc / openh264enc use "bitrate" property (in kbps)
        g_object_set(G_OBJECT(g_encoder), "bitrate", bps / 1000, nullptr);
    }
}

// ─── GStreamer bus callback ───────────────────────────────────────────────────

static gboolean bus_callback(GstBus *, GstMessage *msg, gpointer) {
    switch (GST_MESSAGE_TYPE(msg)) {
        case GST_MESSAGE_ERROR: {
            GError *err = nullptr;
            gchar  *dbg = nullptr;
            gst_message_parse_error(msg, &err, &dbg);
            std::cerr << "[GST ERROR] " << err->message << "\n";
            if (dbg) std::cerr << "[GST DEBUG] " << dbg << "\n";
            g_error_free(err);
            g_free(dbg);
            g_running = false;
            g_queue_cv.notify_all();
            break;
        }
        case GST_MESSAGE_EOS:
            std::cerr << "[GST] EOS\n";
            g_running = false;
            g_queue_cv.notify_all();
            break;
        case GST_MESSAGE_WARNING: {
            GError *err = nullptr;
            gchar  *dbg = nullptr;
            gst_message_parse_warning(msg, &err, &dbg);
            std::cerr << "[GST WARN] " << err->message << "\n";
            g_error_free(err);
            g_free(dbg);
            break;
        }
        default:
            break;
    }
    return TRUE;
}

// ─── Frame push thread (libcamera → appsrc) ──────────────────────────────────

static void frame_push_thread() {
    GstClockTime pts = 0;
    // At 60fps, each frame is ~16.67ms
    const GstClockTime frame_duration = GST_SECOND / 60;

    while (g_running) {
        Frame frame;
        {
            std::unique_lock<std::mutex> lock(g_queue_mutex);
            g_queue_cv.wait(lock, [] {
                return !g_frame_queue.empty() || !g_running;
            });
            if (!g_running && g_frame_queue.empty()) break;
            frame = std::move(g_frame_queue.front());
            g_frame_queue.pop();
        }

        // Apply bitrate change if pending
        if (g_bitrate_changed.exchange(false))
            apply_bitrate(g_bitrate.load());

        // Wrap frame data in a GstBuffer
        GstBuffer *buf = gst_buffer_new_allocate(
            nullptr, frame.data.size(), nullptr
        );
        GstMapInfo map;
        gst_buffer_map(buf, &map, GST_MAP_WRITE);
        std::memcpy(map.data, frame.data.data(), frame.data.size());
        gst_buffer_unmap(buf, &map);

        GST_BUFFER_PTS(buf)      = pts;
        GST_BUFFER_DURATION(buf) = frame_duration;
        pts += frame_duration;

        GstFlowReturn ret = gst_app_src_push_buffer(
            GST_APP_SRC(g_appsrc), buf
        );
        if (ret != GST_FLOW_OK) {
            std::cerr << "[GST] appsrc push error: " << ret << "\n";
        }
    }

    gst_app_src_end_of_stream(GST_APP_SRC(g_appsrc));
}

// ─── libcamera request callback ──────────────────────────────────────────────

static void on_request_completed(Request *request) {
    if (request->status() == Request::RequestCancelled) return;

    const auto &buffers = request->buffers();
    for (auto &[stream, fb] : buffers) {
        // Map the first plane (YUV420 Y plane start)
        const FrameBuffer::Plane &plane0 = fb->planes()[0];

        // For simplicity we copy all planes into a contiguous buffer
        // A production impl would use dmabuf / zero-copy
        size_t total = 0;
        for (auto &p : fb->planes()) total += p.length;

        Frame frame;
        frame.data.resize(total);
        frame.width  = stream->configuration().size.width;
        frame.height = stream->configuration().size.height;

        size_t offset = 0;
        for (const auto &plane : fb->planes()) {
            void *mem = mmap(nullptr, plane.length, PROT_READ,
                             MAP_SHARED, plane.fd.get(), plane.offset);
            if (mem != MAP_FAILED) {
                std::memcpy(frame.data.data() + offset, mem, plane.length);
                munmap(mem, plane.length);
            }
            offset += plane.length;
        }

        {
            std::lock_guard<std::mutex> lock(g_queue_mutex);
            if (g_frame_queue.size() < MAX_QUEUE) {
                g_frame_queue.push(std::move(frame));
            }
            // else: drop frame (encoder can't keep up)
        }
        g_queue_cv.notify_one();
    }

    // Requeue the request so libcamera keeps delivering frames
    if (g_running) {
        request->reuse(Request::ReuseBuffers);
        s_cam->queueRequest(request);
    }
}

// ─── Build GStreamer pipeline ─────────────────────────────────────────────────

static bool build_pipeline(
    const std::string &host,
    uint16_t           port,
    uint32_t           width,
    uint32_t           height,
    uint32_t           framerate,
    uint32_t           initial_bitrate,
    uint8_t            fec_percentage
) {
    gst_init(nullptr, nullptr);

    // Try hardware encoder first, fall back to software
    g_encoder = gst_element_factory_make("v4l2h264enc", "encoder");
    bool hw_enc = (g_encoder != nullptr);
    if (!hw_enc) {
        std::cerr << "[GST] v4l2h264enc not found, falling back to x264enc\n";
        g_encoder = gst_element_factory_make("x264enc", "encoder");
        if (!g_encoder) {
            std::cerr << "[GST] No H264 encoder found!\n";
            return false;
        }
    }

    g_pipeline  = gst_pipeline_new("streamer");
    g_appsrc    = gst_element_factory_make("appsrc",       "source");
    auto *parse  = gst_element_factory_make("h264parse",   "parse");
    auto *pay    = gst_element_factory_make("rtph264pay",  "pay");
    g_fecenc    = gst_element_factory_make("rtpulpfecenc", "fec");
    auto *sink   = gst_element_factory_make("udpsink",     "sink");

    if (!g_pipeline || !g_appsrc || !g_encoder || !parse || !pay || !g_fecenc || !sink) {
        std::cerr << "[GST] Failed to create elements\n";
        return false;
    }

    // ── appsrc caps: raw YUV420 from libcamera ────────────────────────────
    GstCaps *src_caps = gst_caps_new_simple(
        "video/x-raw",
        "format",    G_TYPE_STRING,  "I420",
        "width",     G_TYPE_INT,     (gint)width,
        "height",    G_TYPE_INT,     (gint)height,
        "framerate", GST_TYPE_FRACTION, (gint)framerate, 1,
        nullptr
    );
    g_object_set(G_OBJECT(g_appsrc),
        "caps",         src_caps,
        "is-live",      TRUE,
        "format",       GST_FORMAT_TIME,
        "do-timestamp", FALSE,
        nullptr
    );
    gst_caps_unref(src_caps);

    // ── Encoder settings ──────────────────────────────────────────────────
    if (hw_enc) {
        // v4l2h264enc: set initial bitrate via extra-controls
        GstStructure *ctrls = gst_structure_new(
            "extra-controls",
            "video_bitrate",           G_TYPE_INT, (gint)initial_bitrate,
            "h264_profile",            G_TYPE_INT, 0,   // Baseline
            "h264_level",              G_TYPE_INT, 41,  // Level 4.1
            "video_gop_size",          G_TYPE_INT, (gint)(framerate / 2), // keyframe every 0.5s
            "video_b_frames",          G_TYPE_INT, 0,   // no B-frames
            nullptr
        );
        g_object_set(G_OBJECT(g_encoder), "extra-controls", ctrls, nullptr);
        gst_structure_free(ctrls);
    } else {
        // x264enc software fallback
        g_object_set(G_OBJECT(g_encoder),
            "bitrate",       initial_bitrate / 1000,  // x264enc uses kbps
            "tune",          0x00000004,               // zerolatency
            "speed-preset",  1,                        // ultrafast
            "key-int-max",   (guint)(framerate / 2),
            "bframes",       0u,
            nullptr
        );
    }

    // ── rtph264pay ────────────────────────────────────────────────────────
    g_object_set(G_OBJECT(pay),
        "config-interval", -1,
        "pt",              96,
        "mtu",             1200u,
        nullptr
    );

    // ── FEC ───────────────────────────────────────────────────────────────
    g_object_set(G_OBJECT(g_fecenc),
        "percentage", (guint)fec_percentage,
        "pt",         122u,
        nullptr
    );

    // ── h264parse ─────────────────────────────────────────────────────────
    g_object_set(G_OBJECT(parse),
        "config-interval", -1,
        nullptr
    );

    // ── udpsink ───────────────────────────────────────────────────────────
    g_object_set(G_OBJECT(sink),
        "host", host.c_str(),
        "port", (gint)port,
        "sync", FALSE,
        "async", FALSE,
        nullptr
    );

    // ── Add and link ──────────────────────────────────────────────────────
    gst_bin_add_many(GST_BIN(g_pipeline),
        g_appsrc, g_encoder, parse, pay, g_fecenc, sink,
        nullptr
    );

    if (!gst_element_link_many(g_appsrc, g_encoder, parse, pay, g_fecenc, sink, nullptr)) {
        std::cerr << "[GST] Failed to link pipeline\n";
        return false;
    }

    // ── Bus watcher ───────────────────────────────────────────────────────
    GstBus *bus = gst_element_get_bus(g_pipeline);
    gst_bus_add_watch(bus, bus_callback, nullptr);
    gst_object_unref(bus);

    return true;
}

// ─── libcamera setup ─────────────────────────────────────────────────────────

static std::shared_ptr<Camera> setup_camera(
    CameraManager        &mgr,
    uint32_t              width,
    uint32_t              height,
    uint32_t              framerate,
    std::vector<std::unique_ptr<Request>> &requests_out,
    FrameBufferAllocator *&alloc_out
) {
    if (mgr.cameras().empty()) {
        std::cerr << "[CAM] No cameras found\n";
        return nullptr;
    }

    auto cam = mgr.cameras()[0];
    if (cam->acquire()) {
        std::cerr << "[CAM] Failed to acquire camera\n";
        return nullptr;
    }

    auto config = cam->generateConfiguration({StreamRole::VideoRecording});
    if (!config) {
        std::cerr << "[CAM] Failed to generate config\n";
        return nullptr;
    }

    StreamConfiguration &scfg = config->at(0);
    scfg.pixelFormat = formats::YUV420;
    scfg.size        = {width, height};
    scfg.bufferCount = 4;

    switch (config->validate()) {
        case CameraConfiguration::Valid:    break;
        case CameraConfiguration::Adjusted: std::cerr << "[CAM] Config adjusted\n"; break;
        case CameraConfiguration::Invalid:
            std::cerr << "[CAM] Invalid config\n";
            return nullptr;
    }

    if (cam->configure(config.get())) {
        std::cerr << "[CAM] Failed to configure camera\n";
        return nullptr;
    }

    alloc_out = new FrameBufferAllocator(cam);
    Stream *stream = scfg.stream();
    if (alloc_out->allocate(stream) < 0) {
        std::cerr << "[CAM] Buffer allocation failed\n";
        return nullptr;
    }

    // Set frame duration controls (for framerate)
    ControlList controls(cam->controls());
    int64_t frame_duration = 1'000'000 / framerate; // microseconds
    controls.set(controls::FrameDurationLimits,
        Span<const int64_t, 2>({frame_duration, frame_duration}));

    for (const auto &fb : alloc_out->buffers(stream)) {
        auto req = cam->createRequest();
        if (!req) {
            std::cerr << "[CAM] Failed to create request\n";
            return nullptr;
        }
        if (req->addBuffer(stream, fb.get())) {
            std::cerr << "[CAM] Failed to add buffer to request\n";
            return nullptr;
        }
        requests_out.push_back(std::move(req));
    }

    return cam;
}

// ─── main ────────────────────────────────────────────────────────────────────

int main(int argc, char *argv[]) {
    // ── Parse args ────────────────────────────────────────────────────────
    std::string host     = "127.0.0.1";
    uint16_t    port     = 9600;
    uint32_t    width    = 720;
    uint32_t    height   = 480;
    uint32_t    fps      = 60;
    uint32_t    bitrate  = 1'000'000;
    uint8_t     fec_pct  = 30;

    for (int i = 1; i < argc; ++i) {
        std::string arg = argv[i];
        if (arg == "--host"    && i+1 < argc) host    = argv[++i];
        else if (arg == "--port"    && i+1 < argc) port    = (uint16_t)std::stoi(argv[++i]);
        else if (arg == "--width"   && i+1 < argc) width   = std::stoul(argv[++i]);
        else if (arg == "--height"  && i+1 < argc) height  = std::stoul(argv[++i]);
        else if (arg == "--fps"     && i+1 < argc) fps     = std::stoul(argv[++i]);
        else if (arg == "--bitrate" && i+1 < argc) bitrate = std::stoul(argv[++i]);
        else if (arg == "--fec"     && i+1 < argc) fec_pct = (uint8_t)std::stoi(argv[++i]);
        else if (arg == "--help") {
            std::cout <<
                "Usage: streamer [options]\n"
                "  --host     <ip>    Destination IP    (default: 127.0.0.1)\n"
                "  --port     <port>  Destination port  (default: 9600)\n"
                "  --width    <px>    Frame width        (default: 720)\n"
                "  --height   <px>    Frame height       (default: 480)\n"
                "  --fps      <fps>   Framerate          (default: 60)\n"
                "  --bitrate  <bps>   Initial bitrate    (default: 1000000)\n"
                "  --fec      <pct>   FEC percentage     (default: 30)\n"
                "\nSend new bitrate (bps) via stdin: echo 500000\n";
            return 0;
        }
    }

    std::cerr << "[STREAMER] " << width << "x" << height << "@" << fps
              << "fps → " << host << ":" << port
              << " bitrate=" << bitrate/1000 << "kbps fec=" << (int)fec_pct << "%\n";

    // ── Signals ───────────────────────────────────────────────────────────
    std::signal(SIGINT,  on_signal);
    std::signal(SIGTERM, on_signal);

    // ── GStreamer pipeline ────────────────────────────────────────────────
    if (!build_pipeline(host, port, width, height, fps, bitrate, fec_pct))
        return 1;

    // ── libcamera ─────────────────────────────────────────────────────────
    CameraManager cam_mgr;
    if (cam_mgr.start()) {
        std::cerr << "[CAM] Failed to start CameraManager\n";
        return 1;
    }

    FrameBufferAllocator *alloc = nullptr;
    std::vector<std::unique_ptr<Request>> requests;
    auto cam = setup_camera(cam_mgr, width, height, fps, requests, alloc);
    if (!cam) return 1;

    // Store cam pointer for requeue in callback
    s_cam = cam.get();

    cam->requestCompleted.connect(&on_request_completed);

    // ── Start everything ──────────────────────────────────────────────────
    gst_element_set_state(g_pipeline, GST_STATE_PLAYING);

    ControlList cam_controls(cam->controls());
    if (cam->start(&cam_controls)) {
        std::cerr << "[CAM] Failed to start camera\n";
        return 1;
    }

    // Queue all requests
    for (auto &req : requests)
        cam->queueRequest(req.get());

    // ── Threads ───────────────────────────────────────────────────────────
    std::thread push_thread(frame_push_thread);
    std::thread stdin_thread(stdin_reader_thread);

    // ── GLib main loop (for GStreamer bus) ────────────────────────────────
    GMainLoop *loop = g_main_loop_new(nullptr, FALSE);
    // Run until g_running is false
    std::thread glib_thread([&loop] {
        while (g_running) std::this_thread::sleep_for(std::chrono::milliseconds(100));
        g_main_loop_quit(loop);
    });
    g_main_loop_run(loop);
    g_main_loop_unref(loop);

    // ── Cleanup ───────────────────────────────────────────────────────────
    g_running = false;
    g_queue_cv.notify_all();

    cam->stop();
    cam->release();
    cam_mgr.stop();

    if (alloc) delete alloc;

    push_thread.join();
    stdin_thread.join();
    glib_thread.join();

    gst_element_set_state(g_pipeline, GST_STATE_NULL);
    gst_object_unref(g_pipeline);

    std::cerr << "[STREAMER] Done\n";
    return 0;
}
