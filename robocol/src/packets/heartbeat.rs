//! Heartbeat packet (uid 1): liveness + clock sync, sent by the DS every
//! ~200 ms once a peer is known. The RC's replies carry its current
//! RobotState. (Distinct from the minimal KeepAlive packet, uid 6.)

use crate::types::RobotState;
use crate::wire::{self, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Heartbeat {
    pub seq: u16,
    /// Nanoseconds (reference DS sends wall-clock ms * 1e6).
    pub timestamp: i64,
    pub robot_state: RobotState,
    /// Clock-sync scratch values; the DS fills t0, the RC echoes/fills t1/t2.
    pub t0: i64,
    pub t1: i64,
    pub t2: i64,
    pub timezone_id: String,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Heartbeat {
            seq: 0,
            timestamp: 0,
            robot_state: RobotState::Unknown,
            t0: 0,
            t1: 0,
            t2: 0,
            timezone_id: "GMT".to_string(),
        }
    }
}

impl Heartbeat {
    // LIBROBOCOL DEVIATION apparent formatting bug: Real RC data suggests the following is correct.
    pub fn serialize(&self) -> Vec<u8> {
        let tz = self.timezone_id.as_bytes();
        let mut buf = wire::with_header(super::UID_HEARTBEAT, 34 + tz.len(), self.seq);
        wire::put_i64(&mut buf, 5, self.timestamp);
        buf[13] = self.robot_state.to_byte();
        wire::put_i64(&mut buf, 14, self.t0);
        wire::put_i64(&mut buf, 22, self.t1);
        wire::put_i64(&mut buf, 30, self.t2);
        wire::put_str_u8(&mut buf, 38, tz);
        buf
    }

    pub fn parse(buf: &[u8]) -> Result<Heartbeat> {
        Ok(Heartbeat {
            seq: wire::get_u16(buf, 3)?,
            timestamp: wire::get_i64(buf, 5)?,
            robot_state: RobotState::from_byte(wire::get_u8(buf, 13)?),
            t0: wire::get_i64(buf, 14)?,
            t1: wire::get_i64(buf, 22)?,
            t2: wire::get_i64(buf, 30)?,
            timezone_id: wire::get_str_u8(buf, 38)?.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::HEADER_LEN;

    #[test]
    fn round_trip() {
        let hb = Heartbeat {
            seq: 7,
            timestamp: 1_700_000_000_000_000_000,
            robot_state: RobotState::Running,
            t0: 123,
            t1: 456,
            t2: 789,
            timezone_id: "GMT".to_string(),
        };
        let bytes = hb.serialize();
        assert_eq!(bytes.len(), HEADER_LEN + 34 + 3);
        assert_eq!(Heartbeat::parse(&bytes).unwrap(), hb);
    }
}
