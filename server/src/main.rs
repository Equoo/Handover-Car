use std::{collections::HashMap, env, net::SocketAddr, str::FromStr, sync::Arc};

use anyhow::{Context, Result, bail};

use common::{ArchivedMessage, CommonError, Message, PacketType, Socket, SocketEvent};
use parking_lot::Mutex;
use rkyv::{Archived, api::low::deserialize, rancor::Error};
use tokio::sync::Mutex as AsyncMutex;
use tokio::{net::UdpSocket, task::AbortHandle, time::Instant};
use tracing::{debug, error, field::debug, info, level_filters::LevelFilter, warn};
use uuid::Uuid;

const SLAVES_LIMIT: u8 = 16;
const MAIN_PORT: u16 = 9500;
const VIDEO_PORT: u16 = 9600; // - 9616
const AUDIO_PORT: u16 = 9700; // - 9716

struct Slave {
    name: String,
    uniqueid: u8,
    last_addr: SocketAddr,
    latency: u16,
    video_thread: AbortHandle,
    // audio_thread: AbortHandle,
    subscriber: Uuid,
}

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
        Ok(tokio::spawn(async move {
            let socket = UdpSocket::bind(format!("0.0.0.0:{port}")).await.unwrap();
            let mut last_addr: Option<SocketAddr> = None;

            loop {
                let mut buf = [0; 1 << 18];
                let (size, addr) = socket.recv_from(&mut buf).await.unwrap();

                if size != 1 {
                    if let Some(addr) = last_addr {
                        socket.send_to(&buf[..size], addr).await.unwrap();
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
                latency,
                video_thread,
                subscriber: Uuid::nil(),
            },
        );

        Ok((VIDEO_PORT + uniqueid as u16, AUDIO_PORT + uniqueid as u16))
    }

    async fn handle_messages(
        self: &Arc<AppState>,
        addr: SocketAddr,
        msg: &ArchivedMessage,
    ) -> Result<()> {
        match msg {
            ArchivedMessage::SlaveAnnounce {
                id,
                latency,
                packet_id,
            } => {
                let id = id.as_str().to_string();

                if let Some(slave) = self.slaves.lock().await.get_mut(&id) {
                    slave.latency = latency.into();
                } else {
                    if *self.slaves_count.lock() == SLAVES_LIMIT {
                        self.socket.send(
                            &addr,
                            &Message::ServerError {
                                err: CommonError::SlaveLimit,
                            },
                            PacketType::Reliable,
                        )?;
                    }

                    let (video_port, audio_port) = self
                        .clone()
                        .new_slave(id.clone(), addr, latency.into())
                        .await?;

                    self.socket.send(
                        &addr,
                        &Message::ServerSlaveConnected {
                            video_port,
                            audio_port,
                        },
                        PacketType::Reliable,
                    )?;

                    info!("Connection established with slave '{id}'");
                }

                self.socket.send(
                    &addr,
                    &Message::ServerPong {
                        packet_id: packet_id.into(),
                    },
                    PacketType::Unreliable,
                )?;
            }
            ArchivedMessage::MasterSubscribe { slave_id } => {
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
                        &Message::ServerClientConnected {
                            uuid: id.to_string(),
                            video_port: VIDEO_PORT + slave.uniqueid as u16,
                            audio_port: AUDIO_PORT + slave.uniqueid as u16,
                        },
                        PacketType::Reliable,
                    )?;

                    info!("{addr} connected and suscribed to {slave_id}");
                }
            }
            ArchivedMessage::MasterInput {
                slave_id,
                throttle: _,
                steering: _,
            } => {
                if let Some(slave) = self.slaves.lock().await.get(slave_id.as_ref())
                    && let Some(client) = self.clients.lock().await.get(&slave.subscriber)
                {
                    self.socket.send(
                        &client.addr,
                        &deserialize::<Message, Error>(msg)?, // Note: This is really ugly
                        PacketType::Unreliable,
                    )?;
                }
            }
            ArchivedMessage::MasterAnnounce { uuid, packet_id } => {
                if let Some(client) = self.clients.lock().await.get(&Uuid::from_str(uuid)?)
                    && let Some(slave) = self.slaves.lock().await.get(&client.subscribe)
                {
                    self.socket.send(
                        &addr,
                        &Message::ServerClientPong {
                            latency: slave.latency,
                            packet_id: packet_id.into()
                        },
                        PacketType::Unreliable,
                    )?;
                }
            }
            _ => {}
        };

        Ok(())
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
}

async fn run() -> Result<()> {
    let state = Arc::new(AppState::new()?);

    info!("Listening on 0.0.0.0:{MAIN_PORT}.");

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
        state
            .fetch_packets()
            .await
            .context("packets fetching failed")?;
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
