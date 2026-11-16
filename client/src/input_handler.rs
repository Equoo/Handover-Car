// SPDX-FileCopyrightText: 2026 David Derny
// SPDX-FileCopyrightText: 2026 Handover contributors (see AUTHORS)
//
// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0

use gilrs::{Gilrs, Event, EventType, GamepadId, Axis};
use tracing::{debug, info, warn};
use super::app::AppState;
use std::sync::Arc;

const STEERING_DEADZONE: f32 = 0.06;
const TRIGGER_DEADZONE: f32 = 0.03;

#[derive(Debug, Clone, Copy, Default)]
pub struct CarCommand {
    pub steering: f32,
    pub throttle: f32,
    pub reverse: f32,
}

impl CarCommand {
    /// Maps the stick axes onto the servo/ESC pulse range the Arduino expects
    /// (see the PWM figures in `os/TODO.md`).
    fn to_pulses(self) -> (i16, i16) {
        let power = self.throttle - self.reverse;
        (
            (power * -20.) as i16 + 90,
            (self.steering * -15.) as i16 + 92,
        )
    }

    /// Sent on the data plane rather than through the server, so that once a
    /// direct path is up the steering stops paying for a round trip via the
    /// VPS -- which is the half of the link a driver actually feels.
    pub fn send(&self, state: Arc<AppState>) {
        let (throttle, steering) = self.to_pulses();

        if self.steering != 0. || self.throttle != 0. || self.reverse != 0. {
            info!("sending pilot command: throttle={throttle} steering={steering}");
        }

        state.send_input(throttle, steering);
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
            
            let raw_throttle = -gamepad.value(Axis::RightStickY); // right trigger, 0.0..1.0
            // self.command.throttle = apply_deadzone(raw_throttle, TRIGGER_DEADZONE).max(0.0);
            self.command.throttle = apply_deadzone(raw_throttle, TRIGGER_DEADZONE);
 
            // let raw_reverse = gamepad.value(Axis::LeftTrigger); // left trigger, 0.0..1.0
            let raw_reverse = 0.0;
            self.command.reverse = apply_deadzone(raw_reverse, TRIGGER_DEADZONE).max(0.0);
            debug!("RightZ {}, lEftZ {}", raw_throttle, raw_reverse);
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

