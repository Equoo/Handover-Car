use anyhow::{Result, anyhow};
use bincode;
use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
use crossbeam_channel::Sender;
use futures_util::future::FutureExt;
use laminar::{Packet, Socket, SocketEvent};
use std::error::Error;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tokio::time::sleep;

mod messages;
use crate::messages::Message;
use crate::messages::metric_now;

// BuWizz BLE constants
const BUWIZZ_SERVICE_UUID: &str = "936E67B1-1999-B388-8144-FB74D1920550";
const BUWIZZ_CHARACTERISTIC_UUID: &str = "50052901-74fb-4481-88b3-9919b1676e93";

pub struct BluetoothInput {
    adapter: Adapter,
    device: Option<Peripheral>,
    characteristic: Option<btleplug::api::Characteristic>,
}

impl BluetoothInput {
    pub async fn new() -> Result<Self> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or(anyhow!("No Bluetooth adapter found"))?;

        Ok(BluetoothInput {
            adapter,
            device: None,
            characteristic: None,
        })
    }

    pub async fn connect(&mut self) -> Result<()> {
        println!("[BLE] Scanning for BuWizz...");
        self.adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(|e| anyhow!("scan failed: {e}"))?;

        sleep(Duration::from_secs(5)).await;
        self.adapter.stop_scan().await.ok();

        let peripherals = self.adapter.peripherals().await?;
        let mut device = None;
        for p in peripherals {
            if let Some(Some(name)) = p.properties().await?.map(|props| props.local_name) {
                if name.contains("BuWizz") {
                    device = Some(p);
                }
            }
        }
        if device.is_none() {
            return Err(anyhow!("No device found"));
        }
        let device = device.unwrap();

        println!(
            "[BLE] Found {:?}",
            device.properties().await?.unwrap().local_name
        );
        device.connect().await?;
        device.discover_services().await?;

        // Find the motor characteristic
        let characteristic = device
            .characteristics()
            .into_iter()
            .find(|c| {
                c.uuid
                    .to_string()
                    .eq_ignore_ascii_case(BUWIZZ_CHARACTERISTIC_UUID)
            })
            .ok_or(anyhow!("BuWizz motor characteristic not found"))?;

        println!("[BLE] Connected to BuWizz!");
        self.device = Some(device);
        self.characteristic = Some(characteristic);

        Ok(())
    }

    pub async fn send_motors(&self, left: i8, right: i8) -> Result<()> {
        let device = self.device.as_ref().ok_or(anyhow!("Not connected"))?;
        let characteristic = self
            .characteristic
            .as_ref()
            .ok_or(anyhow!("No characteristic"))?;
        let packet = Self::build_motor_command(left, right);

        device
            .write(characteristic, &packet, WriteType::WithoutResponse)
            .await?;

        println!("[BLE] Motors → L={left:4} R={right:4}");
        Ok(())
    }

    pub fn build_motor_command(m5: i8, m6: i8) -> [u8; 9] {
        fn clamp(v: i8) -> u8 {
            v.clamp(-127, 127) as u8
        }
        [
            0x30,
            0,
            0,
            0,
            0, // m1..m4 unused
            clamp(m5),
            clamp(m6),
            0x00, // brake
            0x00,
        ]
    }

    pub async fn disconnect(&mut self) {
        if let Some(device) = &self.device {
            let _ = device.disconnect().await;
            println!("[BLE] Disconnected.");
        }
        self.device = None;
        self.characteristic = None;
    }
}

const SERVER: &str = "37.59.106.4:9500";

async fn handle_message(
    msg: Message,
    sender: &Sender<Packet>,
    ble: &BluetoothInput,
) -> Result<(), Box<dyn Error>> {
    match msg {
        Message::MasterInput {
            slave_id,
            motor_left,
            motor_right,
        } => {
            ble.send_motors(motor_left as i8, motor_right as i8).await?;
        }
        _ => {}
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let addr = "0.0.0.0:12353";
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

    // BLUETHOOTH TEST
    let mut ble = BluetoothInput::new().await?;
    ble.connect().await?;

    for i in (0..127).step_by(20) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    ble.send_motors(0, 0).await?;
    // ble.disconnect().await;
    // BLUETHOOTH TEST

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
                        Ok((msg, size)) => match handle_message(msg, &sender, &ble).await {
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
