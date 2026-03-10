use anyhow::{Context, Result, bail};
use gstreamer as gst;
use gstreamer::glib::object::{Cast, ObjectExt};
use gstreamer::{Buffer, prelude::*};
use gstreamer::{ClockTime, Message, MessageView, Registry};
use gstreamer_app as gst_app;
use gtk::{Application, ApplicationWindow};
use gtk::{Box as GtkBox, Label, Orientation, Overlay, Window, prelude::*};
use parking_lot::Mutex;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::{env, thread};
use tokio::io::Join;
use tokio::join;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::level_filters::LevelFilter;
use tracing::{error, info, warn};

use crate::app::{AppState, ema_average};

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
    let pipeline = gst::Pipeline::new();

    let appsrc = gst::ElementFactory::make("appsrc").name("source").build()?;

    // RTP caps directly on udpsrc
    let caps = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "H264")
        .field("payload", 96i32)
        .field("clock-rate", 90000i32)
        .field("seqnum-base", 0i32)
        .build();

    appsrc.set_property("caps", &caps);
    appsrc.set_property("is-live", true);
    appsrc.set_property("format", gst::Format::Time);
    appsrc.set_property("do-timestamp", true); // may go to true

    // let queue = gst::ElementFactory::make("queue").build()?;
    // queue.set_property("max-size-buffers", 14u32);
    // queue.set_property("max-size-time", 0u64);
    // queue.set_property("max-size-bytes", 0u32);
    // queue.set_property_from_str("leaky", "downstream");

    let fecdec = gst::ElementFactory::make("rtpulpfecdec")
    .build()?;
    fecdec.set_property("pt", 122u32);

    let storage = gst::ElementFactory::make("rtpstorage").build()?;
    storage.set_property("size-time", 250_000_000u64);

    let jitter = gst::ElementFactory::make("rtpjitterbuffer").build()?;
    jitter.set_property("latency", 40u32);
    jitter.set_property("drop-on-latency", true);
    jitter.set_property("do-lost", true);
    jitter.set_property("do-retransmission", true);
    let storage_val = storage.to_value();
    jitter.set_property("rtp-storage", &storage_val);

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
        &appsrc,
        &storage,
        &jitter,
        &fecdec,
        &depay,
        &parse,
        &dec,
        &conv,
        &caps_filter,
        sink,
    ])?;
    gst::Element::link_many([
        &appsrc,
        &storage,
        &jitter,
        &fecdec,
        &depay,
        &parse,
        &dec,
        &conv,
        &caps_filter,
        sink,
    ])?;

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
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let stats = jitter_clone.property::<gst::Structure>("stats");
            // Fields: num-pushed, num-lost, num-late, num-duplicates, rtx-count...
            let num_lost = stats.get::<u64>("num-lost").unwrap_or(0);
            let num_pushed = stats.get::<u64>("num-pushed").unwrap_or(0);
            let loss_pct = if num_pushed > 0 {
                num_lost as f64 / (num_lost + num_pushed) as f64 * 100.0
            } else {
                0.0
            };
            let mut loss = state_clone.loss.lock();
            loss.num_lost = num_lost;
            loss.num_pushed = num_pushed;
            loss.loss_pct = loss_pct;
        }
    });

    match appsrc.dynamic_cast::<gst_app::AppSrc>() {
        Ok(appsrc_casted) => Ok((pipeline, appsrc_casted)),
        Err(_) => anyhow::bail!("Dynamic cast of appsrc failed"),
    }
}
