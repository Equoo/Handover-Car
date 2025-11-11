use laminar::{SocketEvent, Socket, Packet};
use crossbeam_channel::Sender;
use std::time::Duration;
use std::time::Instant;
use std::error::Error;
use std::thread;
use bincode;

mod messages;
use crate::messages::Message;

const SERVER: &str = "37.59.106.4:9500";



fn handle_message(msg: Message, sender: &Sender<Packet>) -> Result<(), Box<dyn Error>> {
	println!("Message: {:?}", msg);
	Ok(())
}



fn main() -> Result<(), Box<dyn Error>> {

	let addr = "127.0.0.1:12353";
	let mut socket = Socket::bind(addr)?;

	let server = SERVER.parse().unwrap();

	let ping_message: Message = Message::SlaveAnnounce {
		id: "1".to_string()
	};

	let bincode_config = bincode::config::standard();

	let sender = socket.get_packet_sender();

	let ping_sender = socket.get_packet_sender();
	let _thread = thread::spawn(move || {
		loop {
			ping_sender.send(Packet::reliable_unordered(
				server,
				bincode::encode_to_vec(&ping_message, bincode::config::standard()).unwrap(),
			)).unwrap();
			thread::sleep(Duration::from_millis(10));
		}
	});


	loop {	
		thread::sleep(Duration::from_millis(10));
		socket.manual_poll(Instant::now());
		match socket.recv() {
			Some(SocketEvent::Packet(packet)) => {
				if packet.addr() == server {
					// println!("Server sent: {:#?}", packet.payload());

					match bincode::decode_from_slice::<Message, _>(packet.payload(), bincode_config) {
						Err(err) => eprintln!("decoding error {:?}", err),
						Ok((msg, size)) => {
							match handle_message(msg, &sender) {
								Err(err) => eprintln!("handling error {:?}", err),
								_ => { }
							}
						}
					}


				} else {
					println!("Unknown sender.");
				}
			}
			Some(SocketEvent::Timeout(_)) => {}
			_ => {},
		}
	}

}

