use anyhow::{Context, Result, bail};
use common::{ArchivedMessage, Message, PacketType, Socket, SocketEvent};
use dotenv::dotenv;
use gstreamer as gst;
use gstreamer::glib::object::{Cast, ObjectExt};
use gstreamer::{Buffer, prelude::*};
use gstreamer::{ClockTime, Registry};
use gstreamer_app as gst_app;
use gtk::{Application, ApplicationWindow};
use parking_lot::Mutex;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration};
use std::{env, thread};
use tokio::io::Join;
use tokio::join;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

const SERVER_PORT: u16 = 9500;
const SLAVE: &str = "FlashMcqueen";

pub struct AppState {
    socket: Socket,
    server: SocketAddr,
    // tx: mpsc::Sender<AppEvent>,
    uuid: Mutex<Uuid>,
    last_ping: Mutex<Instant>,
    pub appsrc: Mutex<Option<gst_app::AppSrc>>,
    pub decode_latency: Mutex<f64>,
    pub encode_latency: Mutex<f64>,
    pub slave_latency: Mutex<u16>,
    pub client_latency: Mutex<u16>,
    pub fps: Mutex<u8>,
}

enum AppEvent {
    FrameStats { latency_ms: u64 },
    Error(String),
}

impl AppState {
    pub fn new() -> Result<Self> {
        dotenv()?;
        let server_str = format!("{}:{}", env::var("SERVER")?, SERVER_PORT);
        let mut server = server_str
            .to_socket_addrs()
            .with_context(|| format!("failed to resolve '{server_str}'"))?
            .next()
            .with_context(|| format!("no addresses found for '{server_str}'"))?;
        if server.ip().is_loopback() {
            server.set_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        }
        let socket = Socket::start(0).context("failed to create socket")?;

        Ok(Self {
            socket,
            server,
            appsrc: Mutex::new(None),
            uuid: Mutex::new(Uuid::nil()),
            last_ping: Mutex::new(Instant::now()),
            decode_latency: Mutex::new(0f64),
            encode_latency: Mutex::new(0f64),
            slave_latency: Mutex::new(0),
            client_latency: Mutex::new(0),
            fps: Mutex::new(0),
        })
    }

    fn receive_video(self: Arc<AppState>, port: u16) {
        tokio::spawn(async move {
            let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
            debug!("Send to {}:{port}", self.server.ip().to_string());
            socket
                .send_to(&[1], format!("{}:{port}", self.server.ip().to_string()))
                .await
                .unwrap();

            let mut buffer = [0u8; 1 << 18];

            loop {
                if let Ok((size, _)) = socket.recv_from(&mut buffer).await {
                    let mut gst_buffer = gst::Buffer::with_size(size).unwrap();
                    {
                        let buf = gst_buffer.get_mut().unwrap();
                        buf.copy_from_slice(0, &buffer[..size]).unwrap();
                    }
                    if let Some(appsrc) = self.appsrc.lock().as_mut() {
                        appsrc.push_buffer(gst_buffer)?;
                    }
                }
                if false {
                    break;
                }
            }
            Ok::<(), anyhow::Error>(())
        });
    }

    fn announce_loop(&self) -> Result<()> {
        loop {
            *self.last_ping.lock() = Instant::now();
            self.socket.send(
                &self.server,
                &Message::MasterAnnounce {
                    uuid: self.uuid.lock().to_string()
                },
                PacketType::Unreliable,
            )?;

            std::thread::sleep(Duration::from_millis(100));
        }
    }

    async fn handle_messages(
        self: &Arc<AppState>,
        addr: SocketAddr,
        msg: &ArchivedMessage,
    ) -> Result<()> {
        match msg {
            ArchivedMessage::ServerClientConnected {
                uuid,
                video_port,
                audio_port: _,
            } => {
                info!("Connected to the server");
                *self.uuid.lock() = Uuid::from_str(uuid)?;
                self.clone().receive_video(video_port.into());

                let state_clone = self.clone();
                let announce_thread = tokio::spawn(async move { state_clone.announce_loop() });
            }
            ArchivedMessage::ServerClientPong { latency } => {
                *self.slave_latency.lock() = latency.into();
                *self.client_latency.lock() = self.last_ping.lock().elapsed().as_millis() as u16;
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

pub async fn run(state: Arc<AppState>) -> Result<()> {
    state.socket.send(
        &state.server,
        &Message::MasterSubscribe {
            slave_id: SLAVE.to_string(),
        },
        PacketType::Reliable,
    )?;

    loop {
        state.fetch_packets().await?;
    }

    // Ok(())
}
