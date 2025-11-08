use laminar::{SocketEvent, Socket, Result as laminarResult};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::thread;
use bincode;

mod messages;
use crate::messages::Message;


fn handle_message(msg: Message, peer: SocketAddr, clients: &mut HashMap<String, SocketAddr>, socket: &mut Socket) -> Result<(), Box<dyn std::Error>> {
	Ok(());
}


fn main() -> laminarResult<()> {

	let mut socket = Socket::bind("0.0.0.0:8080")?;
	let event_receiver = socket.get_event_receiver();

	let _thread = thread::spawn(move || socket.start_polling());
	
	let bincode_config = bincode::config::standard();

	let clients: HashMap<String, SocketAddr> = HashMap::new();


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

						match bincode::decode_from_slice::<Message, bincode::config::Configuration>(packet.payload(), bincode_config) {
							Err(err) => eprintln!("decoding error {:?}", err),
							Ok((msg, size)) => {
								println!("Message: {} {} {:?}", peer, size, msg);
								match handle_message(msg, peer, &mut clients, &mut socket) {
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
