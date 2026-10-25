use anyhow::{Context, Result};
use eframe::egui;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer::{Message, MessageView};
use std::sync::Arc;
use std::thread;
use tracing::{error, info, warn};

use crate::app::AppState;
use crate::overlay::Overlay;
use crate::video::{RawRtpUdp, VideoReceiver};
use crate::input_handler::InputHandler;

/// eframe (winit) doesn't run a GLib main loop, but `bus.add_watch` below
/// needs one somewhere to actually dispatch its callback -- GTK used to
/// provide that for free via `Application::run()`. Spawned once and left
/// running for the process lifetime; nothing else in this app needs GLib's
/// loop directly.
fn spawn_glib_main_loop() {
    thread::spawn(|| {
        glib::MainLoop::new(None, false).run();
    });
}

pub struct App {
    state: Arc<AppState>,
    pipeline: gst::Pipeline,
    texture: Option<egui::TextureHandle>,
    overlay: Overlay,
    // Dropping this removes the bus watch and silently kills GST logging,
    // so it has to live as long as the app does.
    _bus_watch: gst::bus::BusWatchGuard,
    input_handler: InputHandler,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, state: Arc<AppState>) -> Result<Self> {
        // Read by the appsink callback in `video.rs` to request a repaint
        // the moment a new frame lands.
        state
            .egui_ctx
            .set(cc.egui_ctx.clone())
            .map_err(|_| anyhow::anyhow!("egui context already set"))?;

        spawn_glib_main_loop();

        let (pipeline, appsrc) = RawRtpUdp::build_pipeline(state.clone())?;
        pipeline.set_state(gst::State::Playing)?;
        *state.appsrc.lock() = Some(appsrc);

        let bus = pipeline.bus().context("pipeline has no bus")?;
        let bus_watch = bus.add_watch(move |_, msg: &Message| {
            match msg.view() {
                MessageView::Error(err) => {
                    error!("GST ERROR: {} {:?}", err.error(), err.debug());
                }
                MessageView::Warning(w) => {
                    warn!("GST WARN: {} (src: {:?}, dbg: {:?})",
                          w.error(),
                          msg.src().map(|s| s.name()),
                          w.debug());
                }
                MessageView::StateChanged(s) => {
                    info!("State: {:?} -> {:?}", s.old(), s.current());
                }
                MessageView::Eos(_) => warn!("GST Receive EOS"),
                _ => {}
            }
            glib::ControlFlow::Continue
        })?;

        Ok(Self {
            state,
            pipeline,
            texture: None,
            overlay: Overlay::new(),
            _bus_watch: bus_watch,
            input_handler: InputHandler::new(),
        })
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(image) = self.state.latest_frame.lock().take() {
            match &mut self.texture {
                Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.texture =
                        Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR));
                }
            }
        }

        self.input_handler.update(self.state.clone());

        egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                let available = ui.available_size();
                match &self.texture {
                    Some(tex) => {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(available));
                    }
                    None => {
                        ui.centered_and_justified(|ui| ui.label("Waiting for video..."));
                    }
                }

            });

        self.overlay.draw(ctx, &self.state);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
