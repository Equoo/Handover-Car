# Handover

An RC car you drive over the phone network instead of a radio.

You know the feeling: the car goes behind a wall and the signal is gone. We wondered what would happen if it used 5G instead. In theory you could drive it from anywhere with coverage, watching through a small camera on the front.

So we put a 5G modem, a camera and a small computer on an ordinary RC car. Getting the car to move was the easy part. Minimizing the latency took a lot of work.

We are three students from 42 Lyon: [David](https://github.com/Equoo), [Amidigov](https://github.com/amidigov) and [Timeo](https://github.com/42Timeo). This is a side project, built on evenings and weekends.

## Where it's at

It drives. Today the car:

- streams low latency (150~ ms) live video and sound over 5G to a desktop app
- follows a game controller
- lowers the picture quality when the network gets weak instead of freezing, then raises it again
- finds a direct route between car and driver on its own, and falls back to our server in under half a second when that route dies
- cuts the throttle and straightens the wheels if orders stop coming in
- give some realtime data for debuging

What it doesn't have yet is a body made for the job. More on that at the end.

### What's in the repo

| Folder | What it is |
| --- | --- |
| `rpicam_stream/` | The camera program, in C++. Takes frames from libcamera, pushes them through the Pi's hardware H.264 encoder (V4L2), and sends RTP with forward error correction. Bitrate, FEC and keyframes can be changed while it runs. |
| `slave/` | Runs on the Pi, in Rust. Starts the camera and the microphone, talks to the server, and passes the driver's orders to the Arduino over serial. |
| `client/` | The driver's app, in Rust (egui + GStreamer). Shows the video, reads the gamepad, and decides how much bitrate and FEC to ask for based on loss and delay. |
| `server/` | The meeting point and relay, in Rust. |
| `common/` | Shared protocol and the path selection logic. |
| `serial_to_pwm/` | The Arduino sketch. Turns serial lines into PWM for the ESC and the steering servo, and goes back to neutral if nothing arrives for 100 ms. |
| `os/` | Boot config and systemd services for the Pi: modem bring-up, a watchdog that redoes the connection when it's lost, reverse SSH. |

## How it works

```
 driver's PC                    small server                         car
┌────────────┐               ┌───────────────┐            ┌──────────────────────┐
│ client     │◄─────────────►│ meeting point │◄──────────►│ Raspberry Pi 4       │
│ (Rust)     │               │ + relay       │    5G      │  camera streamer     │
│            │               └───────────────┘            │  (C++)               │
│ video,     │                                            │  slave (Rust)        │
│ sound,     │◄──────────────────────────────────────────►│     │ serial         │
│ gamepad    │        direct path, when one exists        │  Arduino Nano        │
└────────────┘                                            │     │ PWM            │
                                                          │  ESC + steering servo│
                                                          └──────────────────────┘
```

On the mobile network, nobody from outside can reach the car. It’s like a hotel where every room shares the same phone number and there’s no extension system: guests can call out, but an outside caller has no way to reach a specific room. So both sides call a small server we rent, and it passes messages between them.

That works, but every picture and every turn of the wheel then takes a detour, which added 15 to 30 ms each way for us. So once the two sides have met at the server, they also look for a direct road to each other (local addresses, IPv6, or a hole punched through the NAT). As soon as one answers, they switch to it. The server path stays ready the whole time, so when the direct one drops, for example when the car moves from antenna to another, the video doesn't even restart.

Video, sound and controls all share a single UDP socket on each side, so one working path carries everything.

### Hardware

- an ordinary 1/12 RC car (Wltoys Nouveau 124017)
- Raspberry Pi 4B (we want its hardware H.264 encoder)
- Waveshare 5G HAT with a Quectel RM500U-CNV modem, and four stick-on antennas
- a CSI camera
- Arduino Nano for the ESC and the steering servo
- a phone power bank for the Pi and the modem

## Running it

It's still a project you run by hand, and a few things are hardcoded. Roughly:

**Server** (any small machine with a public IP, UDP 9500 and 9600-9615 open):

```sh
make server
```

**Car** (on the Pi, from the repo root):

```sh
# build the camera streamer
cmake -S rpicam_stream -B rpicam_stream/build
cmake --build rpicam_stream/build

# the slave looks for a host called "server", so add yours to /etc/hosts
make slave
```

The Arduino is expected on `/dev/ttyUSB0` at 115200 baud, flashed with `serial_to_pwm/serialtopwm.ino` (steering on pin 9, throttle on pin 10).

**Driver** (Linux, GStreamer installed, a gamepad plugged in):

```sh
echo "SERVER=your.server.address" > .env
make client
```

Left stick steers, right stick is the throttle. Press `i` to see which path is in use and how long it takes.

There is a `flake.nix` if you use Nix (`nix develop`).

## What can it do?

We took it out for real tests several times in August 2026.

When the car stands still, the video is very good, even with a lot of movement in front of the camera. As soon as it starts rolling, the network gets worse and the video with it. On top of that the camera shakes with every bump, since nothing steadies it. Driving gets hard, especially near top speed, which is about 50 km/h even with all the extra gear. Our theory is that the motor and the ESC are interfering with the 5G connection.

The hardest part is probably the build itself. It's a small RC car with a 3D-printed lid. On top sit the Pi and its 5G board, already fairly heavy. Then an Arduino placed wherever it fits, cables everywhere, and a 600-gram power bank with two USB cables dangling. The antennas are the worst: four flat stick-on ones, and finding a good spot for each is a puzzle.

## What's next

The software does its job on a body that was never meant to carry it. So first, a proper build, where every part and every antenna has its place and the camera stops shaking. After that, it's time to see how far the phone network can take it.

---
*Licensed under PolyForm Noncommercial 1.0.0*
