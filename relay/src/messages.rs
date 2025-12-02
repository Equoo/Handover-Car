use bincode::{Decode, Encode};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Encode, Decode, Clone)]
pub enum Message {
    SlaveAnnounce {
        id: String,
        send: u128,
        server_receive: u128,
        server_send: u128,
    },
    SlaveLocation {
        id: String,
        latitude: f32,
        altitude: f32,
    },
    MasterSubscribe {
        slave_id: String,
    },
    MasterInput {
        slave_id: String,
        throttle: i16,
        steering: i16,
    },
}

pub fn metric_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
