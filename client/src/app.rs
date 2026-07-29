use anyhow::{Context, Result, bail};
use common::{ArchivedMessage, Message, PacketType, Socket, SocketEvent};
use dotenv::dotenv;
use gstreamer as gst;
use gstreamer_app as gst_app;
use parking_lot::Mutex;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::{env, thread};
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::video::PacketLoss;

const SERVER_PORT: u16 = 9500;
const SLAVE: &str = "FlashMcqueen";

pub struct AppState {
    socket: Socket,
    server: SocketAddr,
    // tx: mpsc::Sender<AppEvent>,
    uuid: Mutex<Uuid>,
    last_ping: Mutex<Instant>,
    announce_id: Mutex<u16>,
    pub appsrc: Mutex<Option<gst_app::AppSrc>>,
    pub decode_latency: Mutex<f64>,
    pub encode_latency: Mutex<f64>,
    pub slave_latency: Mutex<u16>,
    pub client_latency: Mutex<u16>,
    pub fps: Mutex<u8>,
    pub bitrate: Mutex<u32>,
    pub loss: Mutex<PacketLoss>,
    last_keyframe_request: Mutex<Option<Instant>>,
}

enum AppEvent {
    FrameStats { latency_ms: u64 },
    Error(String),
}

const ALPHA: f64 = 0.2;
pub fn ema_average(average: f64, value: f64) -> f64 {
    ALPHA * value + (1.0f64 - ALPHA) * average
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
            announce_id: Mutex::new(0),
            fps: Mutex::new(0),
            bitrate: Mutex::new(1000000),
            loss: Mutex::new(PacketLoss {num_lost: 0, num_pushed: 0, loss_pct: 0f64}),
            last_keyframe_request: Mutex::new(None),
        })
    }

    /// Ask the car's encoder for an IDR. Sent reliably: a dropped request
    /// means the picture stays broken until the GOP fallback comes round.
    ///
    /// Rate limited, because a keyframe is several times the size of a normal
    /// frame. Under sustained loss an unthrottled version would emit them
    /// continuously and recreate exactly the bitrate spikes the long GOP was
    /// introduced to remove.
    pub fn request_keyframe(&self) {
        const MIN_INTERVAL: Duration = Duration::from_millis(1000);

        {
            let mut last = self.last_keyframe_request.lock();
            if let Some(t) = *last
                && t.elapsed() < MIN_INTERVAL
            {
                return;
            }
            *last = Some(Instant::now());
        }

        if let Err(e) = self.socket.send(
            &self.server,
            &Message::MasterKeyframe {
                slave_id: SLAVE.to_string(),
            },
            PacketType::Reliable,
        ) {
            warn!(error = ?e, "failed to request keyframe");
        } else {
            debug!("Requested keyframe");
        }
    }

    fn receive_video(self: Arc<AppState>, port: u16) {
        tokio::spawn(async move {
            let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
            debug!("Send to {}:{port}", self.server.ip().to_string());
            socket
                .send_to(&[1], format!("{}:{port}", self.server.ip()))
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
            *self.announce_id.lock() += 1;

            self.socket.send(
                &self.server,
                &Message::MasterAnnounce {
                    uuid: self.uuid.lock().to_string(),
                    packet_id: *self.announce_id.lock(),
                    bitrate: *self.bitrate.lock()
                },
                PacketType::Unreliable,
            )?;
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    async fn run_abr(self: Arc<AppState>) {
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        let mut bitrate: u32 = 1_000_000;

        loop {
            interval.tick().await;

            let losses = *self.loss.lock();
            let decode_latency = *self.decode_latency.lock();
            let loss = losses.num_lost as f32 / (losses.num_pushed as f32).max(1f32);

            // AIMD
            bitrate = if loss > 0.05 {
                (bitrate as f64 * 0.7) as u32 // cut 30% on bad loss
            } else if loss > 0.02 {
                (bitrate as f64 * 0.9) as u32 // cut 10% on mild loss
            } else if decode_latency > 10.0 {
                bitrate // hold if decoder struggling
            } else {
                bitrate + 25_000 // additive increase
            };

            bitrate = bitrate.clamp(200_000, 4_000_000);

            tracing::info!(
                "ABR: loss={:.1}% latency={:.1}ms → {}kbps",
                loss * 100.0,
                decode_latency,
                bitrate / 1000
            );

            *self.bitrate.lock() = bitrate;
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

                // The stream is already running and its opening IDR is long
                // gone, so without this we would decode nothing until the GOP
                // fallback fires.
                self.request_keyframe();

                let state_clone = self.clone();
                let announce_thread = tokio::spawn(async move { state_clone.announce_loop() });
                let state_clone = self.clone();
                let abr_thread = tokio::spawn(async move { state_clone.run_abr().await });
            }
            ArchivedMessage::ServerClientPong { latency, packet_id } => {
                let latency: u16 = latency.into();
                let mut slave_latency = self.slave_latency.lock();

                *slave_latency = ema_average(*slave_latency as f64, latency as f64) as u16;

                let packet_id: u16 = packet_id.into();
                if packet_id == *self.announce_id.lock() {
                    let mut client_latency = self.client_latency.lock();
                    let cl_latency = self.last_ping.lock().elapsed().as_millis() as f64;
                    *client_latency = ema_average(*client_latency as f64, cl_latency) as u16;
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
