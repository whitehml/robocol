//! `ds_daemon`: owns the one `robocol::RobocolClient` connection to the Robot
//! Controller and serves its front ends (`ds_agent`, `ds_cli`) over a Unix
//! domain socket, one JSON object per line. See the repo README for usage.

mod connection;
mod request;
mod state;
mod waiters;

use std::net::IpAddr;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use robocol::{ClientConfig, RobocolClient};

use connection::handle_connection;
use state::DaemonState;
use waiters::{Subscribers, WaiterRegistry};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

struct Args {
    peer: Option<IpAddr>,
    peer_port: Option<u16>,
    bind_port: Option<u16>,
}

fn parse_args() -> Args {
    let mut args = Args {
        peer: None,
        peer_port: None,
        bind_port: None,
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--peer" => {
                args.peer = raw.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            "--peer-port" => {
                args.peer_port = raw.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            "--bind-port" => {
                args.bind_port = raw.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            _ => i += 1,
        }
    }
    args
}

/// Timeout for request/response commands. `DS_DAEMON_TIMEOUT_MS` lets tests
/// use something far shorter than the 5s production default.
fn resolve_timeout() -> Duration {
    std::env::var("DS_DAEMON_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TIMEOUT)
}

/// Binds the daemon's Unix socket, clearing a stale one left behind by a
/// crashed previous instance. If a live daemon already owns the path, exits
/// the process immediately rather than running two daemons against one
/// robot connection.
fn bind_socket() -> UnixListener {
    let path = ds_proto::socket_path();
    match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(_) => {
            if UnixStream::connect(&path).is_ok() {
                eprintln!("ds_daemon already running at {}", path.display());
                std::process::exit(0);
            }
            let _ = std::fs::remove_file(&path);
            UnixListener::bind(&path).expect("bind ds_daemon socket after clearing stale file")
        }
    }
}

fn main() {
    let args = parse_args();
    let mut config = ClientConfig::default();
    if let Some(peer) = args.peer {
        config.peer_addrs = vec![peer];
    }
    if let Some(port) = args.peer_port {
        config.peer_port = port;
    }
    if let Some(port) = args.bind_port {
        config.bind_port = port;
    }

    // Claim the Unix socket first: if another daemon already owns it this
    // exits 0 before touching UDP, so the "already running" path is
    // reachable even with the default bind port.
    let listener = bind_socket();

    let bind_port = config.bind_port;
    let (client, events) = match RobocolClient::start(config) {
        Ok(started) => started,
        Err(e) => {
            eprintln!(
                "ds_daemon: cannot bind UDP port {bind_port}: {e} (is Deck Station or ds_cli already running?)"
            );
            // Don't leave a socket file that ds_agent would mistake for a
            // live daemon.
            let _ = std::fs::remove_file(ds_proto::socket_path());
            std::process::exit(1);
        }
    };
    let client = Arc::new(Mutex::new(client));
    let state = Arc::new(Mutex::new(DaemonState::new()));
    let waiters = Arc::new(Mutex::new(WaiterRegistry::new()));
    let subscribers = Arc::new(Mutex::new(Subscribers::new()));

    {
        let state = state.clone();
        let waiters = waiters.clone();
        let subscribers = subscribers.clone();
        std::thread::spawn(move || {
            for event in events {
                state
                    .lock()
                    .expect("state mutex poisoned")
                    .apply_event(&event);
                waiters
                    .lock()
                    .expect("waiters mutex poisoned")
                    .resolve(&event);
                subscribers
                    .lock()
                    .expect("subscribers mutex poisoned")
                    .broadcast(&event);
            }
            // The robocol worker only drops its sender when it dies. A
            // daemon with no robot connection must not linger answering
            // `connected: true` from a frozen cache.
            eprintln!("ds_daemon: robocol client stopped; exiting");
            std::process::exit(1);
        });
    }

    let timeout = resolve_timeout();
    for incoming in listener.incoming() {
        let Ok(stream) = incoming else { continue };
        let client = client.clone();
        let state = state.clone();
        let waiters = waiters.clone();
        let subscribers = subscribers.clone();
        std::thread::spawn(move || {
            handle_connection(stream, client, state, waiters, subscribers, timeout);
        });
    }
}
