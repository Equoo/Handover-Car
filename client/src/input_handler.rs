use gilrs::{Gilrs, Event, EventType, GamepadId, Axis};
use common::{PilotCommand, ServerToSlave, Message, PacketType, ClientMessage};
use tracing::{debug, info, warn};
use super::app::{AppState, SLAVE};
use std::sync::Arc;

const STEERING_DEADZONE: f32 = 0.08;
const TRIGGER_DEADZONE: f32 = 0.03;

#[derive(Debug, Clone, Copy, Default)]
pub struct CarCommand {
    pub steering: f32,
    pub throttle: f32,
    pub reverse: f32,
}

impl CarCommand {
    fn to_packet(&self) -> PilotCommand {
        let power = self.throttle - self.reverse;
        PilotCommand::Input {
            throttle: (power * -14.) as i16 + 90,
            steering: (self.steering * -14.) as i16 + 90,
        }
    }

    pub fn send(&self, state: Arc<AppState>) {
        let pilot_cmd = self.to_packet();

        if self.steering != 0. || self.throttle != 0. || self.reverse != 0. {
            info!("sending pilot command: {:?}", pilot_cmd);
        }


        let msg = Message::Client(ClientMessage::Pilot {
            slave_id: SLAVE.to_string(),
            cmd: self.to_packet(),
        });

        if state.sendmsg_to_server(&msg, PacketType::Unreliable).is_err() {
            warn!("failed to send input packet to server");
        }

    }
}


pub struct InputHandler {
    gamepad: Option<GamepadId>,
    gilrs: Gilrs,
    command: CarCommand,
}


impl InputHandler {

    pub fn new() -> Self {
        let gilrs = Gilrs::new().unwrap();

        for (_id, gamepad) in gilrs.gamepads() {
            info!("gamepad: {} is {:?}", gamepad.name(), gamepad.power_info());
        }

        Self {
            gamepad: None,
            gilrs,
            command: Default::default()
        }
    }


    pub fn update(&mut self, state: Arc<AppState>) {

        while let Some(Event { id, event, time, .. }) = self.gilrs.next_event() {
            debug!("{:?} New event from {}: {:?}", time, id, event);
            self.gamepad = Some(id);
            match event {
                EventType::Connected => {
                    let gamepad = self.gilrs.gamepad(id);
                    info!("gamepad connected: {} ({:?})", gamepad.name(), id);
                }
                EventType::Disconnected => {
                    warn!("gamepad {:?} disconnected -- stopping car", id);
                    self.command = CarCommand::default();
                }
                _ => { }
            }
        }


        if let Some(gamepad) = self.gamepad.map(|id| self.gilrs.gamepad(id)) {
            let raw_steering = gamepad.value(Axis::LeftStickX);
            self.command.steering = apply_deadzone(raw_steering, STEERING_DEADZONE);
 
            let raw_throttle = gamepad.value(Axis::RightZ); // right trigger, 0.0..1.0
            self.command.throttle = apply_deadzone(raw_throttle, TRIGGER_DEADZONE).max(0.0);
 
            let raw_reverse = gamepad.value(Axis::LeftZ); // left trigger, 0.0..1.0
            self.command.reverse = apply_deadzone(raw_reverse, TRIGGER_DEADZONE).max(0.0);
        }

        self.command.send(state);

    }
}

fn apply_deadzone(value: f32, deadzone: f32) -> f32 {
    if value.abs() < deadzone {
        0.0
    } else {
        value
    }
}

