use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::process::ExitCode;
use std::sync::Arc;
use std::{env, thread};
use std::str::FromStr;
use gstreamer::{ClockTime, Message, MessageView, Registry};
use gstreamer::glib::object::{Cast, ObjectExt};
use gstreamer::{Buffer, prelude::*};
use gstreamer as gst;
use gstreamer_app as gst_app;
use gtk::{Label, Box as GtkBox, Orientation, Overlay, Window, prelude::*};
use gtk::{Application, ApplicationWindow};
use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use tokio::io::Join;
use tokio::join;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{error, info, warn};
use tracing::level_filters::LevelFilter;

use crate::app::AppState;

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
pub fn build_video(sink: &gst::Element, state: Arc<AppState>) -> Result<(gst::Pipeline, gst_app::AppSrc)> {
    let pipeline = gst::Pipeline::new();

    let appsrc = gst::ElementFactory::make("appsrc")
        .name("source")
        .build()?;

    // RTP caps directly on udpsrc
    let caps = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "H264")
        .field("payload", 96i32)
        .field("clock-rate", 90000i32)
        .build();

    appsrc.set_property("caps", &caps);
    appsrc.set_property("is-live", true);
    appsrc.set_property("format", gst::Format::Time);
    appsrc.set_property("do-timestamp", false);

    let queue = gst::ElementFactory::make("queue").build()?;
    queue.set_property("max-size-buffers", 10u32);
    queue.set_property("max-size-time", 0u64);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property_from_str("leaky", "downstream");

    let jitter = gst::ElementFactory::make("rtpjitterbuffer").build()?;
    jitter.set_property("latency", 0u32);
    jitter.set_property("drop-on-latency", true);
    // jitter.set_property("latency", 20u32);
    // jitter.set_property("drop-on-latency", false);

    let depay  = gst::ElementFactory::make("rtph264depay").build()?;
    let parse  = gst::ElementFactory::make("h264parse").build()?;
    parse.set_property_from_str("config-interval", "-1");
    // parse.set_property_from_str("parse-only", "true"); // non existing ?

    let decoder = choose_h264_decoder();
    let dec = gst::ElementFactory::make(decoder)
        .build()?;
    // dec.set_property_from_str("low-latency", "true"); // AVDEC_H264 ONLY ? // DO NOT EXIST ?
    // dec.set_property_from_str("skip-frame", "nonref"); // AVDEC_H264 ONLY  // TOO

    let conv   = gst::ElementFactory::make("videoconvert").build()?;
    let caps_filter = gst::ElementFactory::make("capsfilter").build()?;
    caps_filter.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "BGRx")
            .build(),
    );

    pipeline.add_many([&appsrc, &queue, &jitter, &depay, &parse, &dec, &conv, &caps_filter, sink])?;
    gst::Element::link_many([&appsrc, &queue, &jitter, &depay, &parse, &dec, &conv, &caps_filter, sink])?;

    let entry_time: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let entry_clone = entry_time.clone();
    dec.static_pad("sink").unwrap()
    .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        *entry_clone.lock() = Some(Instant::now());
        gst::PadProbeReturn::Ok
    });

    dec.static_pad("src").unwrap()
    .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        if let Some(t) = *entry_time.lock() {
            *state.decode_latency.lock() = t.elapsed().as_secs_f64() * 1000f64;
        }
        gst::PadProbeReturn::Ok
    });

    match appsrc.dynamic_cast::<gst_app::AppSrc>() {
        Ok(appsrc_casted) => Ok((pipeline, appsrc_casted)),
        Err(_) => anyhow::bail!("Dynamic cast of appsrc failed")
    }
}
