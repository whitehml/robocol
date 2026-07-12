//! MJPEG-over-HTTP client for camera feeds. Connects to a
//! `multipart/x-mixed-replace` stream (e.g. a Limelight or the RC's webcam
//! server), pulls out each JPEG frame, and hands the raw bytes to the caller
//! to decode. Pure `std` — the Godot bridge owns the threads and turns the
//! bytes into textures.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

pub(crate) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
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
    let mut logged_unreachable = false;
    while !stop.load(Ordering::Relaxed) {
        let delivered = stream_once(&cfg, &tx, &stop).unwrap_or_else(|e| {
            if !logged_unreachable {
                eprintln!(
                    "video: {} unreachable at {}:{}: {e}",
                    cfg.source, cfg.host, cfg.port
                );
                logged_unreachable = true;
            }
            false
        });
        // Only clear the pane if we had actually been delivering frames — a
        // "not yet reachable" (no OpMode running) shouldn't spam a clear.
        if delivered {
            logged_unreachable = false;
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
    let addr = (cfg.host.as_str(), cfg.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "host resolved to no addresses",
            )
        })?;
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let mut w = &stream;
    write!(
        w,
        "GET {} HTTP/1.0\r\nHost: {}\r\nAccept: */*\r\n\r\n",
        cfg.path, cfg.host
    )?;
    w.flush()?;

    let debug = std::env::var_os("DECK_VIDEO_DEBUG").is_some();
    let mut head_reader = BufReader::new(&stream);
    let head = read_response_head(&mut head_reader, &cfg.source, debug)?;
    let delim = format!("--{}", head.boundary).into_bytes();
    if debug {
        eprintln!("video[{}]: chunked={}", cfg.source, head.chunked);
    }
    if head.chunked {
        let mut reader = BufReader::new(ChunkedReader::new(head_reader, &cfg.source, debug));
        pump_frames(&mut reader, &delim, cfg, tx, stop, debug)
    } else {
        pump_frames(&mut head_reader, &delim, cfg, tx, stop, debug)
    }
}

fn pump_frames<R: BufRead>(
    reader: &mut R,
    delim: &[u8],
    cfg: &StreamConfig,
    tx: &Sender<VideoEvent>,
    stop: &Arc<AtomicBool>,
    debug: bool,
) -> std::io::Result<bool> {
    if let Err(e) = read_header_line(reader) {
        if debug {
            eprintln!("video[{}]: first boundary read failed: {e}", cfg.source);
        }
        return Ok(false);
    }
    let mut delivered = false;
    let mut index = 0u64;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(delivered);
        }
        // Past the response head, a read error means the stream dropped (the
        // RC stopped serving when the OpMode ended); report what we managed
        // to deliver rather than bubbling it up as a never-connected error.
        let content_length = match skip_part_headers(reader) {
            Ok(cl) => cl,
            Err(e) => {
                if debug {
                    eprintln!("video[{}]: part header read failed: {e}", cfg.source);
                }
                return Ok(delivered);
            }
        };
        let mut jpeg = match read_until_delim(reader, delim, debug.then_some(cfg.source.as_str())) {
            Ok(j) => j,
            Err(e) => {
                if debug {
                    eprintln!("video[{}]: frame body read failed: {e}", cfg.source);
                }
                return Ok(delivered);
            }
        };
        if jpeg.ends_with(b"\r\n") {
            jpeg.truncate(jpeg.len() - 2);
        }
        let _ = read_header_line(reader);
        if debug {
            log_frame(&cfg.source, index, &jpeg, content_length);
        }
        index += 1;
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

struct ResponseHead {
    boundary: String,
    chunked: bool,
}

fn read_response_head<R: BufRead>(
    reader: &mut R,
    source: &str,
    debug: bool,
) -> std::io::Result<ResponseHead> {
    let status = read_header_line(reader)?;
    if debug {
        eprintln!("video[{source}]: {status}");
    }
    if !status.split_whitespace().any(|tok| tok == "200") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unexpected HTTP status: {status}"),
        ));
    }
    let mut boundary = None;
    let mut chunked = false;
    loop {
        let line = read_header_line(reader)?;
        if line.is_empty() {
            break;
        }
        if debug {
            eprintln!("video[{source}]: | {line}");
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
        if let Some(b) = parse_boundary(&line) {
            boundary = Some(b);
        }
    }
    let boundary = boundary.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no multipart boundary in response",
        )
    })?;
    Ok(ResponseHead { boundary, chunked })
}

struct ChunkedReader<R: BufRead> {
    inner: R,
    remaining: usize,
    done: bool,
    source: String,
    debug: bool,
    chunks_seen: u64,
}

impl<R: BufRead> ChunkedReader<R> {
    fn new(inner: R, source: &str, debug: bool) -> Self {
        ChunkedReader {
            inner,
            remaining: 0,
            done: false,
            source: source.to_string(),
            debug,
            chunks_seen: 0,
        }
    }
}

impl<R: BufRead> std::io::Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut line = read_header_line(&mut self.inner)?;
            while line.is_empty() {
                line = read_header_line(&mut self.inner)?;
            }
            let hex = line.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(hex, 16).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("bad chunk size: {line:?}"),
                )
            })?;
            if self.debug && self.chunks_seen < 4 {
                eprintln!("video[{}]: chunk size line {line:?} -> {size}", self.source);
                self.chunks_seen += 1;
            }
            if size == 0 {
                self.done = true;
                return Ok(0);
            }
            self.remaining = size;
        }
        let want = self.remaining.min(buf.len());
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "chunk body truncated",
            ));
        }
        self.remaining -= n;
        if self.remaining == 0 {
            // Consume the CRLF that terminates this chunk's data.
            let mut crlf = [0u8; 2];
            self.inner.read_exact(&mut crlf)?;
        }
        Ok(n)
    }
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

fn skip_part_headers<R: BufRead>(reader: &mut R) -> std::io::Result<Option<usize>> {
    let mut content_length = None;
    loop {
        let line = read_header_line(reader)?;
        if line.is_empty() {
            return Ok(content_length);
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }
}

fn kmp_failure(pattern: &[u8]) -> Vec<usize> {
    let mut failure = vec![0usize; pattern.len()];
    let mut k = 0;
    for i in 1..pattern.len() {
        while k > 0 && pattern[i] != pattern[k] {
            k = failure[k - 1];
        }
        if pattern[i] == pattern[k] {
            k += 1;
        }
        failure[i] = k;
    }
    failure
}

fn read_until_delim<R: BufRead>(
    reader: &mut R,
    delim: &[u8],
    dump: Option<&str>,
) -> std::io::Result<Vec<u8>> {
    let failure = kmp_failure(delim);
    let mut out: Vec<u8> = Vec::new();
    let mut matched = 0;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "stream ended mid-frame",
            ));
        }
        let mut consumed = 0;
        for &byte in available {
            consumed += 1;
            loop {
                if byte == delim[matched] {
                    matched += 1;
                    if matched == delim.len() {
                        reader.consume(consumed);
                        return Ok(out);
                    }
                    break;
                } else if matched == 0 {
                    out.push(byte);
                    break;
                }
                let fallback = failure[matched - 1];
                out.extend_from_slice(&delim[..matched - fallback]);
                matched = fallback;
            }
            if out.len() > MAX_FRAME_BYTES {
                reader.consume(consumed);
                if let Some(source) = dump {
                    let path = format!("/tmp/deck_video_{source}_overflow.bin");
                    let _ = std::fs::write(&path, &out[..300_000.min(out.len())]);
                    eprintln!(
                        "video[{source}]: no boundary in {} bytes; dumped head to {path}",
                        out.len()
                    );
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "frame exceeds size cap",
                ));
            }
        }
        reader.consume(consumed);
    }
}

fn log_frame(source: &str, index: u64, jpeg: &[u8], content_length: Option<usize>) {
    let head: Vec<String> = jpeg.iter().take(4).map(|b| format!("{b:02X}")).collect();
    let tail: Vec<String> = jpeg
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|b| format!("{b:02X}"))
        .collect();
    let soi = jpeg.starts_with(&[0xFF, 0xD8]);
    let eoi = jpeg.ends_with(&[0xFF, 0xD9]);
    eprintln!(
        "video[{source}]: frame {index} len={} content_length={content_length:?} \
         soi={soi} eoi={eoi} head=[{}] tail=[{}]",
        jpeg.len(),
        head.join(" "),
        tail.join(" ")
    );
    if index < 3 {
        let path = format!("/tmp/deck_video_{source}_{index}.jpg");
        if let Err(e) = std::fs::write(&path, jpeg) {
            eprintln!("video[{source}]: could not dump {path}: {e}");
        } else {
            eprintln!("video[{source}]: dumped {path}");
        }
    }
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
        buf.extend_from_slice(b"--frame--\r\n");
        buf
    }

    fn frames_from<R: BufRead>(reader: &mut R, delim: &[u8]) -> Vec<Vec<u8>> {
        read_header_line(reader).unwrap();
        let mut frames = Vec::new();
        while skip_part_headers(reader).is_ok() {
            let Ok(mut frame) = read_until_delim(reader, delim, None) else {
                break;
            };
            if frame.ends_with(b"\r\n") {
                frame.truncate(frame.len() - 2);
            }
            let _ = read_header_line(reader);
            frames.push(frame);
        }
        frames
    }

    fn read_all_frames(buf: Vec<u8>) -> Vec<Vec<u8>> {
        let mut reader = Cursor::new(buf);
        let head = read_response_head(&mut reader, "test", false).unwrap();
        assert!(!head.chunked);
        let delim = format!("--{}", head.boundary).into_bytes();
        frames_from(&mut reader, &delim)
    }

    fn chunk_encode(body: &[u8], chunk: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for part in body.chunks(chunk) {
            out.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
            out.extend_from_slice(part);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    }

    #[test]
    fn dechunks_before_framing() {
        // Body identical to multipart_bytes but served chunked, split mid-JPEG.
        let body = {
            let full = multipart_bytes();
            let head_end = find(&full, b"\r\n\r\n").unwrap() + 4;
            (full[..head_end].to_vec(), full[head_end..].to_vec())
        };
        let mut buf = Vec::new();
        buf.extend_from_slice(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\
              Content-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n",
        );
        buf.extend_from_slice(&chunk_encode(&body.1, 4));
        let mut reader = Cursor::new(buf);
        let head = read_response_head(&mut reader, "test", false).unwrap();
        assert!(head.chunked);
        let delim = format!("--{}", head.boundary).into_bytes();
        let mut dechunked = BufReader::new(ChunkedReader::new(reader, "test", false));
        let frames = frames_from(&mut dechunked, &delim);
        assert_eq!(frames[0], vec![0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9]);
        assert_eq!(frames[1], vec![0xFF, 0xD8, 0xAA, 0xBB, 0xCC, 0xFF, 0xD9]);
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }


    #[test]
    fn dechunks_large_multi_chunk_frames() {
        let mut jpeg = vec![0xFFu8, 0xD8];
        jpeg.extend(std::iter::repeat_n(0xAB, 20_000));
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        let mut body = Vec::new();
        for _ in 0..2 {
            body.extend_from_slice(
                format!(
                    "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                    jpeg.len()
                )
                .as_bytes(),
            );
            body.extend_from_slice(&jpeg);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"--frame--\r\n");

        let mut buf = Vec::new();
        buf.extend_from_slice(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\
              Content-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n",
        );
        buf.extend_from_slice(&chunk_encode(&body, 8192));
        let mut reader = Cursor::new(buf);
        let head = read_response_head(&mut reader, "test", false).unwrap();
        assert!(head.chunked);
        let delim = format!("--{}", head.boundary).into_bytes();
        let mut dechunked = BufReader::new(ChunkedReader::new(reader, "test", false));
        let frames = frames_from(&mut dechunked, &delim);
        assert_eq!(frames.len(), 2, "expected 2 frames, got {}", frames.len());
        assert_eq!(frames[0], jpeg);
        assert_eq!(frames[1], jpeg);
    }

    #[test]
    fn frames_delimited_by_boundary() {
        let frames = read_all_frames(multipart_bytes());
        assert_eq!(frames[0], vec![0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9]);
        assert_eq!(frames[1], vec![0xFF, 0xD8, 0xAA, 0xBB, 0xCC, 0xFF, 0xD9]);
    }

    #[test]
    fn boundary_framing_ignores_content_length_and_marker_bytes() {
        let jpeg = [0xFFu8, 0xD8, 0xFF, 0xDB, 0x00, 0xFF, 0xD9, 0x22, 0xFF, 0xD9];
        let mut buf = Vec::new();
        buf.extend_from_slice(
            b"HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=f\r\n\r\n",
        );
        buf.extend_from_slice(b"--f\r\nContent-Type: image/jpeg\r\nContent-Length: 3\r\n\r\n");
        buf.extend_from_slice(&jpeg);
        buf.extend_from_slice(b"\r\n--f\r\n");
        assert_eq!(read_all_frames(buf)[0], jpeg);
    }

    #[test]
    fn rejects_non_200_status() {
        let mut reader = Cursor::new(b"HTTP/1.0 404 Not Found\r\n\r\n".to_vec());
        assert!(read_response_head(&mut reader, "test", false).is_err());
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
