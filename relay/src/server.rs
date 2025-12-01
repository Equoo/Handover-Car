use anyhow::Result;
use bimap::BiHashMap;
use bincode;
use crossbeam_channel::Sender;
use laminar::{Packet, Socket, SocketEvent};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::thread;

mod messages;
use crate::messages::Message;
use crate::messages::metric_now;

fn video_pipe(id: String, clients: Arc<Mutex<HashMap<String, SocketAddr>>>) -> Result<()> {
    let mut socket = UdpSocket::bind(format!("0.0.0.0:1300{}", id))?;
    let mut subscribes: HashSet<SocketAddr> = HashSet::new();

    loop {
        let mut buf = [0; 1 << 15];
        let (amt, src) = socket.recv_from(&mut buf)?;
        if let Some(addr) = clients.lock().unwrap().get(&id) {
            if addr.ip() != src.ip() {
                subscribes.insert(src);
                continue;
            }
            for adr in &subscribes {
                socket.send_to(&buf[..amt], adr);
            }
        }
    }
}

fn handle_message(
    msg: Message,
    peer: SocketAddr,
    clients: Arc<Mutex<HashMap<String, SocketAddr>>>,
    subscribes: Arc<Mutex<HashMap<String, Vec<SocketAddr>>>>,
    sender: &Sender<Packet>,
) -> Result<()> {
    // println!("Message: {} {:?}", peer, msg);
    match msg {
        Message::SlaveAnnounce {
            id,
            send,
            server_receive,
            server_send,
        } => {
            let receive = metric_now();
            let id_cp = id.clone();
            if (clients
                .lock()
                .unwrap()
                .insert(id.clone(), peer.clone())
                .is_none())
            {
                println!("New Slave: {} from {:?}", &id, &peer);
                let _thread = thread::spawn(move || video_pipe(id_cp, clients));
            }
            if let Some(targets) = subscribes.lock().unwrap().get(&id) {
                let msg = Message::SlaveAnnounce {
                    id: id.clone(),
                    send,
                    server_receive: receive,
                    server_send: metric_now(),
                };
                for target in targets {
                    sender.send(Packet::unreliable(
                        *target,
                        bincode::encode_to_vec(msg.clone(), bincode::config::standard()).unwrap(),
                    ))?;
                }
            }
        }
        Message::MasterSubscribe { ref slave_id } => {
            if subscribes.lock().unwrap().get(slave_id).is_none() {
                subscribes
                    .lock()
                    .unwrap()
                    .insert(slave_id.clone(), Vec::new());
            }
            if subscribes
                .lock()
                .unwrap()
                .get(slave_id)
                .unwrap()
                .iter()
                .find(|v| v.to_owned().to_owned() == peer)
                .is_none()
            {
                subscribes
                    .lock()
                    .unwrap()
                    .get_mut(slave_id)
                    .unwrap()
                    .push(peer);
            }
        }
        Message::MasterInput {
            ref slave_id,
            motor_right,
            motor_left,
        } => {
            if let Some(target) = clients.lock().unwrap().get(slave_id) {
                sender.send(Packet::reliable_unordered(
                    *target,
                    bincode::encode_to_vec(msg.clone(), bincode::config::standard()).unwrap(),
                ))?;
            }
        }
        _ => {}
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
    let subscribes: Arc<Mutex<HashMap<String, Vec<SocketAddr>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    loop {
        let result = event_receiver.recv();
        match result {
            Err(e) => {
                println!("Something went wrong when receiving, error: {:?}", e);
            }
            Ok(socket_event) => {
                match socket_event {
                    // all of these event are never received
                    SocketEvent::Connect(_connect_event) => {}
                    SocketEvent::Timeout(_timeout_event) => {}
                    SocketEvent::Disconnect(_disconnect_event) => {}

                    SocketEvent::Packet(packet) => {
                        let peer: SocketAddr = packet.addr();

                        match bincode::decode_from_slice::<Message, _>(
                            packet.payload(),
                            bincode_config,
                        ) {
                            Err(err) => eprintln!("decoding error {:?}", err),
                            Ok((msg, size)) => {
                                match handle_message(
                                    msg,
                                    peer,
                                    clients.clone(),
                                    subscribes.clone(),
                                    &sender,
                                ) {
                                    Err(err) => eprintln!("handling error {:?}", err),
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
