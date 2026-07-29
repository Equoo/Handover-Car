use rkyv::{Archive, Deserialize, Serialize, rancor::Error};
use std::net::SocketAddr;
use crossbeam_channel::{Receiver, Sender};
use anyhow::{Context, Result};

pub mod asyncqueue;

#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum CommonError {
    SlaveLimit,
    SlaveNotFound
}

#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum Message {
    ServerSlaveConnected {
        video_port: u16,
        audio_port: u16,
    },
    ServerClientConnected {
        uuid: String,
        video_port: u16,
        audio_port: u16
    },
    ServerError {
        err: CommonError,
    },
    ServerPong {
        packet_id: u16,
        bitrate: u32
    },
    ServerClientPong {
        latency: u16,
        packet_id: u16,
    },
    SlaveAnnounce {
        id: String,
        latency: u16,
        packet_id: u16,
    },
    SlaveError {
        id: String,
        err: String,
    },
    SlaveLocation {
        id: String,
        latitude: f32,
        altitude: f32,
    },
    MasterSubscribe {
        slave_id: String,
    },
    MasterInput {
        slave_id: String,
        throttle: i16,
        steering: i16,
    },
    MasterAnnounce {
        uuid: String,
        packet_id: u16,
        bitrate: u32
    },
    /// Ask the slave's encoder for an IDR. With a long GOP nothing else
    /// produces one, so a receiver that has lost sync stays corrupted until
    /// this arrives -- hence sent reliably.
    MasterKeyframe {
        slave_id: String,
    },
    /// Encoder-side pipeline stats parsed from the streamer's own stdout
    /// (capture/encode timings + the bitrate it's actually applying).
    SlaveStats {
        id: String,
        capture_latency_ms: f32,
        encode_latency_ms: f32,
        total_latency_ms: f32,
        bitrate_bps: u32,
    },
    ServerClientSlaveStats {
        capture_latency_ms: f32,
        encode_latency_ms: f32,
        total_latency_ms: f32,
        bitrate_bps: u32,
    },
    Disconnect {}
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
