//! Gamepad state packet (uid 2), 65 bytes total.
//!
//! `user` (offset 46) selects: 1 = gamepad1, 2 = gamepad2.

use crate::types::{GamepadType, GAMEPAD_ID_SYNTHETIC};
use crate::wire::{self, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Gamepad {
    pub seq: u16,
    pub version: u8,
    pub user: u8,
    pub id: i32,
    pub timestamp: i64,
    pub gamepad_type: GamepadType,

    pub left_stick_x: f32,
    pub left_stick_y: f32,
    pub right_stick_x: f32,
    pub right_stick_y: f32,
    pub left_trigger: f32,
    pub right_trigger: f32,

    pub dpad_up: bool,
    pub dpad_down: bool,
    pub dpad_left: bool,
    pub dpad_right: bool,
    pub a: bool,
    pub b: bool,
    pub x: bool,
    pub y: bool,
    pub guide: bool,
    pub start: bool,
    pub back: bool,
    pub left_bumper: bool,
    pub right_bumper: bool,
    pub left_stick_button: bool,
    pub right_stick_button: bool,
    pub touchpad: bool,
    pub touchpad_finger_1: bool,
    pub touchpad_finger_2: bool,
    pub touchpad_finger_1_x: f32,
    pub touchpad_finger_1_y: f32,
    pub touchpad_finger_2_x: f32,
    pub touchpad_finger_2_y: f32,
}

impl Default for Gamepad {
    fn default() -> Self {
        Gamepad {
            seq: 0,
            version: 5,
            user: 1,
            id: GAMEPAD_ID_SYNTHETIC,
            timestamp: 0,
            gamepad_type: GamepadType::Unknown,
            left_stick_x: 0.0,
            left_stick_y: 0.0,
            right_stick_x: 0.0,
            right_stick_y: 0.0,
            left_trigger: 0.0,
            right_trigger: 0.0,
            dpad_up: false,
            dpad_down: false,
            dpad_left: false,
            dpad_right: false,
            a: false,
            b: false,
            x: false,
            y: false,
            guide: false,
            start: false,
            back: false,
            left_bumper: false,
            right_bumper: false,
            left_stick_button: false,
            right_stick_button: false,
            touchpad: false,
            touchpad_finger_1: false,
            touchpad_finger_2: false,
            touchpad_finger_1_x: 0.0,
            touchpad_finger_1_y: 0.0,
            touchpad_finger_2_x: 0.0,
            touchpad_finger_2_y: 0.0,
        }
    }
}

impl Gamepad {
    pub const TOTAL_LEN: usize = 65; // header + 60-byte v5 payload

    fn buttons_bitfield(&self) -> u32 {
        let mut b = 0u32;
        b |= (self.touchpad_finger_1 as u32) << 17;
        b |= (self.touchpad_finger_2 as u32) << 16;
        b |= (self.touchpad as u32) << 15;
        b |= (self.left_stick_button as u32) << 14;
        b |= (self.right_stick_button as u32) << 13;
        b |= (self.dpad_up as u32) << 12;
        b |= (self.dpad_down as u32) << 11;
        b |= (self.dpad_left as u32) << 10;
        b |= (self.dpad_right as u32) << 9;
        b |= (self.a as u32) << 8;
        b |= (self.b as u32) << 7;
        b |= (self.x as u32) << 6;
        b |= (self.y as u32) << 5;
        b |= (self.guide as u32) << 4;
        b |= (self.start as u32) << 3;
        b |= (self.back as u32) << 2;
        b |= (self.left_bumper as u32) << 1;
        b |= self.right_bumper as u32;
        b
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = wire::with_header(super::UID_GAMEPAD, 60, self.seq);
        buf[5] = self.version;
        wire::put_i32(&mut buf, 6, self.id);
        wire::put_i64(&mut buf, 10, self.timestamp);
        wire::put_f32(&mut buf, 18, self.left_stick_x);
        wire::put_f32(&mut buf, 22, self.left_stick_y);
        wire::put_f32(&mut buf, 26, self.right_stick_x);
        wire::put_f32(&mut buf, 30, self.right_stick_y);
        wire::put_f32(&mut buf, 34, self.left_trigger);
        wire::put_f32(&mut buf, 38, self.right_trigger);
        wire::put_u32(&mut buf, 42, self.buttons_bitfield());
        buf[46] = self.user;
        buf[47] = self.gamepad_type as u8;
        buf[48] = self.gamepad_type as u8;
        wire::put_f32(&mut buf, 49, self.touchpad_finger_1_x);
        wire::put_f32(&mut buf, 53, self.touchpad_finger_1_y);
        wire::put_f32(&mut buf, 57, self.touchpad_finger_2_x);
        wire::put_f32(&mut buf, 61, self.touchpad_finger_2_y);
        buf
    }

    pub fn parse(buf: &[u8]) -> Result<Gamepad> {
        let buttons = wire::get_u32(buf, 42)?;
        let type_byte = wire::get_u8(buf, 48).or_else(|_| wire::get_u8(buf, 47))?;
        Ok(Gamepad {
            seq: wire::get_u16(buf, 3)?,
            version: wire::get_u8(buf, 5)?,
            id: wire::get_i32(buf, 6)?,
            timestamp: wire::get_i64(buf, 10)?,
            left_stick_x: wire::get_f32(buf, 18)?,
            left_stick_y: wire::get_f32(buf, 22)?,
            right_stick_x: wire::get_f32(buf, 26)?,
            right_stick_y: wire::get_f32(buf, 30)?,
            left_trigger: wire::get_f32(buf, 34)?,
            right_trigger: wire::get_f32(buf, 38)?,
            touchpad_finger_1: buttons & (1 << 17) != 0,
            touchpad_finger_2: buttons & (1 << 16) != 0,
            touchpad: buttons & (1 << 15) != 0,
            left_stick_button: buttons & (1 << 14) != 0,
            right_stick_button: buttons & (1 << 13) != 0,
            dpad_up: buttons & (1 << 12) != 0,
            dpad_down: buttons & (1 << 11) != 0,
            dpad_left: buttons & (1 << 10) != 0,
            dpad_right: buttons & (1 << 9) != 0,
            a: buttons & (1 << 8) != 0,
            b: buttons & (1 << 7) != 0,
            x: buttons & (1 << 6) != 0,
            y: buttons & (1 << 5) != 0,
            guide: buttons & (1 << 4) != 0,
            start: buttons & (1 << 3) != 0,
            back: buttons & (1 << 2) != 0,
            left_bumper: buttons & (1 << 1) != 0,
            right_bumper: buttons & 1 != 0,
            user: wire::get_u8(buf, 46)?,
            gamepad_type: match type_byte {
                1 => GamepadType::LogitechF310,
                2 => GamepadType::Xbox360,
                3 => GamepadType::SonyPs4,
                4 => GamepadType::SonyPs4Kernel,
                _ => GamepadType::Unknown,
            },
            touchpad_finger_1_x: wire::get_f32(buf, 49)?,
            touchpad_finger_1_y: wire::get_f32(buf, 53)?,
            touchpad_finger_2_x: wire::get_f32(buf, 57)?,
            touchpad_finger_2_y: wire::get_f32(buf, 61)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let gp = Gamepad {
            seq: 99,
            user: 2,
            timestamp: 1_700_000_000_000,
            left_stick_x: -0.5,
            right_trigger: 1.0,
            a: true,
            dpad_up: true,
            left_bumper: true,
            touchpad_finger_1: true,
            ..Default::default()
        };
        let bytes = gp.serialize();
        assert_eq!(bytes.len(), Gamepad::TOTAL_LEN);
        assert_eq!(Gamepad::parse(&bytes).unwrap(), gp);
    }

    #[test]
    fn header_and_user_offsets() {
        let gp = Gamepad {
            user: 2,
            ..Default::default()
        };
        let bytes = gp.serialize();
        assert_eq!(bytes[0], super::super::UID_GAMEPAD);
        // Payload-only length, excluding the 5-byte header (the real RC's
        // semantics — see wire::with_header).
        assert_eq!(u16::from_be_bytes([bytes[1], bytes[2]]), 60);
        assert_eq!(bytes[46], 2);
    }

    #[test]
    fn buttons_bitfield_matches_reference() {
        // right_bumper is bit 0, a is bit 8, touchpad_finger_1 is bit 17.
        let gp = Gamepad {
            right_bumper: true,
            a: true,
            touchpad_finger_1: true,
            ..Default::default()
        };
        assert_eq!(gp.buttons_bitfield(), (1 << 17) | (1 << 8) | 1);
    }
}
