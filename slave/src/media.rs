use std::process::{Child, Command};
use std::io::Result;
use tracing::info;

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
        // rpicam-vid -t 0 \
        //     --flush \
        //     --codec h264 \
        //     --inline \
        //     --width 853 \
        //     --height 480 \
        //     --framerate 60 \
        //     --bitrate 2000000 \
        //     --intra 60 \
        //     -n \
        //     -o udp://37.59.106.4:13001

        // --profile baseline
        // --level 4
        // --intra 30
        let video = Command::new("rpicam-vid")
            .args([
                "-t", "0",
                "--codec", "h264",
                "--inline",
                "--width", "853",
                "--height", "480",
                "--framerate", "60",
                "--bitrate", "2000000",
                "-n", "-o", &format!("server:{port}")
            ])
            .spawn()?;
        info!("Camera video started.");
        Ok(video)
    }

    // fn start_audio() -> Result<Child> {
    //     Ok()
    // }
}
