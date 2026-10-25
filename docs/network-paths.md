# Network Paths — direct first, relay always

*How video, audio and pilot input get from the car to the driver, and how the
link picks the fastest route available at any moment.*

Code: [`common/src/transport.rs`](../common/src/transport.rs) (path selection),
[`server/src/main.rs`](../server/src/main.rs) (`new_relay`, rendezvous),
[`slave/src/pump.rs`](../slave/src/pump.rs) (loopback intake).

---

## 1. The problem

Every byte used to go through the VPS. The car sent RTP to a relay port, the
server forwarded it to whichever client had subscribed, and steering commands
made the same detour in reverse. That costs a full extra internet leg in each
direction — on a French 5G link, roughly 15–30 ms of pure detour on top of the
20–50 ms the cellular hop already costs.

The car has no reachable public address (CGNAT — see `CLAUDE.md`), so a direct
connection cannot simply be dialled. It has to be *discovered*.

---

## 2. Three kinds of path

All three are the same socket on each peer, pointed at a different address.
Video, audio and pilot input share it, tagged with a one-byte channel id, so
**one punched NAT mapping carries every stream**.

| Path | How it's found | Typical RTT | When it works |
|---|---|---|---|
| **Host** | Peer advertises its own interface addresses | 1–3 ms on LAN | Same network, or both ends have routable IPv6 |
| **Reflexive** | Relay reports the public address it saw | cellular RTT, no detour | When NAT mapping is endpoint-independent |
| **Relay** | The VPS forwards, as before | cellular RTT + detour | Always |

IPv6 is worth calling out: French carriers usually hand the modem a real,
un-NATed v6 address. When the driver's side also has v6, the "hard" CGNAT case
collapses into an ordinary host candidate and nothing has to be punched at all.
In local testing this is the path that wins.

---

## 3. How a session comes up

```
slave                          server                          client
  |-- Announce ---------------->|                                |
  |<-- Connected{relay,token} --|                                |
  |                             |<------------------ Subscribe --|
  |                             |-- Connected{relay,token} ----->|
  |                                                              |
  |== REGISTER (data plane) ===>|<==================== REGISTER ==|
  |<= REGISTER_ACK{your addr} ==|=================> REGISTER_ACK =|
  |                                                              |
  |-- Candidates -------------->|-- PeerCandidates ------------->|
  |<-- PeerCandidates ----------|<------------------ Candidates -|
  |                                                              |
  |<================= PROBE / PROBE_ACK ========================>|   punch
  |<================= video / audio / input =====================>|   direct
```

The relay port is the session's TURN server *and* its STUN server: the address
it reports back in `REGISTER_ACK` is the only way a peer can learn the public
address of the socket that actually carries media. The control socket's public
address is a different NAT mapping and says nothing useful about it.

Registration repeats every second, so a cellular NAT rebind is noticed and
republished rather than silently breaking the session.

---

## 4. Selection rules

- **The relay is live from the first packet.** A direct path only takes over
  once it has answered a probe. Nothing ever waits on a punch that may not
  succeed.
- **A dead incumbent is abandoned immediately** (no inbound packet for 500 ms).
  Video going into a black hole is worth reacting to without hesitation.
- **A live incumbent is only displaced** by a challenger that is faster by both
  20% *and* 2 ms, for 3 consecutive evaluations.
- **Switching is seamless.** Receivers accept datagrams from any source
  address, so packets already in flight on the old path still arrive. Nothing
  is duplicated across paths.

### Why every candidate is probed at the same rate

The first version probed the selected path at 100 ms and idle candidates at
500 ms, to save uplink. That silently rigged every comparison: the incumbent's
RTT average was built from five times as many samples, so it tracked reality
and drifted up on any slow sample, while an idle candidate kept whatever lucky
low value it last got. The result was a standing bias *against whatever was
currently in use* — a real run rotated between a LAN address, a loopback
address and an IPv6 address every second or two, each switch costing packet
reordering for nothing.

Comparing two averages only means something when they are built the same way.
Candidates that have **never** answered are backed off to 2 s, which is safe
precisely because they are never compared against anything.

Regression coverage: `equally_good_candidates_do_not_flap` in
[`common/tests/transport_paths.rs`](../common/tests/transport_paths.rs).

---

## 5. Why the Pi's RTP goes through loopback

The camera streamer and the audio pipeline both end in a GStreamer `udpsink`,
which owns its own socket and picks its own source port. If they sent straight
out, their packets would leave from a 5-tuple that never punched anything, and
the peer's NAT would drop them on every path except the relay.

Instead both send to `127.0.0.1`, and `pump.rs` forwards onto whichever path is
selected. Cost: one loopback copy, tens of microseconds against a link measured
in tens of milliseconds. Benefit: a single punched mapping serves everything,
and a path switch changes a destination in the slave process rather than
reconfiguring a running encoder — no camera re-open, no blackout.

---

## 6. Security

The server mints a random 16-byte token per session and hands it to both peers
over the reliable control channel. Probes and registrations carry it. Without
it, anything on the internet could register itself as a peer-reflexive
candidate and pull the car's video — or, worse, become the accepted source of
steering commands.

Media packets themselves are unauthenticated, exactly as before. The defence
there is that a path is only used after it answers a token-carrying probe.

---

## 7. Reading it live

The client overlay has a `Path` metric (key `i`, on by default) showing the
current route and its RTT:

```
Path: direct/host 192.168.1.44:51834 (0.2 ms)
Path: direct/punched 89.30.12.7:41234 (28.4 ms)
Path: relay 37.59.106.4:9600 (44.1 ms)
```

`punched*` means a peer-reflexive candidate — an address a probe actually
arrived from, which the peer never advertised. That is the symmetric-NAT case
being solved by observation rather than by signalling.

Server-side, `RUST_LOG=info` logs each side registering with the relay; both
peers log every path change and their discovered public address.

---

## 8. Bring-up order

Unchanged from `CLAUDE.md`'s advice, and this makes it easier to follow: bring
the link up on LAN first and confirm the overlay shows `direct/host` at 1–3 ms,
then move to 5G and watch whether it settles on `direct/punched` or falls back
to `relay`. The overlay tells you which, so a latency change no longer has to
be guessed at.
