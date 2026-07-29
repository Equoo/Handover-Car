use anyhow::{Error, Result};
use gstreamer::Registry;
use gstreamer::glib::object::{Cast, ObjectExt};
use gstreamer::prelude::*;
use gstreamer::{self as gst, element_error};
use gstreamer_app as gst_app;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tracing::{info, warn};

use derive_more::derive::{Display, Error};

#[derive(Debug, Display, Error)]
#[display("Received error from {src}: {error} (debug: {debug:?})")]
struct ErrorMessage {
    src: glib::GString,
    error: glib::Error,
    debug: Option<glib::GString>,
}

use crate::app::{AppState, ema_average};

#[derive(Clone, Copy)]
pub struct PacketLoss {
    pub num_lost: u64,
    pub num_pushed: u64,
    pub loss_pct: f64,
}

fn choose_h264_decoder() -> &'static str {
    let registry = Registry::get();

    if registry
        .find_feature("nvh264dec", gstreamer::ElementFactory::static_type())
        .is_some()
    {
        info!("Using NVIDIA NVDEC");
        "nvh264dec"
    } else if registry
        .find_feature("vah264dec", gstreamer::ElementFactory::static_type())
        .is_some()
    {
        info!("Using VAAPI (new)");
        "vah264dec"
    } else if registry
        .find_feature("vaapih264dec", gstreamer::ElementFactory::static_type())
        .is_some()
    {
        info!("Using VAAPI (legacy)");
        "vaapih264dec"
    } else {
        warn!("No hardware available, using software decoder");
        "avdec_h264"
    }
}

// for NVDEC: need 4:2:0, not 4:4:4
pub fn build_video(
    sink: &gst::Element,
    state: Arc<AppState>,
) -> Result<(gst::Pipeline, gst_app::AppSrc)> {
    let caps = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "H264")
        .field("clock-rate", 90000i32)
        .field("payload", 96i32)
        .build();

    let pipeline = gst::Pipeline::default();
    let src = gst::ElementFactory::make("appsrc").name("source").build()?;
    src.set_property("caps", &caps);
    src.set_property("is-live", true);
    src.set_property("format", gst::Format::Time);
    src.set_property("do-timestamp", true); // true may needed but when true if receive packet before playing it warn

    let jitter = gst::ElementFactory::make("rtpjitterbuffer").build()?;
    jitter.set_property("latency", 40u32);
    jitter.set_property("drop-on-latency", true);
    jitter.set_property("do-lost", true);
    jitter.set_property("do-retransmission", true);

    let depay = gst::ElementFactory::make("rtph264depay").build()?;

    let parse = gst::ElementFactory::make("h264parse").build()?;
    parse.set_property_from_str("config-interval", "-1");
    parse.set_property_from_str("update-timecode", "false"); // NOTE: may is wrong

    let decoder = choose_h264_decoder();
    let dec = gst::ElementFactory::make(decoder).build()?;
    if decoder == "avdec_h264" {
        dec.set_property_from_str("lowres", "0");
    }

    let conv = gst::ElementFactory::make("videoconvert").build()?;

    let caps_filter = gst::ElementFactory::make("capsfilter").build()?;
    caps_filter.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "BGRx")
            .build(),
    );

    pipeline.add_many([
        &src,
        &jitter,
        &depay,
        &parse,
        &dec,
        &conv,
        &caps_filter,
        sink,
    ])?;

    gst::Element::link_many([
        &src,
        &jitter,
        &depay,
        &parse,
        &dec,
        &conv,
        &caps_filter,
        sink,
    ])?;

    // jitter.connect("request-fec-decoder", false, |values| {
    //     let jitter = values[0]
    //         .get::<gst::Element>()
    //         .expect("jitter \"request-fec-decoder\" signal values[0]");
    //     let sess_id = values[1]
    //         .get::<u32>()
    //         .expect("jitter \"request-fec-decoder\" signal values[1]");
    //
    //     match make_fec_decoder(&jitter, sess_id) {
    //         Ok(elem) => Some(elem.to_value()),
    //         Err(err) => {
    //             element_error!(
    //                 jitter,
    //                 gst::LibraryError::Failed,
    //                 ("Failed to make FEC decoder"),
    //                 ["{}", err]
    //             );
    //             None
    //         }
    //     }
    // });

    let entry_time: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let entry_clone = entry_time.clone();
    dec.static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            *entry_clone.lock() = Some(Instant::now());
            gst::PadProbeReturn::Ok
        });

    let state_clone2 = state.clone();
    dec.static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            if let Some(t) = *entry_time.lock() {
                let mut decode_latency = state_clone2.decode_latency.lock();
                *decode_latency = ema_average(*decode_latency, t.elapsed().as_secs_f64() * 1000f64);
            }
            gst::PadProbeReturn::Ok
        });

    let jitter_clone = jitter.clone();
    let state_clone = state.clone();
    tokio::spawn(async move {
        // Polled fast because this drives keyframe recovery, not just the
        // stats overlay: every tick spent here is time the picture stays
        // broken after a loss.
        let mut last_lost: u64 = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let stats = jitter_clone.property::<gst::Structure>("stats");
            // Fields: num-pushed, num-lost, num-late, num-duplicates, rtx-count...
            let num_lost = stats.get::<u64>("num-lost").unwrap_or(0);
            let num_pushed = stats.get::<u64>("num-pushed").unwrap_or(0);
            let loss_pct = if num_pushed > 0 {
                num_lost as f64 / (num_lost + num_pushed) as f64 * 100.0
            } else {
                0.0
            };
            {
                let mut loss = state_clone.loss.lock();
                loss.num_lost = num_lost;
                loss.num_pushed = num_pushed;
                loss.loss_pct = loss_pct;
            }

            // A lost packet breaks the P-frame chain: everything after it
            // decodes against a reference the encoder and decoder no longer
            // agree on. Only an IDR resets that.
            // if num_lost > last_lost {
            //     state_clone.request_keyframe();
            // }
            last_lost = num_lost;
        }
    });

    match src.dynamic_cast::<gst_app::AppSrc>() {
        Ok(appsrc_casted) => Ok((pipeline, appsrc_casted)),
        Err(_) => anyhow::bail!("Dynamic cast of appsrc failed"),
    }
}
