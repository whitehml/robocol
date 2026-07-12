//! Telemetry packet (uid 5): tagged bundles of string and float32 key/value
//! pairs pushed by the RC (`telemetry.update()` on the robot side).
//!
//! LIBROBOCOL DEVIATION (real-hardware finding, not a librobocol mismatch):
//! for ordinary OpMode `telemetry.addData`/`addLine` lines, the real RC keys
//! each string entry by an opaque, monotonically increasing sort-order tag —
//! not a caption — and bakes the full "Caption : Data" text into the *value*
//! already. Only the reserved keys below (`BATTERY_LEVEL_KEY`,
//! `SYSTEM_KEY_PREFIX`) carry a real, meaningful key. `strings` is a
//! `Vec` rather than a map so that wire order (which the sort tags encode) is
//! preserved rather than silently re-sorted by key bytes.

use std::collections::BTreeMap;

use crate::types::RobotState;
use crate::wire::{self, Result};

pub const BATTERY_LEVEL_KEY: &str = "$Robot$Battery$Level$";
const NO_VOLTAGE_SENSOR: &str = "$no$voltage$sensor$";
/// Reserved key prefix for RC-originated system telemetry — warnings the
/// Robot Controller raises itself, independent of any OpMode (lost Expansion
/// Hub, low battery, and the like).
pub const SYSTEM_KEY_PREFIX: &str = "$System$";

#[derive(Debug, Clone, PartialEq)]
pub struct Telemetry {
    pub seq: u16,
    pub timestamp: i64,
    pub is_sorted: bool,
    pub robot_state: RobotState,
    pub tag: String,
    pub strings: Vec<(String, String)>,
    pub numbers: BTreeMap<String, f32>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Telemetry {
            seq: 0,
            timestamp: 0,
            is_sorted: false,
            robot_state: RobotState::Unknown,
            tag: String::new(),
            strings: Vec::new(),
            numbers: BTreeMap::new(),
        }
    }
}

impl Telemetry {
    /// Pushes an ordinary (non-reserved) telemetry line the way the real RC
    /// does: the caption is baked directly into `text` (e.g. `"FPS : 10.7"`)
    /// and the key is an opaque, meaningless sort tag — never a real
    /// caption. Only `BATTERY_LEVEL_KEY`/`SYSTEM_KEY_PREFIX` lines carry a
    /// real key.
    pub fn push_line(&mut self, text: impl Into<String>) {
        self.strings.push((String::new(), text.into()));
    }

    pub fn serialize(&self) -> Vec<u8> {
        let tag = self.tag.as_bytes();
        // LIBROBOCOL DEVIATION: librobocol sizes the payload as 9 + tag,
        // 4 bytes short of the fields it then writes (JS typed arrays drop
        // out-of-bounds writes silently, so its own telemetry sends are
        // truncated). Correct size: ts(8) + sorted(1) + state(1) +
        // tag_len(1) + tag + str_count(1) + num_count(1) = 13 + tag.
        let mut payload = 13 + tag.len();
        for (k, v) in &self.strings {
            payload += 4 + k.len() + v.len();
        }
        for k in self.numbers.keys() {
            payload += 6 + k.len();
        }
        let mut buf = wire::with_header(super::UID_TELEMETRY, payload, self.seq);
        wire::put_i64(&mut buf, 5, self.timestamp);
        buf[13] = self.is_sorted as u8;
        buf[14] = self.robot_state.to_byte();
        let mut off = wire::put_str_u8(&mut buf, 15, tag);

        buf[off] = self.strings.len() as u8;
        off += 1;
        for (k, v) in &self.strings {
            off = wire::put_str_u16(&mut buf, off, k.as_bytes());
            off = wire::put_str_u16(&mut buf, off, v.as_bytes());
        }

        buf[off] = self.numbers.len() as u8;
        off += 1;
        for (k, v) in &self.numbers {
            off = wire::put_str_u16(&mut buf, off, k.as_bytes());
            wire::put_f32(&mut buf, off, *v);
            off += 4;
        }
        buf
    }

    pub fn parse(buf: &[u8]) -> Result<Telemetry> {
        let mut t = Telemetry {
            seq: wire::get_u16(buf, 3)?,
            timestamp: wire::get_i64(buf, 5)?,
            is_sorted: wire::get_u8(buf, 13)? != 0,
            robot_state: RobotState::from_byte(wire::get_u8(buf, 14)?),
            ..Default::default()
        };
        let (tag, mut off) = wire::get_str_u8(buf, 15)?;
        t.tag = tag;

        let string_count = wire::get_u8(buf, off)?;
        off += 1;
        for _ in 0..string_count {
            let (key, o) = wire::get_str_u16(buf, off)?;
            let (value, next) = wire::get_str_u16(buf, o)?;
            off = next;
            t.strings.push((key, value));
        }

        let number_count = wire::get_u8(buf, off)?;
        off += 1;
        for _ in 0..number_count {
            let (key, o) = wire::get_str_u16(buf, off)?;
            off = o;
            let value = wire::get_f32(buf, off)?;
            off += 4;
            t.numbers.insert(key, value);
        }
        Ok(t)
    }

    pub fn battery_voltage(&self) -> Option<f32> {
        let raw = self
            .strings
            .iter()
            .find(|(k, _)| k == BATTERY_LEVEL_KEY)
            .map(|(_, v)| v.as_str())?;
        if raw == NO_VOLTAGE_SENSOR {
            return Some(0.0);
        }
        Some(raw.parse().unwrap_or(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut t = Telemetry {
            seq: 3,
            timestamp: 987654321,
            is_sorted: true,
            robot_state: RobotState::Init,
            tag: "TELEMETRY_DATA".to_string(),
            ..Default::default()
        };
        t.strings.push(("Alliance".into(), "RED".into()));
        t.strings.push(("State".into(), "DRIVING".into()));
        t.numbers.insert("flywheel_rpm".into(), 2812.5);
        t.numbers.insert("heading_deg".into(), -42.0);
        assert_eq!(Telemetry::parse(&t.serialize()).unwrap(), t);
    }

    #[test]
    fn empty_round_trip() {
        let t = Telemetry::default();
        assert_eq!(Telemetry::parse(&t.serialize()).unwrap(), t);
    }

    #[test]
    fn battery_voltage_reads_real_capture_value() {
        let mut t = Telemetry::default();
        t.strings.push((BATTERY_LEVEL_KEY.into(), "12.06".into()));
        assert_eq!(t.battery_voltage(), Some(12.06));
    }

    #[test]
    fn battery_voltage_sentinel_is_zero() {
        let mut t = Telemetry::default();
        t.strings
            .push((BATTERY_LEVEL_KEY.into(), NO_VOLTAGE_SENSOR.into()));
        assert_eq!(t.battery_voltage(), Some(0.0));
    }

    #[test]
    fn battery_voltage_absent_is_none() {
        assert_eq!(Telemetry::default().battery_voltage(), None);
    }
}
