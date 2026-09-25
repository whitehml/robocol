//! Socket path and line framing shared by `ds_daemon` and its front ends.
//! Every process must resolve the same path and speak the same framing, so
//! this logic lives once instead of being duplicated on each side of the pipe.

pub mod filter;
pub mod request;

pub use filter::WatchFilter;
pub use request::{GamepadInput, PARSE_TABLE, Request, Verb, parse, split_line};

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

/// Where `ds_daemon` binds its Unix domain socket and `ds_agent` connects.
/// Prefers `$XDG_RUNTIME_DIR` (cleaned up by the OS on logout); falls back to
/// a per-user path under `/tmp` so this also works in a plain SSH session.
pub fn socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("ds_daemon.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/ds_daemon-{uid}.sock"))
}

/// Writes one JSON value as a single line: the whole `\n`-terminated buffer
/// goes out in one `write_all`, so a reader never sees a partial object.
pub fn write_message(stream: &mut impl Write, value: &serde_json::Value) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)
}

/// Reads one `\n`-terminated line, with the newline stripped. `Ok(None)`
/// means clean EOF (the peer closed the connection); any other read error
/// (including an unterminated final line) still surfaces as `Err`.
pub fn read_message(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trips_one_message() {
        let mut buf = Vec::new();
        write_message(&mut buf, &serde_json::json!({"ok": true, "n": 1})).unwrap();
        let mut reader = std::io::BufReader::new(Cursor::new(buf));
        let line = read_message(&mut reader).unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["n"], 1);
    }

    #[test]
    fn reads_multiple_messages_in_sequence() {
        let mut buf = Vec::new();
        write_message(&mut buf, &serde_json::json!({"seq": 1})).unwrap();
        write_message(&mut buf, &serde_json::json!({"seq": 2})).unwrap();
        let mut reader = std::io::BufReader::new(Cursor::new(buf));
        let first = read_message(&mut reader).unwrap().unwrap();
        let second = read_message(&mut reader).unwrap().unwrap();
        assert!(first.contains("\"seq\":1"));
        assert!(second.contains("\"seq\":2"));
    }

    #[test]
    fn clean_eof_yields_none() {
        let mut reader = std::io::BufReader::new(Cursor::new(Vec::new()));
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn socket_path_prefers_xdg_runtime_dir() {
        // SAFETY: test runs single-threaded within this process's env mutation.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/tmp/ds-agent-test-runtime") };
        assert_eq!(
            socket_path(),
            PathBuf::from("/tmp/ds-agent-test-runtime/ds_daemon.sock")
        );
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    }
}
