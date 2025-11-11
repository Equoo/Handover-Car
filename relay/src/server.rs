use bimap::BiHashMap;
use bluest::pairing::PairingAgent;
use laminar::{SocketEvent, Socket, Packet};
use anyhow::Result;
use crossbeam_channel::Sender;
use std::net::SocketAddr;
use std::error::Error;
use std::thread;
use std::collections::{HashMap, HashSet};
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use bincode;

mod messages;
use crate::messages::Message;

fn video_pipe(id: String, clients: Arc<Mutex<HashMap<String, SocketAddr>>>) -> Result<()> {
	let mut socket = UdpSocket::bind(format!("0.0.0.0:950{}", id))?;
	let mut subscribes: HashSet<SocketAddr> = HashSet::new();

	loop {
		let mut buf = [0; 2 ^ 20];
		let (amt, src) = socket.recv_from(&mut buf)?;
		if let Some(addr) = clients.lock().unwrap().get(&id) {
			if (addr.ip() != src.ip()) {
				subscribes.insert(src);
				continue;
			}
			println!("{:?}", &buf);
			for adr in &subscribes {
				socket.send_to(&buf, adr);
			}
		}	
	}
}

fn handle_message(msg: Message, peer: SocketAddr, clients: Arc<Mutex<HashMap<String, SocketAddr>>>, sender: &Sender<Packet>) -> Result<()> {

	// println!("Message: {} {:?}", peer, msg);
	match msg {
		Message::SlaveAnnounce { id } => {
			if (clients.lock().unwrap().insert(id.clone(), peer.clone()).is_none()) {
				println!("New Slave: {} from {:?}", &id, &peer);
				let _thread = thread::spawn(move || video_pipe(id, clients));
			}
		}
		Message::MasterInput { ref slave_id, motor_right, motor_left } => {
			if let Some(target) = clients.lock().unwrap().get(slave_id) {
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
	let mut socket = Socket::bind("0.0.0.0:9500")?;

	let event_receiver = socket.get_event_receiver();
	let sender = socket.get_packet_sender();
	let _thread = thread::spawn(move || socket.start_polling());	
	let bincode_config = bincode::config::standard();

	let clients: Arc<Mutex<HashMap<String, SocketAddr>>> = Arc::new(Mutex::new(HashMap::new()));
	let subscribes: Arc<Mutex<HashMap<String, Vec<SocketAddr>>>> = Arc::new(Mutex::new(HashMap::new()));

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
								match handle_message(msg, peer, clients.clone(), &sender) {
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
