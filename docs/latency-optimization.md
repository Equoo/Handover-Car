# Video Latency — Analysis & Optimization Plan

*A review of the video transmission path with one question in mind: **what actually
reduces glass-to-glass latency?** Quality, security and cleanup issues are
deliberately out of scope here.*

Scope of the review: [`rpicam_stream/src/streamer.cpp`](../rpicam_stream/src/streamer.cpp)
(send), [`client/src/video.rs`](../client/src/video.rs) +
[`client/src/app.rs`](../client/src/app.rs) (receive), and
[`server/src/main.rs`](../server/src/main.rs) (relay).

---

## 1. Rough latency budget

Where the current ~120–160 ms is likely going:

| Stage | Est. | Notes |
|---|---|---|
| Sensor + ISP | 15–30 ms | exposure, denoise |
| Capture → encode queue | 0–100 ms | **`MAX_QUEUE=6` — worst offender under load** |
| HW encode | 5–10 ms | fine |
| Network (5G uplink + relay) | 30–60 ms | plus bufferbloat when overshooting |
| Jitter buffer | **40 ms** | fixed, deliberate |
| Decode + present | 5–60 ms | 60 if on the software fallback |

---

## 2. Tier 1 — biggest wins, least work

### 1. Shrink the capture queue, drop the *oldest* frame

[`streamer.cpp:454`](../rpicam_stream/src/streamer.cpp#L454) sets `MAX_QUEUE = 6`;
at 60 fps that is up to **100 ms of pure buffering**. Worse,
[`streamer.cpp:698`](../rpicam_stream/src/streamer.cpp#L698) drops the *incoming*
frame when full — so once you fall behind you keep serving stale frames and
latency never recovers.

**Fix:** `MAX_QUEUE = 2`, and on overflow `pop()` the front before pushing.

This is the classic "latency creeps up and stays up" bug. ~10 lines.

### 2. Cut the jitter buffer

[`video.rs:81`](../client/src/video.rs#L81) uses `latency=40`. On a decent link
20 ms is playable — 20 ms straight off the top. Make it a CLI flag so it can be
dialled per-link rather than guessed.

### 3. Turn off retransmission

[`video.rs:84`](../client/src/video.rs#L84) sets `do-retransmission=true`, but
nothing on the sender can answer an RTX request (no `rtpbin` / `rtprtxsend`). The
jitter buffer still extends its wait window for retransmits that will never
arrive, so losses are declared *later* than necessary.

**Fix:** set it `false`.

### 4. Stop the software decoder from frame-threading

If the `avdec_h264` fallback is in use ([`video.rs:88`](../client/src/video.rs#L88)),
libavcodec's frame threading buffers **N frames before emitting the first** —
2–4 frame times (33–66 ms) of invisible latency.

**Fix:** `thread-type=2` (slice) or `max-threads=1`.

Potentially the single biggest win, and it is one property. Does not apply to the
hardware paths (`nvh264dec` / `vah264dec`).

---

## 3. Tier 2 — moderate effort, real gains

### 5. Drop the FEC encoder

[`streamer.cpp:578`](../rpicam_stream/src/streamer.cpp#L578) adds `rtpulpfecenc`
at 30%, which (a) buffers packets to compute parity and (b) puts **30% more bytes
on the scarcest resource** — the cellular uplink. The client never decodes it, so
it is pure added latency. Either remove it or wire up the decoder; for latency
alone, removing wins.

### 6. CBR instead of VBR, long GOP with on-demand keyframes

[`streamer.cpp:306`](../rpicam_stream/src/streamer.cpp#L306) uses VBR with 1.5×
peak, and [`streamer.cpp:297`](../rpicam_stream/src/streamer.cpp#L297) forces an
IDR every 0.5 s. Both create bitrate spikes; spikes queue in the carrier's uplink
buffer and surface as **latency spikes, not just quality dips**.

CBR paces evenly. A long GOP plus `V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME` on client
request (the control channel already exists) gets recovery without the periodic
cost.

> Note: `CLAUDE.md` originally specified CBR — the code drifted.

### 7. Stop mmap/munmap on every frame

[`streamer.cpp:667–694`](../rpicam_stream/src/streamer.cpp#L667) maps and unmaps
the libcamera buffers **per frame** — two syscalls plus TLB shootdowns, 60×/sec.

**Fix:** map each fd once at setup, cache `fd → base`, never unmap.

### 8. Hoist the relay's receive buffer out of the loop

[`server/src/main.rs:74`](../server/src/main.rs#L74) declares
`let mut buf = [0; 1 << 18];` **inside** the receive loop — 256 KB zeroed per
packet, on the hop every frame passes through. Move it above the loop.

(The client version at [`app.rs:92`](../client/src/app.rs#L92) already does this
correctly.)

### 9. Only push bitrate changes when they actually change

The slave writes the FIFO on *every* `ServerPong` — every 100 ms
([`slave/src/main.rs:145`](../slave/src/main.rs#L145)) — and each write triggers a
`set_bitrate` ioctl from the FIFO thread, concurrent with `encode()`'s
DQBUF/QBUF on the same fd ([`streamer.cpp:530`](../rpicam_stream/src/streamer.cpp#L530)
vs [`:169`](../rpicam_stream/src/streamer.cpp#L169)). Rate-control resets 10×/sec
can cause visible hiccups.

**Fix:** gate on "value actually changed", and apply it in one place only (the
atomic + flag path inside `encode()`).

---

## 4. Tier 3 — architectural

### 10. DMABUF zero-copy capture → encoder

Currently **three copies per frame** (mmap → vector, vector → encoder mmap). At
720×480 that is ~1.5 MB/frame, ~93 MB/s at 60 fps on a Pi 4.
`V4L2_MEMORY_DMABUF` with libcamera's fd passed straight through eliminates all
of it — this is what rpicam-apps does. Biggest capture-side win, biggest effort.

### 11. Let the sink negotiate its own format

[`video.rs:99`](../client/src/video.rs#L99) forces `BGRx` through `videoconvert`;
`gtk4paintablesink` can usually take I420 directly and upload via GL. Removing the
capsfilter drops a full-frame CPU conversion per frame.

### 12. Disable temporal denoise

The C++ streamer sets no `NoiseReductionMode`; the ISP's temporal denoise holds a
frame. `NoiseReductionModeMinimal` / `Off` is what `rpicam-vid --denoise cdn_off`
does.

---

## 5. The thing that isn't on the list but matters most

The **ABR loop reacts in 3–5 seconds** ([`video.rs:167`](../client/src/video.rs#L167)
polls at 2 s, [`app.rs:130`](../client/src/app.rs#L130) decides at 3 s). That reads
like a quality issue but it is really a *latency* issue: when the uplink is
overshot, packets pile up in the carrier's buffer and latency balloons by hundreds
of ms until the sender backs off.

Tightening that loop to ~500 ms is arguably worth more than items 2–4 combined on
a real 5G link.

### Related bug — infinite latency

[`app.rs:97`](../client/src/app.rs#L97) — `appsrc.push_buffer(gst_buffer)?`
propagates out of the async block, **permanently killing the receive task** on a
single transient error. That is not latency, that is infinite latency. Log and
continue instead.

---

## 6. How much would all this actually win?

> **Caveat:** these are engineering estimates with stated confidence, not
> measurements. The current pipeline has not been benchmarked — the 117–158 ms
> figures in [`stream`](../stream) came from a different setup (`rpicam-vid`
> straight to UDP, no relay, no C++ streamer).

| # | Change | Mean gain | Confidence |
|---|---|---|---|
| 1 | `MAX_QUEUE` 6→2 + drop-oldest | **0 ms** steady state, **up to 65 ms** recovered under load | High |
| 2 | Jitter buffer 40→20 ms | **10–20 ms** | High — trades loss resilience |
| 3 | `do-retransmission=false` | ~0 ms mean, tightens tail | Medium |
| 4 | Decoder frame-threading fix | **0 ms** *or* **33–66 ms** | Conditional |
| 5 | Remove FEC (30% fewer bytes) | **5–15 ms** | Medium-high |
| 6 | CBR + on-demand IDR | ~0–5 ms mean, **20–40 ms** off spikes | Medium |
| 7 | Map buffers once | **~0.5 ms** | High |
| 8 | Hoist relay buffer | **~0.03 ms** | High — negligible for latency, real for CPU |
| 9 | Stop 10 Hz bitrate ioctls | 0–5 ms, mostly jitter | Low-medium |
| 10 | DMABUF zero-copy | **1–3 ms** | Medium — less than commonly assumed |
| 11 | Drop `videoconvert` | **~1 ms** | Medium |
| 12 | Denoise off | **0–16 ms** | Low — uncertain how much the Pi ISP holds |

### Three scenarios

**Already on a hardware decoder** (`nvh264dec` / `vah264dec` — the code prefers
them): **≈ 25–50 ms off the mean.**

**Falling back to `avdec_h264` with frame threading:** **≈ 60–110 ms off the
mean.** Item 4 alone dominates everything else combined.

**Under a degraded link:** items 1, 6 and the ABR retune do not lower the mean
much — they **eliminate 100–300 ms excursions** that currently stick around.
Practically this is the change that would be *felt* most while driving.

### The floor you cannot cross

Sensor exposure + ISP (~15–25 ms) + hardware encode (~5–10 ms) + 5G RTT through a
relay (~30–60 ms) + decode/present (~5–15 ms) ≈ **60–90 ms** that no change here
touches.

So from ~130 ms today, landing at **85–95 ms** is realistic. Getting to 40 ms is
not achievable over a cellular link with a relay hop — that needs the relay
edge-hosted closer to the driver, or a P2P path.

---

## 7. Measure item 4 before anything else

The client already prints **decode latency** in the HUD
([`ui.rs:117`](../client/src/ui.rs#L117)) and logs which decoder it picked
([`video.rs:31`](../client/src/video.rs#L31)). Check both:

- Log says NVDEC/VAAPI, decode latency ~1–5 ms → the **25–50 ms** scenario.
- Log says *"No hardware available, using software decoder"*, decode latency
  15 ms+ → item 4 is worth more than all of Tier 2, and it is a one-property
  change.

That single readout decides whether the total is ~30 ms or ~100 ms.

---

## 8. Suggested implementation order

1. **Measure** — confirm which decoder path is active.
2. **Batch A** — items 1–4 (small, independent, most of the easily-recoverable delay).
3. **Batch B** — items 5–9.
4. **Then** — tighten the ABR loop and fix the `push_buffer` bug.
5. **Leave item 10** until the `[LATENCY]` numbers show the copies actually matter.
