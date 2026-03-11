use anyhow::{Error, Result};
use gstreamer::{self as gst, element_error};
use gstreamer::glib::object::{Cast, ObjectExt};
use gstreamer::{ prelude::*};
use gstreamer::{Registry};
use gstreamer_app as gst_app;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::time::Instant;
use tracing::{info, warn};

use derive_more::derive::{Display, Error};

#[derive(Debug, Display, Error)]
#[display("No such pad {_0} in {_1}")]
struct NoSuchPad(#[error(not(source))] &'static str, String);

#[derive(Debug, Display, Error)]
#[display("Unknown payload type {_0}")]
struct UnknownPT(#[error(not(source))] u32);

#[derive(Debug, Display, Error)]
#[display("Usage: {_0} (play | record) DROP_PROBABILITY")]
struct UsageError(#[error(not(source))] String);

#[derive(Debug, Display, Error)]
#[display("Received error from {src}: {error} (debug: {debug:?})")]
struct ErrorMessage {
    src: glib::GString,
    error: glib::Error,
    debug: Option<glib::GString>,
}

fn static_pad(element: &gst::Element, pad_name: &'static str) -> Result<gst::Pad, Error> {
    match element.static_pad(pad_name) {
        Some(pad) => Ok(pad),
        None => {
            let element_name = element.name();
            Err(Error::from(NoSuchPad(pad_name, element_name.to_string())))
        }
    }
}

fn request_pad(element: &gst::Element, pad_name: &'static str) -> Result<gst::Pad, Error> {
    match element.request_pad_simple(pad_name) {
        Some(pad) => Ok(pad),
        None => {
            let element_name = element.name();
            Err(Error::from(NoSuchPad(pad_name, element_name.to_string())))
        }
    }
}

fn connect_rtpbin_srcpad(src_pad: &gst::Pad, sink: &gst::Element) -> Result<(), Error> {
    let name = src_pad.name();
    let split_name = name.split('_');
    let split_name = split_name.collect::<Vec<&str>>();
    let pt = split_name[5].parse::<u32>()?;

    match pt {
        96 => {
            let sinkpad = static_pad(sink, "sink")?;
            src_pad.link(&sinkpad)?;
            Ok(())
        }
        _ => Err(Error::from(UnknownPT(pt))),
    }
}

fn make_fec_decoder(rtpbin: &gst::Element, sess_id: u32) -> Result<gst::Element, Error> {
    let internal_storage = rtpbin.emit_by_name::<glib::Object>("get-internal-storage", &[&sess_id]);
    let fecdec = gst::ElementFactory::make("rtpulpfecdec")
        .property("storage", &internal_storage)
        .property("pt", 122u32)
        .build()?;

    Ok(fecdec)
}

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
    let drop_probability = 0.3f32;
    let caps = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("clock-rate", 90000i32)
        .build();

    let pipeline = gst::Pipeline::default();
    let src = gst::ElementFactory::make("appsrc").name("source").build()?;
        src.set_property("caps", &caps);
        src.set_property("is-live", true);
        src.set_property("format", gst::Format::Time);
        src.set_property("do-timestamp", false); // true may needed but when true if receive packet before playing it warn

    let netsim = gst::ElementFactory::make("netsim").build()?;
        netsim.set_property("drop-probability", drop_probability);

    let rtpbin = gst::ElementFactory::make("rtpbin").build()?;
        rtpbin.set_property("latency", 40u32);
        rtpbin.set_property("drop-on-latency", true);
        rtpbin.set_property("do-lost", true);
        rtpbin.set_property("do-retransmission", true);

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
        &netsim,
        &rtpbin,
        &depay,
        &parse,
        &dec,
        &conv,
        &caps_filter,
        sink,
    ])?;

    gst::Element::link_many([
        &depay,
        &parse,
        &dec,
        &conv,
        &caps_filter,
        sink,
    ])?;

    src.link(&netsim)?;

    rtpbin.connect("new-storage", false, |values| {
        let storage = values[1]
            .get::<gst::Element>()
            .expect("rtpbin \"new-storage\" signal values[1]");
        storage.set_property("size-time", 100_000_000u64);

        None
    });

    rtpbin.connect("request-pt-map", false, |values| {
        let pt = values[2]
            .get::<u32>()
            .expect("rtpbin \"new-storage\" signal values[2]");
        match pt {
            122 => Some(
                gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("clock-rate", 90000i32)
                    .field("is-fec", true)
                    .build()
                    .to_value(),
            ),
            96 => Some(
                gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("encoding-name", "H264")
                    .field("clock-rate", 90000i32)
                    .field("payload", 96i32)
                    .build()
                    .to_value(),
            ),
            _ => None,
        }
    });

    rtpbin.connect("request-fec-decoder", false, |values| {
        let rtpbin = values[0]
            .get::<gst::Element>()
            .expect("rtpbin \"request-fec-decoder\" signal values[0]");
        let sess_id = values[1]
            .get::<u32>()
            .expect("rtpbin \"request-fec-decoder\" signal values[1]");

        match make_fec_decoder(&rtpbin, sess_id) {
            Ok(elem) => Some(elem.to_value()),
            Err(err) => {
                element_error!(
                    rtpbin,
                    gst::LibraryError::Failed,
                    ("Failed to make FEC decoder"),
                    ["{}", err]
                );
                None
            }
        }
    });

    let srcpad = static_pad(&netsim, "src")?;
    let sinkpad = request_pad(&rtpbin, "recv_rtp_sink_0")?;
    srcpad.link(&sinkpad)?;

    let depay_weak = depay.downgrade();
    rtpbin.connect_pad_added(move |rtpbin, src_pad| {
        let Some(depay) = depay_weak.upgrade() else {
            return;
        };

        match connect_rtpbin_srcpad(src_pad, &depay) {
            Ok(_) => (),
            Err(err) => {
                element_error!(
                    rtpbin,
                    gst::LibraryError::Failed,
                    ("Failed to link srcpad"),
                    ["{}", err]
                );
            }
        }
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

    // let jitter_clone = jitter.clone();
    // let state_clone = state.clone();
    // tokio::spawn(async move {
    //     loop {
    //         tokio::time::sleep(Duration::from_secs(2)).await;
    //         let stats = jitter_clone.property::<gst::Structure>("stats");
    //         // Fields: num-pushed, num-lost, num-late, num-duplicates, rtx-count...
    //         let num_lost = stats.get::<u64>("num-lost").unwrap_or(0);
    //         let num_pushed = stats.get::<u64>("num-pushed").unwrap_or(0);
    //         let loss_pct = if num_pushed > 0 {
    //             num_lost as f64 / (num_lost + num_pushed) as f64 * 100.0
    //         } else {
    //             0.0
    //         };
    //         let mut loss = state_clone.loss.lock();
    //         loss.num_lost = num_lost;
    //         loss.num_pushed = num_pushed;
    //         loss.loss_pct = loss_pct;
    //     }
    // });

    match src.dynamic_cast::<gst_app::AppSrc>() {
        Ok(appsrc_casted) => Ok((pipeline, appsrc_casted)),
        Err(_) => anyhow::bail!("Dynamic cast of appsrc failed"),
    }
}
