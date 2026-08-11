//! Link/IP/UDP unwrapping, including IPv4 reassembly. Reassembly is not
//! optional here: a CMD_NOTIFY_OP_MODE_LIST with a full OpMode registry, a
//! config XML response, or a 4 KiB webcam frame chunk all exceed the MTU and
//! arrive fragmented. Without this the interesting payloads are exactly the
//! ones you cannot read.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::pcap::Frame;

pub struct Datagram {
    pub ts: f64,
    pub src: IpAddr,
    pub dst: IpAddr,
    pub sport: u16,
    pub dport: u16,
    pub payload: Vec<u8>,
    pub fragmented: bool,
}

const LINKTYPE_ETHERNET: u32 = 1;
const LINKTYPE_RAW: u32 = 101;
const LINKTYPE_LINUX_SLL: u32 = 113;
const LINKTYPE_LINUX_SLL2: u32 = 276;
const LINKTYPE_NULL: u32 = 0;

const PROTO_UDP: u8 = 17;

#[derive(Default)]
struct Reassembler {
    pending: HashMap<(Ipv4Addr, Ipv4Addr, u16), Pending>,
}

#[derive(Default)]
struct Pending {
    chunks: Vec<(usize, Vec<u8>)>,
    total: Option<usize>,
    ts: f64,
}

pub fn datagrams(frames: &[Frame]) -> (Vec<Datagram>, usize) {
    let mut out = Vec::new();
    let mut reasm = Reassembler::default();
    let mut skipped = 0;

    for frame in frames {
        let Some(ip) = strip_link(frame.linktype, &frame.data) else {
            skipped += 1;
            continue;
        };
        match ip.first().map(|b| b >> 4) {
            Some(4) => handle_v4(frame.ts, ip, &mut reasm, &mut out),
            Some(6) => handle_v6(frame.ts, ip, &mut out),
            _ => skipped += 1,
        }
    }
    (out, skipped)
}

fn strip_link(linktype: u32, data: &[u8]) -> Option<&[u8]> {
    match linktype {
        LINKTYPE_ETHERNET => {
            let mut pos = 14;
            let mut ethertype = u16::from_be_bytes([*data.get(12)?, *data.get(13)?]);
            while ethertype == 0x8100 || ethertype == 0x88a8 {
                ethertype = u16::from_be_bytes([*data.get(pos + 2)?, *data.get(pos + 3)?]);
                pos += 4;
            }
            match ethertype {
                0x0800 | 0x86dd => data.get(pos..),
                _ => None,
            }
        }
        LINKTYPE_LINUX_SLL => match u16::from_be_bytes([*data.get(14)?, *data.get(15)?]) {
            0x0800 | 0x86dd => data.get(16..),
            _ => None,
        },
        LINKTYPE_LINUX_SLL2 => match u16::from_be_bytes([*data.first()?, *data.get(1)?]) {
            0x0800 | 0x86dd => data.get(20..),
            _ => None,
        },
        LINKTYPE_RAW => Some(data),
        LINKTYPE_NULL => data.get(4..),
        _ => None,
    }
}

fn handle_v4(ts: f64, ip: &[u8], reasm: &mut Reassembler, out: &mut Vec<Datagram>) {
    if ip.len() < 20 || ip[9] != PROTO_UDP {
        return;
    }
    let ihl = ((ip[0] & 0x0f) as usize) * 4;
    let total_len = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    if ihl < 20 || total_len < ihl || ip.len() < ihl {
        return;
    }
    let end = total_len.min(ip.len());
    let body = &ip[ihl..end];

    let src = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
    let dst = Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]);
    let ident = u16::from_be_bytes([ip[4], ip[5]]);
    let flags_frag = u16::from_be_bytes([ip[6], ip[7]]);
    let more = flags_frag & 0x2000 != 0;
    let offset = (flags_frag & 0x1fff) as usize * 8;

    if !more && offset == 0 {
        if let Some(dg) = parse_udp(ts, src.into(), dst.into(), body, false) {
            out.push(dg);
        }
        return;
    }

    let entry = reasm.pending.entry((src, dst, ident)).or_default();
    if entry.chunks.is_empty() {
        entry.ts = ts;
    }
    entry.chunks.push((offset, body.to_vec()));
    if !more {
        entry.total = Some(offset + body.len());
    }

    let Some(total) = entry.total else { return };
    let mut buf = vec![0u8; total];
    let mut filled = vec![false; total];
    for (off, chunk) in &entry.chunks {
        let hi = (off + chunk.len()).min(total);
        if *off >= total {
            continue;
        }
        buf[*off..hi].copy_from_slice(&chunk[..hi - off]);
        filled[*off..hi].iter_mut().for_each(|f| *f = true);
    }
    if filled.iter().all(|f| *f) {
        let ts = entry.ts;
        reasm.pending.remove(&(src, dst, ident));
        if let Some(dg) = parse_udp(ts, src.into(), dst.into(), &buf, true) {
            out.push(dg);
        }
    }
}

fn handle_v6(ts: f64, ip: &[u8], out: &mut Vec<Datagram>) {
    if ip.len() < 40 || ip[6] != PROTO_UDP {
        return;
    }
    let src = Ipv6Addr::from(<[u8; 16]>::try_from(&ip[8..24]).unwrap());
    let dst = Ipv6Addr::from(<[u8; 16]>::try_from(&ip[24..40]).unwrap());
    if let Some(dg) = parse_udp(ts, src.into(), dst.into(), &ip[40..], false) {
        out.push(dg);
    }
}

fn parse_udp(ts: f64, src: IpAddr, dst: IpAddr, body: &[u8], fragmented: bool) -> Option<Datagram> {
    if body.len() < 8 {
        return None;
    }
    let sport = u16::from_be_bytes([body[0], body[1]]);
    let dport = u16::from_be_bytes([body[2], body[3]]);
    let len = u16::from_be_bytes([body[4], body[5]]) as usize;
    let end = if len >= 8 {
        len.min(body.len())
    } else {
        body.len()
    };
    Some(Datagram {
        ts,
        src,
        dst,
        sport,
        dport,
        payload: body[8..end].to_vec(),
        fragmented,
    })
}
