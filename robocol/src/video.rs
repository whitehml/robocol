//! MJPEG-over-HTTP client for camera feeds. Connects to a
//! `multipart/x-mixed-replace` stream (e.g. a Limelight or the RC's webcam
//! server), pulls out each JPEG frame, and hands the raw bytes to the caller
//! to decode. Pure `std` — the Godot bridge owns the threads and turns the
//! bytes into textures.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

/// Refuse any single frame larger than this — guards against a garbage
/// Content-Length turning into a multi-gigabyte allocation.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_millis(1000);

/// What one camera source is doing, drained on the main thread.
pub enum VideoEvent {
    /// A decoded-elsewhere JPEG frame for `source`.
    Frame { source: String, jpeg: Vec<u8> },
    /// The stream closed or could not be reached — the pane should clear.
    Disconnected { source: String },
}

/// One camera source to stream: a logical name plus where to fetch it.
#[derive(Clone)]
pub struct StreamConfig {
    pub source: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl StreamConfig {
    /// Parses `http://host:port/path`. Scheme optional; port defaults to 80,
    /// path to `/`.
    pub fn parse(source: &str, url: &str) -> Option<StreamConfig> {
        let rest = url.strip_prefix("http://").unwrap_or(url);
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (authority, 80),
        };
        if host.is_empty() {
            return None;
        }
        Some(StreamConfig {
            source: source.to_string(),
            host: host.to_string(),
            port,
            path: path.to_string(),
        })
    }
}

/// Runs one source until `stop` is set: connect, forward frames, and on any
/// disconnect report it and retry after a short delay. Meant to run on its
/// own thread. Returns once `stop` is observed or the receiver is dropped.
pub fn run_stream(cfg: StreamConfig, tx: Sender<VideoEvent>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let delivered = stream_once(&cfg, &tx, &stop).unwrap_or(false);
        // Only clear the pane if we had actually been delivering frames — a
        // "not yet reachable" (no OpMode running) shouldn't spam a clear.
        if delivered {
            let _ = tx.send(VideoEvent::Disconnected {
                source: cfg.source.clone(),
            });
        }
        if sleep_or_stop(RECONNECT_DELAY, &stop) {
            return;
        }
    }
}

/// Connects and pumps frames until the stream ends or `stop` is set. Returns
/// whether at least one frame was delivered, so the caller knows if this was
/// a live stream dropping vs. a source that was simply never up.
fn stream_once(
    cfg: &StreamConfig,
    tx: &Sender<VideoEvent>,
    stop: &Arc<AtomicBool>,
) -> std::io::Result<bool> {
    let stream = TcpStream::connect_timeout(
        &format!("{}:{}", cfg.host, cfg.port).parse().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad stream address")
        })?,
        CONNECT_TIMEOUT,
    )?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let mut w = &stream;
    write!(
        w,
        "GET {} HTTP/1.0\r\nHost: {}\r\nAccept: */*\r\n\r\n",
        cfg.path, cfg.host
    )?;
    w.flush()?;

    let mut reader = BufReader::new(&stream);
    read_multipart_boundary(&mut reader)?;
    let mut delivered = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(delivered);
        }
        // Past the response head, a read error means the stream dropped (the
        // RC stopped serving when the OpMode ended); report what we managed
        // to deliver rather than bubbling it up as a never-connected error.
        let Ok(jpeg) = read_part(&mut reader) else {
            return Ok(delivered);
        };
        if tx
            .send(VideoEvent::Frame {
                source: cfg.source.clone(),
                jpeg,
            })
            .is_err()
        {
            return Ok(delivered);
        }
        delivered = true;
    }
}

/// Reads the HTTP response head and returns the multipart boundary token.
fn read_multipart_boundary<R: BufRead>(reader: &mut R) -> std::io::Result<String> {
    let mut boundary = None;
    loop {
        let line = read_header_line(reader)?;
        if line.is_empty() {
            break;
        }
        if let Some(b) = parse_boundary(&line) {
            boundary = Some(b);
        }
    }
    boundary.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no multipart boundary in response",
        )
    })
}

fn parse_boundary(header_line: &str) -> Option<String> {
    let (name, value) = header_line.split_once(':')?;
    if !name.trim().eq_ignore_ascii_case("content-type") {
        return None;
    }
    let idx = value.to_ascii_lowercase().find("boundary=")?;
    let raw = value[idx + "boundary=".len()..].trim();
    Some(raw.trim_matches('"').to_string())
}

/// Reads part headers (skipping the leading boundary/blank lines) and then
/// exactly `Content-Length` bytes of JPEG payload.
fn read_part<R: BufRead>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = None;
    loop {
        let line = read_header_line(reader)?;
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                len = value.trim().parse::<usize>().ok();
            }
        } else if line.is_empty() && len.is_some() {
            break;
        }
    }
    let len = len.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "part missing Content-Length",
        )
    })?;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame exceeds size cap",
        ));
    }
    let mut jpeg = vec![0u8; len];
    reader.read_exact(&mut jpeg)?;
    Ok(jpeg)
}

/// Reads a single CRLF-terminated header line, returned without the CRLF.
fn read_header_line<R: BufRead>(reader: &mut R) -> std::io::Result<String> {
    let mut line = Vec::new();
    let n = read_until_crlf(reader, &mut line)?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "stream ended mid-header",
        ));
    }
    while matches!(line.last(), Some(b'\r' | b'\n')) {
        line.pop();
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// Like `read_until(b'\n')` but caps line length so a pathological stream
/// without newlines can't grow the buffer without bound.
fn read_until_crlf<R: BufRead>(reader: &mut R, out: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut total = 0;
    loop {
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            Ok(0) => return Ok(total),
            Ok(_) => {
                total += 1;
                out.push(byte[0]);
                if byte[0] == b'\n' || out.len() > 8192 {
                    return Ok(total);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

fn sleep_or_stop(dur: Duration, stop: &Arc<AtomicBool>) -> bool {
    let step = Duration::from_millis(50);
    let mut waited = Duration::ZERO;
    while waited < dur {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(step);
        waited += step;
    }
    stop.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn multipart_bytes() -> Vec<u8> {
        let jpeg_a = [0xFFu8, 0xD8, 0x01, 0x02, 0xFF, 0xD9];
        let jpeg_b = [0xFFu8, 0xD8, 0xAA, 0xBB, 0xCC, 0xFF, 0xD9];
        let mut buf = Vec::new();
        buf.extend_from_slice(
            b"HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n",
        );
        for jpeg in [jpeg_a.as_slice(), jpeg_b.as_slice()] {
            buf.extend_from_slice(
                format!(
                    "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                    jpeg.len()
                )
                .as_bytes(),
            );
            buf.extend_from_slice(jpeg);
            buf.extend_from_slice(b"\r\n");
        }
        buf
    }

    #[test]
    fn parses_boundary_and_frames() {
        let mut reader = Cursor::new(multipart_bytes());
        let boundary = read_multipart_boundary(&mut reader).unwrap();
        assert_eq!(boundary, "frame");
        let first = read_part(&mut reader).unwrap();
        assert_eq!(first, vec![0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9]);
        let second = read_part(&mut reader).unwrap();
        assert_eq!(second, vec![0xFF, 0xD8, 0xAA, 0xBB, 0xCC, 0xFF, 0xD9]);
    }

    #[test]
    fn parses_quoted_boundary() {
        assert_eq!(
            parse_boundary("Content-Type: multipart/x-mixed-replace; boundary=\"abc\""),
            Some("abc".to_string())
        );
    }

    #[test]
    fn stream_config_parse() {
        let c = StreamConfig::parse("limelight", "http://127.0.0.1:5800/stream").unwrap();
        assert_eq!(
            (c.host.as_str(), c.port, c.path.as_str()),
            ("127.0.0.1", 5800, "/stream")
        );
        let d = StreamConfig::parse("webcam", "10.0.0.2").unwrap();
        assert_eq!(
            (d.host.as_str(), d.port, d.path.as_str()),
            ("10.0.0.2", 80, "/")
        );
    }
}
