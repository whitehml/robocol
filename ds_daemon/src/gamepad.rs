//! Turning a front end's `GamepadInput` into the `robocol` packet the RC
//! reads.

use ds_proto::GamepadInput;
use robocol::packets::Gamepad;

/// The packet for `input`; any field left unset is zero / released.
pub fn to_packet(input: &GamepadInput) -> Gamepad {
    Gamepad {
        left_stick_x: input.left_stick_x.unwrap_or(0.0),
        left_stick_y: input.left_stick_y.unwrap_or(0.0),
        right_stick_x: input.right_stick_x.unwrap_or(0.0),
        right_stick_y: input.right_stick_y.unwrap_or(0.0),
        left_trigger: input.left_trigger.unwrap_or(0.0),
        right_trigger: input.right_trigger.unwrap_or(0.0),
        dpad_up: input.dpad_up.unwrap_or(false),
        dpad_down: input.dpad_down.unwrap_or(false),
        dpad_left: input.dpad_left.unwrap_or(false),
        dpad_right: input.dpad_right.unwrap_or(false),
        a: input.a.unwrap_or(false),
        b: input.b.unwrap_or(false),
        x: input.x.unwrap_or(false),
        y: input.y.unwrap_or(false),
        start: input.start.unwrap_or(false),
        back: input.back.unwrap_or(false),
        left_bumper: input.left_bumper.unwrap_or(false),
        right_bumper: input.right_bumper.unwrap_or(false),
        left_stick_button: input.left_stick_button.unwrap_or(false),
        right_stick_button: input.right_stick_button.unwrap_or(false),
        ..Gamepad::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_fields_are_neutral_and_set_ones_carry_over() {
        let packet = to_packet(&GamepadInput {
            left_stick_y: Some(-0.5),
            a: Some(true),
            ..Default::default()
        });
        assert_eq!(packet.left_stick_y, -0.5);
        assert!(packet.a);
        assert_eq!(packet.right_stick_x, 0.0);
        assert!(!packet.b);
    }
}
