//! Non-interactive front end for `ds_daemon`. One invocation, one JSON request,
//! one reply on stdout. Verbs and their arguments come from `ds_proto`'s
//! shared table, so anything `ds_cli` accepts, this accepts too.
//!
//! ```sh
//! ds_agent list
//! ds_agent init "Duo (TeleOp)"
//! ds_agent run "Duo (TeleOp)"
//! ds_agent gamepad --left-stick-y -0.5 --duration 300ms
//! ds_agent watch --types telemetry --tag TELEMETRY_DATA --key heading_deg
//! ds_agent ping       # is a daemon running? never starts one
//! ds_agent shutdown   # stop the daemon; never starts one
//! DS_DAEMON_ARGS="--peer 192.168.43.1 --bind-port 0" ds_agent status   # extra ds_daemon flags on auto-spawn
//! ```

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

use ds_proto::{Request, WatchFilter};

/// Connects to the daemon, spawning it detached (its own process group) if
/// needed. A spawned daemon's stdout and stderr are redirected to a `.log`
/// file beside the socket, so a crash or panic during startup can be
/// diagnosed after the fact. If `DS_DAEMON_ARGS` is set, it is split on
/// whitespace and passed through as extra `ds_daemon` arguments.
fn connect_or_spawn() -> Result<UnixStream, String> {
    let path = ds_proto::socket_path();
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(stream);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let daemon_path = exe.with_file_name("ds_daemon");
    let log_path = path.with_extension("log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("failed to open {}: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("failed to open {}: {e}", log_path.display()))?;
    let mut command = std::process::Command::new(&daemon_path);
    if let Ok(extra) = std::env::var("DS_DAEMON_ARGS") {
        command.args(extra.split_whitespace());
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err))
        .process_group(0)
        .spawn()
        .map_err(|e| {
            format!(
                "failed to start {}: {e}; see {}",
                daemon_path.display(),
                log_path.display()
            )
        })?;

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(stream) = UnixStream::connect(&path) {
            return Ok(stream);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "ds_daemon did not become ready in time; see {}",
                log_path.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Prints the `daemon_unreachable` error to stdout (for the caller's JSON
/// parser) and stderr (for a human), then exits 1.
fn print_unreachable(message: &str) -> ! {
    let line = serde_json::json!({"ok": false, "error": "daemon_unreachable", "message": message})
        .to_string();
    println!("{line}");
    eprintln!("{line}");
    std::process::exit(1);
}

/// Sends one request over an already-connected stream and reads one reply
/// line; `None` when the daemon closed without answering.
fn round_trip(mut stream: UnixStream, request: &serde_json::Value) -> Option<serde_json::Value> {
    ds_proto::write_message(&mut stream, request).ok()?;
    let mut reader = BufReader::new(stream);
    let line = ds_proto::read_message(&mut reader).ok().flatten()?;
    serde_json::from_str(&line).ok()
}

/// `ping` and `shutdown` are about the daemon itself, so they never start
/// one: with no daemon running they answer `{"ok":true,"running":false}`.
fn run_without_spawning(request: &serde_json::Value) -> i32 {
    let Ok(stream) = UnixStream::connect(ds_proto::socket_path()) else {
        println!(r#"{{"ok":true,"running":false}}"#);
        return 0;
    };
    match round_trip(stream, request) {
        Some(mut reply) if reply["ok"] == true => {
            if request["cmd"] == "ping"
                && let Some(obj) = reply.as_object_mut()
            {
                obj.insert("running".into(), serde_json::Value::Bool(true));
            }
            println!("{reply}");
            0
        }
        Some(reply) => {
            println!("{reply}");
            1
        }
        None => print_unreachable("ds_daemon accepted the connection but did not answer"),
    }
}

fn main() {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = raw_args.iter().map(String::as_str).collect();
    if words.is_empty() {
        let verbs: Vec<&str> = ds_proto::PARSE_TABLE.iter().map(|v| v.name).collect();
        eprintln!("usage: ds_agent <verb> [args]\nverbs: {}", verbs.join(", "));
        std::process::exit(2);
    }
    let request = match ds_proto::parse(&words) {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    let wire = serde_json::to_value(&request).expect("a Request always serializes");
    if matches!(request, Request::Ping | Request::Shutdown) {
        std::process::exit(run_without_spawning(&wire));
    }

    let mut stream = match connect_or_spawn() {
        Ok(s) => s,
        Err(msg) => print_unreachable(&msg),
    };
    if ds_proto::write_message(&mut stream, &wire).is_err() {
        print_unreachable("failed to write to ds_daemon");
    }

    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    if request == Request::Watch {
        let filter = WatchFilter::parse(&words[1..]).expect("parse() validated the watch flags");
        while let Ok(Some(line)) = ds_proto::read_message(&mut reader) {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if let Some(shown) = filter.apply(&event) {
                println!("{shown}");
            }
        }
        return;
    }
    match ds_proto::read_message(&mut reader) {
        Ok(Some(line)) => {
            println!("{line}");
            let ok = serde_json::from_str::<serde_json::Value>(&line)
                .map(|v| v["ok"] == true)
                .unwrap_or(false);
            std::process::exit(if ok { 0 } else { 1 });
        }
        _ => {
            eprintln!("no response from ds_daemon");
            std::process::exit(1);
        }
    }
}
