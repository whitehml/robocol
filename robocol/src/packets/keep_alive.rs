//! KeepAlive packet (uid 6): single-byte id, minimal liveness signal.

use crate::wire::{self, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeepAlive {
    pub seq: u16,
    pub id: u8,
}

impl KeepAlive {
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = wire::with_header(super::UID_KEEP_ALIVE, 1, self.seq);
        buf[5] = self.id;
        buf
    }

    pub fn parse(buf: &[u8]) -> Result<KeepAlive> {
        Ok(KeepAlive {
            seq: wire::get_u16(buf, 3)?,
            id: wire::get_u8(buf, 5)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let ka = KeepAlive { seq: 5, id: 9 };
        assert_eq!(KeepAlive::parse(&ka.serialize()).unwrap(), ka);
    }
}
