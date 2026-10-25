use anyhow::{Context, Result};
use egui::ColorImage;
use gstreamer::Registry;
use gstreamer::glib::object::{Cast, ObjectExt};
use gstreamer::prelude::*;
use gstreamer::{self as gst};
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tracing::{info, warn};

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

/// Converts a `video/x-raw,format=RGBA` buffer straight into the `ColorImage`
/// egui paints, accounting for row stride (GStreamer often pads rows wider
/// than `width * 4` for alignment).
///
/// Builds the `Color32` vector in one pass rather than copying into an
/// intermediate tightly-packed `Vec<u8>` and converting that: this runs on the
/// pipeline's streaming thread once per frame, and at 60fps a second full-frame
/// pass plus its 1.4MB allocation was enough to push that thread past the frame
/// period (see the `queue` in `build_pipeline` for why that matters). Decoder
/// output is opaque, so `from_rgb` is equivalent to `from_rgba_unmultiplied`
/// with alpha 255 and skips the per-pixel premultiply.
fn frame_to_color_image(buffer: &gst::BufferRef, caps: &gst::CapsRef) -> Option<ColorImage> {
    let info = gst_video::VideoInfo::from_caps(caps).ok()?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;

    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0).ok()?;

    let mut pixels = Vec::with_capacity(width * height);
    for y in 0..height {
        let start = y * stride;
        let row = data.get(start..start + width * 4)?;
        pixels.extend(
            row.chunks_exact(4)
                .map(|p| egui::Color32::from_rgb(p[0], p[1], p[2])),
        );
    }

    Some(ColorImage {
        size: [width, height],
        pixels,
    })
}

/// Averages a strided subset of an RGBA buffer's pixels into an RGB color.
/// Used only by the latency test (see `latency_test.rs`): a solid-color
/// frame doesn't need every pixel sampled to get an accurate average, so
/// this stays cheap even at 720p and up.
fn sample_average_color(buffer: &gst::BufferRef, caps: &gst::CapsRef) -> Option<[u8; 3]> {
    const STEP: usize = 8;

    let info = gst_video::VideoInfo::from_caps(caps).ok()?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;

    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0).ok()?;

    let (mut r_sum, mut g_sum, mut b_sum, mut n) = (0u64, 0u64, 0u64, 0u64);
    let mut y = 0;
    while y < height {
        let Some(row) = data.get(y * stride..) else {
            break;
        };
        let mut x = 0;
        while x * 4 + 2 < row.len() && x < width {
            let px = x * 4;
            r_sum += row[px] as u64;
            g_sum += row[px + 1] as u64;
            b_sum += row[px + 2] as u64;
            n += 1;
            x += STEP;
        }
        y += STEP;
    }

    (n > 0).then_some([(r_sum / n) as u8, (g_sum / n) as u8, (b_sum / n) as u8])
}

/// How the client decodes incoming video. Only one implementation exists
/// today (`RtpVideo`, below); a future one (e.g. WebRTC) is a new impl of this
/// trait, not a rewrite of `ui.rs`/`app.rs`'s call sites.
pub trait VideoReceiver {
    /// Builds the pipeline and starts it decoding into `state.latest_frame`
    /// (picked up by `ui.rs`'s `update()` loop each repaint). Called once at
    /// app creation, before the server hands over a transport config.
    fn build_pipeline(state: Arc<AppState>) -> Result<(gst::Pipeline, gst_app::AppSrc)>;

    /// Feeds one RTP packet, already stripped of its data-plane channel tag,
    /// into the pipeline. Called from the transport's receive thread for every
    /// video packet, whichever path it arrived on. Per-packet errors are
    /// logged and skipped, never propagated: one corrupt packet must not be
    /// able to end video for the session.
    fn push_rtp(state: &AppState, packet: &[u8]);
}

/// Payload type carrying `rtpulpfecenc`/`rtpulpfecdec` FEC packets, distinct
/// from the video payload (96, set by `rtph264pay` in streamer.cpp) so both
/// can share one RTP flow/SSRC. Must match the `pt` the streamer's
/// `rtpulpfecenc` is configured with.
const FEC_PT: u32 = 122;

/// Fixed SSRC configured on the streamer's rtph264pay. Using a fixed value
/// lets us set the correct SSRC in caps before receiving any packets, which
/// is required for rtpulpfecdec to work correctly.
const VIDEO_SSRC: u32 = 0x12345678;

/// Raw RTP packets pushed into an `appsrc`, decoded by a `rtpstorage` ->
/// `rtpjitterbuffer` -> `rtpulpfecdec` -> depay -> parse -> decode -> convert
/// chain, and handed to an `appsink` that copies each frame into
/// `state.latest_frame` for `ui.rs` to paint.
///
/// Where the packets came from is not this module's business: the transport
/// picks the path and hands over payloads, so a mid-session switch between a
/// direct and a relayed path is invisible here.
pub struct RtpVideo;

impl VideoReceiver for RtpVideo {
    // for NVDEC: need 4:2:0, not 4:4:4
    fn build_pipeline(state: Arc<AppState>) -> Result<(gst::Pipeline, gst_app::AppSrc)> {
        // No `payload` field: FEC packets (see rtpulpfecenc/dec below) share
        // this flow with a different payload type (122) than the video
        // payload (96). The actual per-packet PT lives in the RTP header and
        // is read from there by rtpjitterbuffer/rtpulpfecdec regardless of
        // what caps say; pinning `payload` here would just be a stale/wrong
        // hint for the FEC packets.
        //
        // Fixed SSRC matching the streamer's rtph264pay configuration. This
        // lets rtpulpfecdec correctly match FEC packets with media packets.
        // Without the correct SSRC, FEC recovery silently fails.
        let caps = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", "H264")
            .field("clock-rate", 90000i32)
            .field("ssrc", VIDEO_SSRC)
            .build();

        let pipeline = gst::Pipeline::default();
        let src = gst::ElementFactory::make("appsrc").name("source").build()?;
        src.set_property("caps", &caps);
        src.set_property("is-live", true);
        src.set_property("format", gst::Format::Time);
        // Bound the internal queue and drop the *oldest* packets on overflow
        // ("downstream" leak direction). 256 packets is generous enough to
        // never bite in normal operation, small enough that a stalled decoder
        // can't accumulate seconds of stale video. Late/missing packets past
        // the jitterbuffer deadline are dropped there anyway.
        src.set_property("max-buffers", 256u64);
        src.set_property("max-bytes", 0u64);
        src.set_property_from_str("leaky-type", "downstream");
        // Required: rtpjitterbuffer uses buffer PTS as packet *arrival* time
        // for skew/jitter estimation (media timing comes from the RTP header
        // timestamps, not from PTS). Cost: tokio scheduling delay between
        // kernel receipt and recv_from() return is indistinguishable from
        // network jitter. Accepted -- the fix, if avg-jitter ever shows it
        // matters, is kernel receive timestamps (SO_TIMESTAMPNS) mapped onto
        // the pipeline clock, not removing do-timestamp.
        src.set_property("do-timestamp", true);

        // Keeps a rolling window of raw RTP packets (media + FEC) so
        // `rtpulpfecdec` below can reach back and recover a lost one. Must sit
        // upstream of the jitterbuffer (element doc requirement). 350ms is
        // comfortably more than 3x the jitterbuffer's own 100ms latency, so the
        // packets needed for a recovery are still in the window by the time
        // the jitterbuffer's lost-event fires at its playout deadline.
        let storage = gst::ElementFactory::make("rtpstorage").build()?;
        storage.set_property("size-time", 250_000_000u64);

        let jitter = gst::ElementFactory::make("rtpjitterbuffer").build()?;
        // 100ms latency buffer: tolerates network jitter during high-motion
        // scenes where frames are large and packets may arrive out of order.
        // Lower values cause late-packet drops that break decoding.
        jitter.set_property("latency", 50u32);
        jitter.set_property("drop-on-latency", true);
        jitter.set_property("do-lost", true);
        // No RTX: there is no feedback session, so a retransmission request can
        // never be answered, and the RTX timers only add up to rtx-retry-period
        // of gap-hold delay on every loss. With do-lost=true the jitterbuffer
        // still fires its lost timer at the playout deadline (latency-bound),
        // which is what emits GstRTPPacketLost -- used below by the FEC decoder
        // and the keyframe-recovery probe. Real loss accounting for the
        // adaptive bitrate/FEC loop doesn't depend on this at all -- see the
        // RTP sequence-number tracking in `run()`.
        jitter.set_property("do-retransmission", false);

        // Attempts to recover a lost packet from `storage` the moment the
        // jitterbuffer's lost event reaches it; on success it injects the
        // reconstructed packet and swallows the event before it reaches the
        // keyframe-recovery probe below, on failure it forwards the event
        // unchanged. `storage` takes the internal storage *object*, not the
        // `rtpstorage` element itself.
        let fecdec = gst::ElementFactory::make("rtpulpfecdec").build()?;
        fecdec.set_property("pt", FEC_PT);
        let internal_storage = storage.property::<gstreamer::glib::Object>("internal-storage");
        fecdec.set_property("storage", &internal_storage);

        // rtpulpfecdec passes ALL packets through (including FEC packets) -
        // it only uses FEC packets for recovery but doesn't filter them out.
        // Drop FEC packets here before they reach rtph264depay which can't
        // handle them (causes "Undefined packet type" warnings).
        fecdec.static_pad("src").unwrap().add_probe(
            gst::PadProbeType::BUFFER,
            move |_, probe_info| {
                if let Some(buffer) = probe_info.buffer() {
                    // RTP payload type is in byte 1, lower 7 bits
                    if let Ok(map) = buffer.map_readable() {
                        if map.len() >= 2 {
                            let pt = map[1] & 0x7F;
                            if pt == FEC_PT as u8 {
                                return gst::PadProbeReturn::Drop;
                            }
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );

        let depay = gst::ElementFactory::make("rtph264depay").build()?;

        let parse = gst::ElementFactory::make("h264parse").build()?;
        parse.set_property_from_str("config-interval", "-1");
        parse.set_property_from_str("update-timecode", "false");

        // Thread boundary. Without this every element below runs on the
        // jitterbuffer's own push task, so the hardware-memory download in
        // `conv` and the per-frame image build in the appsink callback all
        // execute on the one thread that also has to drain the jitterbuffer on
        // schedule. At 30fps that fits in the 33ms frame period; at 60fps it
        // does not, and because `drop-on-latency` is set the jitterbuffer's
        // response to its own thread being busy is to *discard packets* --
        // producing corrupt AUs and motion artifacts while `windowed_loss_pct`
        // (measured on the wire, in `run()`) still reads 0%.
        //
        // Not leaky: dropping an encoded AU here would break the P-frame
        // reference chain exactly like the packet loss this is meant to
        // prevent, and it wouldn't be visible to the keyframe-recovery probe
        // either. A consumer that falls behind should cost bounded latency,
        // which is observable, rather than silent corruption.
        let queue = gst::ElementFactory::make("queue").build()?;
        queue.set_property("max-size-buffers", 8u32);
        queue.set_property("max-size-bytes", 0u32);
        queue.set_property("max-size-time", 0u64);

        let decoder = choose_h264_decoder();
        *state.stats.decoder_name.lock() = decoder;
        let dec = gst::ElementFactory::make(decoder).build()?;
        if decoder == "avdec_h264" {
            dec.set_property_from_str("lowres", "0");
        }

        let conv = gst::ElementFactory::make("videoconvert").build()?;
        // Defaults to 1. This element does the decoder-memory download and the
        // conversion to RGBA; single-threaded it is the widest step on the
        // frame path at 60fps.
        conv.set_property("n-threads", 4u32);

        let caps_filter = gst::ElementFactory::make("capsfilter").build()?;
        caps_filter.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .build(),
        );

        let appsink_elem = gst::ElementFactory::make("appsink").build()?;
        appsink_elem.set_property("sync", false);
        // Live display: always show the newest frame, drop anything older
        // that piles up rather than presenting stale video.
        appsink_elem.set_property("max-buffers", 1u32);
        appsink_elem.set_property("drop", true);

        pipeline.add_many([
            &src,
            &storage,
            &jitter,
            &fecdec,
            &depay,
            &parse,
            &queue,
            &dec,
            &conv,
            &caps_filter,
            &appsink_elem,
        ])?;

        gst::Element::link_many([
            &src,
            &storage,
            &jitter,
            &fecdec,
            &depay,
            &parse,
            &queue,
            &dec,
            &conv,
            &caps_filter,
            &appsink_elem,
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
                    state_clone2
                        .stats
                        .note_decode_latency(t.elapsed().as_secs_f64() * 1000f64);
                }
                state_clone2.stats.note_frame();
                gst::PadProbeReturn::Ok
            });

        // Placed on `fecdec`'s src pad, not `jitter`'s: a lost event that FEC
        // recovered is swallowed by `fecdec` and never reaches this point, so
        // an IDR is only requested for loss FEC couldn't fix. Left on the
        // jitterbuffer's pad, every recovered packet would still cost an IDR
        // burst and FEC would buy nothing on the uplink.
        let srcpad = fecdec.static_pad("src").unwrap();
        let state_clone3 = state.clone();
        srcpad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(ev)) = &info.data
                // Only the jitterbuffer's own loss notification, not any
                // custom event that happens to pass this pad. A lost packet
                // breaks the P-frame chain: everything after it decodes
                // against a reference the encoder and decoder no longer agree
                // on. Only an IDR resets that.
                && ev.type_() == gst::EventType::CustomDownstream
                && ev.structure().is_some_and(|s| s.name() == "GstRTPPacketLost")
            {
                state_clone3.request_keyframe();
            }
            gst::PadProbeReturn::Ok
        });

        // Feeds the camera-on-screen latency test (see `latency_test.rs`).
        // Sampled after `caps_filter` rather than off the decoder directly,
        // since that's the one point in the pipeline where the format is
        // guaranteed `RGBA` instead of whatever the decoder happened to
        // negotiate. Bails out before touching the buffer at all when the
        // test isn't running, so this costs nothing during normal driving.
        let state_clone4 = state.clone();
        caps_filter.static_pad("src").unwrap().add_probe(
            gst::PadProbeType::BUFFER,
            move |pad, probe_info| {
                if !state_clone4.latency_test.is_enabled() {
                    return gst::PadProbeReturn::Ok;
                }

                if let Some(rgb) = probe_info
                    .buffer()
                    .zip(pad.current_caps())
                    .and_then(|(buffer, caps)| sample_average_color(buffer, &caps))
                    && let Some(rtt_ms) = state_clone4.latency_test.observe_frame_color(rgb)
                {
                    state_clone4.stats.note_measured_glass_to_glass(rtt_ms);
                }

                gst::PadProbeReturn::Ok
            },
        );

        let jitter_clone = jitter.clone();
        let state_clone = state.clone();
        tokio::spawn(async move {
            // Polled fast because this drives keyframe recovery, not just the
            // stats overlay: every tick spent here is time the picture stays
            // broken after a loss.
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;

                // Also drives the fps/received-bitrate rate counters and the
                // windowed loss ratio: each only actually recomputes once its
                // own window has elapsed, so polling them here more often than
                // that is free.
                state_clone.stats.tick_rates();
                state_clone.stats.tick_loss_window(Duration::from_secs(2));

                let jitter_stats = jitter_clone.property::<gst::Structure>("stats");
                // num-lost/loss_pct below are cumulative jitterbuffer stats,
                // shown in the overlay for reference only -- see
                // `Stats::windowed_loss_pct` for the ratio the adaptive
                // bitrate/FEC loop actually acts on.
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

        let appsink = appsink_elem
            .dynamic_cast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("dynamic cast of appsink failed"))?;

        let state_video = state.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gst::FlowError::Error)?;

                    if let Some(image) = frame_to_color_image(buffer, caps) {
                        *state_video.latest_frame.lock() = Some(image);
                        if let Some(ctx) = state_video.egui_ctx.get() {
                            ctx.request_repaint();
                        }
                    }

                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        match src.dynamic_cast::<gst_app::AppSrc>() {
            Ok(appsrc_casted) => Ok((pipeline, appsrc_casted)),
            Err(_) => anyhow::bail!("Dynamic cast of appsrc failed"),
        }
    }

    fn push_rtp(state: &AppState, packet: &[u8]) {
        state.stats.note_bytes(packet.len());

        // Raw-wire RTP sequence number tracking, independent of
        // `rtpjitterbuffer`'s own stats (see `Stats::note_seq`/
        // `windowed_loss_pct`). Counts gaps across the combined media+FEC
        // flow -- FEC packets share the same per-SSRC sequence-number space
        // as media (RFC 5109), so this is the same loss the jitterbuffer
        // itself would observe, just measured before any pipeline element
        // gets a chance to recover or reorder it.
        if packet.len() >= 4 {
            let seq = u16::from_be_bytes([packet[2], packet[3]]);
            let mut last_seq = state.last_video_seq.lock();
            if let Some(prev) = *last_seq {
                let diff = seq.wrapping_sub(prev);
                // diff == 0: duplicate, not a fresh arrival.
                // diff < 0x8000: forward progress; diff-1 sequence numbers in
                // between never arrived.
                // diff >= 0x8000: seq is behind prev (a reordered or late
                // packet) -- its gap, if any, was already counted when the
                // higher sequence number came in.
                //
                // A path switch does not perturb this: the sequence space is
                // the streamer's, and both paths carry the same one.
                if diff != 0 && diff < 0x8000 {
                    state.stats.note_seq((diff - 1) as u64, diff as u64);
                }
            }
            *last_seq = Some(seq);
        }

        let pushed = (|| -> Result<()> {
            let mut gst_buffer =
                gst::Buffer::with_size(packet.len()).context("failed to allocate gst buffer")?;
            gst_buffer
                .get_mut()
                .context("gst buffer unexpectedly shared")?
                .copy_from_slice(0, packet)
                .map_err(|_| anyhow::anyhow!("buffer too small for received packet"))?;

            if let Some(appsrc) = state.appsrc.lock().as_mut() {
                appsrc.push_buffer(gst_buffer)?;
            }
            Ok(())
        })();

        // A single corrupted/oversized packet or a transient pipeline hiccup
        // must not propagate -- on the transport's receive thread that would
        // mean permanent video loss for the rest of the session (see
        // docs/latency-optimization.md).
        if let Err(e) = pushed {
            warn!(error = ?e, "dropping video packet");
        }
    }
}
