use std::io::{BufRead, BufReader, Error, ErrorKind, Result};
use std::net::IpAddr;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{debug, info, warn};

use common::VideoMode;

/// One `[LATENCY]` line parsed from the streamer's stdout, printed every ~2s.
#[derive(Debug, Clone, Copy)]
pub struct StreamerStats {
    pub capture_latency_ms: f32,
    pub encode_latency_ms: f32,
    pub total_latency_ms: f32,
    pub bitrate_bps: u32,
}

/// Where slave-side video production goes for a given `VideoMode` -- the
/// server negotiates the mode, `slave/src/main.rs` only ever calls through
/// this trait, and the concrete backend is picked once at `start`. Only one
/// backend exists today (`RawRtpUdp`, matching `VideoMode::RawRtpUdp`); a
/// future one (e.g. WebRTC) is a new impl, not a rewrite of the call sites.
pub trait VideoSource: Sized {
    fn start(
        mode: &VideoMode,
        server_ip: IpAddr,
        on_stats: impl Fn(StreamerStats) + Send + 'static,
    ) -> Result<Self>;

    /// Ask the encoder for an IDR keyframe.
    async fn request_keyframe(&self) -> Result<()>;

    /// Retune the encoder's target bitrate.
    async fn set_bitrate(&self, bps: u32) -> Result<()>;
}

/// Parses lines shaped like:
/// `[LATENCY]  capture=1.234 ms  encode=5.678 ms  total=6.912 ms  bitrate=1000 kbps`
fn parse_latency_line(line: &str) -> Option<StreamerStats> {
    if !line.starts_with("[LATENCY]") {
        return None;
    }
    Some(StreamerStats {
        capture_latency_ms: extract_after(line, "capture=")?,
        encode_latency_ms: extract_after(line, "encode=")?,
        total_latency_ms: extract_after(line, "total=")?,
        bitrate_bps: (extract_after(line, "bitrate=")? * 1000.0) as u32,
    })
}

fn extract_after(line: &str, marker: &str) -> Option<f32> {
    let start = line.find(marker)? + marker.len();
    let rest = &line[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    rest[..end].parse::<f32>().ok()
}

/// Video over raw RTP packets sent directly to the server's relay port --
/// exec's the C++ streamer, which talks straight to the Pi's hardware H.264
/// encoder. This is `VideoMode::RawRtpUdp`'s slave-side implementation.
pub struct RawRtpUdp {
    video: Child,
}

impl VideoSource for RawRtpUdp {
    fn start(
        mode: &VideoMode,
        server_ip: IpAddr,
        on_stats: impl Fn(StreamerStats) + Send + 'static,
    ) -> Result<Self> {
        let video_port = match mode {
            VideoMode::RawRtpUdp { video_port } => *video_port,
        };

        Ok(Self {
            video: Self::start_video(server_ip, video_port, on_stats)?,
        })
    }

    async fn request_keyframe(&self) -> Result<()> {
        tokio::fs::write("/tmp/streamer_ctrl", "idr\n").await
    }

    async fn set_bitrate(&self, bps: u32) -> Result<()> {
        tokio::fs::write("/tmp/streamer_ctrl", format!("{bps}\n")).await
    }
}

impl RawRtpUdp {
    fn start_video(
        host: IpAddr,
        port: u16,
        on_stats: impl Fn(StreamerStats) + Send + 'static,
    ) -> Result<Child> {
        let mut video = Command::new("./rpicam_stream/build/streamer")
            .args(["--host", &host.to_string(), "--port", &format!("{port}")])
            .stdout(Stdio::piped())
            .spawn()?;

        let stdout = video
            .stdout
            .take()
            .ok_or_else(|| Error::new(ErrorKind::Other, "failed to capture streamer stdout"))?;

        let (ready_tx, ready_rx) = mpsc::channel::<()>();

        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            let mut ready_tx = Some(ready_tx);

            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        // debug!(target: "streamer_stdout", "{line}");
                        if line.contains("[CTRL] Ready") {
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(());
                            }
                        } else if let Some(stats) = parse_latency_line(&line) {
                            on_stats(stats);
                        }
                        // on continue à lire même après Ready pour ne pas bloquer
                        // le pipe si le process continue d'écrire sur stdout
                    }
                    Err(e) => {
                        warn!("error reading streamer stdout: {e}");
                        break;
                    }
                }
            }
        });

        // Attend le signal de readiness, avec timeout pour ne pas bloquer
        // indéfiniment si le process crash avant d'afficher la ligne
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(()) => {
                info!("Camera video started.");
                Ok(video)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = video.kill();
                Err(Error::new(
                    ErrorKind::TimedOut,
                    "timed out waiting for streamer [CTRL] Ready",
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = video.kill();
                Err(Error::new(
                    ErrorKind::Other,
                    "streamer process exited before signaling ready",
                ))
            }
        }
    }
}
