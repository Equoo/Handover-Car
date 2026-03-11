use std::sync::Arc;
use anyhow::{Result};
use gstreamer as gst;
use gstreamer::glib::object::{ObjectExt};
use gstreamer::{ prelude::*};
use gstreamer::{Message, MessageView};
use gtk::{Application, ApplicationWindow};
use gtk::{Box as GtkBox, Label, Orientation, Overlay,prelude::*};
use tracing::{error, info, warn};

use crate::app::AppState;
use crate::video::build_video;

fn window_run(win: &ApplicationWindow, sink: &gst::Element, state: Arc<AppState>) {
    let (pipeline, appsrc) = build_video(sink, state.clone()).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();
    *state.appsrc.lock() = Some(appsrc);

    let bus = pipeline.bus().unwrap();
    bus.add_watch(move |_, msg: &Message| {
        match msg.view() {
            MessageView::Error(err) => {
                error!("GST ERROR: {} {:?}", err.error(), err.debug());
            }
            MessageView::Warning(w) => {
                warn!("GST WARN: {}", w.error());
            }
            MessageView::StateChanged(s) => {
                info!("State: {:?} -> {:?}", s.old(), s.current());
            }
            MessageView::Eos(_) => warn!("GST Receive EOS"),
            _ => {}
        }
        glib::ControlFlow::Continue
    })
    .unwrap();

    win.connect_close_request(move |_| {
        pipeline.set_state(gstreamer::State::Null).unwrap();
        glib::Propagation::Proceed
    });
}

pub fn build(app: &Application, state: Arc<AppState>) -> Result<()> {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Rc car client")
        .default_width(1280)
        .default_height(720)
        .build();

    let sink = gst::ElementFactory::make("gtk4paintablesink").build()?;
    sink.set_property("sync", false);
    sink.set_property("async", false);
    sink.set_property("qos", true);
    let paintable: gtk::gdk::Paintable = sink.property("paintable");
    let picture = gtk::Picture::for_paintable(&paintable);

    {
        let overlay = Overlay::new();
        let stats_box = GtkBox::new(Orientation::Vertical, 5);
        stats_box.set_halign(gtk::Align::End);
        stats_box.set_valign(gtk::Align::Start);
        stats_box.set_margin_top(10);
        stats_box.set_margin_end(10);

        let fps_label = Label::new(Some("FPS: --"));
        let decode_label = Label::new(Some("Decode latency: --"));
        let client_label = Label::new(Some("client ping: --"));
        let slave_label = Label::new(Some("Slave ping: --"));
        let loss_label = Label::new(Some(""));
        let bitrate_label = Label::new(Some("Bitrate: --"));

        stats_box.append(&fps_label);
        stats_box.append(&slave_label);
        stats_box.append(&client_label);
        stats_box.append(&decode_label);
        stats_box.append(&loss_label);
        stats_box.append(&bitrate_label);

        let css_provider = gtk::CssProvider::new();
        css_provider.load_from_data(
            "box {
                background-color: rgba(0, 0, 0, 0.7);
                border-radius: 10px;
                padding: 15px;
            }
            label {
                color: white;
                font-size: 16px;
                font-weight: bold;
                font-family: monospace;
            }",
        );

        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().unwrap(),
            &css_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        overlay.add_overlay(&stats_box);
        overlay.set_child(Some(&picture));

        window.set_child(Some(&overlay));

        let decode_clone = decode_label.clone();
        let slave_clone = slave_label.clone();
        let loss_clone = loss_label.clone();
        let client_clone = client_label.clone();
        let state_clone = state.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            // fps_label_clone.set_text(&format!("FPS: {:.1}", stats.fps));
            decode_clone.set_text(&format!(
                "Decode latency: {:.3}ms",
                *state_clone.decode_latency.lock()
            ));
            slave_clone.set_text(&format!(
                "Slave ping: {}ms",
                *state_clone.slave_latency.lock()
            ));
            client_clone.set_text(&format!(
                "Client ping: {}ms",
                *state_clone.client_latency.lock()
            ));
            {
                let loss = state_clone.loss.lock();
                loss_clone.set_text(&format!(
                    "RTP loss: {}/{} ({:.1}%)",
                    loss.num_lost,
                    loss.num_lost + loss.num_pushed,
                    loss.loss_pct
                ));
            }
            // bitrate_label_clone.set_text(&format!(
            //     "Bitrate: {:.1} Mbps",
            //     stats.bitrate as f64 / 1_000_000.0
            // ));
            glib::ControlFlow::Continue
        });
    }

    window.connect_realize(move |win| window_run(win, &sink, state.clone()));
    window.present();
    Ok(())
}
