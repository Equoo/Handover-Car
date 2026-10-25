use rkyv::{Archive, Deserialize, Serialize, rancor::Error};
use std::net::SocketAddr;
use crossbeam_channel::{Receiver, Sender};
use anyhow::{Context, Result};

pub mod asyncqueue;
pub mod link;
pub mod transport;

pub use transport::{Candidate, CandidateKind, Role, SessionToken, TransportConfig};

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

/// Wire form of `PilotCommand::Input` on the data plane, where it rides the
/// same path-selected socket as video rather than the server-relayed control
/// channel (`transport::channel::INPUT`).
///
/// Hand-rolled rather than rkyv: the payload lands at a one-byte offset into
/// the receive buffer, past the channel tag, so an archived type's alignment
/// requirement would not be met. Four fixed bytes need no alignment, no
/// validation pass and no allocation, which suits a message sent every frame.
pub fn encode_input(throttle: i16, steering: i16) -> [u8; 4] {
    let mut out = [0u8; 4];
    out[..2].copy_from_slice(&throttle.to_be_bytes());
    out[2..].copy_from_slice(&steering.to_be_bytes());
    out
}

pub fn decode_input(bytes: &[u8]) -> Option<(i16, i16)> {
    if bytes.len() < 4 {
        return None;
    }
    Some((
        i16::from_be_bytes([bytes[0], bytes[1]]),
        i16::from_be_bytes([bytes[2], bytes[3]]),
    ))
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
    /// Addresses this car's data-plane socket might be reachable at, for the
    /// server to pass on to its subscriber. Re-sent whenever the set changes
    /// -- on cellular the public address moves on its own.
    Candidates {
        id: String,
        candidates: Vec<Candidate>,
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
        /// FEC overhead percentage the client's adaptive loop is currently
        /// commanding (0-50). Relayed to the slave the same way `bitrate` is:
        /// stored server-side and echoed back in `ServerToSlave::Pong`.
        fec_pct: u8,
    },
    Pilot {
        slave_id: String,
        cmd: PilotCommand,
    },
    /// This client's own data-plane candidates, forwarded to the car it is
    /// subscribed to. Mirror of `SlaveMessage::Candidates`.
    Candidates {
        uuid: String,
        candidates: Vec<Candidate>,
    },
}

/// Sent by the server to a car (slave).
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum ServerToSlave {
    Connected {
        transport: TransportConfig,
    },
    /// The subscribed client's candidates. Arrives repeatedly, not once: both
    /// sides republish whenever their address set changes, and a car that
    /// misses the update keeps punching at an address the client's NAT no
    /// longer maps.
    PeerCandidates {
        candidates: Vec<Candidate>,
    },
    Error {
        err: CommonError,
    },
    Pong {
        packet_id: u16,
        bitrate: u32,
        fec_pct: u8,
    },
    Pilot(PilotCommand),
}

/// Sent by the server to a driver's client.
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum ServerToClient {
    Connected {
        uuid: String,
        transport: TransportConfig,
    },
    /// The subscribed car's candidates. See `ServerToSlave::PeerCandidates`.
    PeerCandidates {
        candidates: Vec<Candidate>,
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
