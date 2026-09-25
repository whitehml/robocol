//! `ds_daemon`: owns the one `robocol::RobocolClient` connection to the Robot
//! Controller and serves its front ends (`ds_agent`, `ds_cli`) over a Unix
//! domain socket, one JSON object per line. See the repo README for usage.

mod connection;
mod dispatch;
mod gamepad;
mod state;
mod waiters;
mod watch;

use std::fs::{File, OpenOptions, TryLockError};
use std::net::IpAddr;
use std::os::unix::net::UnixListener;
use std::sync::Arc;
use std::time::Duration;

use robocol::{ClientConfig, Event, RobocolClient};

use connection::handle_connection;
use dispatch::Daemon;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

const USAGE: &str = "usage: ds_daemon [--peer <ip>] [--peer-port <port>] [--bind-port <port>]";

#[derive(Debug, Default, PartialEq)]
struct Args {
    peer: Option<IpAddr>,
    peer_port: Option<u16>,
    bind_port: Option<u16>,
}

/// Rejects anything it does not understand: a mistyped `--peer` must not
/// quietly fall back to the default RC addresses.
fn parse_args(raw: &[String]) -> Result<Args, String> {
    let mut args = Args::default();
    let mut rest = raw.iter();
    while let Some(flag) = rest.next() {
        if !matches!(flag.as_str(), "--peer" | "--peer-port" | "--bind-port") {
            return Err(format!("unknown argument: {flag}"));
        }
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let bad = || format!("{flag}: invalid value {value:?}");
        match flag.as_str() {
            "--peer" => args.peer = Some(value.parse().map_err(|_| bad())?),
            "--peer-port" => args.peer_port = Some(value.parse().map_err(|_| bad())?),
            _ => args.bind_port = Some(value.parse().map_err(|_| bad())?),
        }
    }
    Ok(args)
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

/// Makes this the only daemon for its socket: an exclusive lock on a file
/// beside it, held for the life of the process (the OS drops it however the
/// process ends). A second daemon, as when two front ends start one at the
/// same moment, finds it taken and leaves quietly with status 0.
fn claim_instance() -> File {
    let path = ds_proto::socket_path().with_extension("lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| {
            eprintln!("ds_daemon: cannot open {}: {e}", path.display());
            std::process::exit(1);
        });
    match file.try_lock() {
        Ok(()) => file,
        Err(TryLockError::WouldBlock) => {
            eprintln!(
                "ds_daemon already running for {}",
                ds_proto::socket_path().display()
            );
            std::process::exit(0);
        }
        Err(TryLockError::Error(e)) => {
            eprintln!("ds_daemon: cannot lock {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

/// Binds the daemon's socket. Only the lock holder gets here, so a file
/// already at the path was left by a daemon that died: replace it.
fn bind_socket() -> UnixListener {
    let path = ds_proto::socket_path();
    let _ = std::fs::remove_file(&path);
    UnixListener::bind(&path).unwrap_or_else(|e| {
        eprintln!("ds_daemon: cannot listen on {}: {e}", path.display());
        std::process::exit(1);
    })
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&raw).unwrap_or_else(|e| {
        eprintln!("ds_daemon: {e}\n{USAGE}");
        std::process::exit(2);
    });
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

    // Claim the instance and the socket before UDP: a second daemon leaves
    // quietly instead of failing to bind the RC port the first one holds.
    let _instance = claim_instance();
    let listener = bind_socket();

    let bind_port = config.bind_port;
    let (client, events) = match RobocolClient::start(config) {
        Ok(started) => started,
        Err(e) => {
            eprintln!(
                "ds_daemon: cannot bind UDP port {bind_port}: {e} (is Deck Station or another ds_daemon already running?)"
            );
            // Don't leave a socket file that ds_agent would mistake for a
            // live daemon.
            let _ = std::fs::remove_file(ds_proto::socket_path());
            std::process::exit(1);
        }
    };
    let daemon = Arc::new(Daemon::new(client, resolve_timeout()));

    {
        let daemon = daemon.clone();
        std::thread::spawn(move || {
            for event in events {
                if let Event::PeerBusy { peer } = &event {
                    eprintln!(
                        "ds_daemon: {peer} is connected to another driver station; paused until `connect`"
                    );
                }
                daemon
                    .state
                    .lock()
                    .expect("state mutex poisoned")
                    .apply_event(&event);
                daemon
                    .waiters
                    .lock()
                    .expect("waiters mutex poisoned")
                    .resolve(&event);
                if let Some(line) = watch::watch_json(&event) {
                    daemon
                        .subscribers
                        .lock()
                        .expect("subscribers mutex poisoned")
                        .broadcast(&line);
                }
            }
            // The robocol worker only drops its sender when it dies. A
            // daemon with no robot connection must not linger answering
            // `connected: true` from a frozen cache.
            eprintln!("ds_daemon: robocol client stopped; exiting");
            std::process::exit(1);
        });
    }

    for incoming in listener.incoming() {
        let Ok(stream) = incoming else { continue };
        let daemon = daemon.clone();
        std::thread::spawn(move || handle_connection(stream, daemon));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Result<Args, String> {
        parse_args(&words.iter().map(|w| w.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn flags_parse() {
        assert_eq!(parse(&[]), Ok(Args::default()));
        assert_eq!(
            parse(&[
                "--peer",
                "10.0.0.5",
                "--peer-port",
                "20900",
                "--bind-port",
                "0"
            ]),
            Ok(Args {
                peer: Some("10.0.0.5".parse().unwrap()),
                peer_port: Some(20900),
                bind_port: Some(0),
            })
        );
    }

    #[test]
    fn mistakes_are_refused_not_ignored() {
        assert!(parse(&["--peer", "10.0.0"]).is_err());
        assert!(parse(&["--peer"]).is_err());
        assert!(parse(&["--bind-port", "99999"]).is_err());
        assert!(parse(&["--verbose"]).is_err());
    }
}
