/**
 * streamer.cpp
 *
 * Raspberry Pi H264 RTP streamer with dynamic bitrate control.
 * Based on rpicam-apps H264Encoder architecture (direct V4L2 /dev/video11).
 *
 * Pipeline:
 *   libcamera (YUV420) → V4L2Encoder (/dev/video11) → appsrc
 *   → h264parse → rtph264pay → rtpulpfecenc → udpsink
 *
 * Bitrate control: write integer bps to stdin e.g. `echo 500000`
 * FEC control: write `fec:N` (0-50, overhead percent) e.g. `echo fec:10`
 *
 * Build:
 *   g++ streamer.cpp -o streamer \
 *     $(pkg-config --cflags --libs gstreamer-1.0 gstreamer-app-1.0) \
 *     $(pkg-config --cflags --libs libcamera) \
 *     -lpthread -std=c++17
 *
 * Run:
 *   ./streamer --host 37.59.106.4 --port 9600
 */

#include <fcntl.h>
#include <gst/app/gstappsrc.h>
#include <gst/gst.h>
#include <libcamera/libcamera.h>
#include <linux/videodev2.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <csignal>
#include <cstdint>
#include <cstring>
#include <functional>
#include <iostream>
#include <memory>
#include <mutex>
#include <queue>
#include <stdexcept>
#include <string>
#include <thread>
#include <unordered_map>

using namespace libcamera;

// ─── Latency tracking ────────────────────────────────────────────────────────

static std::mutex g_latency_mutex;
// Keyed by sensor timestamp (frame.timestamp_us), not our own per-frame
// sequence number: bcm2835-codec does not propagate the OUTPUT buffer's
// `sequence` field to the corresponding CAPTURE buffer (confirmed on-device
// -- it read back as a constant 0 across hundreds of frames), so keying on
// it made every lookup below miss after the very first frame and left
// encode/total latency frozen. `timestamp` IS propagated end-to-end by the
// M2M convention (also confirmed on-device: advances by a clean ~16.6 ms/
// frame at 60fps), so it's the reliable correlator.
static std::unordered_map<int64_t, std::chrono::steady_clock::time_point>
    g_encode_entry;
static double g_capture_latency_ms = 0.0;
static double g_encode_latency_ms = 0.0;
static double g_total_latency_ms = 0.0;
// Atomic: incremented on the libcamera callback thread, read from the
// logging thread for the periodic captured-fps figure below.
static std::atomic<uint64_t> g_frame_seq{0};

// Frames captured by libcamera but discarded before ever reaching the
// network -- invisible to the client, which only sees what it receives.
// Split by stage so a fps drop can be traced to "camera not producing frames"
// vs "queue full" vs "V4L2 encoder input starved" instead of guessed at.
static std::atomic<uint64_t> g_queue_drops{0};
static std::atomic<uint64_t> g_encoder_drops{0};
static std::atomic<uint64_t> g_encoded_count{0};

// ─── Global state ────────────────────────────────────────────────────────────

static std::atomic<bool> g_running{true};
static std::atomic<uint32_t> g_bitrate{1000000};
static std::atomic<bool> g_bitrate_changed{false};
// Set by the control readers, consumed by the encoder thread. Applying it
// there keeps every ioctl on fd_ on a single thread.
static std::atomic<bool> g_force_keyframe{false};

static GstElement *g_pipeline = nullptr;
static GstElement *g_appsrc = nullptr;
static GstElement *g_fecenc = nullptr;
static Camera *s_cam = nullptr;

// Payload type carrying FEC packets, distinct from the video payload (96,
// set on rtph264pay below) so both share one RTP flow/SSRC. Must match the
// `pt` the client's `rtpulpfecdec` is configured with (see FEC_PT in
// client/src/video.rs).
static constexpr gint FEC_PT = 122;

// ─── RT scheduling ────────────────────────────────────────────────────────
//
// Promotes the calling thread to SCHED_FIFO. Requires CAP_SYS_NICE, granted
// with `sudo setcap cap_sys_nice=eip <binary>` on the built binary after
// each rebuild (capabilities don't survive a fresh build output). The usual
// PAM/rtprio-rlimit route (/etc/security/limits.d) does not apply on this
// Pi: sshd here is dropbear (reverse-tunneled for CGNAT traversal), which
// never runs PAM session hooks, so a plain user can't self-raise. Also note
// setcap is stripped by nosuid mounts (e.g. /tmp) -- the binary must live on
// the regular rootfs.
// Fails gracefully without the capability: every promoted loop below blocks
// in poll()/condvar wait rather than spinning, so a missing capability just
// means "no priority boost", not a functional regression.
static void set_rt_priority(const char *name, int prio) {
  sched_param sp{};
  sp.sched_priority = prio;
  if (pthread_setschedparam(pthread_self(), SCHED_FIFO, &sp) != 0)
    std::cerr << "[RT] " << name << ": SCHED_FIFO unavailable ("
              << strerror(errno) << "), staying SCHED_OTHER" << std::endl;
}

// ─── V4L2 H264 Encoder (mirrors rpicam-apps H264Encoder) ─────────────────────
//
// Talks directly to /dev/video11 (bcm2835-codec-encode).
// Input: YUV420 frames via mmap buffers.
// Output: H264 NAL units via callback.
//
// Two threads:
//   poll_thread_   — waits for V4L2 events (resolution changes etc.)
//   output_thread_ — dequeues encoded H264 buffers and calls output_cb_

class V4L2H264Encoder {
public:
  using OutputCallback =
      std::function<void(uint8_t *data, size_t size, int64_t timestamp_us,
                         bool keyframe, uint64_t seq)>;

  V4L2H264Encoder(uint32_t width, uint32_t height, uint32_t fps,
                  uint32_t bitrate, uint32_t stride, uint32_t gop,
                  OutputCallback cb)
      : width_(width), height_(height), fps_(fps), bitrate_(bitrate),
        stride_(stride), gop_(gop), output_cb_(cb) {
    fd_ = open("/dev/video11", O_RDWR | O_NONBLOCK);
    if (fd_ < 0)
      throw std::runtime_error("Cannot open /dev/video11");

    setup_input();
    setup_output();
    set_bitrate(bitrate);
    set_profile_level();
    alloc_input_buffers();
    alloc_output_buffers();
  }

  ~V4L2H264Encoder() {
    stop();
    for (auto &b : input_buffers_) {
      munmap(b.mem, b.size);
    }
    for (auto &b : output_buffers_) {
      munmap(b.mem, b.size);
    }
    close(fd_);
  }

  void start() {
    int type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    if (ioctl(fd_, VIDIOC_STREAMON, &type) < 0)
      throw std::runtime_error("STREAMON input failed");
    type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    if (ioctl(fd_, VIDIOC_STREAMON, &type) < 0)
      throw std::runtime_error("STREAMON output failed");

    // Queue all output (capture) buffers
    for (size_t i = 0; i < output_buffers_.size(); i++) {
      v4l2_buffer buf{};
      v4l2_plane planes[1]{};
      buf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
      buf.memory = V4L2_MEMORY_MMAP;
      buf.index = i;
      buf.length = 1;
      buf.m.planes = planes;
      if (ioctl(fd_, VIDIOC_QBUF, &buf) < 0)
        throw std::runtime_error("QBUF output failed");
    }

    poll_thread_ = std::thread(&V4L2H264Encoder::poll_loop, this);
    output_thread_ = std::thread(&V4L2H264Encoder::output_loop, this);
  }

  void stop() {
    if (!running_)
      return;
    running_ = false;

    // Send EOS
    v4l2_encoder_cmd cmd{};
    cmd.cmd = V4L2_ENC_CMD_STOP;
    ioctl(fd_, VIDIOC_ENCODER_CMD, &cmd);

    if (poll_thread_.joinable())
      poll_thread_.join();
    if (output_thread_.joinable())
      output_thread_.join();

    int type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    ioctl(fd_, VIDIOC_STREAMOFF, &type);
    type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    ioctl(fd_, VIDIOC_STREAMOFF, &type);
  }

  // Encode one YUV420 frame. Returns false if no input buffer available.
  bool encode(const uint8_t *yuv, size_t size, int64_t timestamp_us,
              uint64_t seq) {
    // Apply pending bitrate change
    if (g_bitrate_changed.exchange(false))
      set_bitrate(g_bitrate.load());

    // Apply a pending keyframe request. With a long GOP this is the only
    // thing that produces an IDR, so a receiver that has lost sync stays
    // broken until someone asks.
    if (g_force_keyframe.exchange(false))
      force_keyframe();

    // Get a free input buffer
    v4l2_buffer buf{};
    v4l2_plane planes[1]{};
    buf.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    buf.memory = V4L2_MEMORY_MMAP;
    buf.length = 1;
    buf.m.planes = planes;

    if (ioctl(fd_, VIDIOC_DQBUF, &buf) < 0) {
      // No free input buffer yet — caller should retry or drop
      return false;
    }

    size_t idx = buf.index;
    size_t copy = std::min(size, input_buffers_[idx].size);
    memcpy(input_buffers_[idx].mem, yuv, copy);

    buf.m.planes[0].bytesused = copy;
    buf.m.planes[0].length = input_buffers_[idx].size;
    buf.timestamp.tv_sec = timestamp_us / 1'000'000;
    buf.timestamp.tv_usec = timestamp_us % 1'000'000;
    buf.sequence = seq;

    if (ioctl(fd_, VIDIOC_QBUF, &buf) < 0) {
      std::cerr << "[ENC] QBUF input failed" << std::endl;
      return false;
    }
    return true;
  }

  void set_bitrate(uint32_t bps) {
    v4l2_control ctrl{};
    ctrl.id = V4L2_CID_MPEG_VIDEO_BITRATE;
    ctrl.value = bps;
    if (xioctl(fd_, VIDIOC_S_CTRL, &ctrl) < 0)
      std::cerr << "[ENC] Failed to set bitrate to " << bps << "" << std::endl;

    // NOTE: V4L2_CID_MPEG_VIDEO_BITRATE_PEAK is not implemented by
    // bcm2835-codec (absent from --list-ctrls), so it is not set here.
  }

  // Ask the encoder to emit an IDR on the next frame. Button control:
  // the value is ignored, writing it is what triggers the action.
  void force_keyframe() {
    v4l2_control ctrl{};
    ctrl.id = V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME;
    ctrl.value = 1;
    if (xioctl(fd_, VIDIOC_S_CTRL, &ctrl) < 0)
      std::cerr << "[ENC] force keyframe failed: " << strerror(errno) << ""
                << std::endl;
  }

private:
  struct Buffer {
    void *mem;
    size_t size;
  };

  int fd_ = -1;
  bool running_ = true;
  uint32_t width_, height_, fps_, bitrate_, stride_, gop_;

  std::vector<Buffer> input_buffers_;
  std::vector<Buffer> output_buffers_;

  std::thread poll_thread_;
  std::thread output_thread_;
  OutputCallback output_cb_;

  static int xioctl(int fd, unsigned long req, void *arg) {
    int r;
    do {
      r = ioctl(fd, req, arg);
    } while (r == -1 && errno == EINTR);
    return r;
  }

  void setup_input() {
    v4l2_format fmt{};
    fmt.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    fmt.fmt.pix_mp.width = width_;
    fmt.fmt.pix_mp.height = height_;
    fmt.fmt.pix_mp.pixelformat = V4L2_PIX_FMT_YUV420;
    fmt.fmt.pix_mp.field = V4L2_FIELD_ANY;
    fmt.fmt.pix_mp.colorspace = V4L2_COLORSPACE_JPEG; // = BT.601 full range
    fmt.fmt.pix_mp.num_planes = 1;
    fmt.fmt.pix_mp.plane_fmt[0].bytesperline = stride_;
    fmt.fmt.pix_mp.plane_fmt[0].sizeimage = stride_ * height_ * 3 / 2;
    if (xioctl(fd_, VIDIOC_S_FMT, &fmt) < 0)
      throw std::runtime_error("S_FMT input failed");

    // Set framerate
    v4l2_streamparm parm{};
    parm.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    parm.parm.output.timeperframe.numerator = 1;
    parm.parm.output.timeperframe.denominator = fps_;
    xioctl(fd_, VIDIOC_S_PARM, &parm); // best effort
  }

  void setup_output() {
    v4l2_format fmt{};
    fmt.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    fmt.fmt.pix_mp.width = width_;
    fmt.fmt.pix_mp.height = height_;
    fmt.fmt.pix_mp.pixelformat = V4L2_PIX_FMT_H264;
    fmt.fmt.pix_mp.field = V4L2_FIELD_ANY;
    fmt.fmt.pix_mp.colorspace = V4L2_COLORSPACE_DEFAULT;
    fmt.fmt.pix_mp.num_planes = 1;
    // Bitstream buffer: generous size
    fmt.fmt.pix_mp.plane_fmt[0].sizeimage = 512 * 1024;
    if (xioctl(fd_, VIDIOC_S_FMT, &fmt) < 0)
      throw std::runtime_error("S_FMT output failed");
  }

  void set_profile_level() {
    v4l2_control ctrl{};

    // High profile
    ctrl.id = V4L2_CID_MPEG_VIDEO_H264_PROFILE;
    ctrl.value = V4L2_MPEG_VIDEO_H264_PROFILE_HIGH;
    xioctl(fd_, VIDIOC_S_CTRL, &ctrl);

    // Level 4.1
    ctrl.id = V4L2_CID_MPEG_VIDEO_H264_LEVEL;
    ctrl.value = V4L2_MPEG_VIDEO_H264_LEVEL_4_1;
    xioctl(fd_, VIDIOC_S_CTRL, &ctrl);

    // Inline SPS/PPS in every keyframe (like rpicam-vid --inline)
    ctrl.id = V4L2_CID_MPEG_VIDEO_REPEAT_SEQ_HEADER;
    ctrl.value = 1;
    xioctl(fd_, VIDIOC_S_CTRL, &ctrl);

    // Long GOP. Periodic keyframes are several times the size of a P-frame,
    // and on a constrained uplink each one queues in the carrier's buffer and
    // shows up as a latency spike. Keyframes are requested on demand instead
    // (see force_keyframe); this is only a recovery net if a request is lost.
    ctrl.id = V4L2_CID_MPEG_VIDEO_H264_I_PERIOD;
    ctrl.value = gop_;
    xioctl(fd_, VIDIOC_S_CTRL, &ctrl);

    // Disable B-frames
    ctrl.id = V4L2_CID_MPEG_VIDEO_B_FRAMES;
    ctrl.value = 0;
    xioctl(fd_, VIDIOC_S_CTRL, &ctrl);

    // VBR mode
    ctrl.id = V4L2_CID_MPEG_VIDEO_BITRATE_MODE;
    ctrl.value = V4L2_MPEG_VIDEO_BITRATE_MODE_VBR;
    xioctl(fd_, VIDIOC_S_CTRL, &ctrl);

    ctrl.id = V4L2_CID_MPEG_VIDEO_H264_MIN_QP;
    ctrl.value = 20; // static scenes need fewer bits; frees headroom for motion
    xioctl(fd_, VIDIOC_S_CTRL, &ctrl);

    ctrl.id = V4L2_CID_MPEG_VIDEO_H264_MAX_QP;
    // Raised: for FPV, a briefly blockier frame beats a 200ms uplink
    // spike. The rate controller can only shrink a burst frame by
    // raising QP; a low cap forces it to blow the size budget instead.
    // (bcm2835-codec exposes no macroblock/frame-level rate control or
    // per-frame size cap to lean on instead -- confirmed absent from
    // `v4l2-ctl -d /dev/video11 --list-ctrls-menus`.)
    ctrl.value = 42;
    xioctl(fd_, VIDIOC_S_CTRL, &ctrl);
  }

  void alloc_input_buffers(uint32_t count = 6) {
    v4l2_requestbuffers req{};
    req.count = count;
    req.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    req.memory = V4L2_MEMORY_MMAP;
    if (xioctl(fd_, VIDIOC_REQBUFS, &req) < 0)
      throw std::runtime_error("REQBUFS input failed");

    input_buffers_.resize(req.count);
    for (uint32_t i = 0; i < req.count; i++) {
      v4l2_buffer buf{};
      v4l2_plane planes[1]{};
      buf.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
      buf.memory = V4L2_MEMORY_MMAP;
      buf.index = i;
      buf.length = 1;
      buf.m.planes = planes;
      if (xioctl(fd_, VIDIOC_QUERYBUF, &buf) < 0)
        throw std::runtime_error("QUERYBUF input failed");

      input_buffers_[i].size = buf.m.planes[0].length;
      input_buffers_[i].mem =
          mmap(nullptr, buf.m.planes[0].length, PROT_READ | PROT_WRITE,
               MAP_SHARED, fd_, buf.m.planes[0].m.mem_offset);
      if (input_buffers_[i].mem == MAP_FAILED)
        throw std::runtime_error("mmap input buffer failed");

      // Pre-queue all input buffers so encoder can receive frames
      if (xioctl(fd_, VIDIOC_QBUF, &buf) < 0)
        throw std::runtime_error("QBUF input pre-queue failed");
    }
  }

  void alloc_output_buffers(uint32_t count = 6) {
    v4l2_requestbuffers req{};
    req.count = count;
    req.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    req.memory = V4L2_MEMORY_MMAP;
    if (xioctl(fd_, VIDIOC_REQBUFS, &req) < 0)
      throw std::runtime_error("REQBUFS output failed");

    output_buffers_.resize(req.count);
    for (uint32_t i = 0; i < req.count; i++) {
      v4l2_buffer buf{};
      v4l2_plane planes[1]{};
      buf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
      buf.memory = V4L2_MEMORY_MMAP;
      buf.index = i;
      buf.length = 1;
      buf.m.planes = planes;
      if (xioctl(fd_, VIDIOC_QUERYBUF, &buf) < 0)
        throw std::runtime_error("QUERYBUF output failed");

      output_buffers_[i].size = buf.m.planes[0].length;
      output_buffers_[i].mem =
          mmap(nullptr, buf.m.planes[0].length, PROT_READ | PROT_WRITE,
               MAP_SHARED, fd_, buf.m.planes[0].m.mem_offset);
      if (output_buffers_[i].mem == MAP_FAILED)
        throw std::runtime_error("mmap output buffer failed");
    }
  }

  void poll_loop() {
    set_rt_priority("enc-poll", 20);
    while (running_) {
      pollfd pfd{fd_, POLLPRI, 0};
      int ret = poll(&pfd, 1, 200);
      if (ret < 0 && errno != EINTR)
        break;
      if (ret > 0 && (pfd.revents & POLLPRI)) {
        v4l2_event ev{};
        xioctl(fd_, VIDIOC_DQEVENT, &ev);
        if (ev.type == V4L2_EVENT_EOS) {
          std::cerr << "[ENC] EOS event" << std::endl;
          running_ = false;
        }
      }
    }
  }

  void output_loop() {
    set_rt_priority("enc-output", 40);
    while (running_) {
      pollfd pfd{fd_, POLLIN, 0};
      int ret = poll(&pfd, 1, 200);
      if (ret < 0 && errno != EINTR)
        break;
      if (!(pfd.revents & POLLIN))
        continue;

      v4l2_buffer buf{};
      v4l2_plane planes[1]{};
      buf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
      buf.memory = V4L2_MEMORY_MMAP;
      buf.length = 1;
      buf.m.planes = planes;

      if (xioctl(fd_, VIDIOC_DQBUF, &buf) < 0) {
        if (errno == EAGAIN)
          continue;
        break;
      }

      size_t idx = buf.index;
      size_t used = buf.m.planes[0].bytesused;
      bool keyframe = (buf.flags & V4L2_BUF_FLAG_KEYFRAME) != 0;
      int64_t ts_us =
          (int64_t)buf.timestamp.tv_sec * 1'000'000 + buf.timestamp.tv_usec;
      uint64_t seq = buf.sequence;

      if (used > 0)
        output_cb_((uint8_t *)output_buffers_[idx].mem, used, ts_us, keyframe,
                   seq);

      // Requeue
      buf.m.planes[0].bytesused = 0;
      xioctl(fd_, VIDIOC_QBUF, &buf);
    }
  }
};

static std::atomic<V4L2H264Encoder *> g_encoder_ptr{nullptr};

// ─── Frame queue: libcamera → encoder ────────────────────────────────────────

struct Frame {
  std::vector<uint8_t> data;
  uint32_t width;
  uint32_t height;
  int64_t timestamp_us;
  uint64_t seq;
};

static std::queue<Frame> g_frame_queue;
static std::mutex g_queue_mutex;
static std::condition_variable g_queue_cv;
// 2 slots: at most one frame in flight to the encoder plus one waiting
// (~16-33 ms standing delay at 60fps). Deeper queues only smooth over an
// encoder that can't keep up, which for FPV should surface as drops instead.
static constexpr size_t MAX_QUEUE = 2;

// ─── Signal handler ──────────────────────────────────────────────────────────

static void on_signal(int) {
  g_running = false;
  g_queue_cv.notify_all();
  if (g_pipeline)
    gst_element_send_event(g_pipeline, gst_event_new_eos());
}

// ─── stdin bitrate reader
// ─────────────────────────────────────────────────────

static constexpr const char *CTRL_FIFO = "/tmp/streamer_ctrl";

// One control line: "idr" requests a keyframe, "fec:N" sets FEC overhead
// percentage, an integer sets the bitrate.
//
// "idr"/bitrate only raise a flag; the encoder thread applies it, so every
// ioctl on the encoder fd stays on one thread. "fec:N" is applied directly
// here instead: it's a plain GObject property set on `rtpulpfecenc`, which is
// safe to do from any thread while the pipeline is PLAYING (no fd/ioctl
// involved, unlike the V4L2 controls), so there's no need to relay it through
// the encoder thread.
static void handle_command(std::string s) {
  s.erase(0, s.find_first_not_of(" \t\r\n"));
  s.erase(s.find_last_not_of(" \t\r\n") + 1);
  if (s.empty())
    return;

  if (s == "idr" || s == "key") {
    g_force_keyframe.store(true);
    std::cout << "[CTRL] Keyframe requested" << std::endl;
    return;
  }

  if (s.rfind("fec:", 0) == 0) {
    try {
      uint32_t pct = std::clamp((uint32_t)std::stoul(s.substr(4)), 0u, 50u);
      if (g_fecenc) {
        g_object_set(G_OBJECT(g_fecenc), "percentage", pct,
                     "percentage-important", pct, nullptr);
        std::cout << "[CTRL] FEC -> " << pct << "%" << std::endl;
      }
    } catch (...) {
      std::cerr << "[CTRL] Bad fec command: " << s << "" << std::endl;
    }
    return;
  }

  try {
    uint32_t bps = std::clamp((uint32_t)std::stoul(s), 100'000u, 8'000'000u);
    // Only flag a real change: the slave writes this on every server pong,
    // and re-applying the same value churns the encoder's rate control.
    if (g_bitrate.exchange(bps) != bps) {
      g_bitrate_changed.store(true);
      std::cout << "[CTRL] Bitrate -> " << bps / 1000 << " kbps" << std::endl;
    }
  } catch (...) {
    std::cerr << "[CTRL] Unrecognised command: " << s << "" << std::endl;
  }
}

static void stdin_reader_thread() {
  std::string line;
  while (g_running && std::getline(std::cin, line))
    handle_command(line);
  std::cerr << "[CTRL] stdin closed" << std::endl;
}

static void fifo_reader_thread() {
  unlink(CTRL_FIFO);
  if (mkfifo(CTRL_FIFO, 0666) < 0) {
    std::cerr << "[CTRL] mkfifo failed: " << strerror(errno) << "" << std::endl;
    return;
  }
  std::cout << "[CTRL] Ready: echo <bps>|idr > " << CTRL_FIFO << ""
            << std::endl;
  fflush(stdout);

  while (g_running) {
    int fd = open(CTRL_FIFO, O_RDONLY);
    if (fd < 0) {
      std::this_thread::sleep_for(std::chrono::milliseconds(100));
      continue;
    }
    char buf[64]{};
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0)
      continue;

    // A single write may carry several lines; handle each.
    std::string chunk(buf, (size_t)n);
    size_t start = 0;
    while (start <= chunk.size()) {
      size_t nl = chunk.find('\n', start);
      if (nl == std::string::npos) {
        handle_command(chunk.substr(start));
        break;
      }
      handle_command(chunk.substr(start, nl - start));
      start = nl + 1;
    }
  }
  unlink(CTRL_FIFO);
}

// ─── GStreamer bus callback
// ───────────────────────────────────────────────────

static gboolean bus_callback(GstBus *, GstMessage *msg, gpointer) {
  switch (GST_MESSAGE_TYPE(msg)) {
  case GST_MESSAGE_ERROR: {
    GError *err = nullptr;
    gchar *dbg = nullptr;
    gst_message_parse_error(msg, &err, &dbg);
    std::cerr << "[GST ERROR] " << err->message << "" << std::endl;
    if (dbg)
      std::cout << "[GST DEBUG] " << dbg << "" << std::endl;
    g_error_free(err);
    g_free(dbg);
    g_running = false;
    g_queue_cv.notify_all();
    break;
  }
  case GST_MESSAGE_EOS:
    std::cout << "[GST] EOS" << std::endl;
    g_running = false;
    g_queue_cv.notify_all();
    break;
  default:
    break;
  }
  return TRUE;
}

// ─── Build GStreamer pipeline (H264 input, no encoder element)
// ────────────────

static bool build_pipeline(const std::string &host, uint16_t port) {
  gst_init(nullptr, nullptr);

  g_pipeline = gst_pipeline_new("streamer");
  g_appsrc = gst_element_factory_make("appsrc", "source");
  auto *parse = gst_element_factory_make("h264parse", "parse");
  auto *pay = gst_element_factory_make("rtph264pay", "pay");
  auto *fecenc = gst_element_factory_make("rtpulpfecenc", "fecenc");
  auto *sink = gst_element_factory_make("udpsink", "sink");

  if (!g_pipeline || !g_appsrc || !parse || !pay || !fecenc || !sink) {
    std::cerr << "[GST] Failed to create elements" << std::endl;
    return false;
  }
  g_fecenc = fecenc;

  // appsrc now receives H264 bitstream directly from our V4L2 encoder
  GstCaps *caps = gst_caps_new_simple("video/x-h264", "stream-format",
                                      G_TYPE_STRING, "byte-stream", "alignment",
                                      G_TYPE_STRING, "au", nullptr);
  g_object_set(G_OBJECT(g_appsrc), "caps", caps, "is-live", TRUE, "format",
               GST_FORMAT_TIME,
               // Bound the internal queue to 2 AUs and drop the *oldest* on
               // overflow. GST_APP_LEAKY_TYPE_DOWNSTREAM (2) is the one
               // that leaks old buffers -- verified on-device against
               // `gst-inspect-1.0 appsrc`/`queue` wording ("Leaky on
               // downstream (old buffers)"); the "upstream" value (1) does
               // the opposite (drops the newly-pushed buffer, keeps stale
               // ones), which is wrong for FPV. Downstream of this is
               // udpsink with sync=FALSE, so a stalled socket is the only
               // way this queue backs up -- newest data should win.
               "max-buffers", 2u, "leaky-type", 2, nullptr);
  gst_caps_unref(caps);

  g_object_set(G_OBJECT(parse), "config-interval", -1, nullptr);

  // Fixed SSRC so the client can configure rtpulpfecdec correctly before
  // receiving any packets. Without this, rtpulpfecdec either crashes (no
  // SSRC in caps) or FEC recovery fails (caps SSRC doesn't match packets).
  g_object_set(G_OBJECT(pay), "config-interval", -1, "pt", 96, "mtu", 1200u,
               "ssrc", (guint)0x12345678, nullptr);

  // FEC starts at 0% (clean-link default) and is retuned live by the
  // client's adaptive loop via "fec:N" on the control FIFO -- see
  // handle_command(). `multipacket` (default true) groups protected packets
  // by RTP timestamp, i.e. one FEC block per video frame, which is what
  // keeps FEC recovery within the ≤1-frame latency budget.
  g_object_set(G_OBJECT(fecenc), "pt", FEC_PT, "percentage", 0u,
               "percentage-important", 0u, nullptr);

  // max-bitrate paces UDP transmission (bytes smoothed at the socket
  // rather than dumped as one burst per AU). 12 Mbps sits 1.5x above the
  // hard bitrate ceiling enforced in handle_command(), so it only bites on
  // oversized burst frames, never on steady state.
  g_object_set(G_OBJECT(sink), "host", host.c_str(), "port", (gint)port, "sync",
               FALSE, "async", FALSE, "max-bitrate", (guint64)12'000'000,
               nullptr);

  gst_bin_add_many(GST_BIN(g_pipeline), g_appsrc, parse, pay, fecenc, sink,
                   nullptr);

  if (!gst_element_link_many(g_appsrc, parse, pay, fecenc, sink, nullptr)) {
    std::cerr << "[GST] Failed to link pipeline" << std::endl;
    return false;
  }

  GstBus *bus = gst_element_get_bus(g_pipeline);
  gst_bus_add_watch(bus, bus_callback, nullptr);
  gst_object_unref(bus);

  return true;
}

// ─── libcamera callback
// ───────────────────────────────────────────────────────

static void on_request_completed(Request *request) {
  // libcamera owns this thread; promote it once, on the first callback.
  static thread_local bool rt_done = false;
  if (!rt_done) {
    set_rt_priority("cam-callback", 45);
    rt_done = true;
  }

  if (request->status() == Request::RequestCancelled)
    return;

  // Real capture latency: from the sensor's own exposure timestamp
  // (SensorTimestamp, nanoseconds on the same CLOCK_MONOTONIC domain
  // std::chrono::steady_clock uses on Linux) to the moment userspace is
  // told the request completed. This replaces a previous measurement that
  // timed how long it took a *recycled* request buffer to complete again
  // -- which mostly reported libcamera's request-pool depth (number of
  // buffers in flight / fps) rather than anything about this frame's
  // actual capture time.
  auto &metadata = request->metadata();
  auto sensor_ts_ns = metadata.get(controls::SensorTimestamp);
  if (sensor_ts_ns) {
    struct timespec now_ts;
    clock_gettime(CLOCK_MONOTONIC, &now_ts);
    int64_t now_ns = (int64_t)now_ts.tv_sec * 1'000'000'000 + now_ts.tv_nsec;

    std::lock_guard<std::mutex> lock(g_latency_mutex);
    g_capture_latency_ms = (now_ns - *sensor_ts_ns) / 1'000'000.0;
  }

  for (auto &[stream, fb] : request->buffers()) {
    size_t total = 0;
    for (auto &p : fb->planes())
      total += p.length;

    Frame frame;
    frame.data.resize(total);
    frame.width = stream->configuration().size.width;
    frame.height = stream->configuration().size.height;
    frame.seq = g_frame_seq.fetch_add(1, std::memory_order_relaxed);
    frame.timestamp_us = sensor_ts_ns ? (*sensor_ts_ns / 1000) : 0;

    // Group planes by fd, find total mapped length needed per fd
    std::unordered_map<int, size_t> fd_total_len;
    for (const auto &plane : fb->planes()) {
      size_t end = plane.offset + plane.length;
      auto &cur = fd_total_len[plane.fd.get()];
      cur = std::max(cur, end);
    }

    // mmap each unique fd exactly once, at offset 0
    std::unordered_map<int, void *> fd_base;
    bool map_ok = true;
    for (auto &[fd, len] : fd_total_len) {
      void *base = mmap(nullptr, len, PROT_READ, MAP_SHARED, fd, 0);
      if (base == MAP_FAILED) {
        std::cerr << "[CAP] mmap failed for fd=" << fd << ": "
                  << strerror(errno) << "" << std::endl;
        map_ok = false;
        break;
      }
      fd_base[fd] = base;
    }

    if (map_ok) {
      size_t offset = 0;
      for (const auto &plane : fb->planes()) {
        void *base = fd_base[plane.fd.get()];
        memcpy(frame.data.data() + offset, (uint8_t *)base + plane.offset,
               plane.length);
        offset += plane.length;
      }
    }

    for (auto &[fd, base] : fd_base) {
      size_t len = fd_total_len[fd];
      munmap(base, len);
    }

    if (map_ok) {
      std::lock_guard<std::mutex> lock(g_queue_mutex);
      // Drop-oldest: evict stale frames so the encoder always sees the
      // most recent capture. The counter still reflects lost frames.
      while (g_frame_queue.size() >= MAX_QUEUE) {
        g_frame_queue.pop();
        g_queue_drops.fetch_add(1, std::memory_order_relaxed);
      }
      g_frame_queue.push(std::move(frame));
    }
    g_queue_cv.notify_one();
  }

  if (g_running) {
    request->reuse(Request::ReuseBuffers);
    s_cam->queueRequest(request);
  }
}

// ─── Encoder + push thread
// ────────────────────────────────────────────────────

static void encoder_thread(uint32_t width, uint32_t height, uint32_t stride,
                           uint32_t fps, uint32_t initial_bitrate,
                           uint32_t gop) {
  // Callback: push encoded H264 directly into GStreamer appsrc
  auto on_encoded = [&](uint8_t *data, size_t size, int64_t ts_us,
                        bool keyframe, uint64_t seq) {
    // Proves whether the encoder actually produced an IDR, separately from
    // whether the client ever received one. Logged against g_encoded_count
    // rather than `seq`: the latter is the V4L2 CAPTURE buffer's
    // `sequence` field, which bcm2835-codec never populates (always 0).
    uint64_t encoded_no =
        g_encoded_count.fetch_add(1, std::memory_order_relaxed);
    if (keyframe)
      std::cout << "[ENC] IDR emitted at frame " << encoded_no << " (" << size
                << " bytes)" << std::endl;

    // Latency: encode done
    {
      std::lock_guard<std::mutex> lock(g_latency_mutex);
      auto it = g_encode_entry.find(ts_us);
      if (it != g_encode_entry.end()) {
        g_encode_latency_ms = std::chrono::duration<double, std::milli>(
                                  std::chrono::steady_clock::now() - it->second)
                                  .count();

        // Total: sensor exposure to encoded output. Both terms are
        // now real per-frame measurements (see on_request_completed
        // for capture), not just an approximation.
        g_total_latency_ms = g_capture_latency_ms + g_encode_latency_ms;
        g_encode_entry.erase(it);
      }
    }

    GstBuffer *buf = gst_buffer_new_allocate(nullptr, size, nullptr);
    GstMapInfo map;
    gst_buffer_map(buf, &map, GST_MAP_WRITE);
    memcpy(map.data, data, size);
    gst_buffer_unmap(buf, &map);

    // Stamp PTS from the sensor exposure time (rebased to the first
    // valid frame) instead of appsrc's do-timestamp, so encode/queue
    // jitter doesn't leak into RTP timestamps. SensorTimestamp is
    // occasionally unavailable during camera warm-up (ts_us == 0 then,
    // confirmed on-device); skip those rather than rebasing against a
    // bogus zero, which would show up as one large PTS jump on the next
    // valid frame.
    static int64_t first_ts_us = -1;
    if (ts_us > 0) {
      if (first_ts_us < 0)
        first_ts_us = ts_us;
      GST_BUFFER_PTS(buf) = (GstClockTime)(ts_us - first_ts_us) * GST_USECOND;
      GST_BUFFER_DTS(buf) = GST_BUFFER_PTS(buf);
      GST_BUFFER_DURATION(buf) = gst_util_uint64_scale(GST_SECOND, 1, fps);
    }

    if (keyframe)
      GST_BUFFER_FLAG_UNSET(buf, GST_BUFFER_FLAG_DELTA_UNIT);
    else
      GST_BUFFER_FLAG_SET(buf, GST_BUFFER_FLAG_DELTA_UNIT);

    GstFlowReturn ret = gst_app_src_push_buffer(GST_APP_SRC(g_appsrc), buf);
    if (ret != GST_FLOW_OK)
      std::cerr << "[GST] appsrc push error: " << ret << "" << std::endl;
  };

  try {
    V4L2H264Encoder enc(width, height, fps, initial_bitrate, stride, gop,
                        on_encoded);
    enc.start();
    set_rt_priority("encoder", 40);

    // The first receiver needs an IDR to start decoding, and with a long GOP
    // the periodic one may be minutes away.
    g_force_keyframe.store(true);

    g_encoder_ptr.store(&enc);

    while (g_running) {
      Frame frame;
      {
        std::unique_lock<std::mutex> lock(g_queue_mutex);
        g_queue_cv.wait(lock,
                        [] { return !g_frame_queue.empty() || !g_running; });
        if (!g_running && g_frame_queue.empty())
          break;
        frame = std::move(g_frame_queue.front());
        g_frame_queue.pop();
      }

      // Stamp encode start
      {
        std::lock_guard<std::mutex> lock(g_latency_mutex);
        g_encode_entry[frame.timestamp_us] = std::chrono::steady_clock::now();
      }

      if (!enc.encode(frame.data.data(), frame.data.size(), frame.timestamp_us,
                      frame.seq)) {
        g_encoder_drops.fetch_add(1, std::memory_order_relaxed);
        // No free V4L2 input buffer, so this frame will never reach
        // on_encoded -- erase its entry now or it sits in the map
        // forever.
        std::lock_guard<std::mutex> lock(g_latency_mutex);
        g_encode_entry.erase(frame.timestamp_us);
      }
    }

    enc.stop();
  } catch (const std::exception &e) {
    std::cerr << "[ENC] Fatal: " << e.what() << "" << std::endl;
    g_running = false;
    g_queue_cv.notify_all();
  }

  gst_app_src_end_of_stream(GST_APP_SRC(g_appsrc));
}

// ─── libcamera setup ─────────────────────────────────────────────────────────

static std::shared_ptr<Camera>
setup_camera(CameraManager &mgr, uint32_t width, uint32_t height, uint32_t fps,
             std::vector<std::unique_ptr<Request>> &requests_out,
             FrameBufferAllocator *&alloc_out, unsigned int &stride_out) {
  if (mgr.cameras().empty()) {
    std::cerr << "[CAM] No cameras found" << std::endl;
    return nullptr;
  }
  auto cam = mgr.cameras()[0];
  if (cam->acquire()) {
    std::cerr << "[CAM] Failed to acquire" << std::endl;
    return nullptr;
  }

  auto config = cam->generateConfiguration({StreamRole::VideoRecording});
  if (!config)
    return nullptr;

  StreamConfiguration &scfg = config->at(0);
  scfg.pixelFormat = formats::YUV420;
  scfg.size = {width, height};
  scfg.bufferCount = 6;

  switch (config->validate()) {
  case CameraConfiguration::Valid:
    break;
  case CameraConfiguration::Adjusted:
    std::cout << "[CAM] Config adjusted" << std::endl;
    break;
  case CameraConfiguration::Invalid:
    std::cerr << "[CAM] Invalid config" << std::endl;
    return nullptr;
  }
  if (cam->configure(config.get())) {
    std::cerr << "[CAM] Configure failed" << std::endl;
    return nullptr;
  }

  stride_out = scfg.stride;

  alloc_out = new FrameBufferAllocator(cam);
  Stream *stream = scfg.stream();
  if (alloc_out->allocate(stream) < 0) {
    std::cerr << "[CAM] Alloc failed" << std::endl;
    return nullptr;
  }

  for (const auto &fb : alloc_out->buffers(stream)) {
    auto req = cam->createRequest();
    if (!req || req->addBuffer(stream, fb.get()))
      return nullptr;
    requests_out.push_back(std::move(req));
  }
  return cam;
}

// ─── main ────────────────────────────────────────────────────────────────────

int main(int argc, char *argv[]) {
  std::string host = "127.0.0.1";
  uint16_t port = 9600;
  uint32_t width = 720;
  uint32_t height = 480;
  uint32_t fps = 60;
  uint32_t bitrate = 1'000'000;
  uint32_t gop = 0; // 0 here means "not given"; derived from fps below
  bool gop_given = false;

  for (int i = 1; i < argc; ++i) {
    std::string a = argv[i];
    if (a == "--host" && i + 1 < argc)
      host = argv[++i];
    else if (a == "--port" && i + 1 < argc)
      port = (uint16_t)std::stoi(argv[++i]);
    else if (a == "--width" && i + 1 < argc)
      width = std::stoul(argv[++i]);
    else if (a == "--height" && i + 1 < argc)
      height = std::stoul(argv[++i]);
    else if (a == "--fps" && i + 1 < argc)
      fps = std::stoul(argv[++i]);
    else if (a == "--bitrate" && i + 1 < argc)
      bitrate = std::stoul(argv[++i]);
    else if (a == "--gop" && i + 1 < argc) {
      gop = std::stoul(argv[++i]);
      gop_given = true;
    } else if (a == "--help") {
      std::cout
          << "Usage: streamer [options]" << std::endl
          << "  --host     <ip>   Destination IP    (default: 127.0.0.1)"
          << std::endl
          << "  --port     <n>    Destination port  (default: 9600)"
          << std::endl
          << "  --width    <px>   Frame width        (default: 720)"
          << std::endl
          << "  --height   <px>   Frame height       (default: 480)"
          << std::endl
          << "  --fps      <n>    Framerate          (default: 60)" << std::endl
          << "  --bitrate  <bps>  Initial bitrate    (default: 1000000)"
          << std::endl
          << "  --gop      <n>    Frames between forced keyframes" << std::endl
          << "                    (default: fps*10, 0 = only on request)"
          << std::endl
          << "\nControl while running:" << std::endl
          << "  echo 500000 > /tmp/streamer_ctrl   set bitrate" << std::endl
          << "  echo idr    > /tmp/streamer_ctrl   force a keyframe"
          << std::endl
          << "  echo fec:10 > /tmp/streamer_ctrl   set FEC overhead to 10%"
          << std::endl;
      return 0;
    }
  }

  setvbuf(stdout, NULL, _IOLBF, 0);

  // Long GOP by default: keyframes are requested on demand, and this is only
  // the fallback if a request never arrives.
  if (!gop_given)
    gop = fps * 10;

  g_bitrate.store(bitrate);

  std::cout << "[STREAMER] " << width << "x" << height << "@" << fps << "fps → "
            << host << ":" << port << "  bitrate=" << bitrate / 1000
            << "kbps  gop=" << gop << "" << std::endl;

  std::signal(SIGINT, on_signal);
  std::signal(SIGTERM, on_signal);

  if (!build_pipeline(host, port))
    return 1;

  CameraManager cam_mgr;
  if (cam_mgr.start()) {
    std::cerr << "[CAM] CameraManager start failed" << std::endl;
    return 1;
  }

  FrameBufferAllocator *alloc = nullptr;
  uint32_t stride = 0;
  std::vector<std::unique_ptr<Request>> requests;
  auto cam = setup_camera(cam_mgr, width, height, fps, requests, alloc, stride);
  if (!cam)
    return 1;

  s_cam = cam.get();
  cam->requestCompleted.connect(&on_request_completed);

  gst_element_set_state(g_pipeline, GST_STATE_PLAYING);

  ControlList cam_controls(cam->controls());
  // Pin the frame duration to the requested fps. Without this the AEC/AGC
  // algorithm is free to stretch exposure time (and so frame duration) as
  // long as it wants for the scene's lighting, silently capping the actual
  // capture rate far below what was requested -- gain compensates instead.
  int64_t frame_duration = 1'000'000 / fps;
  cam_controls.set(controls::FrameDurationLimits,
                   Span<const int64_t, 2>({frame_duration, frame_duration}));
  if (cam->start(&cam_controls)) {
    std::cerr << "[CAM] Start failed" << std::endl;
    return 1;
  }

  for (auto &req : requests) {
    cam->queueRequest(req.get());
  }

  std::thread enc_thread(encoder_thread, width, height, stride, fps, bitrate,
                         gop);
  std::thread stdin_thread(stdin_reader_thread);
  std::thread fifo_thread(fifo_reader_thread);

  GMainLoop *loop = g_main_loop_new(nullptr, FALSE);
  std::thread glib_thread([&loop] {
    constexpr double interval_s = 2.0;
    uint64_t prev_captured = g_frame_seq.load(std::memory_order_relaxed);
    uint64_t prev_encoded = g_encoded_count.load(std::memory_order_relaxed);
    uint64_t prev_queue_drops = g_queue_drops.load(std::memory_order_relaxed);
    uint64_t prev_encoder_drops =
        g_encoder_drops.load(std::memory_order_relaxed);

    while (g_running) {
      std::this_thread::sleep_for(std::chrono::duration<double>(interval_s));

      uint64_t captured = g_frame_seq.load(std::memory_order_relaxed);
      uint64_t encoded = g_encoded_count.load(std::memory_order_relaxed);
      uint64_t queue_drops = g_queue_drops.load(std::memory_order_relaxed);
      uint64_t encoder_drops = g_encoder_drops.load(std::memory_order_relaxed);

      double captured_fps = (captured - prev_captured) / interval_s;
      double encoded_fps = (encoded - prev_encoded) / interval_s;
      uint64_t new_queue_drops = queue_drops - prev_queue_drops;
      uint64_t new_encoder_drops = encoder_drops - prev_encoder_drops;

      prev_captured = captured;
      prev_encoded = encoded;
      prev_queue_drops = queue_drops;
      prev_encoder_drops = encoder_drops;

      std::lock_guard<std::mutex> lock(g_latency_mutex);
      std::cout << "[LATENCY]"
                << "  capture=" << g_capture_latency_ms << " ms"
                << "  encode=" << g_encode_latency_ms << " ms"
                << "  total=" << g_total_latency_ms << " ms"
                << "  bitrate=" << g_bitrate.load() / 1000 << " kbps"
                << "  | captured=" << captured_fps << " fps"
                << "  encoded=" << encoded_fps << " fps"
                << "  queue_drops=" << new_queue_drops
                << "  encoder_drops=" << new_encoder_drops << std::endl;
    }
    g_main_loop_quit(loop);
  });
  g_main_loop_run(loop);
  g_main_loop_unref(loop);

  g_running = false;
  g_queue_cv.notify_all();

  // fifo_reader_thread is blocked in a read-only, blocking open() of the
  // control FIFO waiting for a writer; it can't observe g_running until
  // that call returns. Rendezvous with it here so it does -- otherwise it
  // stays joinable forever and fifo_thread's destructor below calls
  // std::terminate() on process exit (this was the unconditional
  // "terminate called without an active exception" on every shutdown).
  int wake_fd = open(CTRL_FIFO, O_WRONLY);
  if (wake_fd >= 0)
    close(wake_fd);

  cam->stop();
  cam->release();
  cam_mgr.stop();
  if (alloc)
    delete alloc;

  enc_thread.join();
  stdin_thread.detach();
  fifo_thread.join();
  glib_thread.join();

  gst_element_set_state(g_pipeline, GST_STATE_NULL);
  gst_object_unref(g_pipeline);

  std::cout << "[STREAMER] Done" << std::endl;
  return 0;
}
