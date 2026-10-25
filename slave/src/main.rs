use anyhow::Context;
use anyhow::Result;
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
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio_serial::SerialPortBuilderExt;
use tokio_serial::SerialStream;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::level_filters::LevelFilter;
use tracing::warn;

use common::link::PingTracker;
use common::transport::{Role, Transport, TransportConfig, channel};
use common::{
    ArchivedMessage, ArchivedPilotCommand, ArchivedServerToSlave, Candidate, Message, SlaveMessage,
    Socket, SocketEvent,
};
use rkyv::rancor::Error as RkyvError;

mod audio;
mod media;
mod pump;
use crate::audio::AudioSource;
use crate::media::{RpicamStreamer, StreamerStats, VideoSource};

const SERVER: &str = "server:9500";
const SERIAL_PORT: &str = "/dev/ttyUSB0";
const SERIAL_BAUD: u32 = 115200;

struct AppState {
    id: String,
    socket: Socket,
    server: SocketAddr,
    queue: AsyncQueue<()>,
    serial: AsyncMutex<Option<SerialStream>>,
    // Async mutex: `request_keyframe`/`set_bitrate` are async trait calls
    // made while holding this, which a sync `parking_lot::Mutex` guard
    // can't safely straddle.
    media: AsyncMutex<Option<RpicamStreamer>>,
    audio: Mutex<Option<AudioSource>>,
    /// The path-selected data plane carrying video, audio and pilot input.
    /// `None` until the server's `Connected` hands over the relay port and
    /// session token.
    transport: Mutex<Option<Arc<Transport>>>,
    connected: Mutex<bool>,
    latency: Mutex<Duration>,
    ping: PingTracker,
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
            media: AsyncMutex::new(None),
            audio: Mutex::new(None),
            transport: Mutex::new(None),
            connected: Mutex::new(false),
            latency: Mutex::new(Duration::from_secs(0)),
            ping: PingTracker::new(),
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

    fn setup_media(self: Arc<AppState>, rtp_port: u16) -> Result<()> {
        let state = self.clone();
        self.queue.submit(async move {
            let stats_state = state.clone();
            if let Ok(media) = state
                .retry("media setup", u32::MAX, Duration::from_secs(3), || {
                    debug!("Trying to setup MEDIA");
                    let stats_state = stats_state.clone();
                    RpicamStreamer::start(rtp_port, move |stats: StreamerStats| {
                        let _ = stats_state.socket.send(
                            &stats_state.server,
                            &Message::Slave(SlaveMessage::Stats {
                                id: stats_state.id.clone(),
                                capture_latency_ms: stats.capture_latency_ms,
                                encode_latency_ms: stats.encode_latency_ms,
                                total_latency_ms: stats.total_latency_ms,
                                bitrate_bps: stats.bitrate_bps,
                            }),
                            PacketType::Unreliable,
                        );
                    })
                })
                .await
            {
                *state.media.lock().await = Some(media);
            }
        });
        Ok(())
    }

    /// Start audio streaming (best-effort, no retry).
    /// Audio is independent from video: failures are logged but don't affect video.
    fn setup_audio(&self, rtp_port: u16) {
        *self.audio.lock() = AudioSource::start(rtp_port);
    }

    /// Brings up the data plane and everything that feeds it.
    ///
    /// Ordering matters: the transport has to exist before the loopback pumps
    /// (they hold it), and the pumps have to exist before the producers (they
    /// need the ports). The relay path works from the first packet, so video
    /// flows before any punching has been attempted -- direct paths only ever
    /// take over from something already working.
    fn setup_transport(self: &Arc<AppState>, config: TransportConfig) -> Result<()> {
        let relay = SocketAddr::new(self.server.ip(), config.relay_port);

        // A server restart mints a new token and relay port and re-delivers
        // `Connected`. Re-homing the existing socket keeps the camera running
        // and the pumps pointed somewhere valid; rebinding would mean
        // restarting the streamer, and a camera re-open is seconds of blackout.
        if let Some(transport) = self.transport.lock().as_ref() {
            transport.reconfigure(relay, config.token);
            return Ok(());
        }

        let transport = Transport::bind(Role::Slave, relay, config.token)
            .context("failed to bind the data-plane socket")?;

        let video_port = pump::spawn("video", channel::VIDEO, transport.clone())
            .context("failed to start the video pump")?;
        let audio_port = pump::spawn("audio", channel::AUDIO, transport.clone())
            .context("failed to start the audio pump")?;

        // Bounded, and dropped on overflow rather than blocked on: pilot input
        // is a set-point stream, so a stalled serial port should cost the
        // freshest command nothing. The Arduino's own watchdog is what turns a
        // real gap into a safe stop -- see CLAUDE.md.
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<(i16, i16)>(8);

        let state = self.clone();
        tokio::spawn(async move {
            while let Some((steering, throttle)) = input_rx.recv().await {
                if let Err(e) = state.write_pilot(steering, throttle).await {
                    warn!(error = ?e, "failed to write pilot command to serial");
                }
            }
        });

        let candidate_state = self.clone();
        transport.start(
            move |tag, payload| {
                if tag == channel::INPUT
                    && let Some((throttle, steering)) = common::decode_input(payload)
                {
                    let _ = input_tx.try_send((steering, throttle));
                }
            },
            move |candidates| candidate_state.publish_candidates(candidates),
        );

        *self.transport.lock() = Some(transport);

        self.setup_audio(audio_port);
        self.clone().setup_media(video_port)?;
        Ok(())
    }

    /// Tells the server where this car can be reached, for it to pass on to
    /// the driver's client. Sent reliably: a dropped candidate list is a
    /// direct path that never gets tried, and the next send only happens if
    /// the address set changes again.
    fn publish_candidates(&self, candidates: Vec<Candidate>) {
        debug!("Publishing {} candidate(s)", candidates.len());
        if let Err(e) = self.socket.send(
            &self.server,
            &Message::Slave(SlaveMessage::Candidates {
                id: self.id.clone(),
                candidates,
            }),
            PacketType::Reliable,
        ) {
            warn!(error = ?e, "failed to publish candidates");
        }
    }

    /// The one place a pilot set-point reaches the Arduino, shared by the
    /// data plane (the normal route) and the server-relayed control channel.
    async fn write_pilot(&self, steering: i16, throttle: i16) -> Result<()> {
        debug!("received pilot {throttle} {steering}");
        let line = format!("{steering:} {throttle:}\n");

        if let Some(serial) = &mut *self.serial.lock().await {
            serial.write_all(line.as_bytes()).await?;
            serial.flush().await?;
        }
        Ok(())
    }

    async fn handle_messages(self: Arc<AppState>, msg: &ArchivedMessage) -> Result<()> {
        let ArchivedMessage::ToSlave(msg) = msg else {
            // A slave only ever legitimately receives messages addressed to it.
            return Ok(());
        };

        match msg {
            // Fallback route only: input normally arrives on the data plane,
            // which skips the server entirely whenever a direct path is up.
            ArchivedServerToSlave::Pilot(ArchivedPilotCommand::Input { throttle, steering }) => {
                self.write_pilot(steering.into(), throttle.into()).await?;
            }

            ArchivedServerToSlave::Connected { transport } => {
                let config: TransportConfig = rkyv::deserialize::<_, RkyvError>(transport)
                    .context("failed to deserialize the transport config")?;

                // Handled every time, not just the first: a server restart
                // re-sends this with a new relay port and token, and ignoring
                // it would leave the car streaming into a port nobody is
                // relaying any more. `setup_transport` re-homes rather than
                // rebuilds, so repeating it is cheap.
                if !*self.connected.lock() {
                    info!("We are connected !");
                    *self.connected.lock() = true;
                }
                self.setup_transport(config)?;
            }

            ArchivedServerToSlave::PeerCandidates { candidates } => {
                let candidates: Vec<Candidate> = rkyv::deserialize::<_, RkyvError>(candidates)
                    .context("failed to deserialize peer candidates")?;
                if let Some(transport) = self.transport.lock().as_ref() {
                    transport.set_peer_candidates(&candidates);
                }
            }

            ArchivedServerToSlave::Pilot(ArchivedPilotCommand::KeyframeRequest) => {
                if let Some(media) = self.media.lock().await.as_ref() {
                    debug!("Keyframe requested by client");
                    media.request_keyframe().await?;
                }
            }

            ArchivedServerToSlave::Pong {
                packet_id,
                bitrate,
                fec_pct,
            } => {
                if let Some(rtt) = self.ping.note_pong(packet_id.into()) {
                    *self.latency.lock() = rtt;
                }
                if let Some(media) = self.media.lock().await.as_ref() {
                    media.set_bitrate(bitrate.into()).await?;
                    media.set_fec((*fec_pct).into()).await?;
                }
            }

            ArchivedServerToSlave::Error { .. } => {}
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
        common::link::run_announce_loop(
            &self.socket,
            &self.server,
            &self.ping,
            Duration::from_millis(100),
            |packet_id| {
                Message::Slave(SlaveMessage::Announce {
                    id: self.id.clone(),
                    latency: self.latency.lock().as_millis() as u16,
                    packet_id,
                })
            },
            || {},
        )
    }

    async fn send_err(&self, err: String) -> Result<()> {
        self.socket.send(
            &self.server,
            &Message::Slave(SlaveMessage::Error {
                id: self.id.clone(),
                err,
            }),
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
        // A single malformed/unexpected packet must never take this loop
        // down: on the car, that means losing the control link and the
        // server connection with no way to recover short of a process
        // restart, while a cellular blip is a certainty, not an edge case.
        loop {
            if let Err(e) = state_clone.clone().fetch_packets().await {
                error!(error = ?e, "packet fetching failed, dropping packet");
            }
        }
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
