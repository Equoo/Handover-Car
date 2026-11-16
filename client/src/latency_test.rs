// SPDX-FileCopyrightText: 2026 David Derny
// SPDX-FileCopyrightText: 2026 Handover contributors (see AUTHORS)
//
// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0

//! A best-effort "glass-to-glass" latency measurement: flash the screen
//! through a fixed color palette and detect those same colors coming back
//! through the fully decoded video once the car's camera is pointed at this
//! screen. Not a calibrated instrument -- no perspective correction, no
//! ambient-light compensation -- but it measures the real end-to-end loop
//! rather than summing separately-measured stages the way the existing
//! `MetricId::GlassToGlass` estimate does.

use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tokio::time::Instant;

/// How often the flash advances to the next palette color.
pub const FLASH_INTERVAL_MS: u64 = 400;

/// Saturated, well-separated colors -- avoids black/white, which are easily
/// confused with a dark/no-signal frame (camera not pointed at the screen,
/// or a frame caught mid-transition).
const PALETTE: [[u8; 3]; 6] = [
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [255, 255, 0],
    [255, 0, 255],
    [0, 255, 255],
];

/// Reject a detected average color as ambiguous (camera not aimed at the
/// screen, a mid-transition frame, compression noise on a non-solid capture)
/// unless it's at least this close to a palette entry. The max possible
/// distance between two RGB corners is ~442; this is comfortably tighter
/// than that while still tolerant of encoder/camera noise on what should be
/// a solid-color frame.
const MATCH_THRESHOLD: f64 = 120.0;

pub struct LatencyTest {
    enabled: AtomicBool,
    current: Mutex<usize>,
    last_shown: Mutex<[Option<Instant>; PALETTE.len()]>,
    last_detected: Mutex<Option<usize>>,
}

impl LatencyTest {
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            current: Mutex::new(0),
            last_shown: Mutex::new([None; PALETTE.len()]),
            last_detected: Mutex::new(None),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Flips test mode on/off, returning the new state. Clears the last
    /// detected color either way, so a stale detection from a previous run
    /// can't suppress the first real one after restarting.
    pub fn toggle(&self) -> bool {
        let now_enabled = !self.enabled.fetch_xor(true, Ordering::Relaxed);
        *self.last_detected.lock() = None;
        now_enabled
    }

    /// Called on the GTK timer every `FLASH_INTERVAL_MS`: advances to the
    /// next palette color and stamps when it started. Returns the color to
    /// paint regardless of `enabled` -- the caller decides separately
    /// whether the flash layer should be visible.
    pub fn tick(&self) -> [u8; 3] {
        let mut current = self.current.lock();
        *current = (*current + 1) % PALETTE.len();
        self.last_shown.lock()[*current] = Some(Instant::now());
        PALETTE[*current]
    }

    /// The color that should currently be on screen. Unlike `tick`, this
    /// doesn't advance anything -- for redraws that happen independently of
    /// the flash timer (e.g. a window resize).
    pub fn current_color(&self) -> [u8; 3] {
        PALETTE[*self.current.lock()]
    }

    /// Called from the video pipeline with a decoded frame's average color.
    /// Returns a new round-trip sample, in raw milliseconds, when this
    /// frame's color is a *new* nearest-palette match (different from the
    /// one last detected) -- the elapsed time since that color was last
    /// shown. Returns `None` when disabled, when the color is too ambiguous
    /// to trust, or when it's unchanged from the last detection (not a new
    /// transition). Smoothing is the caller's job (see `Stats::
    /// note_measured_glass_to_glass`), same as every other latency figure.
    pub fn observe_frame_color(&self, rgb: [u8; 3]) -> Option<f64> {
        if !self.is_enabled() {
            return None;
        }

        let (idx, dist) = PALETTE
            .iter()
            .enumerate()
            .map(|(i, c)| (i, color_distance(rgb, *c)))
            .min_by(|a, b| a.1.total_cmp(&b.1))?;

        if dist > MATCH_THRESHOLD {
            return None;
        }

        let mut last_detected = self.last_detected.lock();
        if *last_detected == Some(idx) {
            // Still the same color as last time -- not a new transition.
            return None;
        }
        *last_detected = Some(idx);
        drop(last_detected);

        let shown_at = self.last_shown.lock()[idx]?;
        Some(shown_at.elapsed().as_secs_f64() * 1000.0)
    }
}

impl Default for LatencyTest {
    fn default() -> Self {
        Self::new()
    }
}

fn color_distance(a: [u8; 3], b: [u8; 3]) -> f64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as f64 - *y as f64).powi(2))
        .sum::<f64>()
        .sqrt()
}
