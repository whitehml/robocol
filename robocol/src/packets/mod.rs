//! Packet codecs. Byte offsets mirror librobocol's serializers, which were
//! validated against real Control Hubs.

mod command;
mod gamepad;
mod heartbeat;
mod keep_alive;
mod peer_discovery;
mod telemetry;

pub use command::Command;
pub use gamepad::Gamepad;
pub use heartbeat::Heartbeat;
pub use keep_alive::KeepAlive;
pub use peer_discovery::{PeerDiscovery, PeerType};
pub use telemetry::{Telemetry, BATTERY_LEVEL_KEY, RC_BATTERY_STATUS_KEY, SYSTEM_KEY_PREFIX};

use crate::wire::{Error, Result};

pub const UID_HEARTBEAT: u8 = 1;
pub const UID_GAMEPAD: u8 = 2;
pub const UID_PEER_DISCOVERY: u8 = 3;
pub const UID_COMMAND: u8 = 4;
pub const UID_TELEMETRY: u8 = 5;
pub const UID_KEEP_ALIVE: u8 = 6;

#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    Heartbeat(Heartbeat),
    Gamepad(Gamepad),
    PeerDiscovery(PeerDiscovery),
    Command(Command),
    Telemetry(Telemetry),
    KeepAlive(KeepAlive),
}

impl Packet {
    pub fn parse(buf: &[u8]) -> Result<Packet> {
        match *buf.first().ok_or(Error::Truncated)? {
            UID_HEARTBEAT => Ok(Packet::Heartbeat(Heartbeat::parse(buf)?)),
            UID_GAMEPAD => Ok(Packet::Gamepad(Gamepad::parse(buf)?)),
            UID_PEER_DISCOVERY => Ok(Packet::PeerDiscovery(PeerDiscovery::parse(buf)?)),
            UID_COMMAND => Ok(Packet::Command(Command::parse(buf)?)),
            UID_TELEMETRY => Ok(Packet::Telemetry(Telemetry::parse(buf)?)),
            UID_KEEP_ALIVE => Ok(Packet::KeepAlive(KeepAlive::parse(buf)?)),
            other => Err(Error::UnknownPacketType(other)),
        }
    }
}
