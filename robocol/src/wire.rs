//! Big-endian read/write helpers over fixed offsets.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Truncated,
    UnknownPacketType(u8),
    BadUtf8,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => write!(f, "packet truncated"),
            Error::UnknownPacketType(t) => write!(f, "unknown packet type {t}"),
            Error::BadUtf8 => write!(f, "invalid UTF-8 in packet string"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Header layout shared by all packets: `[type: u8][length: u16][seq: u16]`.
/// The length field holds the *payload* length, excluding the 5-byte
/// header. LIBROBOCOL DEVIATION: librobocol writes the total
/// packet length here, but every packet the real RC emits declares
/// payload-only — we match the RC. (PeerDiscovery is the one exception
/// and writes its own header.)
pub const HEADER_LEN: usize = 5;

pub fn with_header(uid: u8, payload_len: usize, seq: u16) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_LEN + payload_len];
    buf[0] = uid;
    buf[1..3].copy_from_slice(&(payload_len as u16).to_be_bytes());
    buf[3..5].copy_from_slice(&seq.to_be_bytes());
    buf
}

pub fn get_u8(buf: &[u8], off: usize) -> Result<u8> {
    buf.get(off).copied().ok_or(Error::Truncated)
}

pub fn get_u16(buf: &[u8], off: usize) -> Result<u16> {
    let b = buf.get(off..off + 2).ok_or(Error::Truncated)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

pub fn get_i32(buf: &[u8], off: usize) -> Result<i32> {
    let b = buf.get(off..off + 4).ok_or(Error::Truncated)?;
    Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

pub fn get_u32(buf: &[u8], off: usize) -> Result<u32> {
    let b = buf.get(off..off + 4).ok_or(Error::Truncated)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

pub fn get_i64(buf: &[u8], off: usize) -> Result<i64> {
    let b = buf.get(off..off + 8).ok_or(Error::Truncated)?;
    Ok(i64::from_be_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

pub fn get_f32(buf: &[u8], off: usize) -> Result<f32> {
    Ok(f32::from_bits(get_u32(buf, off)?))
}

pub fn get_str(buf: &[u8], off: usize, len: usize) -> Result<String> {
    let b = buf.get(off..off + len).ok_or(Error::Truncated)?;
    String::from_utf8(b.to_vec()).map_err(|_| Error::BadUtf8)
}

/// Reads a `u16`-length-prefixed UTF-8 string at `off`, returning it and the
/// offset just past the string so callers can thread it into the next field.
pub fn get_str_u16(buf: &[u8], off: usize) -> Result<(String, usize)> {
    let len = get_u16(buf, off)? as usize;
    Ok((get_str(buf, off + 2, len)?, off + 2 + len))
}

pub fn get_str_u8(buf: &[u8], off: usize) -> Result<(String, usize)> {
    let len = get_u8(buf, off)? as usize;
    Ok((get_str(buf, off + 1, len)?, off + 1 + len))
}

pub fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_be_bytes());
}

pub fn put_i32(buf: &mut [u8], off: usize, v: i32) {
    buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

pub fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

pub fn put_i64(buf: &mut [u8], off: usize, v: i64) {
    buf[off..off + 8].copy_from_slice(&v.to_be_bytes());
}

pub fn put_f32(buf: &mut [u8], off: usize, v: f32) {
    put_u32(buf, off, v.to_bits());
}

/// Writes a `u16` length prefix followed by `s` at `off`, returning the
/// offset just past the string.
pub fn put_str_u16(buf: &mut [u8], off: usize, s: &[u8]) -> usize {
    put_u16(buf, off, s.len() as u16);
    buf[off + 2..off + 2 + s.len()].copy_from_slice(s);
    off + 2 + s.len()
}

pub fn put_str_u8(buf: &mut [u8], off: usize, s: &[u8]) -> usize {
    buf[off] = s.len() as u8;
    buf[off + 1..off + 1 + s.len()].copy_from_slice(s);
    off + 1 + s.len()
}
