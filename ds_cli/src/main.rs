//! Interactive front end for `ds_daemon`, for a person at a terminal. Takes
//! the same verbs as `ds_agent` (both parse with `ds_proto`), shows replies
//! as text instead of JSON, and prints the daemon's live events as they
//! arrive.
//!
//! ```sh
//! cargo run -p ds_cli                      # starts ds_daemon if none is running
//! cargo run -p ds_cli -- --peer 10.0.0.5   # RC address, if this starts the daemon
//! cargo run -p ds_cli -- --json            # raw JSON lines instead of text
//! ```
//!
//! At the prompt: any verb (`help` lists them), `watch [--types ..] [--tag
//! ..] [--key ..]` to narrow the live events shown, and `quit`.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use ds_proto::{PARSE_TABLE, Request, Role, WatchFilter, parse, render, split_line, transport};
use rustyline::{DefaultEditor, ExternalPrinter};
use serde_json::{Value, json};

#[derive(Debug, PartialEq)]
struct Args {
    json: bool,
    peer: Option<String>,
}

fn parse_args(raw: &[String]) -> Result<Args, String> {
    let mut args = Args {
        json: false,
        peer: None,
    };
    let mut rest = raw.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--json" => args.json = true,
            "--peer" => {
                let ip = rest.next().ok_or("--peer needs an IP address")?;
                ip.parse::<IpAddr>()
                    .map_err(|_| format!("--peer: not an IP address: {ip}"))?;
                args.peer = Some(ip.clone());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

/// One reply or event, as JSON or as text.
fn format_line(value: &Value, json: bool) -> String {
    if json {
        value.to_string()
    } else {
        render(value)
    }
}

fn bad_args(message: impl Into<String>) -> Value {
    json!({"ok": false, "error": "bad_args", "message": message.into()})
}

fn unreachable(message: impl Into<String>) -> Value {
    json!({"ok": false, "error": "daemon_unreachable", "message": message.into()})
}

fn help() -> String {
    let mut lines = vec!["commands (the same as ds_agent's):".to_string()];
    lines.extend(PARSE_TABLE.iter().map(|verb| format!("  {}", verb.usage)));
    lines.push("at this prompt only:".to_string());
    lines.push(
        "  watch [--types a,b] [--tag t,...] [--key k,...]   narrow the live events shown; no flags shows all"
            .to_string(),
    );
    lines.push("  help".to_string());
    lines.push("  quit".to_string());
    lines.join("\n")
}

fn unreachable_exit(message: &str) -> ! {
    eprintln!(
        "{}",
        json!({"ok": false, "error": "daemon_unreachable", "message": message})
    );
    std::process::exit(1);
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&raw).unwrap_or_else(|e| {
        eprintln!("{e}\nusage: ds_cli [--json] [--peer <ip>]");
        std::process::exit(2);
    });
    let spawn_args: Vec<String> = match &args.peer {
        Some(ip) => vec!["--peer".to_string(), ip.clone()],
        None => Vec::new(),
    };

    // One connection for commands, one for the live event stream.
    let mut session = transport::connect_or_spawn(Role::Operator, &spawn_args)
        .unwrap_or_else(|e| unreachable_exit(&e));
    let mut events =
        transport::connect_or_spawn(Role::Operator, &[]).unwrap_or_else(|e| unreachable_exit(&e));
    if let Err(e) = events.send(&Request::Watch) {
        unreachable_exit(&e.to_string());
    }

    let mut editor = DefaultEditor::new().expect("start the line editor");
    // Live events print above the prompt; without a terminal (piped input)
    // there is no prompt to keep clear, so plain stdout is fine.
    let mut print_event: Box<dyn FnMut(String) + Send> = match editor.create_external_printer() {
        Ok(mut printer) => Box::new(move |line| {
            let _ = printer.print(format!("{line}\n"));
        }),
        Err(_) => Box::new(|line| println!("{line}")),
    };
    let filter = Arc::new(Mutex::new(WatchFilter::default()));
    {
        let filter = filter.clone();
        let json = args.json;
        std::thread::spawn(move || {
            let mut last_telemetry = String::new();
            loop {
                let Ok(Some(event)) = events.recv() else {
                    let lost = unreachable("lost the event stream from ds_daemon");
                    print_event(format_line(&lost, json));
                    return;
                };
                let shown = filter.lock().expect("filter mutex poisoned").apply(&event);
                let Some(shown) = shown else { continue };
                let line = format_line(&shown, json);
                // Telemetry repeats at 10 Hz; only show changes.
                if shown["event"] == "telemetry" {
                    if line == last_telemetry {
                        continue;
                    }
                    last_telemetry = line.clone();
                }
                print_event(line);
            }
        });
    }

    while let Ok(line) = editor.readline("> ") {
        let _ = editor.add_history_entry(line.as_str());
        let words = match split_line(&line) {
            Ok(words) => words,
            Err(e) => {
                println!("{}", format_line(&bad_args(e), args.json));
                continue;
            }
        };
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        match words.first().copied() {
            None => continue,
            Some("quit" | "exit") => break,
            Some("help") => {
                println!("{}", help());
                continue;
            }
            // Here `watch` narrows what this prompt shows; the stream itself
            // is already running.
            Some("watch") => {
                let reply = match WatchFilter::parse(&words[1..]) {
                    Ok(new_filter) => {
                        *filter.lock().expect("filter mutex poisoned") = new_filter;
                        json!({"ok": true})
                    }
                    Err(e) => bad_args(e),
                };
                println!("{}", format_line(&reply, args.json));
                continue;
            }
            Some(_) => {}
        }
        let request = match parse(&words) {
            Ok(request) => request,
            Err(e) => {
                let known = PARSE_TABLE.iter().any(|verb| verb.name == words[0]);
                let hint = if known { "" } else { " (`help` lists them)" };
                println!(
                    "{}",
                    format_line(&bad_args(format!("{e}{hint}")), args.json)
                );
                continue;
            }
        };
        match session.request(&request) {
            Ok(reply) => println!("{}", format_line(&reply, args.json)),
            Err(e) => {
                println!("{}", format_line(&unreachable(e.to_string()), args.json));
                std::process::exit(1);
            }
        }
        if request == Request::Shutdown {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Result<Args, String> {
        parse_args(&words.iter().map(|w| w.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn flags_parse() {
        assert_eq!(
            args(&[]),
            Ok(Args {
                json: false,
                peer: None
            })
        );
        assert_eq!(
            args(&["--json", "--peer", "10.0.0.5"]),
            Ok(Args {
                json: true,
                peer: Some("10.0.0.5".into())
            })
        );
        assert!(args(&["--peer"]).is_err());
        assert!(args(&["--peer", "10.0.0"]).is_err());
        assert!(args(&["10.0.0.5"]).is_err());
    }

    #[test]
    fn help_lists_every_verb() {
        let text = help();
        for verb in PARSE_TABLE {
            assert!(text.contains(verb.usage), "help is missing {}", verb.name);
        }
    }

    #[test]
    fn lines_are_text_unless_json_is_asked_for() {
        let reply = json!({"ok": false, "error": "bad_args", "message": "no"});
        assert_eq!(format_line(&reply, false), "!! bad_args: no");
        assert_eq!(format_line(&reply, true), reply.to_string());
    }
}
