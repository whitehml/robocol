//! Command packet (uid 4): a named RPC with a string payload (`extra`,
//! usually JSON). Commands are acked: the receiver echoes name + timestamp
//! with `acknowledged = true`, and senders retransmit until acked.

use crate::wire::{self, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub seq: u16,
    pub timestamp: i64,
    pub acknowledged: bool,
    pub name: String,
    pub extra: String,
}

impl Command {
    pub fn new(name: &str, extra: &str, timestamp: i64) -> Command {
        Command {
            seq: 0,
            timestamp,
            acknowledged: false,
            name: name.to_string(),
            extra: extra.to_string(),
        }
    }

    pub fn ack_of(other: &Command) -> Command {
        Command {
            seq: other.seq,
            timestamp: other.timestamp,
            acknowledged: true,
            name: other.name.clone(),
            extra: String::new(),
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        let extra = self.extra.as_bytes();
        let payload = 11
            + name.len()
            + if self.acknowledged {
                0
            } else {
                2 + extra.len()
            };
        let mut buf = wire::with_header(super::UID_COMMAND, payload, self.seq);
        wire::put_i64(&mut buf, 5, self.timestamp);
        buf[13] = self.acknowledged as u8;
        let off = wire::put_str_u16(&mut buf, 14, name);
        if !self.acknowledged {
            wire::put_str_u16(&mut buf, off, extra);
        }
        buf
    }

    pub fn parse(buf: &[u8]) -> Result<Command> {
        let acknowledged = wire::get_u8(buf, 13)? != 0;
        let (name, off) = wire::get_str_u16(buf, 14)?;
        let extra = if acknowledged {
            String::new()
        } else {
            wire::get_str_u16(buf, off)?.0
        };
        Ok(Command {
            seq: wire::get_u16(buf, 3)?,
            timestamp: wire::get_i64(buf, 5)?,
            acknowledged,
            name,
            extra,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let cmd = Command::new("CMD_INIT_OP_MODE", "Duo (TeleOp)", 1234567890);
        assert_eq!(Command::parse(&cmd.serialize()).unwrap(), cmd);
    }

    #[test]
    fn ack_round_trip() {
        let cmd = Command::new("CMD_REQUEST_OP_MODE_LIST", "", 42);
        let ack = Command::ack_of(&cmd);
        let parsed = Command::parse(&ack.serialize()).unwrap();
        assert!(parsed.acknowledged);
        assert_eq!(parsed.name, cmd.name);
        assert_eq!(parsed.timestamp, cmd.timestamp);
        assert!(parsed.extra.is_empty());
    }
}
