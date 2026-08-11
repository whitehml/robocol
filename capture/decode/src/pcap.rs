//! Minimal reader for both capture container formats: classic pcap (what
//! `tcpdump -w` writes) and pcapng (what Wireshark's "Save As" writes).

pub struct Frame {
    pub ts: f64,
    pub linktype: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn u16(self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        match self {
            Endian::Little => u16::from_le_bytes(a),
            Endian::Big => u16::from_be_bytes(a),
        }
    }

    fn u32(self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        match self {
            Endian::Little => u32::from_le_bytes(a),
            Endian::Big => u32::from_be_bytes(a),
        }
    }
}

const PCAP_MAGIC_US: u32 = 0xa1b2_c3d4;
const PCAP_MAGIC_NS: u32 = 0xa1b2_3c4d;
const PCAPNG_SHB: u32 = 0x0a0d_0d0a;

pub fn read(bytes: &[u8]) -> Result<Vec<Frame>, String> {
    if bytes.len() < 4 {
        return Err("file too short to be a capture".into());
    }
    let head = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if head == PCAPNG_SHB || head.swap_bytes() == PCAPNG_SHB {
        read_pcapng(bytes)
    } else {
        read_classic(bytes)
    }
}

fn read_classic(bytes: &[u8]) -> Result<Vec<Frame>, String> {
    if bytes.len() < 24 {
        return Err("truncated pcap header".into());
    }
    let le = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let (endian, nanos) = match le {
        PCAP_MAGIC_US => (Endian::Little, false),
        PCAP_MAGIC_NS => (Endian::Little, true),
        _ => match le.swap_bytes() {
            PCAP_MAGIC_US => (Endian::Big, false),
            PCAP_MAGIC_NS => (Endian::Big, true),
            other => return Err(format!("not a pcap file (magic {other:#010x})")),
        },
    };

    let linktype = endian.u32(&bytes[20..24]);
    let divisor = if nanos { 1e9 } else { 1e6 };

    let mut frames = Vec::new();
    let mut pos = 24;
    while pos + 16 <= bytes.len() {
        let ts_sec = endian.u32(&bytes[pos..]) as f64;
        let ts_frac = endian.u32(&bytes[pos + 4..]) as f64;
        let incl_len = endian.u32(&bytes[pos + 8..]) as usize;
        pos += 16;
        if pos + incl_len > bytes.len() {
            break;
        }
        frames.push(Frame {
            ts: ts_sec + ts_frac / divisor,
            linktype,
            data: bytes[pos..pos + incl_len].to_vec(),
        });
        pos += incl_len;
    }
    Ok(frames)
}

fn read_pcapng(bytes: &[u8]) -> Result<Vec<Frame>, String> {
    let endian = {
        let bom = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if bom == 0x1a2b_3c4d {
            Endian::Little
        } else if bom.swap_bytes() == 0x1a2b_3c4d {
            Endian::Big
        } else {
            return Err("pcapng byte-order magic not recognized".into());
        }
    };

    let mut interfaces: Vec<(u32, f64)> = Vec::new();
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos + 12 <= bytes.len() {
        let block_type = endian.u32(&bytes[pos..]);
        let total_len = endian.u32(&bytes[pos + 4..]) as usize;
        if total_len < 12 || pos + total_len > bytes.len() {
            break;
        }
        let body = &bytes[pos + 8..pos + total_len - 4];

        match block_type {
            1 => interfaces.push((endian.u16(&body[0..2]) as u32, tsresol(endian, &body[8..]))),
            6 if body.len() >= 20 => {
                let iface = endian.u32(&body[0..4]) as usize;
                let ts = ((endian.u32(&body[4..8]) as u64) << 32) | endian.u32(&body[8..12]) as u64;
                let cap_len = endian.u32(&body[12..16]) as usize;
                let (linktype, scale) = interfaces.get(iface).copied().unwrap_or((1, 1e-6));
                if 20 + cap_len <= body.len() {
                    frames.push(Frame {
                        ts: ts as f64 * scale,
                        linktype,
                        data: body[20..20 + cap_len].to_vec(),
                    });
                }
            }
            _ => {}
        }
        pos += total_len;
    }
    Ok(frames)
}

/// IDB option 9 (`if_tsresol`): one byte, high bit set means the value is a
/// power of two rather than of ten. Absent means microseconds.
fn tsresol(endian: Endian, options: &[u8]) -> f64 {
    let mut pos = 0;
    while pos + 4 <= options.len() {
        let code = endian.u16(&options[pos..]);
        let len = endian.u16(&options[pos + 2..]) as usize;
        let value = &options[pos + 4..];
        if code == 0 {
            break;
        }
        if code == 9 && len >= 1 && !value.is_empty() {
            let raw = value[0];
            return if raw & 0x80 != 0 {
                1.0 / (1u64 << (raw & 0x7f)) as f64
            } else {
                10f64.powi(-(raw as i32))
            };
        }
        pos += 4 + len.next_multiple_of(4);
    }
    1e-6
}
