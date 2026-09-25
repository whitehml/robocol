//! One thread per client connection. A connection is a command session —
//! read a request, write its one reply, repeat until the client hangs up —
//! until it sends `watch`, which turns it into a one-way event stream.
//!
//! A reader thread takes the client's lines off the socket, so a hang-up is
//! noticed even while a request is running: it marks the session, and a
//! running gamepad pulse stops instead of driving on for nobody. A client
//! that closes its sending side counts as gone, so a client must keep the
//! connection open until it has its replies (the front ends do).

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ds_proto::Request;
use serde_json::json;

use crate::dispatch::{Daemon, Session, handle_request};
use crate::waiters::Subscribers;

/// Requests read ahead of the one being handled.
const READ_AHEAD: usize = 16;
/// How often an idle `watch` stream checks whether its client left.
const WATCH_POLL: Duration = Duration::from_millis(200);

/// Reads request lines on their own thread. When the client hangs up (or
/// the socket fails) it sets `hung_up` and ends the stream.
fn read_lines(stream: UnixStream, hung_up: Arc<AtomicBool>) -> Receiver<String> {
    let (tx, rx) = mpsc::sync_channel(READ_AHEAD);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        while let Ok(Some(line)) = ds_proto::read_message(&mut reader) {
            if tx.send(line).is_err() {
                return;
            }
        }
        hung_up.store(true, Ordering::Relaxed);
    });
    rx
}

/// Forwards every watch line to the client until it hangs up. Filtering is
/// the client's job (`ds_proto::WatchFilter`).
fn run_watch(stream: &mut UnixStream, subscribers: &Mutex<Subscribers>, hung_up: &AtomicBool) {
    let rx = subscribers
        .lock()
        .expect("subscribers mutex poisoned")
        .subscribe();
    while !hung_up.load(Ordering::Relaxed) {
        match rx.recv_timeout(WATCH_POLL) {
            Ok(line) => {
                if ds_proto::write_message(stream, &line).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

pub fn handle_connection(stream: UnixStream, daemon: Arc<Daemon>) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut writer = stream;
    let mut session = Session::new();
    let requests = read_lines(read_half, session.hung_up.clone());
    for line in requests.iter() {
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Watch) => {
                // An event stream is not an operator at the controls.
                daemon.end_session(&session);
                return run_watch(&mut writer, &daemon.subscribers, &session.hung_up);
            }
            Ok(Request::Shutdown) => {
                let _ = ds_proto::write_message(&mut writer, &json!({"ok": true}));
                daemon.shut_down();
            }
            Ok(request) => handle_request(request, &mut session, &daemon),
            Err(e) => json!({"ok": false, "error": "bad_args", "message": e.to_string()}),
        };
        if ds_proto::write_message(&mut writer, &reply).is_err() {
            break;
        }
    }
    daemon.end_session(&session);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::test_daemon;
    use serde_json::Value;
    use std::io::{BufRead, Write};

    /// Sends `lines` down one connection, closes the client's write half, runs
    /// the connection to completion, and returns every reply.
    fn session(lines: &[&str]) -> Vec<Value> {
        let daemon = test_daemon(Duration::from_millis(50));
        let (mut client, server) = UnixStream::pair().unwrap();
        for line in lines {
            client.write_all(format!("{line}\n").as_bytes()).unwrap();
        }
        client.shutdown(std::net::Shutdown::Write).unwrap();
        handle_connection(server, daemon);
        BufReader::new(client)
            .lines()
            .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn one_connection_answers_many_requests() {
        let replies = session(&[
            r#"{"cmd":"ping"}"#,
            r#"{"cmd":"status"}"#,
            r#"{"cmd":"ping"}"#,
        ]);
        assert_eq!(replies.len(), 3);
        assert!(replies.iter().all(|r| r["ok"] == true));
        assert_eq!(replies[1]["connected"], false);
    }

    #[test]
    fn bad_json_is_answered_and_the_session_continues() {
        let replies = session(&["not json", r#"{"cmd":"ping"}"#]);
        assert_eq!(replies[0]["error"], "bad_args");
        assert_eq!(replies[1], json!({"ok": true}));
    }

    #[test]
    fn hello_is_answered_with_the_role() {
        let replies = session(&[r#"{"cmd":"hello","role":"operator"}"#]);
        assert_eq!(replies, vec![json!({"ok": true, "role": "operator"})]);
    }

    #[test]
    fn an_operator_that_hangs_up_gives_its_pads_back() {
        let daemon = test_daemon(Duration::from_millis(50));
        let (mut client, server) = UnixStream::pair().unwrap();
        for line in [
            r#"{"cmd":"hello","role":"operator"}"#,
            r#"{"cmd":"claim","pads":["gamepad2"]}"#,
        ] {
            client.write_all(format!("{line}\n").as_bytes()).unwrap();
        }
        client.shutdown(std::net::Shutdown::Write).unwrap();
        handle_connection(server, daemon.clone());
        let status = handle_request(Request::Status, &mut Session::new(), &daemon);
        assert_eq!(status["gamepads"]["gamepad2"], serde_json::Value::Null);
    }

    #[test]
    fn watch_forwards_lines_and_notices_its_client_leave() {
        let daemon = test_daemon(Duration::from_millis(50));
        let (mut cli_side, daemon_side) = UnixStream::pair().unwrap();
        cli_side
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        ds_proto::write_message(&mut cli_side, &json!({"cmd": "watch"})).unwrap();

        let handle = {
            let daemon = daemon.clone();
            std::thread::spawn(move || handle_connection(daemon_side, daemon))
        };

        // Give handle_connection time to subscribe before broadcasting.
        std::thread::sleep(Duration::from_millis(50));
        let running = json!({"event": "state", "state": "Running"});
        let gone = json!({"event": "disconnected"});
        daemon.subscribers.lock().unwrap().broadcast(&running);
        daemon.subscribers.lock().unwrap().broadcast(&gone);

        let mut reader = BufReader::new(cli_side);
        for expected in [&running, &gone] {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let got: Value = serde_json::from_str(line.trim_end()).unwrap();
            assert_eq!(&got, expected);
        }

        // With no further events, the stream still ends once the client
        // closes its side.
        drop(reader);
        handle.join().unwrap();
    }
}
