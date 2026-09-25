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

use std::io::Write;

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
    let out = Output { pretty };
    if words.is_empty() {
        out.usage_error(&usage());
    }
    let request = match parse(&words) {
        Ok(request) => request,
        Err(msg)
            if ds_proto::PARSE_TABLE
                .iter()
                .any(|verb| verb.name == words[0]) =>
        {
            out.usage_error(&msg)
        }
        Err(msg) => out.usage_error(&format!("{msg}\n{}", usage())),
    };
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
    /// Prints one reply or event. Fails only when stdout is gone (a closed
    /// pipe, as in `ds_agent watch | head`).
    fn print(self, value: &Value) -> std::io::Result<()> {
        let line = if self.pretty {
            render(value)
        } else {
            value.to_string()
        };
        writeln!(std::io::stdout().lock(), "{line}")
    }

    /// Reports a usage error as `bad_args` on stdout, like any other error,
    /// with the plain message on stderr, and exits 2.
    fn usage_error(self, message: &str) -> ! {
        let error = json!({"ok": false, "error": "bad_args", "message": message});
        let _ = self.print(&error);
        eprintln!("{message}");
        std::process::exit(2);
    }

    /// Reports `daemon_unreachable` on stdout for the caller, and as JSON on
    /// stderr too, so a script reading either stream sees it. Returns the
    /// exit status.
    fn unreachable(self, message: &str) -> i32 {
        let error = json!({"ok": false, "error": "daemon_unreachable", "message": message});
        let _ = self.print(&error);
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
            let _ = out.print(&reply);
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
            let _ = out.print(&json!({"ok": true, "running": false}));
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
            let _ = out.print(&reply);
            exit_code(&reply)
        }
        Err(e) => out.unreachable(&format!("ds_daemon did not answer: {e}")),
    }
}

/// Prints each event that passes `filter`. Ends with 0 when stdout is closed
/// (`ds_agent watch | head`), and with `daemon_unreachable` and 1 when the
/// daemon goes away.
fn run_watch(filter: &WatchFilter, out: Output) -> i32 {
    let mut conn = match transport::connect_or_spawn(Role::Agent, &[]) {
        Ok(conn) => conn,
        Err(msg) => return out.unreachable(&msg),
    };
    if let Err(e) = conn.send(&Request::Watch) {
        return out.unreachable(&format!("ds_daemon did not accept watch: {e}"));
    }
    loop {
        match conn.recv() {
            Ok(Some(event)) => {
                if let Some(shown) = filter.apply(&event)
                    && out.print(&shown).is_err()
                {
                    return 0;
                }
            }
            Ok(None) => return out.unreachable("ds_daemon closed the event stream"),
            Err(e) => return out.unreachable(&format!("lost the event stream: {e}")),
        }
    }
}
