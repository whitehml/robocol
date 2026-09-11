//! Non-interactive CLI for `ds_agentd`. One invocation, one JSON request,
//! one reply on stdout — see `ds_cli` for the interactive REPL this mirrors.
//!
//! ```sh
//! ds_agent list
//! ds_agent init "Duo (TeleOp)"
//! ds_agent run
//! ds_agent gamepad --left-stick-y -0.5 --duration 300ms
//! ds_agent watch --types telemetry
//! ds_agent daemon status
//! DS_AGENTD_ARGS="--peer 192.168.43.1 --bind-port 0" ds_agent status   # extra ds_agentd flags on auto-spawn
//! ```

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

fn build_request(args: &[String]) -> Result<serde_json::Value, String> {
    let Some(cmd) = args.first() else {
        return Err("usage: ds_agent <command> [args]".into());
    };
    match cmd.as_str() {
        "list" => Ok(serde_json::json!({"cmd": "list"})),
        "init" => {
            let name = args.get(1).ok_or("usage: ds_agent init <name>")?;
            Ok(serde_json::json!({"cmd": "init", "name": name}))
        }
        "run" => Ok(serde_json::json!({"cmd": "run", "name": args.get(1)})),
        "stop" => Ok(serde_json::json!({"cmd": "stop"})),
        "restart" => Ok(serde_json::json!({"cmd": "restart"})),
        "active-config" => Ok(serde_json::json!({"cmd": "active-config"})),
        "configs" => Ok(serde_json::json!({"cmd": "configs"})),
        "config" => {
            let name = args.get(1).ok_or("usage: ds_agent config <name>")?;
            Ok(serde_json::json!({"cmd": "config", "name": name}))
        }
        "save-config" => {
            let json = args.get(1).ok_or("usage: ds_agent save-config <json>")?;
            Ok(serde_json::json!({"cmd": "save-config", "json": json}))
        }
        "activate-config" => {
            let name = args
                .get(1)
                .ok_or("usage: ds_agent activate-config <name>")?;
            Ok(serde_json::json!({"cmd": "activate-config", "name": name}))
        }
        "delete-config" => {
            let name = args.get(1).ok_or("usage: ds_agent delete-config <name>")?;
            Ok(serde_json::json!({"cmd": "delete-config", "name": name}))
        }
        "device-types" => Ok(serde_json::json!({"cmd": "device-types"})),
        "scan" => Ok(serde_json::json!({"cmd": "scan"})),
        "lynx-modules" => {
            let serial = args.get(1).ok_or("usage: ds_agent lynx-modules <serial>")?;
            Ok(serde_json::json!({"cmd": "lynx-modules", "serial": serial}))
        }
        "gamepad" => build_gamepad_request(&args[1..]),
        "status" => Ok(serde_json::json!({"cmd": "status"})),
        "watch" => build_watch_request(&args[1..]),
        other => Err(format!("unknown command: {other}")),
    }
}

const GAMEPAD_FLOAT_FLAGS: &[&str] = &[
    "left_stick_x",
    "left_stick_y",
    "right_stick_x",
    "right_stick_y",
    "left_trigger",
    "right_trigger",
];
const GAMEPAD_BOOL_FLAGS: &[&str] = &[
    "dpad_up",
    "dpad_down",
    "dpad_left",
    "dpad_right",
    "a",
    "b",
    "x",
    "y",
    "start",
    "back",
    "left_bumper",
    "right_bumper",
    "left_stick_button",
    "right_stick_button",
];

fn parse_duration_ms(value: &str) -> Result<u64, String> {
    if let Some(ms) = value.strip_suffix("ms") {
        ms.parse().map_err(|_| format!("invalid duration: {value}"))
    } else if let Some(s) = value.strip_suffix('s') {
        match s.parse::<f64>() {
            Ok(secs) if secs.is_finite() && secs >= 0.0 => Ok((secs * 1000.0) as u64),
            _ => Err(format!("invalid duration: {value}")),
        }
    } else {
        value
            .parse()
            .map_err(|_| format!("invalid duration: {value}"))
    }
}

fn build_gamepad_request(args: &[String]) -> Result<serde_json::Value, String> {
    let mut map = serde_json::Map::new();
    map.insert("cmd".into(), serde_json::json!("gamepad"));
    let mut i = 0;
    while i < args.len() {
        let flag = args[i]
            .strip_prefix("--")
            .ok_or_else(|| format!("expected --flag, got {}", args[i]))?;
        let key = flag.replace('-', "_");
        if key == "duration" {
            let value = args.get(i + 1).ok_or("missing value for --duration")?;
            map.insert(
                "duration_ms".into(),
                serde_json::json!(parse_duration_ms(value)?),
            );
            i += 2;
            continue;
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("missing value for --{flag}"))?;
        if GAMEPAD_FLOAT_FLAGS.contains(&key.as_str()) {
            let parsed: f32 = value
                .parse()
                .map_err(|_| format!("--{flag} expects a number"))?;
            map.insert(key, serde_json::json!(parsed));
        } else if GAMEPAD_BOOL_FLAGS.contains(&key.as_str()) {
            let parsed: bool = value
                .parse()
                .map_err(|_| format!("--{flag} expects true/false"))?;
            map.insert(key, serde_json::json!(parsed));
        } else if key == "duration_ms" {
            let parsed: u64 = value
                .parse()
                .map_err(|_| format!("--{flag} expects milliseconds"))?;
            map.insert(key, serde_json::json!(parsed));
        } else {
            return Err(format!("unknown gamepad flag: --{flag}"));
        }
        i += 2;
    }
    Ok(serde_json::Value::Object(map))
}

fn build_watch_request(args: &[String]) -> Result<serde_json::Value, String> {
    let mut types: Option<Vec<String>> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--types" => {
                let list = args
                    .get(i + 1)
                    .ok_or("usage: ds_agent watch [--types a,b,c]")?;
                types = Some(list.split(',').map(str::to_string).collect());
                i += 2;
            }
            other => return Err(format!("unknown watch flag: {other}")),
        }
    }
    Ok(serde_json::json!({"cmd": "watch", "types": types}))
}

/// Connects to the daemon, spawning it detached (its own process group) if
/// needed. A spawned daemon's stdout and stderr are redirected to a `.log`
/// file beside the socket, so a crash or panic during startup can be
/// diagnosed after the fact. If `DS_AGENTD_ARGS` is set, it is split on
/// whitespace and passed through as extra `ds_agentd` arguments.
fn connect_or_spawn() -> Result<UnixStream, String> {
    let path = ds_agent_ipc::socket_path();
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(stream);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let daemon_path = exe.with_file_name("ds_agentd");
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
    if let Ok(extra) = std::env::var("DS_AGENTD_ARGS") {
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
                "ds_agentd did not become ready in time; see {}",
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
    ds_agent_ipc::write_message(&mut stream, request).ok()?;
    let mut reader = BufReader::new(stream);
    let line = ds_agent_ipc::read_message(&mut reader).ok().flatten()?;
    serde_json::from_str(&line).ok()
}

fn run_daemon_subcommand(args: &[String]) -> i32 {
    let path = ds_agent_ipc::socket_path();
    match args.first().map(String::as_str) {
        Some("status") => match UnixStream::connect(&path) {
            Ok(stream) => match round_trip(stream, &serde_json::json!({"cmd": "status"})) {
                Some(mut status) => {
                    // `running` is always present: false when there is no
                    // socket to connect to, true when the daemon answered.
                    if let Some(obj) = status.as_object_mut() {
                        obj.insert("running".into(), serde_json::Value::Bool(true));
                    }
                    println!("{status}");
                    0
                }
                None => print_unreachable("ds_agentd accepted the connection but did not answer"),
            },
            Err(_) => {
                println!(r#"{{"ok":true,"running":false}}"#);
                0
            }
        },
        Some("stop") => match UnixStream::connect(&path) {
            Ok(stream) => match round_trip(stream, &serde_json::json!({"cmd": "shutdown"})) {
                Some(reply) if reply["ok"] == true => {
                    println!("{reply}");
                    0
                }
                _ => print_unreachable("ds_agentd did not acknowledge shutdown"),
            },
            Err(_) => {
                println!(r#"{{"ok":true,"running":false}}"#);
                0
            }
        },
        _ => {
            eprintln!("usage: ds_agent daemon <status|stop>");
            2
        }
    }
}

fn main() {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if raw_args.first().map(String::as_str) == Some("daemon") {
        std::process::exit(run_daemon_subcommand(&raw_args[1..]));
    }

    let request = match build_request(&raw_args) {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    let is_watch = request["cmd"] == "watch";

    let mut stream = match connect_or_spawn() {
        Ok(s) => s,
        Err(msg) => print_unreachable(&msg),
    };
    if ds_agent_ipc::write_message(&mut stream, &request).is_err() {
        print_unreachable("failed to write to ds_agentd");
    }

    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    if is_watch {
        while let Ok(Some(line)) = ds_agent_ipc::read_message(&mut reader) {
            println!("{line}");
        }
        return;
    }
    match ds_agent_ipc::read_message(&mut reader) {
        Ok(Some(line)) => {
            println!("{line}");
            let ok = serde_json::from_str::<serde_json::Value>(&line)
                .map(|v| v["ok"] == true)
                .unwrap_or(false);
            std::process::exit(if ok { 0 } else { 1 });
        }
        _ => {
            eprintln!("no response from ds_agentd");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_needs_no_arguments() {
        let req = build_request(&["list".to_string()]).unwrap();
        assert_eq!(req, serde_json::json!({"cmd": "list"}));
    }

    #[test]
    fn init_requires_a_name() {
        assert!(build_request(&["init".to_string()]).is_err());
        let req = build_request(&["init".to_string(), "Duo (TeleOp)".to_string()]).unwrap();
        assert_eq!(
            req,
            serde_json::json!({"cmd": "init", "name": "Duo (TeleOp)"})
        );
    }

    #[test]
    fn run_without_a_name_sends_null() {
        let req = build_request(&["run".to_string()]).unwrap();
        assert_eq!(req, serde_json::json!({"cmd": "run", "name": null}));
    }

    #[test]
    fn unknown_command_is_an_error() {
        assert!(build_request(&["not-a-command".to_string()]).is_err());
    }

    #[test]
    fn gamepad_parses_floats_bools_and_duration() {
        let args: Vec<String> = [
            "gamepad",
            "--left-stick-y",
            "-0.5",
            "--a",
            "true",
            "--duration",
            "300ms",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let req = build_request(&args).unwrap();
        assert_eq!(req["cmd"], "gamepad");
        assert_eq!(req["left_stick_y"], -0.5);
        assert_eq!(req["a"], true);
        assert_eq!(req["duration_ms"], 300);
    }

    #[test]
    fn duration_seconds_rejects_negative_and_non_finite() {
        assert_eq!(parse_duration_ms("1.5s"), Ok(1500));
        assert_eq!(parse_duration_ms("0s"), Ok(0));
        assert!(parse_duration_ms("-1s").is_err());
        assert!(parse_duration_ms("nans").is_err());
        assert!(parse_duration_ms("infs").is_err());
    }

    #[test]
    fn gamepad_rejects_unknown_flags() {
        let args: Vec<String> = ["gamepad", "--not-a-flag", "1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(build_request(&args).is_err());
    }

    #[test]
    fn watch_parses_comma_separated_types() {
        let args: Vec<String> = ["watch", "--types", "telemetry,disconnected"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let req = build_request(&args).unwrap();
        assert_eq!(
            req["types"],
            serde_json::json!(["telemetry", "disconnected"])
        );
    }

    #[test]
    fn watch_with_no_flags_sends_null_types() {
        let req = build_request(&["watch".to_string()]).unwrap();
        assert_eq!(req["types"], serde_json::Value::Null);
    }
}
