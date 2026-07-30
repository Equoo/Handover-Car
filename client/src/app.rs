use anyhow::{Context, Result};
use common::link::PingTracker;
use common::{
    ArchivedMessage, ArchivedServerToClient, ArchivedVideoMode, ClientMessage, Message,
    PacketType, PilotCommand, Socket, SocketEvent, VideoMode,
};
use dotenv::dotenv;
use egui::ColorImage;
use gstreamer_app as gst_app;
use parking_lot::Mutex;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::env;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::latency_test::LatencyTest;
use crate::stats::Stats;
use crate::video::{RawRtpUdp, VideoReceiver};

const SERVER_PORT: u16 = 9500;
const SLAVE: &str = "FlashMcqueen";

/// How long the client can go without a `ServerClientPong` before it assumes
/// the server forgot about it (most likely a server restart) and resends
/// `MasterSubscribe` to re-register.
const PONG_STALE: Duration = Duration::from_secs(2);

pub struct AppState {
    socket: Socket,
    pub(crate) server: SocketAddr,
    uuid: Mutex<Uuid>,
    ping: PingTracker,
    last_pong: Mutex<Instant>,
    pub appsrc: Mutex<Option<gst_app::AppSrc>>,
    pub stats: Stats,
    pub latency_test: LatencyTest,
    /// Latest decoded video frame, written by the `appsink` callback in
    /// `video.rs` and consumed (via `take`) by `ui.rs`'s `update()` each
    /// repaint -- replaces what `gtk4paintablesink` used to do for free.
    pub latest_frame: Mutex<Option<ColorImage>>,
    /// Set once at app creation (see `ui.rs`), read by the `appsink`
    /// callback to request a repaint the moment a new frame lands rather
    /// than waiting on a poll/vsync timer.
    pub egui_ctx: OnceLock<egui::Context>,
    last_keyframe_request: Mutex<Option<Instant>>,
    /// Whether `announce_loop`/`run_abr` have been spawned. Set once on the
    /// first `ServerClientConnected` and left set across a later resubscribe
    /// -- those loops don't depend on which server session we're in, they'd
    /// just duplicate traffic if spawned twice.
    connected: Mutex<bool>,
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
            ping: PingTracker::new(),
            last_pong: Mutex::new(Instant::now()),
            stats: Stats::new(),
            latency_test: LatencyTest::new(),
            latest_frame: Mutex::new(None),
            egui_ctx: OnceLock::new(),
            last_keyframe_request: Mutex::new(None),
            connected: Mutex::new(false),
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
        const MIN_INTERVAL: Duration = Duration::from_millis(2000);

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
            &Message::Client(ClientMessage::Pilot {
                slave_id: SLAVE.to_string(),
                cmd: PilotCommand::KeyframeRequest,
            }),
            PacketType::Reliable,
        ) {
            warn!(error = ?e, "failed to request keyframe");
        } else {
            debug!("Requested keyframe");
        }
    }

    /// Spawns the network-receive side of whichever `VideoReceiver` backend
    /// handles `mode`. The pipeline itself (`RawRtpUdp::build_pipeline`) was
    /// already built at window creation, before `mode` was known -- see
    /// `video::VideoReceiver` for why.
    fn start_video(self: Arc<AppState>, mode: VideoMode) {
        tokio::spawn(async move {
            if let Err(e) = RawRtpUdp::run(self.clone(), mode).await {
                error!(error = ?e, "video receive loop failed");
            }
        });
    }

    fn announce_loop(&self) -> Result<()> {
        let mut last_resubscribe = Instant::now();
        common::link::run_announce_loop(
            &self.socket,
            &self.server,
            &self.ping,
            Duration::from_millis(100),
            |packet_id| {
                Message::Client(ClientMessage::Announce {
                    uuid: self.uuid.lock().to_string(),
                    packet_id,
                    bitrate: *self.stats.target_bitrate.lock(),
                })
            },
            || {
                // The server stays silent on MasterAnnounce when it doesn't
                // recognize our uuid (see server's MasterAnnounce handler) --
                // most likely because it restarted and lost our
                // registration. Re-send MasterSubscribe, throttled so a
                // genuinely stale link doesn't turn into a resubscribe every
                // 100ms.
                if self.last_pong.lock().elapsed() > PONG_STALE
                    && last_resubscribe.elapsed() > PONG_STALE
                {
                    warn!("No pong from server in {PONG_STALE:?}, resubscribing");
                    if let Err(e) = self.socket.send(
                        &self.server,
                        &Message::Client(ClientMessage::Subscribe {
                            slave_id: SLAVE.to_string(),
                        }),
                        PacketType::Reliable,
                    ) {
                        warn!(error = ?e, "failed to resubscribe");
                    }
                    last_resubscribe = Instant::now();
                }
            },
        )
    }

    async fn run_abr(self: Arc<AppState>) {
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        let mut bitrate: u32 = 500_000;

        loop {
            interval.tick().await;

            let losses = *self.stats.loss.lock();
            let decode_latency = *self.stats.decode_latency.lock();
            let loss = losses.num_lost as f32 / (losses.num_pushed as f32).max(1f32);

            // AIMD
            bitrate = if loss > 0.05 {
                (bitrate as f64 * 0.6) as u32 // cut 40% on bad loss
            } else if loss > 0.02 {
                (bitrate as f64 * 0.75) as u32 // cut 25% on mild loss
            } else if decode_latency > 10.0 {
                bitrate // hold if decoder struggling
            } else {
                bitrate + 25_000 // additive increase
            };

            bitrate = bitrate.clamp(100_000, 3_000_000);

            tracing::info!(
                "ABR: loss={:.1}% latency={:.1}ms → {}kbps",
                loss * 100.0,
                decode_latency,
                bitrate / 1000
            );

            *self.stats.target_bitrate.lock() = bitrate;
        }
    }

    async fn handle_messages(
        self: &Arc<AppState>,
        _addr: SocketAddr,
        msg: &ArchivedMessage,
    ) -> Result<()> {
        let ArchivedMessage::ToClient(msg) = msg else {
            // A client only ever legitimately receives messages addressed to it.
            return Ok(());
        };

        match msg {
            ArchivedServerToClient::Connected {
                uuid,
                video,
                audio_port: _,
            } => {
                info!("Connected to the server");
                *self.uuid.lock() = Uuid::from_str(uuid)?;
                *self.last_pong.lock() = Instant::now();
                self.stats.mark_connected();

                let mode = match video {
                    ArchivedVideoMode::RawRtpUdp { video_port } => VideoMode::RawRtpUdp {
                        video_port: video_port.into(),
                    },
                };
                self.clone().start_video(mode);

                // The stream is already running and its opening IDR is long
                // gone, so without this we would decode nothing until the GOP
                // fallback fires.
                self.request_keyframe();

                // Only spawn these the first time: a resubscribe (server
                // restart) re-enters this arm with a fresh uuid/video_port,
                // but announce_loop/run_abr already run against `self` and
                // would just duplicate traffic if spawned again.
                let mut connected = self.connected.lock();
                if !*connected {
                    *connected = true;
                    drop(connected);

                    let state_clone = self.clone();
                    tokio::spawn(async move { state_clone.announce_loop() });
                    let state_clone = self.clone();
                    tokio::spawn(async move { state_clone.run_abr().await });
                }
            }
            ArchivedServerToClient::Pong { latency, packet_id } => {
                *self.last_pong.lock() = Instant::now();

                let latency: u16 = latency.into();
                self.stats.note_slave_ping(latency);

                if let Some(rtt) = self.ping.note_pong(packet_id.into()) {
                    self.stats.note_client_ping(rtt.as_millis() as f64);
                }
            }
            ArchivedServerToClient::SlaveStats {
                capture_latency_ms,
                encode_latency_ms,
                total_latency_ms,
                bitrate_bps,
            } => {
                self.stats.note_slave_stats(
                    capture_latency_ms.into(),
                    encode_latency_ms.into(),
                    total_latency_ms.into(),
                    bitrate_bps.into(),
                );
            }
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
        &Message::Client(ClientMessage::Subscribe {
            slave_id: SLAVE.to_string(),
        }),
        PacketType::Reliable,
    )?;

    loop {
        if let Err(e) = state.fetch_packets().await {
            error!(error = ?e, "packet fetching failed, dropping packet");
        }
    }
}
