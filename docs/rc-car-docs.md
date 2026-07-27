# The 5G RC Car — How It Works

*Plain-English project documentation. No prior knowledge assumed.*

> A companion, nicely-formatted version of this document lives at
> [`docs/rc-car-docs.html`](./rc-car-docs.html) — open it in any browser.

---

## 01 — The idea in one breath

It's a **video call, except one caller is a car.**

Imagine a normal video call where both people hold a phone. Here, one "person"
is a small RC car. The car sends you live video from a camera bolted to its
front. You send back not words, but **steering and throttle**. You see through
its eyes and drive it, and the car could be on the other side of the city.

Everything in this project exists to make that one loop work: **your controls
go out, the car's video comes back** — fast enough that driving feels
responsive.

**The mental model:**

```
You (a laptop) ⇄ a rented computer on the internet ⇄ the car (Raspberry Pi + camera + modem + motors)
                        "the middleman"
```

The middleman exists purely so the two ends can find each other (see below).

---

## 02 — Why it isn't trivial

### Problem 1 — The car has no "address"

On a home connection your router has a public address other computers can
reach. But on a **mobile network (4G/5G)**, the car's modem does *not* — the
carrier hides thousands of customers behind shared addresses (this is called
**CGNAT**). So you literally **cannot dial the car directly**. It can reach
out, but nothing can reach in.

> **Analogy:** the car is like someone at a huge company with no direct phone
> line. You can't call their desk. The fix: both of you call a **switchboard**
> that everyone *can* reach, and it connects the two calls. That switchboard is
> a rented server on the internet (a VPS at `37.59.106.4`).

### Problem 2 — Delay ruins driving

If the video lags by even half a second, you crash before you see the wall. So
the whole system is obsessed with **low latency**, which drives two choices:

- **It uses UDP, never TCP.** TCP guarantees every packet arrives *in order* —
  but to do that it stops and waits for lost packets, adding delay. For live
  video and controls, a slightly glitchy-but-instant feed beats a
  perfect-but-late one. UDP just fires packets and doesn't wait.
- **Video is compressed by a dedicated chip.** Raw camera video is enormous.
  The Raspberry Pi 4 has a hardware chip that squeezes it into the **H.264**
  format (same as YouTube/Blu-ray) without bogging down its main processor.

---

## 03 — The cast of characters

The project is split into separate programs that each do one job.

| Program | Role | What it does | Tech · location |
|---|---|---|---|
| **Client** | You / the driver | PC app: shows the live video, overlays stats, sends steering + throttle. | Rust · GTK4 · GStreamer · `client/` |
| **Server** | The switchboard | Runs on the rented VPS. Introduces driver and car, forwards traffic between them. | Rust · `server/` |
| **Slave** | The car's brain | Runs on the Pi. Talks to the server, launches the camera streamer, passes commands to the Arduino. | Rust · `slave/` |
| **Streamer** | The eyes | Grabs camera frames, compresses with the Pi's hardware H.264 chip, sends them as video packets. | C++ · libcamera + V4L2 · `rpicam_stream/` |
| **Arduino** | The hands | Turns text commands into the electrical pulses that move the steering servo and motor. | Arduino C · `serial_to_pwm/` |
| **Common** | Shared language | Library defining the messages everyone speaks and the networking layer, so all ends agree on the format. | Rust lib · `common/` |
| **Network tools** | The lifeline | Configure the 5G modem and show a live signal dashboard (bars, strength, ping). | Rust + AT commands · `os/network_manager/` |
| **ABR controller** | Autopilot for quality | Logic in the client + streamer that automatically lowers/raises video quality with the network. | Rust ⇄ C++ |

---

## 04 — Journey A: what happens when you turn the wheel

Following a single steering command from your fingers to the wheels:

1. **Client (your PC)** — You press a key / move a stick. The client packages it
   as a **`MasterInput`** message (a steering number + a throttle number) and
   sends it to the server as fast, no-guarantee UDP — a fresh command is always
   right behind it.
2. **Server (switchboard)** — Looks up which car this driver is subscribed to,
   then **forwards the command** to that car's current network address.
3. **Slave (the Pi)** — Receives the command and writes it as a line of text —
   e.g. `1500 1486\n` — down a serial cable to the Arduino.
4. **Arduino** — Reads that line and converts the two numbers into **PWM
   pulses**, the pulse "language" hobby servos and motor controllers understand.
5. **Servo + motor** — The steering servo turns; the motor speeds up or slows
   down. **The car moves.** Total round-trip is a fraction of a second.

> **Why split this way?** The Pi is great at internet and video but bad at
> precise real-time electrical timing. The Arduino is the opposite. So the Pi
> handles the network and hands the twitchy motor-timing work to the Arduino
> over a plain serial cable.

---

## 05 — Journey B: what happens so you can see the road

A single camera frame's trip from the car's lens to your screen:

1. **Camera + streamer** — Captures a frame and feeds it straight into the Pi's
   **hardware H.264 chip**, which crushes it from megabytes to a few kilobytes.
2. **Streamer (packaging)** — Chops the compressed frame into small network
   packets (**RTP**, the standard for live media) and adds **error-correction
   padding (FEC)** so the receiver can rebuild a few lost packets without
   resends.
3. **Server (switchboard)** — Packets hit the server, which **blindly forwards**
   them to whichever driver most recently "subscribed" to this car's video.
4. **Client (jitter buffer)** — Your PC collects packets in a tiny **40 ms**
   buffer to smooth out the internet's uneven timing, reorders them, and uses
   FEC to patch small losses.
5. **Client (decode + display)** — A hardware video decoder (NVIDIA / Intel /
   AMD if available, otherwise software) turns H.264 back into pictures, and
   GStreamer paints them into the window. **You see the road.**

> **The stats overlay:** while streaming, the client corner-overlays live
> telemetry — frames per second, decode delay, your ping, the car's ping,
> packet loss %, and current bitrate — like a cockpit gauge cluster.

---

## 06 — The clever tricks

### 1. The "hole-punch" so the car can be reached

The car has no reachable address. The trick: the **driver sends one tiny packet
to the server first**. That teaches the server "here's where this driver lives
right now." From then on, the server forwards the car's video to that remembered
address. Both ends reach *out* to the middleman, so neither needs to be
reachable directly.

### 2. Adaptive quality (ABR) — cruise control for video

Mobile signal constantly changes. The client watches its packet-loss meter and,
every few seconds, picks a new target quality with a simple rule: **lose too
many packets → cut the bitrate hard; link is clean → nudge it back up.** That
target is relayed through the server to the streamer, which retunes the hardware
encoder on the fly. The picture gets *softer* instead of freezing when signal
drops, and sharpens again when it recovers — a full feedback loop across three
programs.

> **Analogy:** like a driver easing off the gas on a bad road and speeding up on
> smooth tarmac — automatically, several times a second.

### 3. Talking to the video chip directly

Instead of running the Pi's ready-made camera tool, the streamer talks
**straight to the encoder chip** (the Linux device `/dev/video11`), which shaves
off delay and unlocks live bitrate changes. It's the most technically ambitious
piece of the codebase.

---

## 07 — The safety net

A cellular link **will** drop sometimes — a certainty, not an "if." Without
protection, a dropped connection means "last command was full throttle" and the
car vanishes into a wall. Two safeguards prevent this:

- **Dead-man's switch (Arduino).** The Arduino expects a fresh command
  constantly. If **100 milliseconds** pass with no valid command, it
  automatically **centres the steering and cuts the throttle to neutral.** No
  signal, no movement.
- **Relentless reconnection (Pi).** The car's software keeps retrying its
  connection to the server forever, so a blip becomes a brief pause rather than
  a dead car.

> **Design note:** the safety ultimately rests on the Arduino's timeout. The
> higher-level software doesn't independently force a stop — it relies on the
> commands simply going quiet, which trips the dead-man's switch. Simple and
> robust, but worth knowing where the guarantee actually lives.

---

## 08 — Repo map

The project is a Rust *workspace* (several programs sharing one build) plus a
C++ streamer and the Pi's OS setup.

```
common/          # shared message types + UDP networking (the "language")
client/          # the driver's app: video window, stats, sends controls
  src/app.rs     # networking, the adaptive-quality (ABR) loop
  src/video.rs   # GStreamer receive + decode pipeline
  src/ui.rs      # the GTK4 window and stats overlay
server/          # the relay/switchboard on the rented VPS
  src/main.rs    # connects drivers to cars, forwards video + controls
slave/           # the car's brain (runs on the Raspberry Pi)
  src/main.rs    # server link, serial to Arduino, launches streamer
  src/media.rs   # starts the C++ streamer process
rpicam_stream/   # the C++ camera streamer (hardware H.264 encoder)
  src/streamer.cpp
serial_to_pwm/   # the Arduino sketch: text commands to servo pulses
  serialtopwm.ino
os/              # Raspberry Pi OS image setup, 5G modem config, services
  network_manager/ # modem setup + live signal-strength dashboard
CLAUDE.md        # original planning notes (hardware + constraints)
stream           # a quick manual streaming test script
```

---

## 09 — Honest state of the project

A working prototype under active development, not a finished product.

| Severity | Issue | Detail |
|---|---|---|
| 🔴 **Security** | Private SSH key committed | `os/root/.ssh/id_rsa` is a real private key in version control (the TODO even says "do not push ssh keys"). Rotate it and scrub it from history — treat it as compromised. |
| 🟠 **Bug** | Car IDs can collide | The server picks the first free slot for each car but never marks it taken, so a second car grabs the same video port. Fine with one car; breaks with several. |
| 🟠 **Inconsistency** | Video profile comment mismatch | The streamer's comment says "baseline profile" (the low-latency plan) but the code sets "high profile." One should be corrected. |
| ⚪ **Cleanup** | Dead code & stale docs | An unused `old.rs`, large commented-out blocks, and modem setup that mixes up two connection modes (NCM vs RNDIS). `CLAUDE.md` still describes an older WebRTC plan the code has moved past. |

---

## 10 — Glossary

| Term | One-line meaning |
|---|---|
| **CGNAT** | Why a 5G modem has no reachable address — the carrier hides many customers behind shared addresses. |
| **VPS** | A small computer you rent on the internet. Here it runs the relay/switchboard. |
| **UDP** | "Fire and forget" data sending. Fast, no delivery guarantee — ideal for live video and controls. |
| **TCP** | The careful opposite: guarantees delivery and order, but waits for lost data — too slow for real-time driving. |
| **H.264** | A hugely popular video compression format. The Pi 4 does it in hardware. |
| **RTP** | The standard way of chopping live audio/video into network packets. |
| **FEC** | Forward Error Correction — extra padding sent with video so the receiver can rebuild a few lost packets without resends. |
| **Jitter buffer** | A tiny holding area (40 ms here) that smooths the internet's uneven packet timing before playback. |
| **Bitrate** | How much data per second the video uses. Higher = sharper but heavier; the ABR loop tunes it live. |
| **ABR** | Adaptive Bitrate — automatically raising/lowering video quality to match the network. |
| **Servo / PWM** | A servo is a small precise motor (steering); PWM is the pulse signal telling it where to point. |
| **ESC** | Electronic Speed Controller — drives the car's main motor from a PWM signal. |
| **Serial / UART** | A simple two-wire cable for sending text between the Pi and the Arduino. |
| **libcamera / V4L2** | The Linux systems for talking to cameras and video hardware on the Pi. |
| **GStreamer** | A toolkit for building video pipelines — used on the client to receive, decode, and display. |
| **AT commands** | The old text "language" used to configure cellular modems (e.g. to check signal strength). |
| **RSRP / RSRQ / SINR** | Cellular signal-quality numbers — the modem dashboard colours them green/yellow/red. |

---

*Architecture: Client ⇄ Relay Server ⇄ Car (Pi + Arduino). Generated as a
plain-English overview of the codebase.*
