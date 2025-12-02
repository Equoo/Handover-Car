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
use std::{env, io};
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;
use tokio_serial::SerialPortBuilderExt;
use tokio_serial::SerialStream;

mod messages;
use crate::messages::Message;
use crate::messages::metric_now;

const SERVER: &str = "37.59.106.4:9500";

const default_port: &str = "/dev/ttyUSB0";
const default_baud: u32 = 115200u32;
const default_hz: u64 = 20u64;

async fn handle_message(
    msg: Message,
    sender: &Sender<Packet>,
    port: &mut SerialStream,
) -> Result<(), Box<dyn Error>> {
    match msg {
        Message::MasterInput {
            slave_id,
            throttle,
            steering,
        } => {
            let line = format!("{:} {:}\n", steering, throttle);
            let bytes = line.as_bytes();

            // Send initial line once
            port.write_all(bytes).await?;
            port.flush().await?;
        }
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

    Command::new("bash") // --intra 10
        .args([
            "-c",
            "rpicam-vid -t 0 --flush --codec h264 --inline --width 853 --height 480 --framerate 60 --bitrate 2000000 --segment 1 -n -o udp://37.59.106.4:13001",
        ])
        .spawn()
        .expect("failed to start video");

    let mut port = tokio_serial::new(default_port.clone(), default_baud)
        // default settings are 8N1; you can tweak with .parity(), .stop_bits(), .flow_control()
        .open_native_async()
        .expect("Failed to open serial port");

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
                        Ok((msg, size)) => match handle_message(msg, &sender, &mut port).await {
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
