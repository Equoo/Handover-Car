// SPDX-FileCopyrightText: 2026 David Derny
// SPDX-FileCopyrightText: 2026 Handover contributors (see AUTHORS)
//
// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0

use std::{collections::HashMap, env, net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};

use common::transport::{channel, parse_register, register_ack_body};
use common::{
    ArchivedClientMessage, ArchivedMessage, ArchivedPilotCommand, ArchivedSlaveMessage, Candidate,
    CommonError, Message, PacketType, PilotCommand, Role, ServerToClient, ServerToSlave,
    SessionToken, Socket, SocketEvent, TransportConfig,
};
use parking_lot::Mutex;
use rkyv::rancor::Error as RkyvError;
use tokio::sync::Mutex as AsyncMutex;
use tokio::{net::UdpSocket, task::AbortHandle, time::Instant};
use tracing::{debug, error, info, level_filters::LevelFilter, warn};
use uuid::Uuid;

const SLAVES_LIMIT: u8 = 16;
const MAIN_PORT: u16 = 9500;
/// One relay port per slave (9600 - 9615). Video, audio and pilot input all
/// share it: they share one socket on each peer too, so that a single punched
/// NAT mapping serves every stream, and the relay is just the fallback
/// destination for that same socket.
const RELAY_PORT: u16 = 9600;

struct Slave {
    name: String,
    uniqueid: u8,
    last_addr: SocketAddr,
    last_seen: Instant,
    latency: u16,
    bitrate: u32,
    fec_pct: u8,
    relay_thread: AbortHandle,
    /// Shared secret both sides must present to register with this session's
    /// relay and to have their probes answered by the other peer.
    token: SessionToken,
    /// Latest data-plane candidates this car advertised, kept so a client that
    /// subscribes later doesn't have to wait for the next republish.
    candidates: Vec<Candidate>,
    subscriber: Uuid,
}

/// How long a slave can go without a `SlaveAnnounce` (sent every 100ms)
/// before the server treats it as gone and frees its slot.
const SLAVE_EXPIRY: Duration = Duration::from_secs(5);

struct Client {
    uuid: Uuid,
    addr: SocketAddr,
    subscribe: String,
    candidates: Vec<Candidate>,
}

struct AppState {
    socket: Socket,
    starttime: Instant,
    slaves: AsyncMutex<HashMap<String, Slave>>,
    clients: AsyncMutex<HashMap<Uuid, Client>>,
    slaves_count: Mutex<u8>,
    usedids: Mutex<[bool; SLAVES_LIMIT as usize]>,
}

impl AppState {
    fn new() -> Result<Self> {
        let socket = Socket::start(MAIN_PORT).context("failed to create socket")?;

        Ok(Self {
            socket,
            slaves: AsyncMutex::new(HashMap::new()),
            clients: AsyncMutex::new(HashMap::new()),
            slaves_count: Mutex::new(0),
            usedids: Mutex::new([false; SLAVES_LIMIT as usize]),
            starttime: Instant::now(),
        })
    }

    fn uniqueid(&self) -> u8 {
        let (i, _) = self
            .usedids
            .lock()
            .iter()
            .enumerate()
            .find(|(_, x)| !**x)
            .unwrap_or((0, &false));
        i as u8
    }

    /// The fallback path for one session's data plane: a two-party pipe that
    /// forwards whatever one side sends to the other.
    ///
    /// It learns both addresses from `REGISTER` packets rather than being told
    /// them, because neither side knows its own public address and both move
    /// on their own -- cellular NAT rebinds the car, and the client's home NAT
    /// expires mappings. Re-registering every second is what keeps this
    /// current, and the ack it sends back is also how each peer discovers the
    /// public address it must advertise for hole punching. That makes this
    /// port the session's STUN server as well as its TURN server.
    ///
    /// Everything that is not a registration is forwarded blind. The relay
    /// never parses media, and probes never come here at all -- those go
    /// peer-to-peer by definition.
    async fn new_relay(
        self: Arc<AppState>,
        port: u16,
        id: String,
        token: SessionToken,
    ) -> Result<AbortHandle> {
        debug!("New relay on port {port} for {id}");
        let socket = UdpSocket::bind(format!("0.0.0.0:{port}"))
            .await
            .with_context(|| format!("failed to bind relay on port {port} for {id}"))?;

        Ok(tokio::spawn(async move {
            let mut slave_addr: Option<SocketAddr> = None;
            let mut client_addr: Option<SocketAddr> = None;
            let mut buf = [0; 1 << 18];

            loop {
                let (size, addr) = match socket.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = ?e, "relay {port} recv failed for {id}, dropping packet");
                        continue;
                    }
                };
                if size < 1 {
                    continue;
                }

                if buf[0] == channel::REGISTER {
                    let (role, echo) = match parse_register(&buf[1..size], &token) {
                        Ok(v) => v,
                        Err(e) => {
                            debug!(error = ?e, "relay {port} rejected a registration from {addr}");
                            continue;
                        }
                    };

                    let slot = match role {
                        Role::Slave => &mut slave_addr,
                        Role::Client => &mut client_addr,
                    };
                    if *slot != Some(addr) {
                        info!("Relay {port} ({id}): {role:?} side is now {addr}");
                        *slot = Some(addr);
                    }

                    let mut ack = Vec::with_capacity(1 + 43);
                    ack.push(channel::REGISTER_ACK);
                    ack.extend_from_slice(&register_ack_body(&token, echo, addr));
                    if let Err(e) = socket.send_to(&ack, addr).await {
                        debug!(error = ?e, "relay {port} ack to {addr} failed");
                    }
                    continue;
                }

                // Forward to whichever side didn't send it. An unregistered
                // source is silently dropped: without a registration there is
                // no way to tell which half of the session it claims to be.
                let dst = if Some(addr) == slave_addr {
                    client_addr
                } else if Some(addr) == client_addr {
                    slave_addr
                } else {
                    continue;
                };

                if let Some(dst) = dst
                    && let Err(e) = socket.send_to(&buf[..size], dst).await
                {
                    warn!(error = ?e, "relay {port} forward to {dst} failed for {id}");
                }
            }
        })
        .abort_handle())
    }

    async fn new_slave(
        self: Arc<AppState>,
        id: String,
        addr: SocketAddr,
        latency: u16,
    ) -> Result<TransportConfig> {
        if *self.slaves_count.lock() == SLAVES_LIMIT {
            bail!("slaves limit reach");
        }

        let uniqueid = self.uniqueid();
        self.usedids.lock()[uniqueid as usize] = true;
        *self.slaves_count.lock() += 1;

        // Random per session rather than derived from the slave id: it gates
        // both relay registration and the peer-to-peer probes, so anything
        // guessable would let a third party point the car's video at itself.
        let token: SessionToken = *Uuid::new_v4().as_bytes();
        let relay_port = RELAY_PORT + uniqueid as u16;

        let relay_thread = self
            .clone()
            .new_relay(relay_port, id.clone(), token)
            .await?;

        self.slaves.lock().await.insert(
            id.clone(),
            Slave {
                name: id,
                uniqueid,
                last_addr: addr,
                last_seen: Instant::now(),
                latency,
                relay_thread,
                token,
                candidates: Vec::new(),
                bitrate: 1000000,
                fec_pct: 0,
                subscriber: Uuid::nil(),
            },
        );

        Ok(TransportConfig { relay_port, token })
    }

    async fn handle_slave_message(
        self: &Arc<AppState>,
        addr: SocketAddr,
        msg: &ArchivedSlaveMessage,
    ) -> Result<()> {
        match msg {
            ArchivedSlaveMessage::Announce {
                id,
                latency,
                packet_id,
            } => {
                let id = id.as_str().to_string();
                let mut bitrate: u32 = 1000000;
                let mut fec_pct: u8 = 0;

                if let Some(slave) = self.slaves.lock().await.get_mut(&id) {
                    slave.latency = latency.into();
                    bitrate = slave.bitrate;
                    fec_pct = slave.fec_pct;
                    // Cellular NAT rebinds, so the address a slave announces
                    // from changes. Anything we push to it must use the
                    // latest one, not the one it first connected with.
                    slave.last_addr = addr;
                    slave.last_seen = Instant::now();
                } else {
                    if *self.slaves_count.lock() == SLAVES_LIMIT {
                        self.socket.send(
                            &addr,
                            &Message::ToSlave(ServerToSlave::Error {
                                err: CommonError::SlaveLimit,
                            }),
                            PacketType::Reliable,
                        )?;
                    }

                    let transport = self
                        .clone()
                        .new_slave(id.clone(), addr, latency.into())
                        .await?;

                    self.socket.send(
                        &addr,
                        &Message::ToSlave(ServerToSlave::Connected { transport }),
                        PacketType::Reliable,
                    )?;

                    // Re-link whichever client was already subscribed to this
                    // id. The `Slave` record is brand new -- the car restarted,
                    // or a tunnel outran `SLAVE_EXPIRY` -- so its `subscriber`
                    // would otherwise stay unset until the client resubscribed,
                    // and the client only does that when the *server* goes
                    // quiet, not when the car does. Left unset, stats and
                    // candidates both stop flowing towards a client that is
                    // still perfectly happy.
                    let waiting = self
                        .clients
                        .lock()
                        .await
                        .values()
                        .find(|c| c.subscribe == id)
                        .map(|c| (c.uuid, c.addr, c.candidates.clone()));

                    if let Some((uuid, client_addr, client_candidates)) = waiting {
                        if let Some(slave) = self.slaves.lock().await.get_mut(&id) {
                            slave.subscriber = uuid;
                        }
                        info!("Re-linked slave '{id}' to its waiting client {client_addr}");

                        // Hand each side the other's last known candidates so
                        // punching restarts now rather than at whatever point
                        // one of the two addresses happens to change.
                        if !client_candidates.is_empty() {
                            self.socket.send(
                                &addr,
                                &Message::ToSlave(ServerToSlave::PeerCandidates {
                                    candidates: client_candidates,
                                }),
                                PacketType::Reliable,
                            )?;
                        }
                    }

                    info!("Connection established with slave '{id}'");
                }

                self.socket.send(
                    &addr,
                    &Message::ToSlave(ServerToSlave::Pong {
                        packet_id: packet_id.into(),
                        bitrate,
                        fec_pct,
                    }),
                    PacketType::Unreliable,
                )?;
            }
            ArchivedSlaveMessage::Stats {
                id,
                capture_latency_ms,
                encode_latency_ms,
                total_latency_ms,
                bitrate_bps,
            } => {
                if let Some(slave) = self.slaves.lock().await.get(id.as_ref())
                    && let Some(client) = self.clients.lock().await.get(&slave.subscriber)
                {
                    self.socket.send(
                        &client.addr,
                        &Message::ToClient(ServerToClient::SlaveStats {
                            capture_latency_ms: capture_latency_ms.into(),
                            encode_latency_ms: encode_latency_ms.into(),
                            total_latency_ms: total_latency_ms.into(),
                            bitrate_bps: bitrate_bps.into(),
                        }),
                        PacketType::Unreliable,
                    )?;
                }
            }
            ArchivedSlaveMessage::Candidates { id, candidates } => {
                let candidates: Vec<Candidate> = rkyv::deserialize::<_, RkyvError>(candidates)
                    .context("failed to deserialize slave candidates")?;
                debug!("Slave '{id}' advertises {} candidate(s)", candidates.len());

                let subscriber = {
                    let mut slaves = self.slaves.lock().await;
                    let Some(slave) = slaves.get_mut(id.as_ref()) else {
                        return Ok(());
                    };
                    slave.candidates = candidates.clone();
                    slave.subscriber
                };

                if let Some(client) = self.clients.lock().await.get(&subscriber) {
                    self.socket.send(
                        &client.addr,
                        &Message::ToClient(ServerToClient::PeerCandidates { candidates }),
                        PacketType::Reliable,
                    )?;
                }
            }
            // Not wired up to anything yet on the receiving end.
            ArchivedSlaveMessage::Error { .. } | ArchivedSlaveMessage::Location { .. } => {}
        }

        Ok(())
    }

    async fn handle_client_message(
        self: &Arc<AppState>,
        addr: SocketAddr,
        msg: &ArchivedClientMessage,
    ) -> Result<()> {
        match msg {
            ArchivedClientMessage::Subscribe { slave_id } => {
                let id = Uuid::from_u128_le(self.starttime.elapsed().as_millis());
                let cl = Client {
                    uuid: id,
                    addr,
                    subscribe: slave_id.to_string(),
                    candidates: Vec::new(),
                };

                debug!("Trying to subscribe to {slave_id}");

                self.clients.lock().await.insert(id, cl);
                if let Some(slave) = self.slaves.lock().await.get_mut(slave_id.as_ref()) {
                    slave.subscriber = id;

                    self.socket.send(
                        &addr,
                        &Message::ToClient(ServerToClient::Connected {
                            uuid: id.to_string(),
                            transport: TransportConfig {
                                relay_port: RELAY_PORT + slave.uniqueid as u16,
                                token: slave.token,
                            },
                        }),
                        PacketType::Reliable,
                    )?;

                    // The car has usually been announcing for a while by now,
                    // so its candidates already exist. Sending them with the
                    // handshake rather than waiting for the next republish is
                    // what lets punching start immediately instead of a couple
                    // of seconds into the session.
                    if !slave.candidates.is_empty() {
                        self.socket.send(
                            &addr,
                            &Message::ToClient(ServerToClient::PeerCandidates {
                                candidates: slave.candidates.clone(),
                            }),
                            PacketType::Reliable,
                        )?;
                    }

                    info!("{addr} connected and suscribed to {slave_id}");
                }
            }
            ArchivedClientMessage::Candidates { uuid, candidates } => {
                let candidates: Vec<Candidate> = rkyv::deserialize::<_, RkyvError>(candidates)
                    .context("failed to deserialize client candidates")?;
                debug!("Client advertises {} candidate(s)", candidates.len());

                let subscribe = {
                    let mut clients = self.clients.lock().await;
                    let Some(client) = clients.get_mut(&Uuid::from_str(uuid)?) else {
                        return Ok(());
                    };
                    client.candidates = candidates.clone();
                    client.subscribe.clone()
                };

                if let Some(slave) = self.slaves.lock().await.get(&subscribe) {
                    self.socket.send(
                        &slave.last_addr,
                        &Message::ToSlave(ServerToSlave::PeerCandidates { candidates }),
                        PacketType::Reliable,
                    )?;
                }
            }
            ArchivedClientMessage::Pilot { slave_id, cmd } => {
                let slave_addr = self
                    .slaves
                    .lock()
                    .await
                    .get(slave_id.as_ref())
                    .map(|s| s.last_addr);

                let Some(slave_addr) = slave_addr else {
                    return Ok(());
                };

                // Forwarded to the car -- the server never inspects the
                // payload beyond picking a packet type for it.
                let (cmd, packet_type) = match cmd {
                    ArchivedPilotCommand::Input { throttle, steering } => (
                        PilotCommand::Input {
                            throttle: throttle.into(),
                            steering: steering.into(),
                        },
                        PacketType::Unreliable,
                    ),
                    ArchivedPilotCommand::KeyframeRequest => {
                        debug!("Keyframe requested for {slave_id}");
                        (PilotCommand::KeyframeRequest, PacketType::Reliable)
                    }
                };

                self.socket.send(
                    &slave_addr,
                    &Message::ToSlave(ServerToSlave::Pilot(cmd)),
                    packet_type,
                )?;
            }
            ArchivedClientMessage::Announce {
                uuid,
                packet_id,
                bitrate,
                fec_pct,
            } => {
                let clients = self.clients.lock().await;
                let Some(client) = clients.get(&Uuid::from_str(uuid)?) else {
                    // Unknown uuid -- most likely the server restarted and
                    // lost this client's registration. Staying silent (rather
                    // than replying with a hollow pong) lets the client
                    // detect the gap and resubscribe.
                    return Ok(());
                };

                let mut latency: u16 = 0;
                if let Some(slave) = self.slaves.lock().await.get_mut(&client.subscribe) {
                    slave.bitrate = bitrate.into();
                    slave.fec_pct = (*fec_pct).into();
                    latency = slave.latency;
                }
                self.socket.send(
                    &addr,
                    &Message::ToClient(ServerToClient::Pong {
                        latency,
                        packet_id: packet_id.into(),
                    }),
                    PacketType::Unreliable,
                )?;
            }
        }

        Ok(())
    }

    async fn handle_messages(
        self: &Arc<AppState>,
        addr: SocketAddr,
        msg: &ArchivedMessage,
    ) -> Result<()> {
        match msg {
            ArchivedMessage::Slave(msg) => self.handle_slave_message(addr, msg).await,
            ArchivedMessage::Client(msg) => self.handle_client_message(addr, msg).await,
            // The server only ever sends these, never receives them.
            ArchivedMessage::ToSlave(_) | ArchivedMessage::ToClient(_) => Ok(()),
        }
    }

    async fn fetch_packets(self: &Arc<AppState>) -> Result<()> {
        let event = self.socket.recv().await.context("failed to receive")?;
        match event {
            SocketEvent::Connect(addr) => {
                info!("Connected to {:?}", addr);
            }
            SocketEvent::Timeout(addr) => {
                warn!("Timeout with {:?}", addr);
            }
            SocketEvent::Disconnect(addr) => {
                info!("Disconnect from {:?}", addr);
            }
            SocketEvent::Packet(packet) => {
                let msg =
                    Socket::msg_from_packet(&packet).context("failed to deserialize packet")?;
                self.handle_messages(packet.addr(), msg).await?;
            }
        }
        Ok(())
    }

    /// Drops slaves that have gone quiet for longer than `SLAVE_EXPIRY`,
    /// freeing their id slot and stopping their video relay task. Without
    /// this a slave that drops off the network for good keeps its slot (and
    /// a pipesocket task relaying to nobody) forever.
    async fn sweep_stale_slaves(self: &Arc<AppState>) {
        let mut slaves = self.slaves.lock().await;
        let stale: Vec<String> = slaves
            .iter()
            .filter(|(_, s)| s.last_seen.elapsed() > SLAVE_EXPIRY)
            .map(|(id, _)| id.clone())
            .collect();

        for id in stale {
            if let Some(slave) = slaves.remove(&id) {
                warn!("Slave '{id}' timed out, freeing slot {}", slave.uniqueid);
                slave.relay_thread.abort();
                self.usedids.lock()[slave.uniqueid as usize] = false;
                *self.slaves_count.lock() -= 1;
            }
        }
    }
}

async fn run() -> Result<()> {
    let state = Arc::new(AppState::new()?);

    info!("Listening on 0.0.0.0:{MAIN_PORT}.");

    let sweep_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            sweep_state.sweep_stale_slaves().await;
        }
    });

    // let state_clone = state.clone();
    // let receive_thread = tokio::spawn(async move {
    //     let mut res: Result<()>;
    //     loop {
    //         debug!("Receive");
    //         res = state_clone
    //             .lock()
    //             .await
    //             .fetch_packets()
    //             .await
    //             .context("packets fetching failed");
    //         if res.is_err() {
    //             break;
    //         }
    //     }
    //     res
    // });

    loop {
        if let Err(e) = state.fetch_packets().await {
            error!(error = ?e, "packet fetching failed, dropping packet");
        }
    }
    // let _ = tokio::try_join!(setup_thread, announce_thread, receive_thread)?;
    // Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(
            LevelFilter::from_str(&env::var("RUST_LOG").unwrap_or("INFO".into())).unwrap(),
        )
        .init();

    if let Err(e) = run().await {
        error!(error = ?e, "fatal error");
        std::process::exit(1);
    }
    Ok(())
}
