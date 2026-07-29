use anyhow::Result;
use gstreamer as gst;
use gstreamer::glib::object::ObjectExt;
use gstreamer::prelude::*;
use gstreamer::{Message, MessageView};
use gtk::prelude::*;
use gtk::{Application, ApplicationWindow};
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::app::AppState;
use crate::overlay;
use crate::video::build_video;

fn window_run(win: &ApplicationWindow, sink: &gst::Element, state: Arc<AppState>) {
    let (pipeline, appsrc) = build_video(sink, state.clone()).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();
    *state.appsrc.lock() = Some(appsrc);

    let bus = pipeline.bus().unwrap();
    let bus_watch = bus
        .add_watch(move |_, msg: &Message| {
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
        // Dropping the guard removes the bus watch, so it must be kept alive
        // (by being captured here) for as long as the window is; letting it
        // drop when `window_run` returns would silently kill all GST logging.
        let _ = &bus_watch;
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

    let overlay = overlay::build(&window, &picture, state.clone());
    window.set_child(Some(&overlay));

    window.connect_realize(move |win| window_run(win, &sink, state.clone()));
    window.present();
    Ok(())
}
