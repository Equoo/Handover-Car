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

    let src = gst::ElementFactory::make("srtsrc")
        .property(
            "uri",
            &format!("srt://{}:{}?mode=caller", pi_public_ip, my_local_port),
        )
        .property("latency", 150u32) // ms, tune per earlier discussion
        .build()?;

    let tsdemux = gst::ElementFactory::make("tsdemux").build()?;

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

    pipeline.add_many([&src, &tsdemux, &parse, &dec, &conv, &caps_filter, sink])?;
    src.link(&tsdemux)?;
    gst::Element::link_many([&src, &tsdemux, &parse, &dec, &conv, &caps_filter, sink])?;

    tsdemux.connect_pad_added(move |_, src_pad| {
        let sink_pad = parse.static_pad("sink").unwrap();
        src_pad.link(&sink_pad).unwrap();
    });

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

    match src.dynamic_cast::<gst_app::AppSrc>() {
        Ok(appsrc_casted) => Ok((pipeline, appsrc_casted)),
        Err(_) => anyhow::bail!("Dynamic cast of appsrc failed"),
    }
}
