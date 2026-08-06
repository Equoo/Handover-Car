//! Loopback intake for the two local RTP producers -- the camera streamer
//! subprocess and the audio `gst-launch` -- feeding whatever path the
//! transport currently has selected.
//!
//! Both producers are GStreamer pipelines ending in a `udpsink`, which owns
//! its own socket and picks its own source port. Letting them send straight
//! out to the internet would mean their packets leave from a 5-tuple that
//! never punched anything, so they would be dropped by the peer's NAT on any
//! path except the relay. Bouncing them through the transport's one socket
//! instead is what lets a single punched mapping carry video, audio and pilot
//! input at once -- and it makes a path switch instantaneous, since it changes
//! a destination in this process rather than reconfiguring a running encoder.
//!
//! The cost is one extra copy across loopback per packet, tens of
//! microseconds against a link measured in tens of milliseconds.

use std::io::Result;
use std::net::UdpSocket;
use std::sync::Arc;

use common::transport::Transport;
use tracing::{debug, warn};

/// The streamer's `rtph264pay` is set to a 1200-byte MTU and Opus frames are
/// far smaller, so nothing legitimate comes close to this. Sized for headroom
/// rather than for the expected case: a truncated RTP packet is a corrupt
/// frame at the far end, which is much harder to diagnose than a big buffer.
const MAX_DATAGRAM: usize = 4096;

/// Binds a loopback socket, hands back the port for the producer to send to,
/// and forwards everything that arrives onto `channel` of the data plane.
pub fn spawn(name: &'static str, channel: u8, transport: Arc<Transport>) -> Result<u16> {
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    let port = socket.local_addr()?.port();

    std::thread::Builder::new()
        .name(format!("pump-{name}"))
        .spawn(move || {
            let mut buf = [0u8; MAX_DATAGRAM];
            loop {
                let size = match socket.recv(&mut buf) {
                    Ok(size) => size,
                    Err(e) => {
                        warn!(error = ?e, "{name} pump recv failed, dropping packet");
                        continue;
                    }
                };
                // Logged, never fatal: a send failure here is one lost RTP
                // packet on a link that already tolerates loss, and the pump
                // dying would take the whole stream with it for good.
                if let Err(e) = transport.send(channel, &buf[..size]) {
                    debug!(error = ?e, "{name} pump send failed, dropping packet");
                }
            }
        })?;

    debug!("{name} pump listening on 127.0.0.1:{port}");
    Ok(port)
}
