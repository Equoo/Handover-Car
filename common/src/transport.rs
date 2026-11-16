// SPDX-FileCopyrightText: 2026 David Derny
// SPDX-FileCopyrightText: 2026 Handover contributors (see AUTHORS)
//
// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0

//! Runtime path selection for the latency-critical data plane.
//!
//! Video, audio and pilot input all share one UDP socket per peer, tagged with
//! a one-byte channel id. That socket is what gets steered: the transport
//! keeps a list of candidate addresses for the peer, probes all of them
//! continuously, and points every outgoing datagram at whichever one is
//! currently best.
//!
//! Three kinds of path exist, in the order they win:
//!
//! - **Host** -- an address the peer advertised for one of its own interfaces.
//!   On a LAN (the bring-up setup in CLAUDE.md: Pi and PC on the same box)
//!   this needs no traversal at all and lands around 1-3ms. A global IPv6
//!   address is a host candidate too, and cellular IPv6 is usually handed out
//!   un-NATed -- so when both ends have v6 the "hard" case turns into the easy
//!   one and no punching happens at all.
//! - **Reflexive** -- the public address the relay observed the peer sending
//!   from. Reaching it means punching: both sides fire probes at each other on
//!   the same cadence, so each one's NAT already has an outbound mapping for
//!   the 5-tuple when the other's packet arrives. Works often enough on
//!   carrier NAT to be worth always trying, never often enough to rely on.
//! - **Relay** -- the server forwards for us, exactly as it always has. Never
//!   fails, always pays the detour.
//!
//! The relay is not a last resort switched on after direct fails: it is live
//! from the first packet, and a direct path only takes over once it has
//! actually answered a probe. Losing the direct path falls straight back with
//! no renegotiation, which matters because on a moving car the direct path is
//! *expected* to die -- a cell handover changes the public address and every
//! punched mapping with it.
//!
//! Switching direction is free and needs no coordination: the receiving side
//! accepts datagrams from any source address, so packets already in flight on
//! the old path still land. That is also why nothing is duplicated across
//! paths during a switch -- there is no gap to paper over.
//!
//! # Threading
//!
//! Blocking `std::net::UdpSocket` on two dedicated threads (receive, probe),
//! matching the pattern `Socket::start` already uses for the laminar control
//! socket. Sends go straight out from whatever thread calls `send`, so the
//! egui frame loop can push a pilot command without an async hop. A dedicated
//! receive thread also removes the executor-scheduling delay that the old
//! tokio receive loop fed into `do-timestamp` as fake jitter (see the note on
//! `do-timestamp` in `client/src/video.rs`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use rkyv::{Archive, Deserialize, Serialize};
use tracing::{debug, info, warn};

/// One-byte tag prefixed to every datagram on the data plane. The relay
/// forwards anything it does not recognise as its own verbatim, so these
/// survive the relayed path unchanged and both sides demux identically no
/// matter which path a packet came in on.
pub mod channel {
    /// Connectivity check. Also the punch itself: sending one is what opens
    /// the local NAT mapping the peer's probe needs to find.
    pub const PROBE: u8 = 0x01;
    /// Echo of a `PROBE`, sent straight back to its source address.
    pub const PROBE_ACK: u8 = 0x02;
    /// Sent to the relay only: claims one of the two sides of a session.
    pub const REGISTER: u8 = 0x03;
    /// Relay's answer to `REGISTER`, carrying the source address it saw.
    pub const REGISTER_ACK: u8 = 0x04;
    /// RTP video, car to client.
    pub const VIDEO: u8 = 0x05;
    /// RTP Opus audio, car to client.
    pub const AUDIO: u8 = 0x06;
    /// Pilot input set-point, client to car.
    pub const INPUT: u8 = 0x07;
}

pub const TOKEN_LEN: usize = 16;

/// Per-session shared secret, minted by the server and handed to both sides
/// over the reliable control channel. Probes carry it so that a stray packet
/// from the internet at large cannot register itself as a peer-reflexive
/// candidate and pull the media flow onto a path pointing at an attacker.
pub type SessionToken = [u8; TOKEN_LEN];

/// Which side of a session a transport is. The relay needs this to know which
/// of its two slots an incoming registration claims.
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq, Eq, Clone, Copy)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum Role {
    Slave,
    Client,
}

impl Role {
    fn to_wire(self) -> u8 {
        match self {
            Role::Slave => 0,
            Role::Client => 1,
        }
    }

    pub fn from_wire(b: u8) -> Option<Self> {
        match b {
            0 => Some(Role::Slave),
            1 => Some(Role::Client),
            _ => None,
        }
    }
}

/// How a candidate address was learned. Only affects logging and the
/// tie-break in `select`: what actually decides a path is whether it answers
/// probes and how fast.
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq, Eq, Clone, Copy)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum CandidateKind {
    /// An address of one of the peer's own interfaces, as it sees them.
    Host,
    /// The peer's public address as observed by the relay.
    Reflexive,
    /// An address a valid probe actually arrived from, which the peer never
    /// advertised. This is the symmetric-NAT case: the peer's mapping towards
    /// *us* differs from the one the relay saw, so its advertised reflexive
    /// candidate is wrong and only the packet itself reveals the real one.
    PeerReflexive,
}

/// One address the peer might be reachable at.
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq, Clone, Copy)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub struct Candidate {
    pub addr: SocketAddr,
    pub kind: CandidateKind,
}

/// Everything a peer needs to join a session's data plane, negotiated once
/// through the control channel's connect handshake.
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq, Clone, Copy)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub struct TransportConfig {
    /// Port on the server running this session's two-party relay.
    pub relay_port: u16,
    pub token: SessionToken,
}

/// Where the transport is currently sending.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Route {
    Relay,
    Direct(SocketAddr),
}

/// Probe cadence, applied to *every* candidate equally. It has to be well
/// under `PATH_TIMEOUT` so a path is only declared dead after several probes
/// in a row went unanswered, not after one unlucky drop.
///
/// Uniform on purpose. An earlier version probed the selected path at this
/// rate and idle candidates five times slower, to save uplink -- which
/// silently rigged every comparison it then made. The incumbent, sampled
/// often, had an EWMA that tracked the link honestly and drifted up on any
/// slow sample; an idle candidate, sampled rarely, kept whatever lucky low
/// value it last happened to get. The result was a standing bias against
/// whichever path was in use, and the route rotated between equally good
/// candidates every second or two. Comparing two averages only means
/// something when they are built the same way.
///
/// The uplink it costs is real but small: ~50 bytes per candidate per tick,
/// so a handful of candidates is under 2 kB/s against a video budget in
/// megabits, bounded by `MAX_CANDIDATES`.
const PROBE_INTERVAL: Duration = Duration::from_millis(100);
/// Cadence for a candidate that has never once answered. Backing those off is
/// safe where backing off a working one is not: a candidate with no RTT at all
/// is never `alive`, so it never enters a comparison and cannot be biased by
/// how often it was sampled. What it saves is real -- a laptop advertises its
/// docker bridges, a phone-tethered car advertises addresses the peer's NAT
/// will never admit, and none of those will ever reply. Probing a handful of
/// dead addresses ten times a second is tens of kbps taken from video on the
/// exact uplink that is already the bottleneck.
const COLD_PROBE_INTERVAL: Duration = Duration::from_millis(2000);
/// How many unanswered probes in a row before a candidate is treated as cold.
/// At `PROBE_INTERVAL` this is two seconds of trying, which is far longer than
/// a punch needs when it is going to work at all.
const COLD_AFTER: u32 = 20;
/// Re-registering keeps the relay's idea of our address current across a NAT
/// rebind, and its reply re-measures our reflexive address for free.
const REGISTER_INTERVAL: Duration = Duration::from_secs(1);
/// How long a path may go without any inbound packet before it stops counting
/// as usable. At `PROBE_INTERVAL` that is four unanswered probes.
const PATH_TIMEOUT: Duration = Duration::from_millis(500);
/// Weight of each new RTT sample. Low enough that one outlier can't trigger a
/// switch on its own, high enough to track a genuinely degrading path.
const RTT_ALPHA: f64 = 0.25;
/// A challenger must be this much better than the incumbent to displace it.
/// Without a margin two paths within noise of each other flap continuously.
const SWITCH_MARGIN: f64 = 0.8;
/// ...and better by at least this many milliseconds in absolute terms, and
/// ...it must stay better for this many consecutive evaluations.
///
/// The relative margin alone is not enough. On a LAN, where every candidate
/// answers in a fraction of a millisecond, 0.1ms against 0.2ms clears a 20%
/// bar on nothing but measurement noise -- observed in practice as a switch
/// every second or two between a host address, a loopback address and an IPv6
/// address that were all equally good. A floor expressed in milliseconds says
/// the thing actually meant: below a couple of milliseconds the difference
/// cannot be felt through a 30ms link, so it is not worth the reordering a
/// switch causes.
const MIN_SWITCH_GAIN_MS: f64 = 2.0;
const SWITCH_STREAK: u32 = 3;
/// Guards against a pathological interface list (VM bridges, docker, every
/// address of a multi-homed host) turning into a probe storm.
const MAX_CANDIDATES: usize = 8;

struct Path {
    addr: SocketAddr,
    kind: CandidateKind,
    /// EWMA over `RTT_ALPHA`. `None` until the first ack comes back.
    rtt_ms: Option<f64>,
    /// Last time *anything* arrived from this address, probe ack or media.
    /// Counting media too means a path busy carrying video is never declared
    /// dead just because a probe ack was the packet that got dropped.
    last_rx: Option<Instant>,
    last_probe: Option<Instant>,
    /// Consecutive probes sent with no answer, reset by any ack. Drives the
    /// cold backoff only -- liveness is `last_rx`, which media traffic also
    /// refreshes.
    unanswered: u32,
}

impl Path {
    /// Slow down only if nothing has ever come back from this address. A path
    /// that worked and then stopped keeps the fast cadence: it is a live
    /// fallback candidate, and noticing it has recovered quickly is the whole
    /// point of still probing it.
    fn probe_interval(&self) -> Duration {
        if self.rtt_ms.is_none() && self.unanswered >= COLD_AFTER {
            COLD_PROBE_INTERVAL
        } else {
            PROBE_INTERVAL
        }
    }

    fn alive(&self, now: Instant) -> bool {
        self.rtt_ms.is_some()
            && self
                .last_rx
                .is_some_and(|t| now.saturating_duration_since(t) < PATH_TIMEOUT)
    }
}

pub struct Transport {
    socket: Arc<UdpSocket>,
    role: Role,
    /// Session parameters, swappable in place. When the server restarts it
    /// mints a new token and relay port, and both peers are told about it
    /// through a fresh `Connected`. Rebinding the socket in response would
    /// throw away the one thing worth keeping across that -- the NAT mapping
    /// it holds, which took a round trip through the relay to discover and
    /// which the peer may already be punching towards.
    token: Mutex<SessionToken>,
    relay: Mutex<SocketAddr>,
    /// True when the socket is IPv6, in which case every IPv4 destination has
    /// to be rewritten as v4-mapped before it can be sent to.
    v6: bool,
    /// Monotonic origin for the timestamps echoed through probes. Probes carry
    /// our own clock and get it handed straight back, so an RTT needs no
    /// per-probe state and no clock agreement between the two sides.
    epoch: Instant,
    txid: AtomicU64,
    paths: Mutex<Vec<Path>>,
    route: Mutex<Route>,
    switch_streak: Mutex<u32>,
    reflexive: Mutex<Option<SocketAddr>>,
    relay_rtt_ms: Mutex<Option<f64>>,
}

impl Transport {
    /// Binds the data-plane socket. Prefers a dual-stack IPv6 socket so a
    /// native v6 path can be used when both ends have one, and falls back to
    /// IPv4-only if the host has `bindv6only` set or no v6 stack at all --
    /// binding v6 and silently losing v4 would take the relay down with it.
    pub fn bind(role: Role, relay: SocketAddr, token: SessionToken) -> Result<Arc<Self>> {
        let (socket, v6) = match UdpSocket::bind("[::]:0") {
            Ok(socket) if dual_stack_works(relay) => (socket, true),
            Ok(_) => {
                debug!("IPv6 socket is v6-only, falling back to IPv4 for the data plane");
                (
                    UdpSocket::bind("0.0.0.0:0").context("failed to bind data-plane socket")?,
                    false,
                )
            }
            Err(e) => {
                debug!(error = ?e, "no IPv6 data-plane socket, using IPv4");
                (
                    UdpSocket::bind("0.0.0.0:0").context("failed to bind data-plane socket")?,
                    false,
                )
            }
        };

        let transport = Arc::new(Self {
            socket: Arc::new(socket),
            role,
            token: Mutex::new(token),
            relay: Mutex::new(relay),
            v6,
            epoch: Instant::now(),
            txid: AtomicU64::new(0),
            paths: Mutex::new(Vec::new()),
            route: Mutex::new(Route::Relay),
            switch_streak: Mutex::new(0),
            reflexive: Mutex::new(None),
            relay_rtt_ms: Mutex::new(None),
        });

        info!(
            "Data plane on port {} ({}), relaying through {relay} until a direct path answers",
            transport.local_port(),
            if v6 { "dual-stack" } else { "IPv4" },
        );
        Ok(transport)
    }

    pub fn relay(&self) -> SocketAddr {
        *self.relay.lock()
    }

    /// Points this socket at a new session without rebinding it.
    ///
    /// Every candidate is dropped: they belong to the previous session and
    /// would fail the new token check anyway. The route goes back to the
    /// relay, which is always the safe state -- it works before anything has
    /// been probed.
    pub fn reconfigure(&self, relay: SocketAddr, token: SessionToken) {
        if *self.relay.lock() == relay && *self.token.lock() == token {
            return;
        }
        info!("Data plane re-homed to relay {relay}, restarting path discovery");
        *self.relay.lock() = relay;
        *self.token.lock() = token;
        self.paths.lock().clear();
        *self.route.lock() = Route::Relay;
        *self.switch_streak.lock() = 0;
        *self.reflexive.lock() = None;
        *self.relay_rtt_ms.lock() = None;
    }

    pub fn local_port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn route(&self) -> Route {
        *self.route.lock()
    }

    /// Round-trip over the path currently in use, for the overlay. The relay
    /// figure is measured by the register/ack exchange rather than a probe,
    /// which is the same round trip a media packet makes to the server.
    pub fn rtt_ms(&self) -> Option<f64> {
        match self.route() {
            Route::Relay => *self.relay_rtt_ms.lock(),
            Route::Direct(addr) => self
                .paths
                .lock()
                .iter()
                .find(|p| p.addr == addr)
                .and_then(|p| p.rtt_ms),
        }
    }

    /// Short human label for the overlay, e.g. `direct/host 192.168.1.42:41234`.
    pub fn route_label(&self) -> String {
        match self.route() {
            Route::Relay => format!("relay {}", self.relay()),
            Route::Direct(addr) => {
                let kind = self
                    .paths
                    .lock()
                    .iter()
                    .find(|p| p.addr == addr)
                    .map(|p| match p.kind {
                        CandidateKind::Host => "host",
                        CandidateKind::Reflexive => "punched",
                        CandidateKind::PeerReflexive => "punched*",
                    })
                    .unwrap_or("?");
                format!("direct/{kind} {addr}")
            }
        }
    }

    /// Addresses to advertise to the peer: every usable local interface, plus
    /// the public address the relay reported seeing us at once that arrives.
    pub fn local_candidates(&self) -> Vec<Candidate> {
        let port = self.local_port();
        let mut candidates: Vec<Candidate> = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .map(|iface| iface.ip())
            .filter(|ip| is_advertisable(*ip))
            .map(|ip| Candidate {
                addr: SocketAddr::new(ip, port),
                kind: CandidateKind::Host,
            })
            .collect();

        if let Some(addr) = *self.reflexive.lock() {
            candidates.push(Candidate {
                addr,
                kind: CandidateKind::Reflexive,
            });
        }

        candidates.truncate(MAX_CANDIDATES);
        candidates
    }

    /// Merges what the peer advertised into the probe list. Additive on
    /// purpose: a candidate that has been working is not dropped just because
    /// it fell out of a later advertisement, and peer-reflexive candidates
    /// discovered locally are never in the peer's list at all.
    pub fn set_peer_candidates(&self, candidates: &[Candidate]) {
        let mut paths = self.paths.lock();
        for candidate in candidates {
            // Our own addresses come back to us whenever both ends sit on the
            // same LAN; probing ourselves would produce a path that looks
            // perfect and delivers video nowhere.
            if candidate.addr.port() == self.local_port() && is_local_ip(&candidate.addr.ip()) {
                continue;
            }
            if paths.iter().any(|p| p.addr == candidate.addr) {
                continue;
            }
            if paths.len() >= MAX_CANDIDATES {
                break;
            }
            debug!("New {:?} candidate {}", candidate.kind, candidate.addr);
            paths.push(Path {
                addr: candidate.addr,
                kind: candidate.kind,
                rtt_ms: None,
                last_rx: None,
                last_probe: None,
                unanswered: 0,
            });
        }
    }

    /// Sends one datagram on the current path. Never blocks and never retries:
    /// this is the unreliable plane, and a socket that would block has a full
    /// queue, where the freshest frame is the one worth keeping.
    pub fn send(&self, channel: u8, payload: &[u8]) -> Result<()> {
        let target = match self.route() {
            Route::Relay => self.relay(),
            Route::Direct(addr) => addr,
        };
        self.send_to(target, channel, payload)
    }

    fn send_to(&self, addr: SocketAddr, channel: u8, payload: &[u8]) -> Result<()> {
        let mut datagram = Vec::with_capacity(payload.len() + 1);
        datagram.push(channel);
        datagram.extend_from_slice(payload);
        self.socket.send_to(&datagram, self.remap(addr))?;
        Ok(())
    }

    /// Rewrites an IPv4 destination as v4-mapped when the socket is IPv6.
    fn remap(&self, addr: SocketAddr) -> SocketAddr {
        match (self.v6, addr.ip()) {
            (true, IpAddr::V4(v4)) => SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), addr.port()),
            _ => addr,
        }
    }

    /// Starts the receive and probe threads. `on_payload` is handed the
    /// channel tag and the payload with the tag stripped, borrowed straight
    /// out of the receive buffer so nothing is copied on the way to a decoder.
    /// It runs on the receive thread, so it must not block for long.
    ///
    /// `on_local_candidates` fires whenever our own advertisable set changes
    /// (in practice: when the relay first reports our public address, and
    /// again after a NAT rebind moves it). The caller republishes it to the
    /// peer over the control channel.
    pub fn start(
        self: &Arc<Self>,
        on_payload: impl Fn(u8, &[u8]) + Send + 'static,
        on_local_candidates: impl Fn(Vec<Candidate>) + Send + 'static,
    ) {
        let receiver = self.clone();
        std::thread::Builder::new()
            .name("transport-rx".into())
            .spawn(move || receiver.recv_loop(on_payload))
            .expect("failed to spawn transport receive thread");

        let prober = self.clone();
        std::thread::Builder::new()
            .name("transport-probe".into())
            .spawn(move || prober.probe_loop(on_local_candidates))
            .expect("failed to spawn transport probe thread");
    }

    fn recv_loop(self: Arc<Self>, on_payload: impl Fn(u8, &[u8])) {
        let mut buf = [0u8; 1 << 16];
        loop {
            let (size, from) = match self.socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = ?e, "data-plane recv failed, dropping packet");
                    continue;
                }
            };
            if size < 1 {
                continue;
            }

            let from = unmap(from);
            let (tag, body) = (buf[0], &buf[1..size]);

            match tag {
                channel::PROBE => self.on_probe(from, body),
                channel::PROBE_ACK => self.on_probe_ack(from, body),
                channel::REGISTER_ACK => self.on_register_ack(body),
                channel::VIDEO | channel::AUDIO | channel::INPUT => {
                    self.note_rx(from);
                    on_payload(tag, body);
                }
                _ => debug!("Unknown data-plane channel {tag} from {from}"),
            }
        }
    }

    /// A probe is both a liveness check and the punch itself, so answering one
    /// from an address we've never heard of is how the symmetric-NAT case gets
    /// solved: the packet proves that address reaches us, whatever the relay
    /// thought the peer's mapping was.
    fn on_probe(&self, from: SocketAddr, body: &[u8]) {
        if !self.token_ok(body) {
            return;
        }
        self.note_rx(from);
        self.learn_peer_reflexive(from);
        if let Err(e) = self.send_to(from, channel::PROBE_ACK, body) {
            debug!(error = ?e, "failed to answer probe from {from}");
        }
    }

    fn on_probe_ack(&self, from: SocketAddr, body: &[u8]) {
        if !self.token_ok(body) || body.len() < TOKEN_LEN + 16 {
            return;
        }
        // [token 16][txid 8][our send time 8]
        let sent_us = u64::from_be_bytes(
            body[TOKEN_LEN + 8..TOKEN_LEN + 16]
                .try_into()
                .expect("slice is 8 bytes"),
        );
        let rtt_ms = self.elapsed_since_us(sent_us) as f64 / 1000.0;

        let mut paths = self.paths.lock();
        if let Some(path) = paths.iter_mut().find(|p| p.addr == from) {
            path.last_rx = Some(Instant::now());
            path.unanswered = 0;
            path.rtt_ms = Some(match path.rtt_ms {
                Some(prev) => prev * (1.0 - RTT_ALPHA) + rtt_ms * RTT_ALPHA,
                None => {
                    debug!("Path {from} answered in {rtt_ms:.1}ms");
                    rtt_ms
                }
            });
        }
    }

    fn on_register_ack(&self, body: &[u8]) {
        // [token 16][our send time 8][family 1][ip 16][port 2]
        if !self.token_ok(body) || body.len() < TOKEN_LEN + 8 + 19 {
            return;
        }
        let sent_us = u64::from_be_bytes(
            body[TOKEN_LEN..TOKEN_LEN + 8]
                .try_into()
                .expect("slice is 8 bytes"),
        );
        let rtt_ms = self.elapsed_since_us(sent_us) as f64 / 1000.0;
        {
            let mut relay_rtt = self.relay_rtt_ms.lock();
            *relay_rtt = Some(match *relay_rtt {
                Some(prev) => prev * (1.0 - RTT_ALPHA) + rtt_ms * RTT_ALPHA,
                None => rtt_ms,
            });
        }

        let Some(observed) = decode_addr(&body[TOKEN_LEN + 8..]) else {
            return;
        };
        let mut reflexive = self.reflexive.lock();
        if *reflexive != Some(observed) {
            match *reflexive {
                // A changed reflexive address means the NAT rebound us, which
                // on cellular happens without warning. Every punched mapping
                // the peer holds is stale, so this has to reach it.
                Some(old) => info!("Public address moved {old} -> {observed} (NAT rebind)"),
                None => info!("Public address is {observed}"),
            }
            *reflexive = Some(observed);
        }
    }

    fn token_ok(&self, body: &[u8]) -> bool {
        body.len() >= TOKEN_LEN && body[..TOKEN_LEN] == *self.token.lock()
    }

    fn note_rx(&self, from: SocketAddr) {
        let mut paths = self.paths.lock();
        if let Some(path) = paths.iter_mut().find(|p| p.addr == from) {
            path.last_rx = Some(Instant::now());
        }
    }

    fn learn_peer_reflexive(&self, from: SocketAddr) {
        let mut paths = self.paths.lock();
        if paths.iter().any(|p| p.addr == from) || paths.len() >= MAX_CANDIDATES {
            return;
        }
        info!("Discovered peer-reflexive candidate {from}");
        paths.push(Path {
            addr: from,
            kind: CandidateKind::PeerReflexive,
            rtt_ms: None,
            last_rx: Some(Instant::now()),
            last_probe: None,
            unanswered: 0,
        });
    }

    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    /// Saturating so an echoed timestamp from a previous epoch (or a corrupted
    /// one that passed the token check) can't underflow into a huge RTT that
    /// would poison the EWMA.
    fn elapsed_since_us(&self, sent_us: u64) -> u64 {
        self.now_us().saturating_sub(sent_us)
    }

    fn probe_loop(self: Arc<Self>, on_local_candidates: impl Fn(Vec<Candidate>)) {
        let mut last_register: Option<Instant> = None;
        let mut published: Vec<Candidate> = Vec::new();

        loop {
            let now = Instant::now();

            if last_register.is_none_or(|t| now.duration_since(t) >= REGISTER_INTERVAL) {
                if let Err(e) = self.send_register() {
                    debug!(error = ?e, "relay registration failed");
                }
                last_register = Some(now);
            }

            self.probe_due_paths(now);
            self.select();

            // Compared rather than driven off a flag so a rebind that happens
            // to land back on the same address doesn't cost a republish, and
            // so the peer's list is refreshed on any interface change too.
            let current = self.local_candidates();
            if current != published {
                published = current.clone();
                on_local_candidates(current);
            }

            std::thread::sleep(PROBE_INTERVAL);
        }
    }

    /// Registration doubles as the relay's keepalive and as our own reflexive
    /// discovery -- the relay answers with the source address it saw, which is
    /// the only way to learn the public address of *this* socket. The control
    /// socket's public address is a different NAT mapping and says nothing
    /// about this one.
    fn send_register(&self) -> Result<()> {
        let mut body = Vec::with_capacity(TOKEN_LEN + 9);
        body.extend_from_slice(&*self.token.lock());
        body.push(self.role.to_wire());
        body.extend_from_slice(&self.now_us().to_be_bytes());
        self.send_to(self.relay(), channel::REGISTER, &body)
    }

    fn probe_due_paths(&self, now: Instant) {
        let due: Vec<SocketAddr> = {
            let paths = self.paths.lock();
            paths
                .iter()
                .filter(|p| {
                    p.last_probe
                        .is_none_or(|t| now.saturating_duration_since(t) >= p.probe_interval())
                })
                .map(|p| p.addr)
                .collect()
        };

        for addr in due {
            let txid = self.txid.fetch_add(1, Ordering::Relaxed);
            let mut body = Vec::with_capacity(TOKEN_LEN + 16);
            body.extend_from_slice(&*self.token.lock());
            body.extend_from_slice(&txid.to_be_bytes());
            body.extend_from_slice(&self.now_us().to_be_bytes());

            if let Err(e) = self.send_to(addr, channel::PROBE, &body) {
                debug!(error = ?e, "probe to {addr} failed");
            }
            if let Some(path) = self.paths.lock().iter_mut().find(|p| p.addr == addr) {
                path.last_probe = Some(now);
                path.unanswered = path.unanswered.saturating_add(1);
            }
        }
    }

    /// Picks the route for the next batch of packets.
    ///
    /// Two rules, in this order: a dead incumbent is abandoned *immediately*
    /// (video going into a black hole is the one failure worth reacting to
    /// without hesitation), and a live incumbent is only displaced by a
    /// challenger that is clearly and repeatedly better.
    fn select(&self) {
        let now = Instant::now();
        let paths = self.paths.lock();

        let best = paths
            .iter()
            .filter(|p| p.alive(now))
            .min_by(|a, b| {
                a.rtt_ms
                    .unwrap_or(f64::MAX)
                    .total_cmp(&b.rtt_ms.unwrap_or(f64::MAX))
            })
            .map(|p| (p.addr, p.rtt_ms.unwrap_or(f64::MAX)));

        let mut route = self.route.lock();
        let mut streak = self.switch_streak.lock();

        let incumbent = match *route {
            Route::Direct(addr) => paths
                .iter()
                .find(|p| p.addr == addr && p.alive(now))
                .map(|p| (addr, p.rtt_ms.unwrap_or(f64::MAX))),
            Route::Relay => None,
        };

        let Some((incumbent_addr, incumbent_rtt)) = incumbent else {
            // Either we're on the relay and something direct just came up, or
            // the direct path we were on stopped answering.
            let next = best.map_or(Route::Relay, |(addr, _)| Route::Direct(addr));
            if *route != next {
                info!("Path: {:?} -> {:?}", *route, next);
                *route = next;
                *streak = 0;
            }
            return;
        };

        match best {
            Some((addr, rtt))
                if addr != incumbent_addr
                    && rtt < incumbent_rtt * SWITCH_MARGIN
                    && rtt + MIN_SWITCH_GAIN_MS < incumbent_rtt =>
            {
                *streak += 1;
                if *streak >= SWITCH_STREAK {
                    info!("Path: {incumbent_addr} ({incumbent_rtt:.1}ms) -> {addr} ({rtt:.1}ms)");
                    *route = Route::Direct(addr);
                    *streak = 0;
                }
            }
            _ => *streak = 0,
        }
    }
}

/// A `[::]` socket only reaches IPv4 peers when the kernel leaves
/// `IPV6_V6ONLY` off. Rather than read the sockopt (and guess at platform
/// quirks), this asks the routing table directly: `connect` on a UDP socket
/// performs no I/O, it just resolves a route, so it fails immediately on a
/// v6-only socket asked for a v4-mapped destination.
///
/// Tested on a throwaway socket, never on the real one. Connecting a UDP
/// socket makes it drop datagrams from every address except the connected
/// peer, and there is no portable guarantee that un-connecting it afterwards
/// works -- getting that wrong would silently discard every probe and every
/// direct-path packet for the life of the process.
fn dual_stack_works(relay: SocketAddr) -> bool {
    let IpAddr::V4(v4) = relay.ip() else {
        return true;
    };
    let Ok(probe) = UdpSocket::bind("[::]:0") else {
        return false;
    };
    probe
        .connect(SocketAddr::new(
            IpAddr::V6(v4.to_ipv6_mapped()),
            relay.port(),
        ))
        .is_ok()
}

/// Collapses a v4-mapped v6 address back to plain IPv4 so that an address
/// learned from a received packet compares equal to the same address as the
/// peer advertised it.
fn unmap(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), addr.port()),
            None => addr,
        },
        IpAddr::V4(_) => addr,
    }
}

/// Whether an interface address is worth telling the peer about. Loopback and
/// link-local are unreachable from anywhere else; IPv6 unique-local won't
/// route between a car and a driver on different networks.
fn is_advertisable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_broadcast()
                && !v4.is_multicast()
                && !v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_multicast()
                && !v6.is_unspecified()
                && (v6.segments()[0] & 0xffc0) != 0xfe80
                && (v6.segments()[0] & 0xfe00) != 0xfc00
        }
    }
}

/// Whether an address belongs to this host, used to drop our own advertised
/// candidates when the peer echoes the shared LAN back at us.
fn is_local_ip(ip: &IpAddr) -> bool {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .iter()
        .any(|iface| iface.ip() == *ip)
}

/// `[family 1][ip 16][port 2]`, fixed width so the relay can emit it without
/// a serializer and both sides can parse it with a length check.
pub fn encode_addr(addr: SocketAddr) -> [u8; 19] {
    let mut out = [0u8; 19];
    match addr.ip() {
        IpAddr::V4(v4) => {
            out[0] = 4;
            out[1..5].copy_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            out[0] = 6;
            out[1..17].copy_from_slice(&v6.octets());
        }
    }
    out[17..19].copy_from_slice(&addr.port().to_be_bytes());
    out
}

pub fn decode_addr(bytes: &[u8]) -> Option<SocketAddr> {
    if bytes.len() < 19 {
        return None;
    }
    let port = u16::from_be_bytes([bytes[17], bytes[18]]);
    let ip = match bytes[0] {
        4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[1..5]).ok()?)),
        6 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[1..17]).ok()?)),
        _ => return None,
    };
    Some(unmap(SocketAddr::new(ip, port)))
}

/// Parses the body of a `REGISTER`, as the relay sees it: `[token][role][ts]`.
/// Returns the role and the echo bytes to hand back in the ack.
pub fn parse_register(body: &[u8], token: &SessionToken) -> Result<(Role, [u8; 8])> {
    if body.len() < TOKEN_LEN + 9 {
        bail!("short register packet");
    }
    if &body[..TOKEN_LEN] != token {
        bail!("register token mismatch");
    }
    let role = Role::from_wire(body[TOKEN_LEN]).context("bad role byte")?;
    let echo = <[u8; 8]>::try_from(&body[TOKEN_LEN + 1..TOKEN_LEN + 9]).expect("slice is 8 bytes");
    Ok((role, echo))
}

/// Builds a `REGISTER_ACK` body: `[token][echoed ts][observed addr]`.
pub fn register_ack_body(token: &SessionToken, echo: [u8; 8], observed: SocketAddr) -> Vec<u8> {
    let mut body = Vec::with_capacity(TOKEN_LEN + 8 + 19);
    body.extend_from_slice(token);
    body.extend_from_slice(&echo);
    body.extend_from_slice(&encode_addr(observed));
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_roundtrips_both_families() {
        for addr in [
            "192.168.1.42:41234".parse().unwrap(),
            "[2a01:cb00::1]:9000".parse().unwrap(),
        ] {
            assert_eq!(decode_addr(&encode_addr(addr)), Some(addr));
        }
    }

    /// The relay is IPv4, so a dual-stack socket reports its peers as
    /// v4-mapped. Without collapsing that back, an address learned from a
    /// packet never matches the same address as the peer advertised it and
    /// every direct path stays permanently unselected.
    fn mapped(addr: &str) -> SocketAddr {
        addr.parse().unwrap()
    }

    #[test]
    fn v4_mapped_addresses_collapse_to_v4() {
        assert_eq!(
            unmap(mapped("[::ffff:192.168.1.42]:41234")),
            mapped("192.168.1.42:41234")
        );
        assert_eq!(
            decode_addr(&encode_addr(mapped("[::ffff:10.0.0.1]:5000"))),
            Some(mapped("10.0.0.1:5000"))
        );
    }

    #[test]
    fn register_rejects_a_foreign_token() {
        let token: SessionToken = [7u8; TOKEN_LEN];
        let mut body = Vec::new();
        body.extend_from_slice(&[9u8; TOKEN_LEN]);
        body.push(Role::Client.to_wire());
        body.extend_from_slice(&0u64.to_be_bytes());
        assert!(parse_register(&body, &token).is_err());
    }

    #[test]
    fn register_roundtrips_role_and_echo() {
        let token: SessionToken = [7u8; TOKEN_LEN];
        let mut body = Vec::new();
        body.extend_from_slice(&token);
        body.push(Role::Slave.to_wire());
        body.extend_from_slice(&42u64.to_be_bytes());

        let (role, echo) = parse_register(&body, &token).unwrap();
        assert_eq!(role, Role::Slave);
        assert_eq!(u64::from_be_bytes(echo), 42);
    }

    #[test]
    fn loopback_and_link_local_are_not_advertised() {
        assert!(!is_advertisable("127.0.0.1".parse().unwrap()));
        assert!(!is_advertisable("169.254.1.1".parse().unwrap()));
        assert!(!is_advertisable("fe80::1".parse().unwrap()));
        assert!(!is_advertisable("fd00::1".parse().unwrap()));
        assert!(is_advertisable("192.168.1.42".parse().unwrap()));
        assert!(is_advertisable("2a01:cb00::1".parse().unwrap()));
    }
}
