use std::process::{Child, Command, Stdio};
use std::io::Result;
use tracing::{debug, info};

pub struct Media {
    video: Child,
    // audio: Child
}

impl Media {
    pub fn start_stream(video_port: u16, _audio_port: u16) -> Result<Self> {
        Ok(Self {
           video: Media::start_video(video_port)?
        })
    }

    fn start_video(port: u16) -> Result<Child> {
        // --profile baseline // but make freeze pipeline ?

        let mut rpicam = Command::new("rpicam-vid")
            .args([
                "-t", "0",
                "--codec", "h264",
                "--inline",
                "--level", "4.1",
                "--intra", "10",
                "--width", "720",
                "--height", "480",
                "--framerate", "60",
                "--bitrate", "500000",
                "-o", "-", // stdout
            ])
            .stdout(Stdio::piped())
            .spawn()?;

        // Get rpicam stdout
        let rpicam_stdout = rpicam.stdout.take()
            .expect("Failed to capture rpicam stdout");

        // Spawn gst-launch-1.0
        let video = Command::new("gst-launch-1.0")
            .arg("-q")
            .args([
                "fdsrc",
                "!", "h264parse",
                "!", "rtph264pay", "config-interval=-1", "pt=96", "mtu=1200",
                "!", "udpsink", "host=37.59.106.4", &format!("port={port}"),
            ])
            .stdin(Stdio::from(rpicam_stdout))
            .spawn()?;

        info!("Camera video started.");
        Ok(video)
    }

    // fn start_audio() -> Result<Child> {
    //     Ok()
    // }
}
