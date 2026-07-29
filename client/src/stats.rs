//! All the numbers the UI can show, gathered in one place so `app.rs` and
//! `video.rs` stay focused on networking/pipeline logic instead of also
//! being the place where "what does the overlay display" lives.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;

/// Exponential moving average smoothing factor shared by every metric that
/// jitters too much frame-to-frame to display raw (pings, decode time...).
const EMA_ALPHA: f64 = 0.2;

pub fn ema_average(average: f64, value: f64) -> f64 {
    EMA_ALPHA * value + (1.0 - EMA_ALPHA) * average
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PacketLoss {
    /// Sequence numbers that never arrived and were given up on.
    pub num_lost: u64,
    /// Packets that arrived but past the jitterbuffer's playout deadline.
    pub num_late: u64,
    pub num_pushed: u64,
    pub num_duplicates: u64,
    pub avg_jitter: u64,
    pub loss_pct: f64,
}

/// A counter paired with a window start, used to turn "N events happened"
/// into a per-second rate without allocating a new task per metric.
struct RateCounter {
    count: AtomicU64,
    window_start: Mutex<Instant>,
    rate: Mutex<f64>,
}

impl RateCounter {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            window_start: Mutex::new(Instant::now()),
            rate: Mutex::new(0.0),
        }
    }

    fn add(&self, n: u64) {
        self.count.fetch_add(n, Ordering::Relaxed);
    }

    /// Recomputes the rate once a second has elapsed since the last window;
    /// cheap to call as often as you like in between.
    fn tick(&self) {
        let mut start = self.window_start.lock();
        let elapsed = start.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            let count = self.count.swap(0, Ordering::Relaxed);
            *self.rate.lock() = count as f64 / elapsed;
            *start = Instant::now();
        }
    }

    fn rate(&self) -> f64 {
        *self.rate.lock()
    }
}

pub struct Stats {
    pub decode_latency: Mutex<f64>,
    pub capture_latency: Mutex<f64>,
    pub encode_latency: Mutex<f64>,
    /// Slave-side capture+encode total, as measured by the streamer itself.
    pub encoder_latency: Mutex<f64>,
    pub slave_ping: Mutex<u16>,
    pub client_ping: Mutex<u16>,
    /// Bitrate the client's ABR loop is currently targeting.
    pub target_bitrate: Mutex<u32>,
    /// Bitrate the slave's encoder is actually applying (may lag the target
    /// slightly while the request is in flight).
    pub encoder_bitrate: Mutex<u32>,
    pub loss: Mutex<PacketLoss>,
    pub decoder_name: Mutex<&'static str>,
    pub connected_at: Mutex<Option<Instant>>,
    frames: RateCounter,
    bytes: RateCounter,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            decode_latency: Mutex::new(0.0),
            capture_latency: Mutex::new(0.0),
            encode_latency: Mutex::new(0.0),
            encoder_latency: Mutex::new(0.0),
            slave_ping: Mutex::new(0),
            client_ping: Mutex::new(0),
            target_bitrate: Mutex::new(500_000),
            encoder_bitrate: Mutex::new(0),
            loss: Mutex::new(PacketLoss::default()),
            decoder_name: Mutex::new("--"),
            connected_at: Mutex::new(None),
            frames: RateCounter::new(),
            bytes: RateCounter::new(),
        }
    }

    pub fn note_decode_latency(&self, ms: f64) {
        let mut latency = self.decode_latency.lock();
        *latency = ema_average(*latency, ms);
    }

    pub fn note_slave_stats(&self, capture_ms: f32, encode_ms: f32, total_ms: f32, bitrate_bps: u32) {
        *self.capture_latency.lock() = capture_ms as f64;
        *self.encode_latency.lock() = encode_ms as f64;
        *self.encoder_latency.lock() = total_ms as f64;
        *self.encoder_bitrate.lock() = bitrate_bps;
    }

    pub fn note_slave_ping(&self, ms: u16) {
        let mut ping = self.slave_ping.lock();
        *ping = ema_average(*ping as f64, ms as f64) as u16;
    }

    pub fn note_client_ping(&self, ms: f64) {
        let mut ping = self.client_ping.lock();
        *ping = ema_average(*ping as f64, ms) as u16;
    }

    pub fn note_frame(&self) {
        self.frames.add(1);
    }

    pub fn note_bytes(&self, n: usize) {
        self.bytes.add(n as u64);
    }

    /// Call regularly (e.g. from an existing periodic loop): cheap no-op
    /// until a full second has elapsed, at which point fps/received bitrate
    /// get refreshed.
    pub fn tick_rates(&self) {
        self.frames.tick();
        self.bytes.tick();
    }

    pub fn fps(&self) -> f64 {
        self.frames.rate()
    }

    /// Received video bitrate, in bits per second.
    pub fn received_bitrate(&self) -> f64 {
        self.bytes.rate() * 8.0
    }

    pub fn mark_connected(&self) {
        *self.connected_at.lock() = Some(Instant::now());
    }

    pub fn uptime(&self) -> Duration {
        self.connected_at
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }
}
