use anyhow::{Context, Result};
use common::link::PingTracker;
use common::{
    ArchivedMessage, ArchivedServerToClient, ArchivedVideoMode, ClientMessage, Message, PacketType,
    PilotCommand, Socket, SocketEvent, VideoMode,
};
use dotenv::dotenv;
use egui::ColorImage;
use gstreamer_app as gst_app;
use parking_lot::Mutex;
use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
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
        // Reduced from 5s: under sustained loss the decoder stays broken until
        // an IDR arrives, and 5s of glitched video is unacceptable for FPV.
        // 1s still limits burst frequency while allowing faster recovery.
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
                    fec_pct: *self.stats.target_fec_pct.lock(),
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

    /// Adapts both video bitrate (AIMD) and FEC overhead to current link
    /// conditions, replacing what used to be a bitrate-only `run_abr`.
    ///
    /// Both decisions read `Stats::windowed_loss_pct` -- a ratio over a short,
    /// decaying ~2s window (see `stats.rs`/`video.rs`'s RTP sequence-number
    /// tracking) -- rather than `Stats::loss` (cumulative jitterbuffer stats
    /// since the pipeline started). The old code used the cumulative ratio:
    /// after running an hour, a brief bad patch barely moved it since the
    /// denominator was already huge, so the bitrate cut lagged the real
    /// condition of the link by however long the session had been running.
    /// The windowed ratio reacts to *current* loss regardless of session age.
    async fn run_adaptive(self: Arc<AppState>) {
        // Matches the loss-tracking window in `stats.rs`'s `tick_loss_window`
        // call, so every tick here sees a freshly recomputed ratio.
        const TICK: Duration = Duration::from_secs(2);
        // Additive increase expressed per second of real time, not per tick:
        // a delayed tick (executor stall, GC-style pause) shouldn't silently
        // slow the ramp, and a fast one shouldn't overshoot it. At the
        // nominal 2s tick this is the same +25kbps/tick as before.
        const ADDITIVE_INCREASE_BPS_PER_SEC: f64 = 12_500.0;
        // Hysteresis for stepping FEC back down: step up immediately on the
        // first window crossing a threshold, but only step down after this
        // many consecutive windows below it. Loss on cellular links is
        // bursty -- flapping the FEC rate is worse than briefly
        // over-protecting.
        const CLEAN_STREAK_TO_STEP_DOWN: u32 = 5;

        let mut interval = tokio::time::interval(TICK);
        let mut link_estimate: u32 = 1_000_000;
        // Non-zero baseline: the first loss burst must already have FEC
        // protection, not trigger a ramp-up that arrives too late. 5% overhead
        // is ~50kbps at 1Mbps video -- negligible cost for artifact prevention.
        let mut fec_pct: u8 = 5;
        let mut clean_streak: u32 = 0;
        let mut last_tick = Instant::now();

        loop {
            interval.tick().await;
            let elapsed = last_tick.elapsed();
            last_tick = Instant::now();

            let loss_pct = self.stats.windowed_loss_pct();
            let decode_latency = *self.stats.decode_latency.lock();

            // AIMD on the link's estimated uplink capacity. FEC overhead is
            // budgeted out of this separately below -- it never feeds back
            // into `link_estimate` itself, so enabling FEC doesn't make the
            // AIMD think the link got worse.
            link_estimate = if loss_pct > 5.0 {
                (link_estimate as f64 * 0.85) as u32 // cut 15% on bad loss
            } else if loss_pct > 2.5 {
                (link_estimate as f64 * 0.95) as u32 // cut 5% on mild loss
            } else if decode_latency > 10.0 {
                link_estimate // hold if decoder struggling
            } else {
                link_estimate + (ADDITIVE_INCREASE_BPS_PER_SEC * elapsed.as_secs_f64()) as u32
            };
            link_estimate = link_estimate.clamp(100_000, 3_000_000);

            // Floor at 5%: dropping to 0% leaves the stream unprotected until
            // the next loss event triggers a ramp-up, causing visible artifacts.
            let target_fec: u8 = if loss_pct > 5.0 {
                30
            } else if loss_pct > 2.0 {
                20
            } else if loss_pct > 0.2 {
                10
            } else {
                5 // never below 5%
            };

            if target_fec > fec_pct {
                fec_pct = target_fec;
                clean_streak = 0;
            } else if target_fec < fec_pct {
                clean_streak += 1;
                if clean_streak >= CLEAN_STREAK_TO_STEP_DOWN {
                    fec_pct = target_fec;
                    clean_streak = 0;
                }
            } else {
                clean_streak = 0;
            }

            // FEC overhead must come out of the video bitrate, not on top of
            // it -- otherwise enabling FEC under loss increases uplink
            // pressure exactly when the link is struggling. `link_estimate`
            // is the AIMD's estimate of what the link can carry in total;
            // this is the slice of it actually left for video once FEC's
            // share is set aside.
            let video_bitrate_cmd = (link_estimate as f64 / (1.0 + fec_pct as f64 / 100.0)) as u32;

            tracing::info!(
                "Adaptive: loss={:.2}% latency={:.1}ms → link={}kbps video={}kbps fec={}%",
                loss_pct,
                decode_latency,
                link_estimate / 1000,
                video_bitrate_cmd / 1000,
                fec_pct
            );

            *self.stats.link_estimate_bitrate.lock() = link_estimate;
            *self.stats.target_bitrate.lock() = video_bitrate_cmd;
            *self.stats.target_fec_pct.lock() = fec_pct;
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
                    tokio::spawn(async move { state_clone.run_adaptive().await });
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
