use std::io::Result;
use std::process::{Child, Command, Stdio};
use tracing::{debug, info};

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
        // --profile baseline // but make freeze pipeline ?

        let mut video = Command::new("./rpicam_stream/build/streamer")
            .args(["--host", "37.59.106.4", "--port", &format!("{port}")])
            .spawn()?;

        info!("Camera video started.");
        Ok(video)
    }

    // fn start_audio() -> Result<Child> {
    //     Ok()
    // }
}
