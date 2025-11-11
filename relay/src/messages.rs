use bincode::{Encode, Decode};

#[derive(Debug, Encode, Decode, Clone)]
pub enum Message {
	SlaveAnnounce {
		id: String,
	},
	SlaveLocation {
		id: String,
		latitude: f32,
		altitude: f32,
	},
	SlaveVideo {
		port: i32
	},
	MasterSubscribe {
		slave_id: String,
	},
	MasterSubscribeVideo {
		slave_id: String,
	},
	MasterInput {
		slave_id: String,
		motor_right: f32,
		motor_left: f32,
	},
}

