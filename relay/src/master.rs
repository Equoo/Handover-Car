use bluest::Adapter;
use anyhow::{Result, anyhow};
use futures_lite::StreamExt;
use laminar::{SocketEvent, Socket, Packet};
use crossbeam_channel::Receiver;
use std::net::{UdpSocket, SocketAddr};
use std::time::Duration;
use std::thread;

mod messages;
use crate::messages::Message;

const SERVER: &str = "37.59.106.4:9500";

async fn receive_messages(receiver: Receiver<SocketEvent>) {
	let server = SERVER.parse().unwrap();
	
	loop {
		match receiver.recv() {
			Err(e) => {
				println!("Something went wrong when receiving, error: {:?}", e);
			}
			Ok(event) => {
				match event {
					SocketEvent::Packet(packet) => {
						if packet.addr() == server {
							println!("Server sent: {}", String::from_utf8_lossy(packet.payload()));
						} else {
							println!("Unknown sender.");
						}
					}
					_ => { }
				}
			}
		}
	}
}

struct bluetooth_input {
	adapter: Adapter
}

impl bluetooth_input {
	pub async fn new() -> Result<bluetooth_input> {
		let adapter = Adapter::default().await.ok_or(anyhow::anyhow!("feur"))?;
		adapter.wait_available().await?;

		let adp = adapter.clone();
		println!("starting scan");
		let mut scan = adp.scan(&[]).await?;
		println!("scan started");
		while let Some(discovered_device) = scan.next().await {
			println!(
				"{}{}: {:?}",
				discovered_device.device.name().as_deref().unwrap_or("(unknown)"),
				discovered_device
				.rssi
				.map(|x| format!(" ({}dBm)", x))
				.unwrap_or_default(),
				discovered_device.adv_data.services
			);
		}
		
		Ok(bluetooth_input{adapter})
	}
}

fn receive_video(server: SocketAddr, slave: String) -> Result<()> {
	let mut socket = UdpSocket::bind("127.0.0.1:3545")?;
	socket.set_nonblocking(true)?;
	let mut received_video = false;
	let server_video: SocketAddr = format!("37.59.106.4:950{}", slave).parse().unwrap();

	loop {
		let buf = [1, 1];
		socket.send_to(&buf, server);	
		thread::sleep(Duration::from_millis(1));
		let mut buf = [0; 2 ^ 20];
		match socket.recv_from(&mut buf) {
			Ok((amt, src)) => {
				socket.send_to(&buf, server_video);
			},
			Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
				let buf = [1];
				socket.send_to(&buf, server_video);
			},
			Err(e) => return Err(e.into())
		}
	}
}

#[tokio::main]
async fn main() -> Result<()> {
	let addr = "127.0.0.1:12352";
	let server: SocketAddr = SERVER.parse().unwrap();
	let slave = "1";

	let mut socket = Socket::bind(addr)?;
	let receiver = socket.get_event_receiver();
	let sender = socket.get_packet_sender();

	let message: Message = Message::MasterInput {
		slave_id: "1".to_string(),
		motor_right: 12.0,
		motor_left: 21.0,
	};

	let _thread = thread::spawn(move || socket.start_polling());
	let _thread = thread::spawn(|| receive_messages(receiver));
	let cl_server = server.clone();
	let _thread = thread::spawn(move || receive_video(cl_server, slave.to_string()));

	// let _blue = bluetooth_input::new().await;

	loop {
		sender.send(Packet::reliable_unordered(
			server,
			bincode::encode_to_vec(&message, bincode::config::standard()).unwrap(),
		))?;
		thread::sleep(Duration::from_millis(10));
	}

}
