/**
 * streamer.cpp
 *
 * Raspberry Pi H264 RTP streamer with dynamic bitrate control.
 * Based on rpicam-apps H264Encoder architecture (direct V4L2 /dev/video11).
 *
 * Pipeline:
 *   libcamera (YUV420) → V4L2Encoder (/dev/video11) → appsrc
 *   → h264parse → rtph264pay → udpsink
 *
 * Bitrate control: write integer bps to stdin e.g. `echo 500000`
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
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
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
static std::unordered_map<uint64_t, std::chrono::steady_clock::time_point>
	g_capture_entry;
static std::unordered_map<uint64_t, std::chrono::steady_clock::time_point>
	g_encode_entry;
static double g_capture_latency_ms = 0.0;
static double g_encode_latency_ms = 0.0;
static double g_total_latency_ms = 0.0;
static uint64_t g_frame_seq = 0;

// ─── Global state ────────────────────────────────────────────────────────────

static std::atomic<bool> g_running{true};
static std::atomic<uint32_t> g_bitrate{1000000};
static std::atomic<bool> g_bitrate_changed{false};
// Set by the control readers, consumed by the encoder thread. Applying it
// there keeps every ioctl on fd_ on a single thread.
static std::atomic<bool> g_force_keyframe{false};

static GstElement *g_pipeline = nullptr;
static GstElement *g_appsrc = nullptr;
static Camera *s_cam = nullptr;

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

	V4L2H264Encoder(uint32_t width, uint32_t height, uint32_t fps, uint32_t bitrate,
					uint32_t stride, uint32_t gop, OutputCallback cb)
		: width_(width),
		  height_(height),
		  fps_(fps),
		  bitrate_(bitrate),
		  stride_(stride),
		  gop_(gop),
		  output_cb_(cb) {
		fd_ = open("/dev/video11", O_RDWR | O_NONBLOCK);
		if (fd_ < 0) throw std::runtime_error("Cannot open /dev/video11");

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
		if (!running_) return;
		running_ = false;

		// Send EOS
		v4l2_encoder_cmd cmd{};
		cmd.cmd = V4L2_ENC_CMD_STOP;
		ioctl(fd_, VIDIOC_ENCODER_CMD, &cmd);

		if (poll_thread_.joinable()) poll_thread_.join();
		if (output_thread_.joinable()) output_thread_.join();

		int type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
		ioctl(fd_, VIDIOC_STREAMOFF, &type);
		type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
		ioctl(fd_, VIDIOC_STREAMOFF, &type);
	}

	// Encode one YUV420 frame. Returns false if no input buffer available.
	bool encode(const uint8_t *yuv, size_t size, int64_t timestamp_us,
				uint64_t seq) {
		// Apply pending bitrate change
		if (g_bitrate_changed.exchange(false)) set_bitrate(g_bitrate.load());

		// Apply a pending keyframe request. With a long GOP this is the only
		// thing that produces an IDR, so a receiver that has lost sync stays
		// broken until someone asks.
		if (g_force_keyframe.exchange(false)) force_keyframe();

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
		fmt.fmt.pix_mp.colorspace = V4L2_COLORSPACE_JPEG;  // = BT.601 full range
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
		xioctl(fd_, VIDIOC_S_PARM, &parm);	// best effort
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

		// Baseline profile — no B-frames, simplest NAL structure
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
		ctrl.value = 16;  // don't waste bits on near-perfect quality when static
		xioctl(fd_, VIDIOC_S_CTRL, &ctrl);

		ctrl.id = V4L2_CID_MPEG_VIDEO_H264_MAX_QP;
		ctrl.value = 36;  // cap how blocky it's allowed to get under motion
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
		while (running_) {
			pollfd pfd{fd_, POLLPRI, 0};
			int ret = poll(&pfd, 1, 200);
			if (ret < 0 && errno != EINTR) break;
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
		while (running_) {
			pollfd pfd{fd_, POLLIN, 0};
			int ret = poll(&pfd, 1, 200);
			if (ret < 0 && errno != EINTR) break;
			if (!(pfd.revents & POLLIN)) continue;

			v4l2_buffer buf{};
			v4l2_plane planes[1]{};
			buf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
			buf.memory = V4L2_MEMORY_MMAP;
			buf.length = 1;
			buf.m.planes = planes;

			if (xioctl(fd_, VIDIOC_DQBUF, &buf) < 0) {
				if (errno == EAGAIN) continue;
				break;
			}

			size_t idx = buf.index;
			size_t used = buf.m.planes[0].bytesused;
			bool keyframe = (buf.flags & V4L2_BUF_FLAG_KEYFRAME) != 0;
			int64_t ts_us =
				(int64_t)buf.timestamp.tv_sec * 1'000'000 + buf.timestamp.tv_usec;
			uint64_t seq = buf.sequence;

			if (used > 0)
				output_cb_((uint8_t *)output_buffers_[idx].mem, used, ts_us,
						   keyframe, seq);

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
	std::chrono::steady_clock::time_point capture_start;
	uint64_t seq;
};

static std::queue<Frame> g_frame_queue;
static std::mutex g_queue_mutex;
static std::condition_variable g_queue_cv;
static constexpr size_t MAX_QUEUE = 6;

// ─── Signal handler ──────────────────────────────────────────────────────────

static void on_signal(int) {
	g_running = false;
	g_queue_cv.notify_all();
	if (g_pipeline) gst_element_send_event(g_pipeline, gst_event_new_eos());
}

// ─── stdin bitrate reader
// ─────────────────────────────────────────────────────

static constexpr const char *CTRL_FIFO = "/tmp/streamer_ctrl";

// One control line: "idr" requests a keyframe, an integer sets the bitrate.
// Both only raise a flag; the encoder thread applies it, so every ioctl on
// the encoder fd stays on one thread.
static void handle_command(std::string s) {
	s.erase(0, s.find_first_not_of(" \t\r" << std::endl));
	s.erase(s.find_last_not_of(" \t\r" << std::endl) + 1);
	if (s.empty()) return;

	if (s == "idr" || s == "key") {
		g_force_keyframe.store(true);
		std::cout << "[CTRL] Keyframe requested" << std::endl;
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
	while (g_running && std::getline(std::cin, line)) handle_command(line);
	std::cerr << "[CTRL] stdin closed" << std::endl;
}

static void fifo_reader_thread() {
	unlink(CTRL_FIFO);
	if (mkfifo(CTRL_FIFO, 0666) < 0) {
		std::cerr << "[CTRL] mkfifo failed: " << strerror(errno) << "" << std::endl;
		return;
	}
	std::cout << "[CTRL] Ready: echo <bps>|idr > " << CTRL_FIFO << "" << std::endl;
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
		if (n <= 0) continue;

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
			if (dbg) std::cout << "[GST DEBUG] " << dbg << "" << std::endl;
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
	auto *sink = gst_element_factory_make("udpsink", "sink");

	if (!g_pipeline || !g_appsrc || !parse || !pay || !sink) {
		std::cerr << "[GST] Failed to create elements" << std::endl;
		return false;
	}

	// appsrc now receives H264 bitstream directly from our V4L2 encoder
	GstCaps *caps = gst_caps_new_simple("video/x-h264", "stream-format",
										G_TYPE_STRING, "byte-stream", "alignment",
										G_TYPE_STRING, "au", nullptr);
	g_object_set(G_OBJECT(g_appsrc), "caps", caps, "is-live", TRUE, "format",
				 GST_FORMAT_TIME, "do-timestamp", TRUE, nullptr);
	gst_caps_unref(caps);

	g_object_set(G_OBJECT(parse), "config-interval", -1, nullptr);

	g_object_set(G_OBJECT(pay), "config-interval", -1, "pt", 96, "mtu", 1200u,
				 nullptr);

	g_object_set(G_OBJECT(sink), "host", host.c_str(), "port", (gint)port, "sync",
				 FALSE, "async", FALSE, nullptr);

	gst_bin_add_many(GST_BIN(g_pipeline), g_appsrc, parse, pay, sink, nullptr);

	if (!gst_element_link_many(g_appsrc, parse, pay, sink, nullptr)) {
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
	if (request->status() == Request::RequestCancelled) return;

	auto capture_end = std::chrono::steady_clock::now();
	std::chrono::steady_clock::time_point capture_start;
	{
		std::lock_guard<std::mutex> lock(g_latency_mutex);
		auto it = g_capture_entry.find(request->cookie());
		if (it != g_capture_entry.end()) {
			capture_start = it->second;
			g_capture_latency_ms = std::chrono::duration<double, std::milli>(
									   capture_end - capture_start)
									   .count();
			g_capture_entry.erase(it);
		} else {
			capture_start = capture_end;
		}
	}

	for (auto &[stream, fb] : request->buffers()) {
		size_t total = 0;
		for (auto &p : fb->planes()) total += p.length;

		Frame frame;
		frame.data.resize(total);
		frame.width = stream->configuration().size.width;
		frame.height = stream->configuration().size.height;
		frame.capture_start = capture_start;
		frame.seq = g_frame_seq++;

		auto &metadata = request->metadata();
		auto ts = metadata.get(controls::SensorTimestamp);
		frame.timestamp_us = ts ? (*ts / 1000) : 0;

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
			if (g_frame_queue.size() < MAX_QUEUE)
				g_frame_queue.push(std::move(frame));
		}
		g_queue_cv.notify_one();
	}

	if (g_running) {
		request->reuse(Request::ReuseBuffers);
		{
			std::lock_guard<std::mutex> lock(g_latency_mutex);
			g_capture_entry[request->cookie()] = std::chrono::steady_clock::now();
		}
		s_cam->queueRequest(request);
	}
}

// ─── Encoder + push thread
// ────────────────────────────────────────────────────

static void encoder_thread(uint32_t width, uint32_t height, uint32_t stride,
						   uint32_t fps, uint32_t initial_bitrate, uint32_t gop) {
	// Callback: push encoded H264 directly into GStreamer appsrc
	auto on_encoded = [&](uint8_t *data, size_t size, int64_t /*ts_us*/,
						  bool keyframe, uint64_t seq) {
		// Proves whether the encoder actually produced an IDR, separately from
		// whether the client ever received one.
		if (keyframe)
			std::cout << "[ENC] IDR emitted at frame " << seq << " (" << size
					  << " bytes)" << std::endl;

		// Latency: encode done
		{
			std::lock_guard<std::mutex> lock(g_latency_mutex);
			auto it = g_encode_entry.find(seq);
			if (it != g_encode_entry.end()) {
				g_encode_latency_ms =
					std::chrono::duration<double, std::milli>(
						std::chrono::steady_clock::now() - it->second)
						.count();

				// Total: from capture submit to encoded output
				// capture_start is stored in g_encode_entry mapped by seq
				// (we'd need to thread it through — encode_entry reused here
				//  stores encode-start, so total = capture_latency + encode_latency)
				g_total_latency_ms = g_capture_latency_ms + g_encode_latency_ms;
				g_encode_entry.erase(it);
			}
		}

		GstBuffer *buf = gst_buffer_new_allocate(nullptr, size, nullptr);
		GstMapInfo map;
		gst_buffer_map(buf, &map, GST_MAP_WRITE);
		memcpy(map.data, data, size);
		gst_buffer_unmap(buf, &map);

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
				if (!g_running && g_frame_queue.empty()) break;
				frame = std::move(g_frame_queue.front());
				g_frame_queue.pop();
			}

			// Stamp encode start
			{
				std::lock_guard<std::mutex> lock(g_latency_mutex);
				g_encode_entry[frame.seq] = std::chrono::steady_clock::now();
			}

			enc.encode(frame.data.data(), frame.data.size(), frame.timestamp_us,
					   frame.seq);
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

static std::shared_ptr<Camera> setup_camera(
	CameraManager &mgr, uint32_t width, uint32_t height, uint32_t fps,
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
	if (!config) return nullptr;

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

	ControlList controls(cam->controls());
	int64_t frame_duration = 1'000'000 / fps;
	controls.set(controls::FrameDurationLimits,
				 Span<const int64_t, 2>({frame_duration, frame_duration}));

	for (const auto &fb : alloc_out->buffers(stream)) {
		auto req = cam->createRequest();
		if (!req || req->addBuffer(stream, fb.get())) return nullptr;
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
	uint32_t gop = 0;  // 0 here means "not given"; derived from fps below
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
			std::cout << "Usage: streamer [options]" << std::endl
					  << "  --host     <ip>   Destination IP    (default: 127.0.0.1)"
					  << std::endl
					  << "  --port     <n>    Destination port  (default: 9600)"
					  << std::endl
					  << "  --width    <px>   Frame width        (default: 720)"
					  << std::endl
					  << "  --height   <px>   Frame height       (default: 480)"
					  << std::endl
					  << "  --fps      <n>    Framerate          (default: 60)"
					  << std::endl
					  << "  --bitrate  <bps>  Initial bitrate    (default: 1000000)"
					  << std::endl
					  << "  --gop      <n>    Frames between forced keyframes"
					  << std::endl
					  << "                    (default: fps*10, 0 = only on request)"
					  << std::endl
					  << "\nControl while running:" << std::endl
					  << "  echo 500000 > /tmp/streamer_ctrl   set bitrate"
					  << std::endl
					  << "  echo idr    > /tmp/streamer_ctrl   force a keyframe"
					  << std::endl;
			return 0;
		}
	}

	setvbuf(stdout, NULL, _IOLBF, 0);

	// Long GOP by default: keyframes are requested on demand, and this is only
	// the fallback if a request never arrives.
	if (!gop_given) gop = fps * 10;

	g_bitrate.store(bitrate);

	std::cout << "[STREAMER] " << width << "x" << height << "@" << fps << "fps → "
			  << host << ":" << port << "  bitrate=" << bitrate / 1000
			  << "kbps  gop=" << gop << "" << std::endl;

	std::signal(SIGINT, on_signal);
	std::signal(SIGTERM, on_signal);

	if (!build_pipeline(host, port)) return 1;

	CameraManager cam_mgr;
	if (cam_mgr.start()) {
		std::cerr << "[CAM] CameraManager start failed" << std::endl;
		return 1;
	}

	FrameBufferAllocator *alloc = nullptr;
	uint32_t stride = 0;
	std::vector<std::unique_ptr<Request>> requests;
	auto cam = setup_camera(cam_mgr, width, height, fps, requests, alloc, stride);
	if (!cam) return 1;

	s_cam = cam.get();
	cam->requestCompleted.connect(&on_request_completed);

	gst_element_set_state(g_pipeline, GST_STATE_PLAYING);

	ControlList cam_controls(cam->controls());
	if (cam->start(&cam_controls)) {
		std::cerr << "[CAM] Start failed" << std::endl;
		return 1;
	}

	for (auto &req : requests) {
		{
			std::lock_guard<std::mutex> lock(g_latency_mutex);
			g_capture_entry[req->cookie()] = std::chrono::steady_clock::now();
		}
		cam->queueRequest(req.get());
	}

	std::thread enc_thread(encoder_thread, width, height, stride, fps, bitrate, gop);
	std::thread stdin_thread(stdin_reader_thread);
	std::thread fifo_thread(fifo_reader_thread);

	GMainLoop *loop = g_main_loop_new(nullptr, FALSE);
	std::thread glib_thread([&loop] {
		while (g_running) {
			std::this_thread::sleep_for(std::chrono::seconds(2));
			std::lock_guard<std::mutex> lock(g_latency_mutex);
			std::cout << "[LATENCY]"
					  << "  capture=" << g_capture_latency_ms << " ms"
					  << "  encode=" << g_encode_latency_ms << " ms"
					  << "  total=" << g_total_latency_ms << " ms"
					  << "  bitrate=" << g_bitrate.load() / 1000 << " kbps"
					  << std::endl;
		}
		g_main_loop_quit(loop);
	});
	g_main_loop_run(loop);
	g_main_loop_unref(loop);

	g_running = false;
	g_queue_cv.notify_all();

	cam->stop();
	cam->release();
	cam_mgr.stop();
	if (alloc) delete alloc;

	enc_thread.join();
	stdin_thread.detach();
	glib_thread.join();

	gst_element_set_state(g_pipeline, GST_STATE_NULL);
	gst_object_unref(g_pipeline);

	std::cout << "[STREAMER] Done" << std::endl;
	return 0;
}
