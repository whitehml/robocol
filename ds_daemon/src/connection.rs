//! One thread per client connection. A connection is a command session —
//! read a request, write its one reply, repeat until the client hangs up —
//! until it sends `watch`, which turns it into a one-way event stream.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use ds_proto::Request;
use serde_json::json;

use crate::dispatch::{Daemon, Session, handle_request};
use crate::waiters::Subscribers;

/// Forwards every watch line to the client until it hangs up. Filtering is
/// the client's job (`ds_proto::WatchFilter`).
fn run_watch(stream: &mut UnixStream, subscribers: &Mutex<Subscribers>) {
    let rx = subscribers
        .lock()
        .expect("subscribers mutex poisoned")
        .subscribe();
    for line in rx {
        if ds_proto::write_message(stream, &line).is_err() {
            break;
        }
    }
}

pub fn handle_connection(stream: UnixStream, daemon: Arc<Daemon>) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let mut session = Session::new();
    while let Ok(Some(line)) = ds_proto::read_message(&mut reader) {
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Watch) => return run_watch(&mut writer, &daemon.subscribers),
            Ok(Request::Shutdown) => {
                let _ = ds_proto::write_message(&mut writer, &json!({"ok": true}));
                std::process::exit(0);
            }
            Ok(request) => handle_request(request, &mut session, &daemon),
            Err(e) => json!({"ok": false, "error": "bad_args", "message": e.to_string()}),
        };
        if ds_proto::write_message(&mut writer, &reply).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::test_daemon;
    use serde_json::Value;
    use std::io::{BufRead, Write};
    use std::time::Duration;

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
    fn watch_forwards_every_broadcast_line() {
        let daemon = test_daemon(Duration::from_millis(50));
        let (mut cli_side, daemon_side) = UnixStream::pair().unwrap();
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

        drop(reader); // closes the CLI side
        // run_watch only notices a closed peer when its next write fails, so
        // push one more line through to wake it and let it exit.
        daemon.subscribers.lock().unwrap().broadcast(&gone);
        handle.join().unwrap();
    }
}
