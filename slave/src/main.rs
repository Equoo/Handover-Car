use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use common::PacketType;
use common::asyncqueue::AsyncQueue;
use parking_lot::Mutex;
use std::env;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio_serial::SerialPortBuilderExt;
use tokio_serial::SerialStream;
use tracing::debug;
use tracing::error;
use tracing::field::debug;
use tracing::info;
use tracing::level_filters::LevelFilter;
use tracing::warn;

use common::{ArchivedMessage, Message, Socket, SocketEvent};

mod media;
use crate::media::Media;

const SERVER: &str = "server:9500";
const SERIAL_PORT: &str = "/dev/ttyUSB0";
const SERIAL_BAUD: u32 = 115200;

struct AppState {
    id: String,
    socket: Socket,
    server: SocketAddr,
    queue: AsyncQueue<()>,
    serial: AsyncMutex<Option<SerialStream>>,
    media: Mutex<Option<Media>>,
    connected: Mutex<bool>,
    latency: Mutex<Duration>,
    last_ping: Mutex<Instant>,
    announce_id: Mutex<u16>,
}

impl AppState {
    fn new() -> Result<Self> {
        let mut server = SERVER
            .to_socket_addrs()
            .with_context(|| format!("failed to resolve '{SERVER}'"))?
            .next()
            .with_context(|| format!("no addresses found for '{SERVER}'"))?;
        if server.ip().is_loopback() {
            server.set_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        }
        let socket = Socket::start(0u16).context("failed to create socket")?;

        let (queue, _) = AsyncQueue::<()>::new();

        debug!("Connecting to the server({:?})", server);

        Ok(Self {
            id: "FlashMcqueen".to_string(),
            socket,
            server,
            queue,
            serial: AsyncMutex::new(None),
            media: Mutex::new(None),
            connected: Mutex::new(false),
            latency: Mutex::new(Duration::from_secs(0)),
            last_ping: Mutex::new(Instant::now()),
            announce_id: Mutex::new(0),
        })
    }

    fn setup_serial(self: Arc<AppState>) -> Result<()> {
        let state = self.clone();
        self.queue.submit(async move {
            if let Ok(serial) = state
                .retry("serial setup", u32::MAX, Duration::from_secs(3), || {
                    tokio_serial::new(SERIAL_PORT, SERIAL_BAUD).open_native_async()
                })
                .await
            {
                *state.serial.lock().await = Some(serial);
            }
        });
        Ok(())
    }

    fn setup_media(self: Arc<AppState>, video_port: u16, audio_port: u16) -> Result<()> {
        let state = self.clone();
        self.queue.submit(async move {
            info!("Is ");
            if let Ok(media) = state
                .retry("media setup", u32::MAX, Duration::from_secs(3), || {
                    debug!("Trying to setup MEDIA");
                    Media::start_stream(video_port, audio_port)
                })
                .await
            {
                *state.media.lock() = Some(media);
            }
        });
        Ok(())
    }

    async fn handle_messages(self: Arc<AppState>, msg: &ArchivedMessage) -> Result<()> {
        match msg {
            ArchivedMessage::MasterInput {
                slave_id: _,
                throttle,
                steering,
            } => {
                let line = format!("{:} {:}\n", steering, throttle);
                let bytes = line.as_bytes();

                if let Some(serial) = &mut *self.serial.lock().await {
                    serial.write_all(bytes).await?;
                    serial.flush().await?;
                }
            }

            ArchivedMessage::ServerSlaveConnected {
                video_port,
                audio_port,
            } => {
                if !*self.connected.lock() {
                    info!("We are connected !");
                    *self.connected.lock() = true;
                    self.setup_media(video_port.into(), audio_port.into())?;
                }
                //     bail!("Connection to server failed caused by '{what}'");
            }

            ArchivedMessage::ServerPong { packet_id, bitrate } => {
                let packet_id: u16 = packet_id.into();
                if packet_id == *self.announce_id.lock() {
                    *self.latency.lock() = self.last_ping.lock().elapsed();
                }
                if self.media.lock().is_some() {
                    fs::write("/tmp/streamer_ctrl", format!("{}\n", bitrate)).await?;
                }
            }
            _ => {}
        };

        Ok(())
    }

    async fn fetch_packets(self: Arc<AppState>) -> Result<()> {
        let event = self.socket.recv().await.context("failed to receive")?;
        match event {
            SocketEvent::Connect(addr) => {
                if addr == self.server {
                    info!("Connected to {:?}", addr);
                }
            }
            SocketEvent::Timeout(addr) => {
                warn!("Timeout with {:?}", addr);
            }
            SocketEvent::Packet(packet) => {
                if packet.addr() != self.server {
                    warn!("Received packets from unknown address");
                    return Ok(());
                }
                let msg =
                    Socket::msg_from_packet(&packet).context("failed to deserialize packet")?;
                self.handle_messages(msg).await?;
            }
            _ => {}
        }
        Ok(())
    }

    fn announce_loop(&self) -> Result<()> {
        loop {
            *self.last_ping.lock() = Instant::now();
            *self.announce_id.lock() += 1;

            self.socket.send(
                &self.server,
                &Message::SlaveAnnounce {
                    id: self.id.clone(),
                    latency: self.latency.lock().as_millis() as u16,
                    packet_id: *self.announce_id.lock(),
                },
                PacketType::Unreliable,
            )?;

            std::thread::sleep(Duration::from_millis(100));
        }
    }

    async fn send_err(&self, err: String) -> Result<()> {
        self.socket.send(
            &self.server,
            &Message::SlaveError {
                id: self.id.clone(),
                err,
            },
            PacketType::Reliable,
        )
    }

    async fn retry<T, E: std::error::Error + std::marker::Send + std::marker::Sync + 'static>(
        &self,
        task_name: &str,
        max_attempts: u32,
        delay: Duration,
        mut f: impl FnMut() -> Result<T, E>,
    ) -> Result<T> {
        for attempt in 1..=max_attempts {
            match f() {
                Ok(val) => return Ok(val),
                Err(e) if attempt < max_attempts => {
                    warn!(attempt, task = task_name, error = %e, "retrying in {:?}", delay);
                    if *self.connected.lock() {
                        let _ = self.send_err(format!("{task_name} caused by {e}")).await;
                    }
                    sleep(delay).await;
                }
                Err(e) => {
                    return Err(anyhow::Error::from(e)).with_context(|| {
                        format!("'{task_name}' failed after {max_attempts} attempts")
                    });
                }
            }
        }
        unreachable!()
    }
}

async fn run() -> Result<()> {
    let state = Arc::new(AppState::new()?);

    state.clone().setup_serial()?;

    let state_clone = state.clone();
    let announce_thread = tokio::spawn(async move { state_clone.announce_loop() });

    let state_clone = state.clone();
    let receive_thread = tokio::spawn(async move {
        let mut res: Result<()>;
        loop {
            res = state_clone
                .clone()
                .fetch_packets()
                .await
                .context("packets fetching failed");
            if res.is_err() {
                break;
            }
        }
        res
    });

    let state_clone = state.clone();
    timeout(Duration::from_secs(5), async move {
        while !*state_clone.connected.lock() {
            sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .context("connection timeout")?;

    let _ = tokio::try_join!(announce_thread, receive_thread)?;
    unreachable!()
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
