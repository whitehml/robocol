//! One-shot front end for `ds_daemon`, for scripts and AI agents: one
//! invocation, one request, one reply on stdout. Verbs and their arguments
//! come from `ds_proto`'s shared table, so anything `ds_cli` accepts, this
//! accepts too.
//!
//! ```sh
//! ds_agent list
//! ds_agent init "Duo (TeleOp)"
//! ds_agent run "Duo (TeleOp)"
//! ds_agent gamepad --left-stick-y -0.5 --duration 300ms
//! ds_agent watch --types telemetry --tag TELEMETRY_DATA --key heading_deg
//! ds_agent --pretty status   # text instead of JSON
//! ds_agent ping              # is a daemon running? never starts one
//! ds_agent shutdown          # stop the daemon; never starts one
//! DS_DAEMON_ARGS="--peer 192.168.43.1" ds_agent status   # flags for a daemon this starts
//! ```
//!
//! Exit status: 0 when the reply has `"ok": true`, 1 otherwise, 2 for a
//! usage error.

use ds_proto::{Request, Role, WatchFilter, parse, render, transport};
use serde_json::{Value, json};

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let pretty = raw.first().map(String::as_str) == Some("--pretty");
    let words: Vec<&str> = raw
        .iter()
        .skip(usize::from(pretty))
        .map(String::as_str)
        .collect();
    if words.is_empty() {
        eprintln!("{}", usage());
        std::process::exit(2);
    }
    let request = match parse(&words) {
        Ok(request) => request,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    let out = Output { pretty };
    let code = match request {
        Request::Ping | Request::Shutdown => run_without_spawning(&request, out),
        Request::Watch => {
            let filter =
                WatchFilter::parse(&words[1..]).expect("parse() validated the watch flags");
            run_watch(&filter, out)
        }
        _ => run_one(&request, out),
    };
    std::process::exit(code);
}

fn usage() -> String {
    let verbs: Vec<&str> = ds_proto::PARSE_TABLE.iter().map(|v| v.name).collect();
    format!(
        "usage: ds_agent [--pretty] <verb> [args]\nverbs: {}",
        verbs.join(", ")
    )
}

#[derive(Clone, Copy)]
struct Output {
    pretty: bool,
}

impl Output {
    fn print(self, value: &Value) {
        if self.pretty {
            println!("{}", render(value));
        } else {
            println!("{value}");
        }
    }

    /// Reports `daemon_unreachable` on stdout for the caller, and as JSON on
    /// stderr too, so a script reading either stream sees it. Returns the
    /// exit status.
    fn unreachable(self, message: &str) -> i32 {
        let error = json!({"ok": false, "error": "daemon_unreachable", "message": message});
        self.print(&error);
        eprintln!("{error}");
        1
    }
}

fn exit_code(reply: &Value) -> i32 {
    if reply["ok"] == true { 0 } else { 1 }
}

fn run_one(request: &Request, out: Output) -> i32 {
    let mut conn = match transport::connect_or_spawn(Role::Agent, &[]) {
        Ok(conn) => conn,
        Err(msg) => return out.unreachable(&msg),
    };
    match conn.request(request) {
        Ok(reply) => {
            out.print(&reply);
            exit_code(&reply)
        }
        Err(e) => out.unreachable(&format!("ds_daemon did not answer: {e}")),
    }
}

/// `ping` and `shutdown` are about the daemon itself, so they never start
/// one: with none running they answer `{"ok":true,"running":false}`.
fn run_without_spawning(request: &Request, out: Output) -> i32 {
    let mut conn = match transport::connect_existing(Role::Agent) {
        Ok(Some(conn)) => conn,
        Ok(None) => {
            out.print(&json!({"ok": true, "running": false}));
            return 0;
        }
        Err(msg) => return out.unreachable(&msg),
    };
    match conn.request(request) {
        Ok(mut reply) => {
            if *request == Request::Ping
                && let Some(fields) = reply.as_object_mut()
            {
                fields.insert("running".into(), Value::Bool(true));
            }
            out.print(&reply);
            exit_code(&reply)
        }
        Err(e) => out.unreachable(&format!("ds_daemon did not answer: {e}")),
    }
}

/// Prints each event that passes `filter` until the daemon goes away.
fn run_watch(filter: &WatchFilter, out: Output) -> i32 {
    let mut conn = match transport::connect_or_spawn(Role::Agent, &[]) {
        Ok(conn) => conn,
        Err(msg) => return out.unreachable(&msg),
    };
    if let Err(e) = conn.send(&Request::Watch) {
        return out.unreachable(&format!("ds_daemon did not accept watch: {e}"));
    }
    while let Ok(Some(event)) = conn.recv() {
        if let Some(shown) = filter.apply(&event) {
            out.print(&shown);
        }
    }
    0
}
