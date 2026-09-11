//! Per-connection handling: reads one request line, and either streams
//! `watch` events, exits the process for `shutdown`, or dispatches
//! everything else through `request::handle_request` for a single reply.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use robocol::client::{Event, RobocolClient};

use crate::request::{Request, handle_request};
use crate::state::DaemonState;
use crate::waiters::{Subscribers, WaiterRegistry};

// Not yet wired up outside tests: Task 6's accept loop is what will spawn a
// thread per connection calling `handle_connection`, as Tasks 2-4 noted for
// `state`/`waiters`/`request`.
fn event_type_name(event: &Event) -> Option<&'static str> {
    match event {
        Event::Connected { .. } => Some("connected"),
        Event::Disconnected => Some("disconnected"),
        Event::RobotState(_) => Some("state"),
        Event::Telemetry(_) => Some("telemetry"),
        Event::Stacktrace(_) => Some("stacktrace"),
        Event::CommandDropped { .. } => Some("command_dropped"),
        Event::ProtocolError(_) => Some("protocol_error"),
        Event::WebcamAvailable(_) => Some("webcam_available"),
        _ => None,
    }
}

fn event_to_watch_json(event: &Event, types: Option<&[String]>) -> Option<serde_json::Value> {
    let kind = event_type_name(event)?;
    if let Some(types) = types
        && !types.iter().any(|t| t == kind)
    {
        return None;
    }
    Some(match event {
        Event::Connected { peer } => serde_json::json!({"event": kind, "peer": peer.to_string()}),
        Event::Disconnected => serde_json::json!({"event": kind}),
        Event::RobotState(s) => serde_json::json!({"event": kind, "state": format!("{s:?}")}),
        Event::Telemetry(t) => {
            let strings: serde_json::Map<String, serde_json::Value> = t
                .strings
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            let numbers: serde_json::Map<String, serde_json::Value> = t
                .numbers
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::json!(v)))
                .collect();
            serde_json::json!({"event": kind, "tag": t.tag, "strings": strings, "numbers": numbers})
        }
        Event::Stacktrace(trace) => serde_json::json!({"event": kind, "trace": trace}),
        Event::CommandDropped { name } => serde_json::json!({"event": kind, "name": name}),
        Event::ProtocolError(message) => serde_json::json!({"event": kind, "message": message}),
        Event::WebcamAvailable(available) => {
            serde_json::json!({"event": kind, "available": available})
        }
        _ => return None,
    })
}

fn run_watch(
    stream: &mut UnixStream,
    subscribers: &Arc<Mutex<Subscribers>>,
    types: Option<Vec<String>>,
) {
    let rx = subscribers
        .lock()
        .expect("subscribers mutex poisoned")
        .subscribe();
    for event in rx {
        if let Some(json) = event_to_watch_json(&event, types.as_deref())
            && ds_agent_ipc::write_message(stream, &json).is_err()
        {
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
    let Ok(Some(line)) = ds_agent_ipc::read_message(&mut reader) else {
        return;
    };
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let _ = ds_agent_ipc::write_message(
                &mut stream,
                &serde_json::json!({"ok": false, "error": "bad_args", "message": e.to_string()}),
            );
            return;
        }
    };
    match req {
        Request::Watch { types } => run_watch(&mut stream, &subscribers, types),
        Request::Shutdown => {
            let _ = ds_agent_ipc::write_message(&mut stream, &serde_json::json!({"ok": true}));
            std::process::exit(0);
        }
        other => {
            let reply = handle_request(other, &client, &state, &waiters, timeout);
            let _ = ds_agent_ipc::write_message(&mut stream, &reply);
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
        ds_agent_ipc::write_message(&mut cli_side, &serde_json::json!({"cmd": "ping"})).unwrap();
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
    fn watch_streams_only_requested_types() {
        let (client, state, waiters, subscribers) = deps();
        let (mut cli_side, daemon_side) = UnixStream::pair().unwrap();
        ds_agent_ipc::write_message(
            &mut cli_side,
            &serde_json::json!({"cmd": "watch", "types": ["disconnected"]}),
        )
        .unwrap();

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
        subscribers
            .lock()
            .unwrap()
            .broadcast(&Event::RobotState(robocol::types::RobotState::Running));
        subscribers.lock().unwrap().broadcast(&Event::Disconnected);

        let mut reader = BufReader::new(cli_side);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let event: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(event["event"], "disconnected");

        drop(reader); // closes the CLI side
        // run_watch only notices a closed peer when its next write fails, so
        // push one more event through to wake it and let it exit.
        subscribers.lock().unwrap().broadcast(&Event::Disconnected);
        handle.join().unwrap();
    }
}
