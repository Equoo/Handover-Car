use laminar::{SocketEvent, Socket, Packet};
use crossbeam_channel::Sender;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::error::Error;
use std::thread;
use bincode;

mod messages;
use crate::messages::Message;


fn handle_message(msg: Message, peer: SocketAddr, clients: &mut HashMap<String, SocketAddr>, sender: &Sender<Packet>) -> Result<(), Box<dyn Error>> {

	// println!("Message: {} {:?}", peer, msg);
	match msg {
		Message::SlaveAnnounce { id } => {
			clients.insert(id, peer);
			println!("Message: {:?}", clients);
		}
		Message::MasterInput { ref slave_id, motor_right, motor_left } => {
			if let Some(target) = clients.get(slave_id) {
				sender.send(Packet::reliable_unordered(
					*target,
					bincode::encode_to_vec(msg.clone(), bincode::config::standard()).unwrap(),
				))?;
			}
		}
		_ => { }
	}
	Ok(())
}


fn main() -> Result<(), Box<dyn Error>> {

	let mut socket = Socket::bind("0.0.0.0:8080")?;

	let event_receiver = socket.get_event_receiver();
	let sender = socket.get_packet_sender();

	let _thread = thread::spawn(move || socket.start_polling());
	
	let bincode_config = bincode::config::standard();

	let mut clients: HashMap<String, SocketAddr> = HashMap::new();

	loop {
		let result = event_receiver.recv();
		match result {
			Err(e) => {
				println!("Something went wrong when receiving, error: {:?}", e);
			}
			Ok(socket_event) => {
				match socket_event {
					// all of these event are never received
					SocketEvent::Connect(_connect_event) => { }
					SocketEvent::Timeout(_timeout_event) => { }
					SocketEvent::Disconnect(_disconnect_event) => { }

					SocketEvent::Packet(packet) => {
						let peer: SocketAddr = packet.addr();

						match bincode::decode_from_slice::<Message, _>(packet.payload(), bincode_config) {
							Err(err) => eprintln!("decoding error {:?}", err),
							Ok((msg, size)) => {
								match handle_message(msg, peer, &mut clients, &sender) {
									Err(err) => eprintln!("handling error {:?}", err),
									_ => { }
								}
							}
						}
					}
				}
			}
		}
	}
}
