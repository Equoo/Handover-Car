use std::io::{BufRead, BufReader, Error, ErrorKind, Result};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{debug, info, warn};

pub struct Media {
    video: Child,
    // audio: Child
}

impl Media {
    pub fn start_stream(video_port: u16, _audio_port: u16) -> Result<Self> {
        Ok(Self {
            video: Media::start_video(video_port)?,
        })
    }

    fn start_video(port: u16) -> Result<Child> {
        let mut video = Command::new("./rpicam_stream/build/streamer")
            .args(["--host", "37.59.106.4", "--port", &format!("{port}")])
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
                        debug!(target: "streamer_stdout", "{line}");
                        if line.contains("[CTRL] Ready") {
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(());
                            }
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
