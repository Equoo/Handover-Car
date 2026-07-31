use anyhow::{Context, Result};
use gstreamer::prelude::*;
use gstreamer::{self as gst};
use gstreamer_app as gst_app;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

/// Audio receiver state. Manages the GStreamer pipeline that decodes
/// incoming Opus audio and plays it through the system audio output.
pub struct AudioState {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
}

impl AudioState {
    /// Build the audio pipeline. Returns None if GStreamer audio elements
    /// are unavailable (best-effort: audio is optional).
    pub fn new() -> Option<Self> {
        match Self::build_pipeline() {
            Ok((pipeline, appsrc)) => {
                if let Err(e) = pipeline.set_state(gst::State::Playing) {
                    warn!("Failed to start audio pipeline: {e:?}");
                    return None;
                }
                info!("Audio pipeline initialized");
                Some(Self { pipeline, appsrc })
            }
            Err(e) => {
                warn!("Audio pipeline build failed (non-fatal): {e:?}");
                None
            }
        }
    }

    fn build_pipeline() -> Result<(gst::Pipeline, gst_app::AppSrc)> {
        // Caps for Opus RTP stream
        let caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "OPUS")
            .field("clock-rate", 48000i32)
            .field("payload", 97i32)
            .build();

        let pipeline = gst::Pipeline::default();

        let src = gst::ElementFactory::make("appsrc").name("audiosource").build()?;
        src.set_property("caps", &caps);
        src.set_property("is-live", true);
        src.set_property("format", gst::Format::Time);
        src.set_property("do-timestamp", true);
        // Small buffer for live audio
        src.set_property("max-buffers", 64u64);
        src.set_property("max-bytes", 0u64);
        src.set_property_from_str("leaky-type", "downstream");

        let jitter = gst::ElementFactory::make("rtpjitterbuffer").build()?;
        jitter.set_property("latency", 50u32);
        jitter.set_property("drop-on-latency", true);

        let depay = gst::ElementFactory::make("rtpopusdepay").build()?;

        let dec = gst::ElementFactory::make("opusdec").build()?;
        // Enable packet loss concealment for smoother audio under loss
        dec.set_property("plc", true);

        let conv = gst::ElementFactory::make("audioconvert").build()?;
        let resample = gst::ElementFactory::make("audioresample").build()?;

        let sink = gst::ElementFactory::make("autoaudiosink").build()?;
        sink.set_property("sync", false);

        pipeline.add_many([&src, &jitter, &depay, &dec, &conv, &resample, &sink])?;
        gst::Element::link_many([&src, &jitter, &depay, &dec, &conv, &resample, &sink])?;

        let appsrc = src
            .dynamic_cast::<gst_app::AppSrc>()
            .map_err(|_| anyhow::anyhow!("dynamic cast of appsrc failed"))?;

        Ok((pipeline, appsrc))
    }

    /// Push an RTP packet into the audio pipeline.
    pub fn push_packet(&self, data: &[u8]) -> Result<()> {
        let mut buffer = gst::Buffer::with_size(data.len())
            .context("failed to allocate gst buffer")?;
        buffer
            .get_mut()
            .context("gst buffer unexpectedly shared")?
            .copy_from_slice(0, data)
            .map_err(|_| anyhow::anyhow!("buffer too small for packet"))?;

        self.appsrc.push_buffer(buffer)?;
        Ok(())
    }
}

impl Drop for AudioState {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Run the audio receiver loop. Connects to the server's audio pipesocket
/// and pushes incoming RTP packets to the audio pipeline.
pub async fn run_audio_receiver(
    audio: Arc<AudioState>,
    server_ip: IpAddr,
    audio_port: u16,
) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let subscribe_addr = format!("{server_ip}:{audio_port}");
    debug!("Subscribing to audio at {subscribe_addr}");
    socket.send_to(&[1], &subscribe_addr).await?;

    // Resubscribe periodically like video does
    let mut resubscribe = tokio::time::interval(Duration::from_secs(2));
    resubscribe.tick().await;

    let mut buffer = [0u8; 2048]; // Opus packets are small
    loop {
        tokio::select! {
            _ = resubscribe.tick() => {
                if let Err(e) = socket.send_to(&[1], &subscribe_addr).await {
                    warn!(error = ?e, "audio resubscribe failed");
                }
            }
            result = socket.recv_from(&mut buffer) => {
                let (size, _) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = ?e, "audio recv failed, dropping packet");
                        continue;
                    }
                };

                // Best-effort: log errors but don't kill the loop
                if let Err(e) = audio.push_packet(&buffer[..size]) {
                    warn!(error = ?e, "dropping audio packet");
                }
            }
        }
    }
}
