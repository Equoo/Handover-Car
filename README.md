# Handover

An RC car you drive over the phone network instead of a radio.

You know the feeling: the car goes behind a wall and the signal is gone. We wondered what would happen if it used 5G instead. In theory you could drive it from anywhere with coverage, watching through a small camera on the front.

So we strapped a 5G modem, a camera and a Raspberry Pi onto an ordinary RC car. Getting it to move was the easy part. Getting it to talk to us took most of a year.

We are two students from 42 Lyon: [David](https://github.com/Equoo) and [Timeo](https://github.com/42Timeo). This is a side project, built on evenings and weekends.

## Where it's at

It drives. Today the car:

- streams live video and sound over 5G to a desktop app
- follows a game controller
- lowers the picture quality when the network gets weak instead of freezing, then raises it again
- finds a direct route between car and driver on its own, and falls back to our server in under half a second when that route dies
- cuts the throttle and straightens the wheels if orders stop coming in

What it doesn't have yet is a body made for the job. More on that at the end.

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

On the mobile network, nobody from outside can reach the car. It's like an apartment with no doorbell: the car can call out, but you can't call in. So both sides call a small server we rent, and it passes messages between them.

That works, but every picture and every turn of the wheel then takes a detour, which added 15 to 30 ms each way for us. So once the two sides have met at the server, they also look for a direct road to each other (local addresses, IPv6, or a hole punched through the NAT). As soon as one answers, they switch to it. The server path stays ready the whole time, so when the direct one drops, for example when the car moves from one phone mast to another, the video doesn't even restart.

Video, sound and controls all share a single UDP socket on each side, so one working path carries everything. The details are in [docs/network-paths.md](docs/network-paths.md).

### What's in the repo

| Folder | What it is |
|---|---|
| `rpicam_stream/` | The camera program, in C++. Takes frames from libcamera, pushes them through the Pi's hardware H.264 encoder (V4L2), and sends RTP with forward error correction. Bitrate, FEC and keyframes can be changed while it runs. |
| `slave/` | Runs on the Pi, in Rust. Starts the camera and the microphone, talks to the server, and passes the driver's orders to the Arduino over serial. |
| `client/` | The driver's app, in Rust (egui + GStreamer). Shows the video, reads the gamepad, and decides how much bitrate and FEC to ask for based on loss and delay. |
| `server/` | The meeting point and relay, in Rust. |
| `common/` | Shared protocol and the path selection logic. |
| `serial_to_pwm/` | The Arduino sketch. Turns serial lines into PWM for the ESC and the steering servo, and goes back to neutral if nothing arrives for 100 ms. |
| `os/` | Boot config and systemd services for the Pi: modem bring-up, a watchdog that redoes the connection when it's lost, reverse SSH. |
| `docs/` | Longer notes on the network paths and on video latency. |

### Hardware

- an ordinary RC car, 2S battery
- Raspberry Pi 4B (we want its hardware H.264 encoder)
- Waveshare 5G HAT with a Quectel RM500U-CNV modem, and four stick-on antennas
- a CSI camera
- Arduino Nano for the ESC and the steering servo
- a phone power bank for the Pi and the modem

## Things that fought back

**The modem.** Getting it online is a long chain: unlock the SIM, switch the modem to the right mode, give it the operator's settings, wait for an address. Any step can fail. There are more than 50 tiny fixes over two days in the history, plus a small screen we built to watch the signal from nearby antennas. In the end we wrote a watchdog that checks the connection and redoes the whole setup when it's gone. Not elegant, but it keeps the car online.

**The camera chip that said yes.** To smooth out the data coming from the encoder, we asked it for a perfectly steady rate. It accepted the setting, confirmed it, then refused to start every single time. After two careful tests we had to admit the option was listed but didn't actually work. We got around it another way: full pictures, the heavy ones that clogged the line, are now mostly sent when the screen actually asks for one. The rest of the time the car only sends what changed.

**Lost milliseconds.** Every bit of delay between the camera and the screen means you turn too late, so the summer was a hunt for them. Along the way we got a picture that came out completely green, and we tried a well-known streaming method that turned out to add delay. That one was removed the same day.

**The setup.** For a long time every test meant starting each piece by hand, one after the other: server, driver app, remote login to the car, rebuild, car program. Change one thing and you did it all again. And the remote login dropped at random.

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

## Pi setup notes

These are our own notes from setting up the Pi. They're rough, but they're what we actually follow.

**OS.** DietPi on Debian 13 (Trixie), "safe" overclock. Flash the card, then `os/setup.sh` copies the boot config and services onto it.

**SSH.** In `/etc/ssh/sshd_config`:

```
GatewayPorts clientspecified
```

Set up an SSH key and add `reverse-ssh.service`. [mosh](https://mosh.org/) makes the remote session a lot more stable.

**Camera.** Driver doc: <https://spotpear.com/index.php/index/study/detail/id/754.html>. rpicam needs the default imx462.

**32-bit libs** (the control scripts needed them):

```sh
sudo dpkg --add-architecture armhf
sudo apt update
sudo apt install libc6:armhf libstdc++6:armhf
```

**PWM values**, in microseconds:

```
throttle:  993 (backward) - 1486 1487 1492 (neutral) - 1986 (forward)
steering:  1087 - 1480 - 1880
```

**Modem.**

```sh
sudo apt install -y modemmanager network-manager usb-modeswitch
```

Remove the ifupdown conf in `/etc/network/interfaces`, then:

```sh
echo -e "AT\r" | sudo tee /dev/ttyUSB2
echo -e "AT+QCFG="usbnet",2\r" | sudo tee /dev/ttyUSB2
echo -e "AT+CFUN=1,1\r" | sudo tee /dev/ttyUSB2

sudo systemctl enable --now ModemManager
mmcli -L
```

To check its state: `mmcli -m 0` and `journalctl -u ModemManager -f`.

```sh
sudo nmcli connection add type gsm ifname "*" con-name "4g-rm500u" apn "sl2sfr"
sudo nmcli connection modify 4g-rm500u connection.autoconnect yes
sudo nmcli connection up 4g-rm500u

# if the operator wants authentication
sudo nmcli connection modify 4g-rm500u gsm.username "user" gsm.password "pass"

nmcli connection show 4g-rm500u
```

Signal and location:

```sh
mmcli -m 0 --signal-setup=5
mmcli -m 0 --signal-get
mmcli -m 0 --location-get
```

**Bufferbloat.** Worth doing since the bitrate adapts:

```sh
sudo tc qdisc replace dev wwan0 root cake bandwidth 5mbit rtt 100ms
```

Set `bandwidth` a bit under what the uplink can really do, and `rtt` to the latency you usually measure.

## What the street taught us

We took it out for real tests several times in August 2026.

When the car stands still, the video is very good, even with a lot of movement in front of the camera. As soon as it starts rolling, the network gets worse and the video with it. On top of that the camera shakes with every bump, since nothing steadies it. Driving gets hard, especially near top speed, which is about 50 km/h even with all the extra gear.

The hardest part is probably the build itself. It's a small RC car with a 3D-printed lid. On top sit the Pi and its 5G board, already fairly heavy. Then an Arduino placed wherever it fits, cables everywhere, and a 600-gram power bank with two USB cables dangling. The antennas are the worst: four flat stick-on ones, and finding a good spot for each is a puzzle.

## What's next

The software does its job on a body that was never meant to carry it. So first, a proper build, where every part and every antenna has its place and the camera stops shaking. After that, it's time to see how far the phone network can take it.
