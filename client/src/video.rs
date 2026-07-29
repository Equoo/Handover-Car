use anyhow::{Context, Result};
use common::VideoMode;
use gstreamer::Registry;
use gstreamer::glib::object::{Cast, ObjectExt};
use gstreamer::prelude::*;
use gstreamer::{self as gst};
use gstreamer_app as gst_app;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::app::AppState;
use crate::stats::PacketLoss;

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

/// How the client receives and decodes video for a given `VideoMode`. Only
/// one mode is implemented today (`RawRtpUdp`, below); a future mode (e.g.
/// WebRTC) is a new impl of this trait, not a rewrite of `ui.rs`/`app.rs`'s
/// call sites.
pub trait VideoReceiver {
    /// Builds the pipeline that decodes onto `sink`. Called once at window
    /// creation, before the server (and so the negotiated `VideoMode`) is
    /// known -- with a single mode implemented there's nothing to select
    /// yet, but this is the seam a mode-aware choice would hook into.
    fn build_pipeline(
        sink: &gst::Element,
        state: Arc<AppState>,
    ) -> Result<(gst::Pipeline, gst_app::AppSrc)>;

    /// Receives video over the network per `mode` and pushes it into
    /// `state.appsrc`. Runs until a fatal setup error occurs; per-packet
    /// errors are logged and skipped, not propagated.
    async fn run(state: Arc<AppState>, mode: VideoMode) -> Result<()>;
}

/// Raw RTP packets read straight off a UDP socket into an `appsrc`, decoded
/// by a `rtpjitterbuffer` -> depay -> parse -> decode -> convert chain. This
/// is `VideoMode::RawRtpUdp`'s client-side implementation.
pub struct RawRtpUdp;

impl VideoReceiver for RawRtpUdp {
    // for NVDEC: need 4:2:0, not 4:4:4
    fn build_pipeline(
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
        jitter.set_property("latency", 80u32);
        jitter.set_property("drop-on-latency", true);
        jitter.set_property("do-lost", true);
        // `rtpjitterbuffer` only runs its "this packet is never coming, give up"
        // timer -- which is also what increments `num-lost` in `stats` -- as
        // part of the retransmission codepath. With this off, a sequence gap is
        // silently skipped the moment a later packet arrives and never gets
        // counted as lost at all, regardless of how much real loss occurs.
        // We have no RTCP feedback session here (no rtpsession/rtprtxreceive
        // upstream of appsrc), so a retransmission request can never actually be
        // answered -- this only switches on the bookkeeping, not real retries.
        jitter.set_property("do-retransmission", true);
        // Give up fast instead of GStreamer's "-1 automatic" RTT-based timeout,
        // which has no real RTCP RTT to estimate from in this setup.
        jitter.set_property("rtx-retry-period", 40i32);
        jitter.set_property("rtx-max-retries", 0i32);

        let depay = gst::ElementFactory::make("rtph264depay").build()?;

        let parse = gst::ElementFactory::make("h264parse").build()?;
        parse.set_property_from_str("config-interval", "-1");
        parse.set_property_from_str("update-timecode", "false"); // NOTE: may is wrong

        let decoder = choose_h264_decoder();
        *state.stats.decoder_name.lock() = decoder;
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
                    state_clone2
                        .stats
                        .note_decode_latency(t.elapsed().as_secs_f64() * 1000f64);
                }
                state_clone2.stats.note_frame();
                gst::PadProbeReturn::Ok
            });

        let srcpad = jitter.static_pad("src").unwrap();
        let state_clone3 = state.clone();
        srcpad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(event) = &info.data {
                if let gst::PadProbeData::Event(ev) = event {
                    if ev.type_() == gst::EventType::CustomDownstream {
                // A lost packet breaks the P-frame chain: everything after it
                // decodes against a reference the encoder and decoder no longer
                // agree on. Only an IDR resets that.
                        state_clone3.request_keyframe();
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });

        let jitter_clone = jitter.clone();
        let state_clone = state.clone();
        tokio::spawn(async move {
            // Polled fast because this drives keyframe recovery, not just the
            // stats overlay: every tick spent here is time the picture stays
            // broken after a loss.
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;

                // Also drives the fps/received-bitrate rate counters: they only
                // actually recompute once a second has passed, so polling them
                // here more often than that is free.
                state_clone.stats.tick_rates();

                let jitter_stats = jitter_clone.property::<gst::Structure>("stats");
                // num-lost: sequence numbers that never arrived and were given
                // up on (needs `do-retransmission` above to be tracked at all).
                // num-late: packets that did arrive, just after the
                // jitterbuffer's playout deadline, so they were dropped anyway
                // -- a different failure mode (buffer too tight vs. real loss).
                let num_lost = jitter_stats.get::<u64>("num-lost").unwrap_or(0);
                let num_late = jitter_stats.get::<u64>("num-late").unwrap_or(0);
                let num_pushed = jitter_stats.get::<u64>("num-pushed").unwrap_or(0);
                let num_duplicates = jitter_stats.get::<u64>("num-duplicates").unwrap_or(0);
                let avg_jitter = jitter_stats.get::<u64>("avg-jitter").unwrap_or(0);
                let loss_pct = if num_pushed > 0 {
                    num_lost as f64 / (num_lost + num_pushed) as f64 * 100.0
                } else {
                    0.0
                };

                *state_clone.stats.loss.lock() = PacketLoss {
                    num_lost,
                    num_late,
                    num_pushed,
                    num_duplicates,
                    avg_jitter,
                    loss_pct,
                };
            }
        });

        match src.dynamic_cast::<gst_app::AppSrc>() {
            Ok(appsrc_casted) => Ok((pipeline, appsrc_casted)),
            Err(_) => anyhow::bail!("Dynamic cast of appsrc failed"),
        }
    }

    async fn run(state: Arc<AppState>, mode: VideoMode) -> Result<()> {
        let VideoMode::RawRtpUdp { video_port: port } = mode;

        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let subscribe_addr = format!("{}:{port}", state.server.ip());
        debug!("Send to {subscribe_addr}");
        socket.send_to(&[1], &subscribe_addr).await?;

        // Re-sent periodically rather than once: the server's pipesocket
        // only remembers our address for as long as it keeps receiving
        // packets, and NAT rebinding on our end can silently invalidate the
        // mapping it learned. Without this a stale subscription would go
        // unnoticed until the app is restarted.
        let mut resubscribe = tokio::time::interval(Duration::from_secs(2));
        resubscribe.tick().await; // first tick fires immediately; we already subscribed above

        let mut buffer = [0u8; 1 << 18];
        loop {
            tokio::select! {
                _ = resubscribe.tick() => {
                    if let Err(e) = socket.send_to(&[1], &subscribe_addr).await {
                        warn!(error = ?e, "video resubscribe failed");
                    }
                }
                result = socket.recv_from(&mut buffer) => {
                    let (size, _) = match result {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(error = ?e, "video recv failed, dropping packet");
                            continue;
                        }
                    };
                    state.stats.note_bytes(size);

                    let pushed = (|| -> Result<()> {
                        let mut gst_buffer = gst::Buffer::with_size(size)
                            .context("failed to allocate gst buffer")?;
                        gst_buffer
                            .get_mut()
                            .context("gst buffer unexpectedly shared")?
                            .copy_from_slice(0, &buffer[..size])
                            .map_err(|_| anyhow::anyhow!("buffer too small for received packet"))?;

                        if let Some(appsrc) = state.appsrc.lock().as_mut() {
                            appsrc.push_buffer(gst_buffer)?;
                        }
                        Ok(())
                    })();

                    // A single corrupted/oversized packet or a transient
                    // pipeline hiccup must not kill the receive loop -- that
                    // would mean permanent video loss for the rest of the
                    // session (see docs/latency-optimization.md).
                    if let Err(e) = pushed {
                        warn!(error = ?e, "dropping video packet");
                    }
                }
            }
        }
    }
}
