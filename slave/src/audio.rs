use std::io::{BufRead, BufReader, Error, ErrorKind, Result};
use std::net::IpAddr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::{error, info, warn};

/// A pipeline that stayed up at least this long is treated as having actually
/// worked. Anything shorter is a startup failure (missing plugin, bad device,
/// no PulseAudio access) rather than a mid-flight crash, and is worth a louder
/// log plus a longer backoff -- retrying a misconfiguration every 3s forever
/// just floods the journal.
const HEALTHY_AFTER: Duration = Duration::from_secs(5);
const RESTART_DELAY: Duration = Duration::from_secs(3);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);

/// Audio streaming source using GStreamer subprocess.
/// Captures from PulseAudio, encodes with Opus, and sends via RTP.
///
/// Supervised: the subprocess is watched and restarted if it dies, and its
/// stderr is forwarded to our own logs. Without that, every failure mode of
/// the pipeline (mic unplugged, PulseAudio restarted, plugin missing) is
/// indistinguishable from "the car is silent".
pub struct AudioSource {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl AudioSource {
    /// Start the audio capture and streaming pipeline.
    /// Best-effort: returns None if audio can't be set up at all (e.g. no
    /// GStreamer on the system).
    pub fn start(server_ip: IpAddr, audio_port: u16) -> Option<Self> {
        if let Err(e) = Command::new("gst-launch-1.0")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            warn!("gst-launch-1.0 unavailable, audio disabled (non-fatal): {e}");
            return None;
        }

        let device = resolve_device();
        match &device {
            Some(d) => info!("Audio capture device: {d}"),
            None => info!("Audio capture device: PulseAudio default"),
        }

        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));

        let source = Self {
            stop: stop.clone(),
            child: child.clone(),
        };

        thread::spawn(move || supervise(server_ip, audio_port, device, stop, child));

        info!("Audio streaming started on port {audio_port}");
        Some(source)
    }

    /// Stop the audio pipeline gracefully.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(child) = self.child.lock().as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for AudioSource {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Which PulseAudio source to capture from.
///
/// `AUDIO_DEVICE` wins when set. Otherwise the first non-monitor source
/// reported by `pactl` is used: monitors are loopbacks of the *outputs*, never
/// a microphone, so picking one yields a stream that is technically valid and
/// permanently silent. Falling all the way through to `None` leaves `pulsesrc`
/// to pick the PulseAudio default.
fn resolve_device() -> Option<String> {
    if let Ok(device) = std::env::var("AUDIO_DEVICE") {
        return Some(device);
    }

    // `pactl list sources short` prints one "<index>\t<name>\t<module>..." line
    // per source.
    let output = match Command::new("pactl")
        .args(["list", "sources", "short"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            warn!(
                "pactl failed ({}), falling back to the default source",
                output.status
            );
            return None;
        }
        Err(e) => {
            warn!("pactl unavailable ({e}), falling back to the default source");
            return None;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let device = stdout
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .find(|name| !name.ends_with(".monitor"))
        .map(str::to_string);

    if device.is_none() {
        warn!("No non-monitor PulseAudio source found, falling back to the default source");
    }
    device
}

/// Keeps one `gst-launch-1.0` alive for as long as the `AudioSource` lives.
fn supervise(
    server_ip: IpAddr,
    audio_port: u16,
    device: Option<String>,
    stop: Arc<AtomicBool>,
    child_slot: Arc<Mutex<Option<Child>>>,
) {
    let mut delay = RESTART_DELAY;

    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();

        match spawn_pipeline(server_ip, audio_port, device.as_deref()) {
            Ok(mut child) => {
                // Taken before the child is published so `stop()` can never
                // race us for it; gst-launch blocks on a full stderr pipe, so
                // this has to be drained even when nothing reads the logs.
                if let Some(stderr) = child.stderr.take() {
                    thread::spawn(move || {
                        for line in BufReader::new(stderr)
                            .lines()
                            .map_while(std::result::Result::ok)
                        {
                            warn!("gst-launch(audio): {line}");
                        }
                    });
                }

                *child_slot.lock() = Some(child);
                let status = wait_for_exit(&stop, &child_slot);
                *child_slot.lock() = None;

                if stop.load(Ordering::Relaxed) {
                    return;
                }

                let uptime = started.elapsed();
                if uptime >= HEALTHY_AFTER {
                    warn!("Audio pipeline exited after {uptime:?} ({status:?}), restarting");
                    delay = RESTART_DELAY;
                } else {
                    error!(
                        "Audio pipeline died immediately ({status:?}) -- check the gst-launch \
                         lines above; likely a missing GStreamer plugin \
                         (gstreamer1.0-pulseaudio / -plugins-base / -plugins-good), a device \
                         name PulseAudio doesn't know, or no access to the PulseAudio server"
                    );
                    delay = (delay * 2).min(MAX_RESTART_DELAY);
                }
            }
            Err(e) => {
                error!("Failed to spawn the audio pipeline: {e}");
                delay = (delay * 2).min(MAX_RESTART_DELAY);
            }
        }

        // Slept in slices so `stop()` doesn't have to wait out a full backoff.
        let deadline = Instant::now() + delay;
        while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Polls rather than `Child::wait()` so `stop()` keeps access to the handle it
/// needs to kill the process.
fn wait_for_exit(
    stop: &AtomicBool,
    child_slot: &Mutex<Option<Child>>,
) -> Option<std::process::ExitStatus> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return None;
        }

        let exited = {
            let mut guard = child_slot.lock();
            match guard.as_mut() {
                Some(child) => child.try_wait().ok().flatten(),
                // `stop()` reaped it out from under us.
                None => return None,
            }
        };

        if exited.is_some() {
            return exited;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn spawn_pipeline(server_ip: IpAddr, audio_port: u16, device: Option<&str>) -> Result<Child> {
    // Pipeline: capture from PulseAudio -> convert -> resample -> Opus encode -> RTP -> UDP
    // - pulsesrc: captures from PulseAudio source (microphone), see `resolve_device`
    // - audioconvert/audioresample: ensure correct format for encoder
    // - opusenc: low-latency voice codec
    //   - bitrate=48000: 48kbps is good for voice
    //   - frame-size=10: 10ms frames for low latency
    //   - audio-type=voice: optimize for speech
    // - rtpopuspay: RTP packetization, pt=97 (dynamic payload type)
    // - udpsink: send to server, sync=false for live streaming
    //
    // Quoted device name: gst-launch tokenizes the pipeline string itself, so
    // an unquoted name containing a space would be parsed as a new element.
    let src = match device {
        Some(device) => format!("pulsesrc device=\"{device}\""),
        None => "pulsesrc".to_string(),
    };

    let pipeline = format!(
        "{src} ! audioconvert ! audioresample \
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
        .map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("failed to spawn gst-launch-1.0: {e}"),
            )
        })
}
