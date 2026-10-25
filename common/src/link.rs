//! Shared plumbing for the "announce" pattern used by both the slave and the
//! client: send a small ping-carrying message to the server at a fixed
//! cadence, then match the matching pong back up to measure round-trip
//! latency. Extracted here because both binaries hand-rolled this
//! identically.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::time::Instant;

use crate::{Message, PacketType, Socket};

/// Tracks the packet id and send time of the most recent announce, so a
/// later pong can be matched back to it and turned into a round-trip time.
pub struct PingTracker {
    last_ping: Mutex<Instant>,
    packet_id: Mutex<u16>,
}

impl PingTracker {
    pub fn new() -> Self {
        Self {
            last_ping: Mutex::new(Instant::now()),
            packet_id: Mutex::new(0),
        }
    }

    /// Bumps the packet id and records "now" as the send time. Call this
    /// right before sending the announce, and embed the returned id in it.
    pub fn record_ping(&self) -> u16 {
        *self.last_ping.lock() = Instant::now();
        let mut id = self.packet_id.lock();
        *id = id.wrapping_add(1);
        *id
    }

    /// If `packet_id` matches the most recent `record_ping`, returns the
    /// elapsed round-trip time. A stale/mismatched id (a pong for an
    /// announce that's no longer the latest one in flight) returns `None`.
    pub fn note_pong(&self, packet_id: u16) -> Option<Duration> {
        if packet_id == *self.packet_id.lock() {
            Some(self.last_ping.lock().elapsed())
        } else {
            None
        }
    }
}

impl Default for PingTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs an announce loop at a fixed cadence: records a ping, builds a
/// `Message` from the resulting packet id via `build`, sends it unreliably,
/// then runs `on_tick` before sleeping. `on_tick` exists so a caller can
/// piggyback per-tick logic (e.g. the client's stale-link resubscribe check)
/// on the same cadence without this function knowing about it.
///
/// Blocks the calling thread for `interval` between sends -- callers are
/// expected to run this on its own spawned task, same as before extraction.
pub fn run_announce_loop(
    socket: &Socket,
    target: &SocketAddr,
    ping: &PingTracker,
    interval: Duration,
    mut build: impl FnMut(u16) -> Message,
    mut on_tick: impl FnMut(),
) -> Result<()> {
    loop {
        let packet_id = ping.record_ping();
        socket.send(target, &build(packet_id), PacketType::Unreliable)?;
        on_tick();
        std::thread::sleep(interval);
    }
}
