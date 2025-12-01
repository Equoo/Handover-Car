use anyhow::{Result, anyhow};
use bincode;
use crossbeam_channel::Sender;
use futures_util::future::FutureExt;
use laminar::{Packet, Socket, SocketEvent};
use std::error::Error;
use std::process::Command;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tokio::time::sleep;

mod messages;
use crate::messages::Message;
use crate::messages::metric_now;

const SERVER: &str = "37.59.106.4:9500";

async fn handle_message(msg: Message, sender: &Sender<Packet>) -> Result<(), Box<dyn Error>> {
    match msg {
        Message::MasterInput {
            slave_id,
            motor_left,
            motor_right,
        } => {}
        _ => {}
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let addr = "0.0.0.0:12000";
    let mut socket = Socket::bind(addr)?;

    let server = SERVER.parse().unwrap();

    let bincode_config = bincode::config::standard();

    let sender = socket.get_packet_sender();

    let ping_sender = socket.get_packet_sender();
    let _thread = thread::spawn(move || {
        loop {
            let ping_message: Message = Message::SlaveAnnounce {
                id: "1".to_string(),
                send: metric_now(),
                server_send: 0,
                server_receive: 0,
            };

            ping_sender
                .send(Packet::unreliable(
                    server,
                    bincode::encode_to_vec(&ping_message, bincode::config::standard()).unwrap(),
                ))
                .unwrap();
            thread::sleep(Duration::from_millis(10));
        }
    });

    thread::sleep(Duration::from_secs(3));

    Command::new("bash")
        .args([
            "-c",
            "rpicam-vid -t 0 --codec h264 --inline --intra 10 --width 720 --height 480 --framerate 60 --bitrate 3000000 -n -o - | nc -u server 13001",
        ])
        .spawn()
        .expect("failed to start video");

    loop {
        thread::sleep(Duration::from_millis(10));
        socket.manual_poll(Instant::now());
        match socket.recv() {
            Some(SocketEvent::Packet(packet)) => {
                if packet.addr() == server {
                    // println!("Server sent: {:#?}", packet.payload());

                    match bincode::decode_from_slice::<Message, _>(packet.payload(), bincode_config)
                    {
                        Err(err) => eprintln!("decoding error {:?}", err),
                        Ok((msg, size)) => match handle_message(msg, &sender).await {
                            Err(err) => eprintln!("handling error {:?}", err),
                            _ => {}
                        },
                    }
                } else {
                    println!("Unknown sender.");
                }
            }
            Some(SocketEvent::Timeout(_)) => {}
            _ => {}
        }
    }
}
