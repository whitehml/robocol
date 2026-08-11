//! Handshake packet (uid 3). The DS broadcasts this until the RC answers
//! with its own. Layout is legacy/special: 13 bytes total, the length field
//! holds 10 (payload after the 3-byte uid+length prefix) and the sequence
//! number lives at offset 5 instead of 3.

use crate::wire::{self, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PeerType {
    Unset = 0,
    #[default]
    Peer = 1,
    GroupOwner = 2,
    NotConnectedDueToPreexistingConnection = 3,
}

impl PeerType {
    fn from_byte(b: u8) -> PeerType {
        match b {
            1 => PeerType::Peer,
            2 => PeerType::GroupOwner,
            3 => PeerType::NotConnectedDueToPreexistingConnection,
            _ => PeerType::Unset,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PeerDiscovery {
    pub seq: u16,
    pub robocol_version: u8,
    pub peer_type: PeerType,
    pub sdk_build_month: u8,
    pub sdk_build_year: u16,
    pub sdk_major_version: u8,
    pub sdk_minor_version: u8,
}

impl Default for PeerDiscovery {
    fn default() -> Self {
        PeerDiscovery {
            seq: 0,
            robocol_version: 124,
            peer_type: PeerType::Peer,
            sdk_build_month: 7,
            sdk_build_year: 2026,
            sdk_major_version: 11,
            sdk_minor_version: 2,
        }
    }
}

impl PeerDiscovery {
    pub const TOTAL_LEN: usize = 13;

    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = vec![0u8; Self::TOTAL_LEN];
        buf[0] = super::UID_PEER_DISCOVERY;
        wire::put_u16(&mut buf, 1, 10);
        buf[3] = self.robocol_version;
        buf[4] = self.peer_type as u8;
        wire::put_u16(&mut buf, 5, self.seq);
        buf[7] = self.sdk_build_month;
        wire::put_u16(&mut buf, 8, self.sdk_build_year);
        buf[10] = self.sdk_major_version;
        buf[11] = self.sdk_minor_version;
        buf
    }

    pub fn parse(buf: &[u8]) -> Result<PeerDiscovery> {
        Ok(PeerDiscovery {
            robocol_version: wire::get_u8(buf, 3)?,
            peer_type: PeerType::from_byte(wire::get_u8(buf, 4)?),
            seq: wire::get_u16(buf, 5)?,
            sdk_build_month: wire::get_u8(buf, 7)?,
            sdk_build_year: wire::get_u16(buf, 8)?,
            sdk_major_version: wire::get_u8(buf, 10)?,
            sdk_minor_version: wire::get_u8(buf, 11)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_bytes() {
        let bytes = PeerDiscovery::default().serialize();
        assert_eq!(bytes, vec![3, 0, 10, 124, 1, 0, 0, 7, 0x07, 0xEA, 11, 2, 0]);
    }

    #[test]
    fn round_trip() {
        let pd = PeerDiscovery {
            seq: 42,
            ..Default::default()
        };
        assert_eq!(PeerDiscovery::parse(&pd.serialize()).unwrap(), pd);
    }
}
