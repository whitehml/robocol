//! How a front end reaches `ds_daemon`: the socket path, newline-delimited
//! JSON framing, and a `Connection` that has already said `hello`. Kept apart
//! from the command surface so the transport can change without touching it.

use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::request::{Request, Role};

/// How long a front end waits for a daemon it just started.
const SPAWN_WAIT: Duration = Duration::from_secs(3);
/// How much of the daemon's log to quote when it dies while starting.
const LOG_TAIL_LINES: usize = 5;
/// How long to wait for a daemon that broke off the handshake to exit.
const EXIT_GRACE: Duration = Duration::from_millis(500);

/// Where `ds_daemon` listens. Prefers `$XDG_RUNTIME_DIR` (cleaned up by the
/// OS on logout); falls back to a per-user path under `/tmp` so this also
/// works in a plain SSH session.
pub fn socket_path() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    socket_path_for(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), uid)
}

fn socket_path_for(runtime_dir: Option<&OsStr>, uid: u32) -> PathBuf {
    match runtime_dir {
        Some(dir) => PathBuf::from(dir).join("ds_daemon.sock"),
        None => PathBuf::from(format!("/tmp/ds_daemon-{uid}.sock")),
    }
}

/// Where a started daemon's stdout and stderr go: beside the socket, so it is
/// per user too.
pub fn log_path() -> PathBuf {
    socket_path().with_extension("log")
}

/// Writes one JSON value as a single line: the whole `\n`-terminated buffer
/// goes out in one `write_all`, so a reader never sees a partial object.
pub fn write_message(stream: &mut impl Write, value: &Value) -> io::Result<()> {
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

/// An open connection to the daemon that has completed the `hello`
/// handshake.
pub struct Connection {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Connection {
    fn new(stream: UnixStream) -> io::Result<Connection> {
        Ok(Connection {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
        })
    }

    pub fn send(&mut self, request: &Request) -> io::Result<()> {
        let value = serde_json::to_value(request).map_err(io::Error::other)?;
        write_message(&mut self.writer, &value)
    }

    /// The next line from the daemon; `Ok(None)` once it has closed the
    /// connection.
    pub fn recv(&mut self) -> io::Result<Option<Value>> {
        match read_message(&mut self.reader)? {
            None => Ok(None),
            Some(line) => serde_json::from_str(&line)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        }
    }

    /// Sends `request` and waits for its one reply.
    pub fn request(&mut self, request: &Request) -> io::Result<Value> {
        self.send(request)?;
        self.recv()?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "ds_daemon closed the connection without replying",
            )
        })
    }
}

fn handshake(stream: UnixStream, role: Role) -> Result<Connection, String> {
    let mut conn = Connection::new(stream).map_err(|e| e.to_string())?;
    let reply = conn
        .request(&Request::Hello { role })
        .map_err(|e| format!("ds_daemon did not answer hello: {e}"))?;
    if reply["ok"] == true {
        Ok(conn)
    } else {
        Err(format!("ds_daemon refused hello: {reply}"))
    }
}

/// Connects to a running daemon; `Ok(None)` when none is listening.
pub fn connect_existing(role: Role) -> Result<Option<Connection>, String> {
    match UnixStream::connect(socket_path()) {
        Ok(stream) => handshake(stream, role).map(Some),
        Err(_) => Ok(None),
    }
}

/// Connects to the daemon, starting one first if none is running.
///
/// The daemon started is the `ds_daemon` binary next to the running
/// executable, in its own process group (so it outlives this front end),
/// with stdin from `/dev/null`, stdout and stderr appended to `log_path()`,
/// and `extra_args` plus the whitespace-split `DS_DAEMON_ARGS` as its
/// arguments. If it exits before its socket answers, the error quotes the
/// end of its log, so the reason reaches the user rather than only the log.
pub fn connect_or_spawn(role: Role, extra_args: &[String]) -> Result<Connection, String> {
    if let Some(conn) = connect_existing(role)? {
        return Ok(conn);
    }
    let mut child = spawn_daemon(extra_args)?;
    let deadline = Instant::now() + SPAWN_WAIT;
    loop {
        if let Ok(stream) = UnixStream::connect(socket_path()) {
            // A daemon that dies right after binding its socket can still
            // accept this connection; report why it died, not the broken
            // handshake.
            return handshake(stream, role)
                .map_err(|e| exit_report(&mut child, EXIT_GRACE).unwrap_or(e));
        }
        if let Some(report) = exit_report(&mut child, Duration::ZERO) {
            return Err(report);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "ds_daemon did not become ready within {} s; see {}",
                SPAWN_WAIT.as_secs(),
                log_path().display()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// If the started daemon has exited (checking for up to `grace`), why: its
/// exit status and the end of its log.
fn exit_report(child: &mut Child, grace: Duration) -> Option<String> {
    let deadline = Instant::now() + grace;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(format!(
                "ds_daemon exited ({status}) before it was ready; end of {}:\n{}",
                log_path().display(),
                log_tail(&log_path(), LOG_TAIL_LINES)
            ));
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_daemon(extra_args: &[String]) -> Result<Child, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate ds_daemon: {e}"))?;
    let daemon = exe.with_file_name("ds_daemon");
    let log = log_path();
    let open_log = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .map_err(|e| format!("cannot open {}: {e}", log.display()))
    };
    let mut command = Command::new(&daemon);
    command.args(extra_args);
    if let Ok(env_args) = std::env::var("DS_DAEMON_ARGS") {
        command.args(env_args.split_whitespace());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(open_log()?))
        .stderr(Stdio::from(open_log()?))
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", daemon.display()))
}

/// The last `n` lines of `path`, or a note saying why there are none.
fn log_tail(path: &Path, n: usize) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(n)..].join("\n")
        }
        Err(e) => format!("(cannot read {}: {e})", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn round_trips_one_message() {
        let mut buf = Vec::new();
        write_message(&mut buf, &json!({"ok": true, "n": 1})).unwrap();
        let mut reader = BufReader::new(Cursor::new(buf));
        let line = read_message(&mut reader).unwrap().unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["n"], 1);
    }

    #[test]
    fn reads_multiple_messages_in_sequence() {
        let mut buf = Vec::new();
        write_message(&mut buf, &json!({"seq": 1})).unwrap();
        write_message(&mut buf, &json!({"seq": 2})).unwrap();
        let mut reader = BufReader::new(Cursor::new(buf));
        let first = read_message(&mut reader).unwrap().unwrap();
        let second = read_message(&mut reader).unwrap().unwrap();
        assert!(first.contains("\"seq\":1"));
        assert!(second.contains("\"seq\":2"));
    }

    #[test]
    fn clean_eof_yields_none() {
        let mut reader = BufReader::new(Cursor::new(Vec::new()));
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn socket_path_prefers_the_runtime_dir() {
        assert_eq!(
            socket_path_for(Some(OsStr::new("/run/user/501")), 501),
            PathBuf::from("/run/user/501/ds_daemon.sock")
        );
        assert_eq!(
            socket_path_for(None, 501),
            PathBuf::from("/tmp/ds_daemon-501.sock")
        );
    }

    #[test]
    fn log_tail_keeps_the_last_lines() {
        let path = std::env::temp_dir().join(format!("ds_proto_log_tail_{}", std::process::id()));
        let text: String = (1..=8).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, text).unwrap();
        assert_eq!(log_tail(&path, 3), "line 6\nline 7\nline 8");
        std::fs::remove_file(&path).unwrap();
        assert!(log_tail(&path, 3).starts_with("(cannot read"));
    }

    /// Plays the daemon on the far end of a socket pair: answers each request
    /// line with the next of `replies`.
    fn fake_daemon(replies: Vec<Value>) -> (UnixStream, std::thread::JoinHandle<Vec<Value>>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let handle = std::thread::spawn(move || {
            let mut reader = BufReader::new(theirs.try_clone().unwrap());
            let mut writer = theirs;
            let mut seen = Vec::new();
            for reply in replies {
                let line = read_message(&mut reader).unwrap().unwrap();
                seen.push(serde_json::from_str(&line).unwrap());
                write_message(&mut writer, &reply).unwrap();
            }
            seen
        });
        (ours, handle)
    }

    #[test]
    fn handshake_says_hello_then_carries_requests() {
        let (stream, daemon) = fake_daemon(vec![
            json!({"ok": true, "role": "agent"}),
            json!({"ok": true}),
        ]);
        let mut conn = handshake(stream, Role::Agent).unwrap();
        assert_eq!(conn.request(&Request::Ping).unwrap(), json!({"ok": true}));
        let seen = daemon.join().unwrap();
        assert_eq!(seen[0], json!({"cmd": "hello", "role": "agent"}));
        assert_eq!(seen[1], json!({"cmd": "ping"}));
    }

    #[test]
    fn a_refused_hello_is_an_error() {
        let (stream, daemon) = fake_daemon(vec![
            json!({"ok": false, "error": "bad_args", "message": "no"}),
        ]);
        assert!(handshake(stream, Role::Operator).is_err());
        daemon.join().unwrap();
    }
}
