use anyhow::{Result, anyhow};
use crossbeam_channel::{Receiver, Sender};
use laminar::{Packet, Socket, SocketEvent};
use pasts::Loop;
use std::error::Error;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::task::Poll::{self, Pending, Ready};
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};
use stick::{Controller, Event, Listener};

mod messages;
use crate::messages::Message;
use crate::messages::metric_now;

type Exit = usize;

struct State {
    sender: Sender<Packet>,
    listener: Listener,
    controllers: Vec<Controller>,
    rumble: (f32, f32),
    server: SocketAddr,
    dir: Arc<Mutex<f64>>,
    power: Arc<Mutex<i8>>,
    power_max: i8,
}

fn to_motor_vector(input: f64, power: i8) -> (i8, i8) {
    // Clamp input between -1.0 and 1.0
    let v = input.clamp(-1.0, 1.0);
    let p = power as f64;

    match v {
        x if x == -1.0 => (-p as i8, p as i8),
        x if x == 0.0 => (p as i8, p as i8),
        x if x == 1.0 => (p as i8, -p as i8),
        x if x < 0.0 => {
            // interpolate between -1 and 0
            let t = (x + 1.0) / 1.0;
            let left = (-p + t * (p - -p)).round() as i8;
            let right = p as i8;
            (left, right)
        }
        x => {
            // interpolate between 0 and 1
            let t = x;
            let left = p as i8;
            let right = (p - t * (p - -p)).round() as i8;
            (left, right)
        }
    }
}

impl State {
    fn connect(&mut self, controller: Controller) -> Poll<Exit> {
        println!(
            "Connected p{}, id: {:016X}, name: {}",
            self.controllers.len() + 1,
            controller.id(),
            controller.name(),
        );
        self.controllers.push(controller);
        Pending
    }

    fn event(&mut self, id: usize, event: Event) -> Poll<Exit> {
        let player = id + 1;
        // println!("p{}: {}", player, event);
        match event {
            Event::Disconnect => {
                self.controllers.swap_remove(id);
            }
            Event::JoyY(y) => {
                *self.power.lock().unwrap() = (y * -1. * self.power_max as f64) as i8;
            }
            Event::CamX(x) => {
                *self.dir.lock().unwrap() = x;
            }
            Event::MenuR(true) => return Ready(player),
            _ => {}
        }
        Pending
    }
}

const SERVER: &str = "37.59.106.4:9500";

fn handle_message(msg: Message) -> Result<()> {
    match msg {
        Message::SlaveAnnounce {
            id,
            send,
            server_receive,
            server_send,
        } => {
            let now_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();

            let elapsed_total = now_ns as i128 - send as i128;
            let elapsed_slave = server_receive as i128 - send as i128;
            let elapsed_master = now_ns as i128 - server_send as i128;

            println!(
                "One-way latency: total={:.3} ms, Slave→Server={:.3} ms, Server→Master={:.3} ms",
                elapsed_total as f64 / 1_000_000.0,
                elapsed_slave as f64 / 1_000_000.0,
                elapsed_master as f64 / 1_000_000.0
            );
        }
        _ => {}
    }
    Ok(())
}

fn receive_messages(receiver: Receiver<SocketEvent>) {
    let server = SERVER.parse().unwrap();
    let bincode_config = bincode::config::standard();

    loop {
        match receiver.recv() {
            Err(e) => {
                println!("Something went wrong when receiving, error: {:?}", e);
            }
            Ok(event) => match event {
                SocketEvent::Packet(packet) => {
                    if packet.addr() == server {
                        match bincode::decode_from_slice::<Message, _>(
                            packet.payload(),
                            bincode_config,
                        ) {
                            Err(err) => eprintln!("decoding error {:?}", err),
                            Ok((msg, size)) => match handle_message(msg) {
                                Err(err) => eprintln!("handling error {:?}", err),
                                _ => {}
                            },
                        }
                    } else {
                        println!("Unknown sender.");
                    }
                }
                _ => {}
            },
        }
    }
}

fn receive_video(server: SocketAddr, slave: String) -> Result<()> {
    let mut socket = UdpSocket::bind("0.0.0.0:3466")?;
    socket.set_nonblocking(true)?;
    let mut received_video = false;
    let server_video: SocketAddr = format!("37.59.106.4:950{}", slave).parse().unwrap();
    let mut last = Instant::now();

    loop {
        let mut buf = [0; 1 << 15];
        // thread::sleep(Duration::from_millis(1));
        match socket.recv_from(&mut buf) {
            Ok((amt, src)) => {
                received_video = true;
                socket.send_to(&buf[..amt], "127.0.0.1:7654".parse::<SocketAddr>().unwrap());
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if !received_video && last.elapsed().as_millis() > 100 {
                    let buf = [1];
                    socket.send_to(&buf, server_video);
                    last = Instant::now();
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let addr = "0.0.0.0:12352";
    let server: SocketAddr = SERVER.parse().unwrap();
    let slave = "1";

    let mut socket = Socket::bind(addr)?;
    let receiver = socket.get_event_receiver();
    let sender = socket.get_packet_sender();

    let message: Message = Message::MasterSubscribe {
        slave_id: "1".to_string(),
    };
    sender.send(Packet::reliable_unordered(
        server,
        bincode::encode_to_vec(&message, bincode::config::standard()).unwrap(),
    ))?;

    let _thread = thread::spawn(move || socket.start_polling());
    let _thread = thread::spawn(|| receive_messages(receiver));
    let cl_server = server.clone();
    let _thread = thread::spawn(move || receive_video(cl_server, slave.to_string()));

    let mut state = State {
        listener: Listener::default(),
        sender: sender.clone(),
        server: server.clone(),
        controllers: Vec::new(),
        rumble: (0.0, 0.0),
        dir: Arc::new(Mutex::new(0.)),
        power: Arc::new(Mutex::new(0)),
        power_max: 50,
    };

    let cl_dir = state.dir.clone();
    let cl_power = state.power.clone();
    let _thread = thread::spawn(move || {
        loop {
            let mut dir = (0 as i8, 0 as i8);
            {
                dir = to_motor_vector(
                    cl_dir.lock().unwrap().clone(),
                    cl_power.lock().unwrap().clone(),
                );
            }
            println!("r: {}, l: {}", dir.1, dir.0);
            let mut message: Message = Message::MasterInput {
                slave_id: "1".to_string(),
                motor_right: dir.0 as f32 * -1.,
                motor_left: dir.1 as f32,
            };
            sender
                .send(Packet::unreliable(
                    server,
                    bincode::encode_to_vec(&message, bincode::config::standard()).unwrap(),
                ))
                .unwrap();
            thread::sleep(Duration::from_millis(50));
        }
    });

    let player_id = Loop::new(&mut state)
        .when(|s| &mut s.listener, State::connect)
        .poll(|s| &mut s.controllers, State::event)
        .await;
    Ok(())
}
