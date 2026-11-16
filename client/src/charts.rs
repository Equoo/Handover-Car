// SPDX-FileCopyrightText: 2026 David Derny
// SPDX-FileCopyrightText: 2026 Handover contributors (see AUTHORS)
//
// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0

//! Time-series charts for latency, bitrate, and loss/FEC metrics.
//! Rendered in a toggleable side panel using egui_plot.

use std::collections::VecDeque;
use std::time::Instant;

use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints};

use crate::stats::Stats;

/// Sampling interval for chart data (200ms = 5Hz).
const SAMPLE_INTERVAL_MS: u64 = 200;

/// Number of samples to keep (300 samples at 5Hz = 60 seconds).
const HISTORY_DEPTH: usize = 300;

/// Width of the charts panel as a fraction of the window width.
const PANEL_WIDTH_FRACTION: f32 = 0.3;

/// A single sample containing up to 4 series values.
#[derive(Clone, Copy, Default)]
struct Sample {
    values: [f32; 4],
}

/// Fixed-capacity ring buffer for time-series data.
struct RingBuffer {
    data: VecDeque<Sample>,
    capacity: usize,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, sample: Sample) {
        if self.data.len() >= self.capacity {
            self.data.pop_front();
        }
        self.data.push_back(sample);
    }

    /// Returns points for a given series index, with X as seconds ago (negative).
    fn points(&self, series_idx: usize) -> PlotPoints {
        let len = self.data.len();
        let points: Vec<[f64; 2]> = self
            .data
            .iter()
            .enumerate()
            .map(|(i, sample)| {
                // X: seconds ago (most recent = 0, oldest = -60)
                let x = (i as f64 - len as f64) * (SAMPLE_INTERVAL_MS as f64 / 1000.0);
                let y = sample.values[series_idx] as f64;
                [x, y]
            })
            .collect();
        PlotPoints::new(points)
    }
}

/// Stores historical data for all chart types.
struct ChartHistory {
    latency: RingBuffer,
    bitrate: RingBuffer,
    loss_fec: RingBuffer,
    last_sample: Instant,
}

impl ChartHistory {
    fn new() -> Self {
        Self {
            latency: RingBuffer::new(HISTORY_DEPTH),
            bitrate: RingBuffer::new(HISTORY_DEPTH),
            loss_fec: RingBuffer::new(HISTORY_DEPTH),
            last_sample: Instant::now(),
        }
    }

    /// Sample current stats if enough time has elapsed.
    fn tick(&mut self, stats: &Stats) {
        if self.last_sample.elapsed().as_millis() < SAMPLE_INTERVAL_MS as u128 {
            return;
        }
        self.last_sample = Instant::now();

        // Latency: client_ping, slave_ping, decode, glass-to-glass estimate
        let client_ping = *stats.client_ping.lock() as f32;
        let slave_ping = *stats.slave_ping.lock() as f32;
        let decode = *stats.decode_latency.lock() as f32;
        let encoder_latency = *stats.encoder_latency.lock() as f32;
        let g2g = encoder_latency + client_ping + slave_ping + decode;
        self.latency.push(Sample {
            values: [client_ping, slave_ping, decode, g2g],
        });

        // Bitrate: target, received, link_estimate, encoder (all in kbps)
        let target = *stats.target_bitrate.lock() as f32 / 1000.0;
        let received = stats.received_bitrate() as f32 / 1000.0;
        let link_est = *stats.link_estimate_bitrate.lock() as f32 / 1000.0;
        let encoder = *stats.encoder_bitrate.lock() as f32 / 1000.0;
        self.bitrate.push(Sample {
            values: [target, received, link_est, encoder],
        });

        // Loss & FEC: windowed_loss, fec_pct
        let windowed_loss = stats.windowed_loss_pct() as f32;
        let fec_pct = *stats.target_fec_pct.lock() as f32;
        self.loss_fec.push(Sample {
            values: [windowed_loss, fec_pct, 0.0, 0.0],
        });
    }
}

/// The charts panel, owning history and visibility state.
pub struct ChartsPanel {
    history: ChartHistory,
    visible: bool,
}

impl ChartsPanel {
    pub fn new() -> Self {
        Self {
            history: ChartHistory::new(),
            visible: false,
        }
    }

    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }

    /// Update history with current stats (call every frame).
    pub fn tick(&mut self, stats: &Stats) {
        self.history.tick(stats);
    }

    /// Draw the charts panel on the right side of the screen.
    pub fn draw(&self, ctx: &egui::Context, _stats: &Stats) {
        if !self.visible {
            return;
        }

        let screen_width = ctx.screen_rect().width();
        let panel_width = screen_width * PANEL_WIDTH_FRACTION;

        egui::SidePanel::right("charts_panel")
            .resizable(false)
            .exact_width(panel_width)
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(200))
                    .inner_margin(8.0),
            )
            .show(ctx, |ui| {
                ui.style_mut().visuals.override_text_color = Some(egui::Color32::WHITE);

                // Calculate available height for each chart
                let available_height = ui.available_height();
                let chart_height = (available_height - 60.0) / 3.0; // 3 charts with some spacing

                self.draw_latency_chart(ui, chart_height);
                ui.add_space(8.0);
                self.draw_bitrate_chart(ui, chart_height);
                ui.add_space(8.0);
                self.draw_loss_fec_chart(ui, chart_height);
            });
    }

    fn draw_latency_chart(&self, ui: &mut egui::Ui, height: f32) {
        ui.label(
            egui::RichText::new("Latency (ms)")
                .strong()
                .color(egui::Color32::WHITE),
        );

        let plot = Plot::new("latency_chart")
            .height(height)
            .show_axes(true)
            .show_grid(true)
            .include_y(0.0)
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .x_axis_label("seconds ago")
            .y_axis_label("ms")
            .legend(egui_plot::Legend::default().position(egui_plot::Corner::LeftTop));

        plot.show(ui, |plot_ui| {
            // Client ping (blue)
            plot_ui.line(
                Line::new(self.history.latency.points(0))
                    .name("Client Ping")
                    .color(egui::Color32::from_rgb(100, 149, 237)),
            );
            // Slave ping (green)
            plot_ui.line(
                Line::new(self.history.latency.points(1))
                    .name("Slave Ping")
                    .color(egui::Color32::from_rgb(50, 205, 50)),
            );
            // Decode (yellow)
            plot_ui.line(
                Line::new(self.history.latency.points(2))
                    .name("Decode")
                    .color(egui::Color32::from_rgb(255, 215, 0)),
            );
            // Glass-to-glass (red)
            plot_ui.line(
                Line::new(self.history.latency.points(3))
                    .name("Glass-to-Glass")
                    .color(egui::Color32::from_rgb(255, 99, 71)),
            );
        });
    }

    fn draw_bitrate_chart(&self, ui: &mut egui::Ui, height: f32) {
        ui.label(
            egui::RichText::new("Bitrate (kbps)")
                .strong()
                .color(egui::Color32::WHITE),
        );

        let plot = Plot::new("bitrate_chart")
            .height(height)
            .show_axes(true)
            .show_grid(true)
            .include_y(0.0)
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .x_axis_label("seconds ago")
            .y_axis_label("kbps")
            .legend(egui_plot::Legend::default().position(egui_plot::Corner::LeftTop));

        plot.show(ui, |plot_ui| {
            // Target (blue)
            plot_ui.line(
                Line::new(self.history.bitrate.points(0))
                    .name("Target")
                    .color(egui::Color32::from_rgb(100, 149, 237)),
            );
            // Received (green)
            plot_ui.line(
                Line::new(self.history.bitrate.points(1))
                    .name("Received")
                    .color(egui::Color32::from_rgb(50, 205, 50)),
            );
            // Link estimate (yellow)
            plot_ui.line(
                Line::new(self.history.bitrate.points(2))
                    .name("Link Est")
                    .color(egui::Color32::from_rgb(255, 215, 0)),
            );
            // Encoder (light red)
            plot_ui.line(
                Line::new(self.history.bitrate.points(3))
                    .name("Encoder")
                    .color(egui::Color32::from_rgb(255, 160, 122)),
            );
        });
    }

    fn draw_loss_fec_chart(&self, ui: &mut egui::Ui, height: f32) {
        ui.label(
            egui::RichText::new("Loss & FEC (%)")
                .strong()
                .color(egui::Color32::WHITE),
        );

        let plot = Plot::new("loss_fec_chart")
            .height(height)
            .show_axes(true)
            .show_grid(true)
            .include_y(0.0)
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .x_axis_label("seconds ago")
            .y_axis_label("%")
            .legend(egui_plot::Legend::default().position(egui_plot::Corner::LeftTop));

        plot.show(ui, |plot_ui| {
            // Windowed loss (red)
            plot_ui.line(
                Line::new(self.history.loss_fec.points(0))
                    .name("Windowed Loss")
                    .color(egui::Color32::from_rgb(255, 99, 71)),
            );
            // FEC overhead (blue)
            plot_ui.line(
                Line::new(self.history.loss_fec.points(1))
                    .name("FEC Overhead")
                    .color(egui::Color32::from_rgb(100, 149, 237)),
            );
        });
    }
}

impl Default for ChartsPanel {
    fn default() -> Self {
        Self::new()
    }
}
