// SPDX-FileCopyrightText: 2026 David Derny
// SPDX-FileCopyrightText: 2026 Handover contributors (see AUTHORS)
//
// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0

//! The on-screen stats panel, help panel, and camera-on-screen latency-test
//! flash layer, all drawn as part of `ui.rs`'s `update()` loop rather than as
//! a separate widget tree (egui is immediate-mode: there's nothing to build
//! once and mutate later, just "draw the current state" every frame).
//!
//! Everything the stats/help panels show comes straight from the `METRICS`
//! registry in `metrics.rs`: one label per entry, shown/hidden per
//! `Visibility`, and the same list doubles as the content of the help panel
//! so the two can never drift apart.

use std::sync::Arc;
use std::time::Duration;

use eframe::egui;

use crate::app::AppState;
use crate::charts::ChartsPanel;
use crate::latency_test::FLASH_INTERVAL_MS;
use crate::metrics::{HELP_KEY, HIDE_ALL_KEY, LATENCY_TEST_KEY, METRICS, SHOW_ALL_KEY, Visibility};

/// Key to toggle the statistics charts panel.
pub const CHARTS_KEY: char = 'o';

fn panel_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(egui::Color32::from_black_alpha(180))
        .rounding(10.0)
        .inner_margin(12.0)
}

fn panel_label(text: impl Into<String>, strong: bool) -> egui::RichText {
    let text = egui::RichText::new(text).monospace().color(egui::Color32::WHITE);
    if strong { text.strong() } else { text.size(12.0) }
}

/// Owns the overlay's own UI-side state -- which metrics are shown, whether
/// help is visible, and the latency-test flash's own timing. None of this
/// touches `AppState`/networking, same separation the GTK version kept.
pub struct Overlay {
    visibility: Visibility,
    last_flash_tick: f64,
    charts: ChartsPanel,
}

impl Overlay {
    pub fn new() -> Self {
        Self {
            visibility: Visibility::new(),
            last_flash_tick: 0.0,
            charts: ChartsPanel::new(),
        }
    }

    /// Call once per frame from `ui.rs`'s `update()`.
    pub fn draw(&mut self, ctx: &egui::Context, state: &Arc<AppState>) {
        self.handle_keyboard(ctx, state);
        self.charts.tick(&state.stats);
        self.charts.draw(ctx, &state.stats);
        self.draw_stats_panel(ctx, state);
        self.draw_help_panel(ctx);
        self.draw_flash_layer(ctx, state);
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context, state: &Arc<AppState>) {
        let chars: Vec<char> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Text(text) => Some(text.chars().collect::<Vec<_>>()),
                    _ => None,
                })
                .flatten()
                .collect()
        });

        for c in chars {
            let key = c.to_ascii_lowercase();
            if key == LATENCY_TEST_KEY {
                let enabled = state.latency_test.toggle();
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(enabled));
            } else if key == CHARTS_KEY {
                self.charts.toggle();
            } else {
                self.visibility.handle_key(c);
            }
        }
    }

    fn draw_stats_panel(&self, ctx: &egui::Context, state: &Arc<AppState>) {
        egui::Area::new(egui::Id::new("stats-panel"))
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-10.0, 10.0))
            .show(ctx, |ui| {
                panel_frame().show(ui, |ui| {
                    for metric in METRICS {
                        if self.visibility.is_enabled(metric.id) {
                            ui.label(panel_label((metric.format)(&state.stats), true));
                        }
                    }
                });
            });
    }

    fn draw_help_panel(&self, ctx: &egui::Context) {
        if !self.visibility.help_visible() {
            return;
        }

        egui::Area::new(egui::Id::new("help-panel"))
            .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(10.0, -10.0))
            .show(ctx, |ui| {
                panel_frame().show(ui, |ui| {
                    ui.label(panel_label(format!("[{HELP_KEY}] toggle this help"), false));
                    ui.label(panel_label(
                        format!(
                            "[{SHOW_ALL_KEY}] show all metrics   [{HIDE_ALL_KEY}] hide all metrics"
                        ),
                        false,
                    ));
                    ui.label(panel_label(
                        format!(
                            "[{LATENCY_TEST_KEY}] toggle glass-to-glass latency test (flashes screen)"
                        ),
                        false,
                    ));
                    ui.label(panel_label(
                        format!("[{CHARTS_KEY}] toggle statistics charts panel"),
                        false,
                    ));
                    for metric in METRICS {
                        ui.label(panel_label(format!("[{}] {}", metric.key, metric.label), false));
                    }
                });
            });
    }

    /// Full-window solid-color flash for the camera-on-screen latency test
    /// (see `latency_test.rs`). Advances the palette on its own timer
    /// (`FLASH_INTERVAL_MS`) using egui's own clock, and keeps a repaint
    /// scheduled so the flash keeps advancing even if nothing else is
    /// triggering redraws (e.g. no video arriving yet).
    fn draw_flash_layer(&mut self, ctx: &egui::Context, state: &Arc<AppState>) {
        if !state.latency_test.is_enabled() {
            return;
        }

        let now = ctx.input(|i| i.time);
        let interval_secs = FLASH_INTERVAL_MS as f64 / 1000.0;
        if now - self.last_flash_tick >= interval_secs {
            state.latency_test.tick();
            self.last_flash_tick = now;
        }
        ctx.request_repaint_after(Duration::from_millis(FLASH_INTERVAL_MS));

        let [r, g, b] = state.latency_test.current_color();
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("latency-flash"),
        ));
        painter.rect_filled(ctx.screen_rect(), 0.0, egui::Color32::from_rgb(r, g, b));
    }
}

impl Default for Overlay {
    fn default() -> Self {
        Self::new()
    }
}
