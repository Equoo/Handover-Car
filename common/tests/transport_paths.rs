//! End-to-end exercise of the data plane against a stand-in relay.
//!
//! Two transports, one relay, no camera and no server: enough to prove the
//! part that can't be checked by reading -- that traffic flows on the relay
//! before anything has been punched, that a direct path takes over once
//! candidates are exchanged, and that losing the direct path falls back
//! rather than going silent.
//!
//! Everything here runs over loopback, so "punching" is trivially successful.
//! What is under test is the state machine around it, not NAT behaviour.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::transport::{
    Candidate, CandidateKind, Role, Route, SessionToken, Transport, channel, parse_register,
    register_ack_body,
};

const TOKEN: SessionToken = [0xAB; 16];

/// The server's two-party relay, reimplemented in miniature. Kept in the test
/// rather than shared with the server binary on purpose: if the two ever
/// disagree about the wire format, this is where it shows up.
fn spawn_relay() -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("failed to bind relay");
    let addr = socket.local_addr().expect("relay has no address");

    std::thread::spawn(move || {
        let mut slave_addr: Option<SocketAddr> = None;
        let mut client_addr: Option<SocketAddr> = None;
        let mut buf = [0u8; 65536];

        loop {
            let Ok((size, from)) = socket.recv_from(&mut buf) else {
                continue;
            };
            if size < 1 {
                continue;
            }

            if buf[0] == channel::REGISTER {
                let Ok((role, echo)) = parse_register(&buf[1..size], &TOKEN) else {
                    continue;
                };
                match role {
                    Role::Slave => slave_addr = Some(from),
                    Role::Client => client_addr = Some(from),
                }
                let mut ack = vec![channel::REGISTER_ACK];
                ack.extend_from_slice(&register_ack_body(&TOKEN, echo, from));
                let _ = socket.send_to(&ack, from);
                continue;
            }

            let dst = if Some(from) == slave_addr {
                client_addr
            } else if Some(from) == client_addr {
                slave_addr
            } else {
                continue;
            };
            if let Some(dst) = dst {
                let _ = socket.send_to(&buf[..size], dst);
            }
        }
    });

    addr
}

/// Spins until `cond` holds or the deadline passes, so the tests wait on the
/// condition they actually care about instead of a fixed sleep long enough to
/// cover the worst case.
fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

struct Peer {
    transport: Arc<Transport>,
    received: Arc<AtomicUsize>,
}

fn spawn_peer(role: Role, relay: SocketAddr) -> Peer {
    let transport = Transport::bind(role, relay, TOKEN).expect("failed to bind transport");
    let received = Arc::new(AtomicUsize::new(0));

    let counter = received.clone();
    transport.start(
        move |_tag, _payload| {
            counter.fetch_add(1, Ordering::Relaxed);
        },
        |_candidates| {},
    );

    Peer {
        transport,
        received,
    }
}

/// The relay has to carry traffic from the very first packet: video must not
/// wait on a punch that may never succeed.
#[test]
fn relay_carries_traffic_before_any_candidates_are_known() {
    let relay = spawn_relay();
    let slave = spawn_peer(Role::Slave, relay);
    let client = spawn_peer(Role::Client, relay);

    // Both sides have to have registered before the relay knows where to
    // forward; that is the one thing worth waiting on here.
    assert!(
        wait_for(Duration::from_secs(3), || {
            slave.transport.rtt_ms().is_some() && client.transport.rtt_ms().is_some()
        }),
        "neither peer completed relay registration"
    );

    assert_eq!(slave.transport.route(), Route::Relay);
    assert_eq!(client.transport.route(), Route::Relay);

    for _ in 0..5 {
        slave
            .transport
            .send(channel::VIDEO, b"frame")
            .expect("send failed");
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        wait_for(Duration::from_secs(2), || client
            .received
            .load(Ordering::Relaxed)
            > 0),
        "client received nothing over the relay"
    );
}

/// Once each side knows where the other is, the direct path must win -- and
/// media must keep flowing across the switch.
#[test]
fn a_direct_path_takes_over_once_candidates_are_exchanged() {
    let relay = spawn_relay();
    let slave = spawn_peer(Role::Slave, relay);
    let client = spawn_peer(Role::Client, relay);

    let host = |port: u16| Candidate {
        addr: SocketAddr::new("127.0.0.1".parse().unwrap(), port),
        kind: CandidateKind::Host,
    };

    slave
        .transport
        .set_peer_candidates(&[host(client.transport.local_port())]);
    client
        .transport
        .set_peer_candidates(&[host(slave.transport.local_port())]);

    assert!(
        wait_for(Duration::from_secs(5), || {
            matches!(slave.transport.route(), Route::Direct(_))
                && matches!(client.transport.route(), Route::Direct(_))
        }),
        "no direct path was selected: slave={:?} client={:?}",
        slave.transport.route(),
        client.transport.route()
    );

    let before = client.received.load(Ordering::Relaxed);
    for _ in 0..5 {
        slave
            .transport
            .send(channel::VIDEO, b"frame")
            .expect("send failed");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        wait_for(Duration::from_secs(2), || client
            .received
            .load(Ordering::Relaxed)
            > before),
        "client received nothing over the direct path"
    );
}

/// A candidate that never answers must never be selected. This is the common
/// case on cellular -- the peer advertises a reflexive address whose NAT
/// mapping doesn't admit us -- and treating "advertised" as "usable" would
/// point the whole stream into a black hole.
#[test]
fn an_unreachable_candidate_never_displaces_the_relay() {
    let relay = spawn_relay();
    let slave = spawn_peer(Role::Slave, relay);

    // Reserved for documentation (RFC 5737), so nothing can answer it.
    slave.transport.set_peer_candidates(&[Candidate {
        addr: "192.0.2.1:9999".parse().unwrap(),
        kind: CandidateKind::Reflexive,
    }]);

    assert!(
        !wait_for(Duration::from_secs(2), || matches!(
            slave.transport.route(),
            Route::Direct(_)
        )),
        "selected a path that never answered a probe"
    );
    assert_eq!(slave.transport.route(), Route::Relay);
}

/// A stand-in peer that answers probes after a jittered delay, to imitate
/// several candidates that are equally good but individually noisy.
fn spawn_jittery_peer(max_extra_micros: u64, seed: u64) -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("failed to bind stand-in peer");
    let addr = socket.local_addr().unwrap();

    std::thread::spawn(move || {
        // Deterministic pseudo-random, so a failure reproduces.
        let mut state = seed | 1;
        let mut buf = [0u8; 2048];
        loop {
            let Ok((size, from)) = socket.recv_from(&mut buf) else {
                continue;
            };
            if size < 1 || buf[0] != channel::PROBE {
                continue;
            }
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let extra = (state >> 33) % max_extra_micros.max(1);
            std::thread::sleep(Duration::from_micros(extra));

            let mut ack = vec![channel::PROBE_ACK];
            ack.extend_from_slice(&buf[1..size]);
            let _ = socket.send_to(&ack, from);
        }
    });

    addr
}

/// Several candidates that are all equally good must settle on one and stay
/// there.
///
/// This is the regression for a bias that made rotation inevitable: idle
/// candidates used to be probed five times slower than the selected one, so
/// their RTT averages were built from far fewer samples and stayed
/// optimistically low while the incumbent's tracked reality. Every evaluation
/// therefore favoured whoever was *not* currently selected, and a real run
/// switched between a LAN address, a loopback address and an IPv6 address
/// every second or two -- each switch costing packet reordering for no gain.
#[test]
fn equally_good_candidates_do_not_flap() {
    let relay = spawn_relay();
    let slave = spawn_peer(Role::Slave, relay);

    // Three peers with the same jitter profile: none is genuinely better, and
    // all differences stay far below the switch floor.
    let peers: Vec<SocketAddr> = (0..3)
        .map(|i| spawn_jittery_peer(1200, 0x9E3779B9 * (i + 1)))
        .collect();

    slave.transport.set_peer_candidates(
        &peers
            .iter()
            .map(|addr| Candidate {
                addr: *addr,
                kind: CandidateKind::Host,
            })
            .collect::<Vec<_>>(),
    );

    assert!(
        wait_for(Duration::from_secs(5), || matches!(
            slave.transport.route(),
            Route::Direct(_)
        )),
        "no direct path was selected"
    );

    let settled = slave.transport.route();
    let mut switches = 0;
    let mut previous = settled;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        let now = slave.transport.route();
        if now != previous {
            switches += 1;
            previous = now;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        switches, 0,
        "path flapped {switches} times between equally good candidates \
         (settled on {settled:?}, ended on {previous:?})"
    );
}

/// The failure mode that matters on a moving car: the direct path dies mid
/// session (cell handover moves the public address) and the session has to
/// drop back to the relay on its own.
#[test]
fn losing_the_direct_path_falls_back_to_the_relay() {
    let relay = spawn_relay();
    let slave = spawn_peer(Role::Slave, relay);

    // A peer that answers probes, then stops. Standing in for the far side
    // going away without any notice, which is what a handover looks like.
    let peer = UdpSocket::bind("127.0.0.1:0").expect("failed to bind stand-in peer");
    let peer_addr = peer.local_addr().unwrap();
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();

    let answering = Arc::new(AtomicUsize::new(1));
    let still_answering = answering.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((size, from)) = peer.recv_from(&mut buf) else {
                continue;
            };
            if still_answering.load(Ordering::Relaxed) == 0 {
                continue;
            }
            if size >= 1 && buf[0] == channel::PROBE {
                let mut ack = vec![channel::PROBE_ACK];
                ack.extend_from_slice(&buf[1..size]);
                let _ = peer.send_to(&ack, from);
            }
        }
    });

    slave.transport.set_peer_candidates(&[Candidate {
        addr: peer_addr,
        kind: CandidateKind::Host,
    }]);

    assert!(
        wait_for(Duration::from_secs(5), || matches!(
            slave.transport.route(),
            Route::Direct(_)
        )),
        "never selected the answering peer"
    );

    answering.store(0, Ordering::Relaxed);

    assert!(
        wait_for(Duration::from_secs(3), || slave.transport.route()
            == Route::Relay),
        "stayed on a dead direct path instead of falling back"
    );
}
