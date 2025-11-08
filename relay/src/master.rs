use std::time::Instant;
use laminar::{SocketEvent, Socket, Result, Packet};
use bincode;

mod messages;
use crate::messages::Message;

const SERVER: &str = "127.0.0.1:8080";

fn main() -> Result<()> {

	let addr = "127.0.0.1:12352";
	let mut socket = Socket::bind(addr)?;

	let server = SERVER.parse().unwrap();

	let message: Message = Message::MasterInput {
		slave_id: "rc-1".to_string(),
		motor_right: 12.0,
		motor_left: 21.0,
	};


	loop {

		socket.send(Packet::reliable_unordered(
				server,
				bincode::encode_to_vec(&message, bincode::config::standard()).unwrap(),
		))?;


		socket.manual_poll(Instant::now());

		match socket.recv() {
			Some(SocketEvent::Packet(packet)) => {
				if packet.addr() == server {
					println!("Server sent: {}", String::from_utf8_lossy(packet.payload()));
				} else {
					println!("Unknown sender.");
				}
			}
			Some(SocketEvent::Timeout(_)) => {}
			_ => {},
		}
	}

}
