// SPDX-FileCopyrightText: 2026 David Derny
// SPDX-FileCopyrightText: 2026 Handover contributors (see AUTHORS)
//
// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0

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
    /// Cumulative loss ratio since the pipeline started (jitterbuffer stats,
    /// never reset). Informational only -- after a long session this barely
    /// moves for a brief bad patch, since the denominator keeps growing. Do
    /// not drive control decisions (bitrate/FEC) off this; use
    /// `Stats::windowed_loss_pct()` instead.
    pub loss_pct: f64,
}

/// A counter for a boolean-ish event (lost vs received) that reports a
/// windowed ratio instead of a cumulative one: it decays every `window`
/// elapsed rather than averaging over the whole session, so a control loop
/// reading it reacts to *current* conditions regardless of how long the
/// stream has already been running.
struct WindowedRatio {
    hits: AtomicU64,
    total: AtomicU64,
    window_start: Mutex<Instant>,
    ratio_pct: Mutex<f64>,
}

impl WindowedRatio {
    fn new() -> Self {
        Self {
            hits: AtomicU64::new(0),
            total: AtomicU64::new(0),
            window_start: Mutex::new(Instant::now()),
            ratio_pct: Mutex::new(0.0),
        }
    }

    fn note(&self, hits: u64, total: u64) {
        self.hits.fetch_add(hits, Ordering::Relaxed);
        self.total.fetch_add(total, Ordering::Relaxed);
    }

    /// Recomputes the ratio once `window` has elapsed since the last one;
    /// cheap to call more often than that. A window with no samples at all
    /// leaves the previous value in place (nothing to conclude either way)
    /// rather than snapping to 0.
    fn tick(&self, window: Duration) {
        let mut start = self.window_start.lock();
        if start.elapsed() < window {
            return;
        }
        let hits = self.hits.swap(0, Ordering::Relaxed);
        let total = self.total.swap(0, Ordering::Relaxed);
        if total > 0 {
            *self.ratio_pct.lock() = hits as f64 / total as f64 * 100.0;
        }
        *start = Instant::now();
    }

    fn ratio_pct(&self) -> f64 {
        *self.ratio_pct.lock()
    }
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
    /// Measured (not estimated) glass-to-glass round-trip from the
    /// camera-on-screen latency test in `latency_test.rs`. `None` until a
    /// sample has actually been observed (test not running, or camera not
    /// pointed at the screen yet).
    pub measured_glass_to_glass_ms: Mutex<Option<f64>>,
    /// Bitrate the adaptive loop would target from link conditions alone,
    /// before subtracting FEC overhead -- see `AppState::run_adaptive`. Shown
    /// separately from `target_bitrate` (which is the post-FEC-overhead value
    /// actually sent to the encoder) so the overlay can tell "the link can
    /// carry this much" apart from "this much of it is video".
    pub link_estimate_bitrate: Mutex<u32>,
    /// FEC overhead percentage the adaptive loop is currently commanding.
    pub target_fec_pct: Mutex<u8>,
    /// Which path the data plane is currently using and how fast it is, as
    /// reported by `common::transport` -- e.g. `direct/host 192.168.1.42:41234`.
    pub path: Mutex<String>,
    pub path_rtt_ms: Mutex<Option<f64>>,
    frames: RateCounter,
    bytes: RateCounter,
    /// RTP sequence-number gaps observed on the wire, tracked in
    /// `video.rs`'s recv loop independent of `rtpjitterbuffer`'s own stats
    /// (see `note_seq`) -- this is what feeds `windowed_loss_pct`.
    loss_window: WindowedRatio,
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
            measured_glass_to_glass_ms: Mutex::new(None),
            link_estimate_bitrate: Mutex::new(500_000),
            target_fec_pct: Mutex::new(0),
            path: Mutex::new("connecting".to_string()),
            path_rtt_ms: Mutex::new(None),
            frames: RateCounter::new(),
            bytes: RateCounter::new(),
            loss_window: WindowedRatio::new(),
        }
    }

    /// Records one packet's worth of raw-wire sequence-number tracking:
    /// `lost` gaps out of `total` sequence numbers seen (both `0` for a
    /// duplicate/reordered packet, which is neither a fresh arrival nor a
    /// loss). Feeds `windowed_loss_pct`, not `PacketLoss` -- see `note()` in
    /// `video.rs`'s `run()`.
    pub fn note_seq(&self, lost: u64, total: u64) {
        self.loss_window.note(lost, total);
    }

    /// Recomputes `windowed_loss_pct` once `window` has elapsed since the
    /// last recompute; cheap to call more often than that.
    pub fn tick_loss_window(&self, window: Duration) {
        self.loss_window.tick(window);
    }

    /// Real-time loss ratio over a short, decaying window -- unlike
    /// `PacketLoss::loss_pct`, this reflects current link conditions
    /// regardless of how long the session has been running. This is what the
    /// adaptive bitrate/FEC loop should read.
    pub fn windowed_loss_pct(&self) -> f64 {
        self.loss_window.ratio_pct()
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

    /// Records one round-trip sample from the camera-on-screen latency test.
    pub fn note_measured_glass_to_glass(&self, ms: f64) {
        let mut measured = self.measured_glass_to_glass_ms.lock();
        *measured = Some(match *measured {
            Some(prev) => ema_average(prev, ms),
            None => ms,
        });
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
