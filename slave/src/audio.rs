use std::io::{Error, ErrorKind, Result};
use std::net::IpAddr;
use std::process::{Child, Command, Stdio};
use tracing::{info, warn};

/// Audio streaming source using GStreamer subprocess.
/// Captures from PulseAudio, encodes with Opus, and sends via RTP.
pub struct AudioSource {
    child: Child,
}

impl AudioSource {
    /// Start the audio capture and streaming pipeline.
    /// Best-effort: returns None if audio setup fails (e.g., no microphone).
    pub fn start(server_ip: IpAddr, audio_port: u16) -> Option<Self> {
        match Self::spawn_pipeline(server_ip, audio_port) {
            Ok(child) => {
                info!("Audio streaming started on port {audio_port}");
                Some(Self { child })
            }
            Err(e) => {
                warn!("Audio setup failed (non-fatal): {e}");
                None
            }
        }
    }

    fn spawn_pipeline(server_ip: IpAddr, audio_port: u16) -> Result<Child> {
        // Pipeline: capture from PulseAudio -> convert -> resample -> Opus encode -> RTP -> UDP
        // - pulsesrc: captures from default PulseAudio source (microphone)
        // - audioconvert/audioresample: ensure correct format for encoder
        // - opusenc: low-latency voice codec
        //   - bitrate=48000: 48kbps is good for voice
        //   - frame-size=10: 10ms frames for low latency
        //   - audio-type=voice: optimize for speech
        // - rtpopuspay: RTP packetization, pt=97 (dynamic payload type)
        // - udpsink: send to server, sync=false for live streaming
        let pipeline = format!(
            "pulsesrc ! audioconvert ! audioresample \
             ! opusenc bitrate=48000 frame-size=10 audio-type=voice \
             ! rtpopuspay pt=97 \
             ! udpsink host={server_ip} port={audio_port} sync=false"
        );

        Command::new("gst-launch-1.0")
            .args(["-q", &pipeline])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::new(ErrorKind::Other, format!("failed to spawn gst-launch-1.0: {e}")))
    }

    /// Stop the audio pipeline gracefully.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for AudioSource {
    fn drop(&mut self) {
        self.stop();
    }
}
