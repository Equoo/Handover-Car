use std::{collections::HashMap, env, net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};

use common::{
    ArchivedClientMessage, ArchivedMessage, ArchivedPilotCommand, ArchivedSlaveMessage,
    CommonError, Message, PacketType, PilotCommand, ServerToClient, ServerToSlave, Socket,
    SocketEvent, VideoMode,
};
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;
use tokio::{net::UdpSocket, task::AbortHandle, time::Instant};
use tracing::{debug, error, info, level_filters::LevelFilter, warn};
use uuid::Uuid;

const SLAVES_LIMIT: u8 = 16;
const MAIN_PORT: u16 = 9500;
const VIDEO_PORT: u16 = 9600; // - 9616
const AUDIO_PORT: u16 = 9700; // - 9716

struct Slave {
    name: String,
    uniqueid: u8,
    last_addr: SocketAddr,
    last_seen: Instant,
    latency: u16,
    bitrate: u32,
    video_thread: AbortHandle,
    // audio_thread: AbortHandle,
    subscriber: Uuid,
}

/// How long a slave can go without a `SlaveAnnounce` (sent every 100ms)
/// before the server treats it as gone and frees its slot.
const SLAVE_EXPIRY: Duration = Duration::from_secs(5);

struct Client {
    uuid: Uuid,
    addr: SocketAddr,
    subscribe: String,
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

    async fn new_pipesocket(self: Arc<AppState>, port: u16, id: String) -> Result<AbortHandle> {
        debug!("New pipesocket on port {port} for {id}");
        let socket = UdpSocket::bind(format!("0.0.0.0:{port}"))
            .await
            .with_context(|| format!("failed to bind pipesocket on port {port} for {id}"))?;

        Ok(tokio::spawn(async move {
            let mut last_addr: Option<SocketAddr> = None;
            let mut buf = [0; 1 << 18];
            loop {
                let (size, addr) = match socket.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = ?e, "pipesocket {port} recv failed for {id}, dropping packet");
                        continue;
                    }
                };

                if size != 1 {
                    if let Some(addr) = last_addr
                        && let Err(e) = socket.send_to(&buf[..size], addr).await
                    {
                        warn!(error = ?e, "pipesocket {port} relay to {addr} failed for {id}");
                    }
                } else {
                    last_addr = Some(addr);
                    debug!("Client from {addr} subscribe pipesocket {port} of {id}");
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
    ) -> Result<(u16, u16)> {
        if *self.slaves_count.lock() == SLAVES_LIMIT {
            bail!("slaves limit reach");
        }

        let uniqueid = self.uniqueid();
        self.usedids.lock()[uniqueid as usize] = true;
        *self.slaves_count.lock() += 1;

        let video_thread = self
            .clone()
            .new_pipesocket(VIDEO_PORT + uniqueid as u16, id.clone())
            .await?;

        self.slaves.lock().await.insert(
            id.clone(),
            Slave {
                name: id,
                uniqueid,
                last_addr: addr,
                last_seen: Instant::now(),
                latency,
                video_thread,
                bitrate: 1000000,
                subscriber: Uuid::nil(),
            },
        );

        Ok((VIDEO_PORT + uniqueid as u16, AUDIO_PORT + uniqueid as u16))
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

                if let Some(slave) = self.slaves.lock().await.get_mut(&id) {
                    slave.latency = latency.into();
                    bitrate = slave.bitrate;
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

                    let (video_port, audio_port) = self
                        .clone()
                        .new_slave(id.clone(), addr, latency.into())
                        .await?;

                    self.socket.send(
                        &addr,
                        &Message::ToSlave(ServerToSlave::Connected {
                            video: VideoMode::RawRtpUdp { video_port },
                            audio_port,
                        }),
                        PacketType::Reliable,
                    )?;

                    info!("Connection established with slave '{id}'");
                }

                self.socket.send(
                    &addr,
                    &Message::ToSlave(ServerToSlave::Pong {
                        packet_id: packet_id.into(),
                        bitrate,
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
                };

                debug!("Trying to subscribe to {slave_id}");

                self.clients.lock().await.insert(id, cl);
                if let Some(slave) = self.slaves.lock().await.get_mut(slave_id.as_ref()) {
                    slave.subscriber = id;

                    self.socket.send(
                        &addr,
                        &Message::ToClient(ServerToClient::Connected {
                            uuid: id.to_string(),
                            video: VideoMode::RawRtpUdp {
                                video_port: VIDEO_PORT + slave.uniqueid as u16,
                            },
                            audio_port: AUDIO_PORT + slave.uniqueid as u16,
                        }),
                        PacketType::Reliable,
                    )?;

                    info!("{addr} connected and suscribed to {slave_id}");
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
                slave.video_thread.abort();
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
