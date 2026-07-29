use rkyv::{Archive, Deserialize, Serialize, rancor::Error};
use std::net::SocketAddr;
use crossbeam_channel::{Receiver, Sender};
use anyhow::{Context, Result};

pub mod asyncqueue;
pub mod link;

#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum CommonError {
    SlaveLimit,
    SlaveNotFound
}

/// Steering/throttle or a keyframe request. The server never inspects this,
/// only forwards it verbatim from the driver's client to the car -- so it's
/// shared by both `ClientMessage` (what the server receives) and
/// `ServerToSlave` (what the server re-sends).
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum PilotCommand {
    Input {
        throttle: i16,
        steering: i16,
    },
    /// Ask the slave's encoder for an IDR. With a long GOP nothing else
    /// produces one, so a receiver that has lost sync stays corrupted until
    /// this arrives -- hence its own variant rather than folded into the
    /// periodic input message.
    KeyframeRequest,
}

/// How video gets from a car to a client, negotiated through the connect
/// handshake rather than assumed by each binary independently. Only one mode
/// is implemented today; this is the seam a second one (e.g. WebRTC) would
/// plug into without touching the handshake shape again.
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq, Clone, Copy)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum VideoMode {
    /// Raw RTP packets sent directly to the server's per-slave relay port,
    /// which blindly forwards them to whichever client last subscribed.
    RawRtpUdp { video_port: u16 },
}

/// Sent by a car (slave) to the server.
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum SlaveMessage {
    Announce {
        id: String,
        latency: u16,
        packet_id: u16,
    },
    Error {
        id: String,
        err: String,
    },
    Location {
        id: String,
        latitude: f32,
        altitude: f32,
    },
    /// Encoder-side pipeline stats parsed from the streamer's own stdout
    /// (capture/encode timings + the bitrate it's actually applying).
    Stats {
        id: String,
        capture_latency_ms: f32,
        encode_latency_ms: f32,
        total_latency_ms: f32,
        bitrate_bps: u32,
    },
}

/// Sent by a driver's client to the server.
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum ClientMessage {
    Subscribe {
        slave_id: String,
    },
    Announce {
        uuid: String,
        packet_id: u16,
        bitrate: u32,
    },
    Pilot {
        slave_id: String,
        cmd: PilotCommand,
    },
}

/// Sent by the server to a car (slave).
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum ServerToSlave {
    Connected {
        video: VideoMode,
        audio_port: u16,
    },
    Error {
        err: CommonError,
    },
    Pong {
        packet_id: u16,
        bitrate: u32,
    },
    Pilot(PilotCommand),
}

/// Sent by the server to a driver's client.
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum ServerToClient {
    Connected {
        uuid: String,
        video: VideoMode,
        audio_port: u16,
    },
    Pong {
        latency: u16,
        packet_id: u16,
    },
    SlaveStats {
        capture_latency_ms: f32,
        encode_latency_ms: f32,
        total_latency_ms: f32,
        bitrate_bps: u32,
    },
}

#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum Message {
    Slave(SlaveMessage),
    Client(ClientMessage),
    ToSlave(ServerToSlave),
    ToClient(ServerToClient),
}

pub type Packet = laminar::Packet;
pub type SocketEvent = laminar::SocketEvent;

pub enum PacketType {
    Reliable,
    Unreliable,
}

pub struct Socket {
    sender: Sender<Packet>,
    receiver: Receiver<SocketEvent>
}

impl Socket {
    pub fn start(port: u16) -> Result<Self> {
        let mut socket = laminar::Socket::bind(format!("0.0.0.0:{port}"))?;
        let sender = socket.get_packet_sender();
        let receiver = socket.get_event_receiver();
        let _thread = std::thread::spawn(move || socket.start_polling());

        Ok(Self {
            sender,
            receiver
        })
    }

    pub fn send(&self, addr: &SocketAddr, msg: &Message, typ: PacketType) -> Result<()> {
        let bytes = rkyv::to_bytes::<Error>(msg).context("failed to serialize")?;
        let packet = match typ {
            PacketType::Reliable => Packet::reliable_unordered(*addr, bytes.into()),
            PacketType::Unreliable => Packet::unreliable(*addr, bytes.into())
        };
        self.sender.send(packet)?;
        Ok(())
    }

    pub async fn recv(&self) -> Result<SocketEvent> {
        let receiver = self.receiver.clone();
        let event = tokio::task::spawn_blocking(move || {
            receiver.recv()
        })
        .await??;
        Ok(event)
    }

    pub fn msg_from_packet(packet: &Packet) -> Result<&ArchivedMessage> {
        Ok(rkyv::access::<ArchivedMessage, Error>(packet.payload())?)
    }
}
