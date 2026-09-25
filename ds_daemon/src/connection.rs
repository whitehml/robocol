//! Per-connection handling: reads one request line, and either streams
//! `watch` events, exits the process for `shutdown`, or dispatches
//! everything else through `dispatch::handle_request` for a single reply.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use robocol::client::RobocolClient;

use ds_proto::Request;

use crate::dispatch::handle_request;
use crate::state::DaemonState;
use crate::waiters::{Subscribers, WaiterRegistry};

/// Forwards every watch line to the client until it hangs up. Filtering is
/// the client's job (`ds_proto::WatchFilter`).
fn run_watch(stream: &mut UnixStream, subscribers: &Arc<Mutex<Subscribers>>) {
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

pub fn handle_connection(
    mut stream: UnixStream,
    client: Arc<Mutex<RobocolClient>>,
    state: Arc<Mutex<DaemonState>>,
    waiters: Arc<Mutex<WaiterRegistry>>,
    subscribers: Arc<Mutex<Subscribers>>,
    timeout: Duration,
) {
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(reader_stream);
    let Ok(Some(line)) = ds_proto::read_message(&mut reader) else {
        return;
    };
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let _ = ds_proto::write_message(
                &mut stream,
                &serde_json::json!({"ok": false, "error": "bad_args", "message": e.to_string()}),
            );
            return;
        }
    };
    match req {
        Request::Watch => run_watch(&mut stream, &subscribers),
        Request::Shutdown => {
            let _ = ds_proto::write_message(&mut stream, &serde_json::json!({"ok": true}));
            std::process::exit(0);
        }
        other => {
            let reply = handle_request(other, &client, &state, &waiters, timeout);
            let _ = ds_proto::write_message(&mut stream, &reply);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    #[allow(clippy::type_complexity)]
    fn deps() -> (
        Arc<Mutex<RobocolClient>>,
        Arc<Mutex<DaemonState>>,
        Arc<Mutex<WaiterRegistry>>,
        Arc<Mutex<Subscribers>>,
    ) {
        let config = robocol::ClientConfig {
            bind_port: 0,
            peer_port: 1,
            ..Default::default()
        };
        let (client, _events) = RobocolClient::start(config).expect("bind UDP socket");
        (
            Arc::new(Mutex::new(client)),
            Arc::new(Mutex::new(DaemonState::new())),
            Arc::new(Mutex::new(WaiterRegistry::new())),
            Arc::new(Mutex::new(Subscribers::new())),
        )
    }

    #[test]
    fn ping_round_trips_over_a_real_socket_pair() {
        let (client, state, waiters, subscribers) = deps();
        let (mut cli_side, daemon_side) = UnixStream::pair().unwrap();
        ds_proto::write_message(&mut cli_side, &serde_json::json!({"cmd": "ping"})).unwrap();
        handle_connection(
            daemon_side,
            client,
            state,
            waiters,
            subscribers,
            Duration::from_millis(50),
        );

        let mut reader = BufReader::new(cli_side);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let reply: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(reply["ok"], true);
    }

    #[test]
    fn bad_json_yields_bad_args() {
        let (client, state, waiters, subscribers) = deps();
        let (mut cli_side, daemon_side) = UnixStream::pair().unwrap();
        cli_side.set_nonblocking(false).unwrap();
        use std::io::Write;
        cli_side.write_all(b"not json\n").unwrap();
        handle_connection(
            daemon_side,
            client,
            state,
            waiters,
            subscribers,
            Duration::from_millis(50),
        );

        let mut reader = BufReader::new(cli_side);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let reply: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "bad_args");
    }

    #[test]
    fn watch_forwards_every_broadcast_line() {
        let (client, state, waiters, subscribers) = deps();
        let (mut cli_side, daemon_side) = UnixStream::pair().unwrap();
        ds_proto::write_message(&mut cli_side, &serde_json::json!({"cmd": "watch"})).unwrap();

        let subs = subscribers.clone();
        let handle = std::thread::spawn(move || {
            handle_connection(
                daemon_side,
                client,
                state,
                waiters,
                subs,
                Duration::from_secs(1),
            );
        });

        // Give handle_connection time to subscribe before broadcasting.
        std::thread::sleep(Duration::from_millis(50));
        let running = serde_json::json!({"event": "state", "state": "Running"});
        let gone = serde_json::json!({"event": "disconnected"});
        subscribers.lock().unwrap().broadcast(&running);
        subscribers.lock().unwrap().broadcast(&gone);

        let mut reader = BufReader::new(cli_side);
        for expected in [&running, &gone] {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let got: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
            assert_eq!(&got, expected);
        }

        drop(reader); // closes the CLI side
        // run_watch only notices a closed peer when its next write fails, so
        // push one more line through to wake it and let it exit.
        subscribers.lock().unwrap().broadcast(&gone);
        handle.join().unwrap();
    }
}
