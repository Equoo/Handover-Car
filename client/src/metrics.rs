//! Registry of every stat the overlay can show, each bound to a single key
//! that toggles it on/off. Adding a metric is a one-line addition to
//! `METRICS`; the overlay and the help panel both derive from this list, so
//! they can't drift out of sync with each other.

use std::collections::HashSet;

use parking_lot::Mutex;

use crate::stats::Stats;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MetricId {
    Fps,
    DecodeLatency,
    CaptureLatency,
    EncodeLatency,
    GlassToGlass,
    MeasuredGlassToGlass,
    ClientPing,
    SlavePing,
    RoundTrip,
    TargetBitrate,
    EncoderBitrate,
    ReceivedBitrate,
    RtpLoss,
    Decoder,
    Uptime,
}

pub struct MetricDef {
    pub id: MetricId,
    pub key: char,
    pub label: &'static str,
    pub format: fn(&Stats) -> String,
}

fn fmt_ms(ms: f64) -> String {
    format!("{ms:.1} ms")
}

pub const METRICS: &[MetricDef] = &[
    MetricDef {
        id: MetricId::Fps,
        key: 'f',
        label: "FPS (decoded)",
        format: |s| format!("FPS: {:.1}", s.fps()),
    },
    MetricDef {
        id: MetricId::DecodeLatency,
        key: 'd',
        label: "Decode latency (client)",
        format: |s| format!("Decode: {}", fmt_ms(*s.decode_latency.lock())),
    },
    MetricDef {
        id: MetricId::CaptureLatency,
        key: 'c',
        label: "Capture latency (slave)",
        format: |s| format!("Capture: {}", fmt_ms(*s.capture_latency.lock())),
    },
    MetricDef {
        id: MetricId::EncodeLatency,
        key: 'e',
        label: "Encode latency (slave)",
        format: |s| format!("Encode: {}", fmt_ms(*s.encode_latency.lock())),
    },
    MetricDef {
        id: MetricId::GlassToGlass,
        key: 'g',
        label: "Glass-to-glass estimate (rough)",
        format: |s| {
            let network = *s.client_ping.lock() as f64 + *s.slave_ping.lock() as f64;
            let total = *s.encoder_latency.lock() + network + *s.decode_latency.lock();
            format!("Glass-to-glass ~{}", fmt_ms(total))
        },
    },
    MetricDef {
        id: MetricId::MeasuredGlassToGlass,
        key: 'm',
        label: "Glass-to-glass measured (camera-on-screen test, [T])",
        format: |s| match *s.measured_glass_to_glass_ms.lock() {
            Some(ms) => format!("Glass-to-glass (measured): {}", fmt_ms(ms)),
            None => "Glass-to-glass (measured): no signal yet".to_string(),
        },
    },
    MetricDef {
        id: MetricId::ClientPing,
        key: 'p',
        label: "Client <-> server ping",
        format: |s| format!("Client ping: {}ms", *s.client_ping.lock()),
    },
    MetricDef {
        id: MetricId::SlavePing,
        key: 's',
        label: "Server <-> slave ping",
        format: |s| format!("Slave ping: {}ms", *s.slave_ping.lock()),
    },
    MetricDef {
        id: MetricId::RoundTrip,
        key: 'r',
        label: "Total link ping (client+slave)",
        format: |s| {
            let total = *s.client_ping.lock() + *s.slave_ping.lock();
            format!("Total ping: {total}ms")
        },
    },
    MetricDef {
        id: MetricId::TargetBitrate,
        key: 'b',
        label: "Target bitrate (ABR)",
        format: |s| format!("Target bitrate: {}kbps", *s.target_bitrate.lock() / 1000),
    },
    MetricDef {
        id: MetricId::EncoderBitrate,
        key: 'n',
        label: "Encoder bitrate (actual, slave)",
        format: |s| format!("Encoder bitrate: {}kbps", *s.encoder_bitrate.lock() / 1000),
    },
    MetricDef {
        id: MetricId::ReceivedBitrate,
        key: 'v',
        label: "Received video bitrate (client)",
        format: |s| format!("Received: {:.0}kbps", s.received_bitrate() / 1000.0),
    },
    MetricDef {
        id: MetricId::RtpLoss,
        key: 'l',
        label: "RTP lost/late/duplicates/jitter",
        format: |s| {
            let loss = *s.loss.lock();
            format!(
                "RTP lost: {}/{} ({:.1}%)\nRTP late: {}\nRTP duplicates: {}\nAvg jitter: {}",
                loss.num_lost,
                loss.num_lost + loss.num_pushed,
                loss.loss_pct,
                loss.num_late,
                loss.num_duplicates,
                loss.avg_jitter
            )
        },
    },
    MetricDef {
        id: MetricId::Decoder,
        key: 'x',
        label: "Active H.264 decoder",
        format: |s| format!("Decoder: {}", *s.decoder_name.lock()),
    },
    MetricDef {
        id: MetricId::Uptime,
        key: 'u',
        label: "Session uptime",
        format: |s| {
            let secs = s.uptime().as_secs();
            format!("Uptime: {:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
        },
    },
];

pub const HELP_KEY: char = 'h';
pub const SHOW_ALL_KEY: char = 'a';
pub const HIDE_ALL_KEY: char = '0';
/// Toggles the camera-on-screen latency test (see `latency_test.rs`).
/// Handled directly in `overlay.rs`, not by `Visibility`, since it has a
/// real side effect (flashing the screen) rather than showing/hiding a label.
pub const LATENCY_TEST_KEY: char = 't';

const DEFAULT_ENABLED: &[MetricId] = &[
    MetricId::Fps,
    MetricId::DecodeLatency,
    MetricId::ClientPing,
    MetricId::SlavePing,
    MetricId::TargetBitrate,
    MetricId::RtpLoss,
];

/// Which metrics are currently shown in the overlay. Purely a UI-side
/// preference: it doesn't touch `AppState`/networking at all.
pub struct Visibility {
    enabled: Mutex<HashSet<MetricId>>,
    help_visible: Mutex<bool>,
}

impl Visibility {
    pub fn new() -> Self {
        Self {
            enabled: Mutex::new(DEFAULT_ENABLED.iter().copied().collect()),
            help_visible: Mutex::new(false),
        }
    }

    pub fn is_enabled(&self, id: MetricId) -> bool {
        self.enabled.lock().contains(&id)
    }

    pub fn help_visible(&self) -> bool {
        *self.help_visible.lock()
    }

    fn toggle(&self, id: MetricId) {
        let mut enabled = self.enabled.lock();
        if !enabled.remove(&id) {
            enabled.insert(id);
        }
    }

    fn show_all(&self) {
        *self.enabled.lock() = METRICS.iter().map(|m| m.id).collect();
    }

    fn hide_all(&self) {
        self.enabled.lock().clear();
    }

    /// Applies a key press. Returns true if the key was recognized (caller
    /// can use that to decide whether to refresh the overlay immediately).
    pub fn handle_key(&self, key: char) -> bool {
        let key = key.to_ascii_lowercase();
        if key == HELP_KEY {
            let mut visible = self.help_visible.lock();
            *visible = !*visible;
            return true;
        }
        if key == SHOW_ALL_KEY {
            self.show_all();
            return true;
        }
        if key == HIDE_ALL_KEY {
            self.hide_all();
            return true;
        }
        if let Some(metric) = METRICS.iter().find(|m| m.key == key) {
            self.toggle(metric.id);
            return true;
        }
        false
    }
}
