//! Decodes Robocol traffic out of a pcap using the robocol crate's own
//! parsers, so a capture can be read as packets rather than as packet sizes.
//!
//! The end-of-run report is the point of the tool: it names every Command the
//! crate does not already know, every OpMode field we would silently drop, and
//! the peer versions each side advertised.

mod net;
mod pcap;
mod report;

use std::collections::BTreeSet;
use std::process::ExitCode;

use robocol::packets::Packet;

use report::Report;

const DEFAULT_PORT: u16 = 20884;
const DEFAULT_MAX_EXTRA: usize = 300;

struct Args {
    path: String,
    port: u16,
    max_extra: usize,
    all: bool,
    quiet: bool,
}

fn usage() -> &'static str {
    "usage: pcap_decode <capture.pcap> [--port N] [--max-extra N] [--all] [--quiet]

  --port N        Robocol UDP port (default 20884). 0 = no port filter: keep
                  every UDP datagram whose payload parses as a Robocol packet,
                  for captures on ephemeral ports.
  --max-extra N   truncate Command payloads at N chars (default 300; 0 = never).
                  OpMode lists and unrecognized commands are never truncated.
  --all           also print heartbeats, gamepads, keep-alives, telemetry and
                  webcam frame chunks (suppressed by default as high-rate noise)
  --quiet         print only the end-of-run report"
}

fn parse_args() -> Result<Args, String> {
    let mut path = None;
    let mut port = DEFAULT_PORT;
    let mut max_extra = DEFAULT_MAX_EXTRA;
    let mut all = false;
    let mut quiet = false;

    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--all" => all = true,
            "--quiet" => quiet = true,
            "--help" | "-h" => return Err(usage().to_string()),
            "--port" => {
                let v = argv.next().ok_or("--port needs a value")?;
                port = v.parse().map_err(|_| format!("bad port: {v}"))?;
            }
            "--max-extra" => {
                let v = argv.next().ok_or("--max-extra needs a value")?;
                max_extra = v.parse().map_err(|_| format!("bad --max-extra: {v}"))?;
            }
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
            other => path = Some(other.to_string()),
        }
    }
    Ok(Args {
        path: path.ok_or_else(|| usage().to_string())?,
        port,
        max_extra,
        all,
        quiet,
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };

    let bytes = match std::fs::read(&args.path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("error: cannot read {}: {err}", args.path);
            return ExitCode::FAILURE;
        }
    };
    let frames = match pcap::read(&bytes) {
        Ok(frames) => frames,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let (datagrams, skipped) = net::datagrams(&frames);
    let mut robocol: Vec<_> = datagrams
        .into_iter()
        .filter(|d| {
            if args.port == 0 {
                Packet::parse(&d.payload).is_ok()
            } else {
                d.sport == args.port || d.dport == args.port
            }
        })
        .collect();
    robocol.sort_by(|a, b| a.ts.total_cmp(&b.ts));

    if robocol.is_empty() {
        eprintln!(
            "warning: {} frames, no UDP/{} traffic. Wrong port, or the capture \
             was taken on an interface that does not carry this traffic.",
            frames.len(),
            args.port
        );
    }

    let rc = infer_rc(&robocol);
    let base = robocol.first().map(|d| d.ts).unwrap_or(0.0);
    let mut report = Report::new(rc.clone());

    for dg in &robocol {
        let parsed = Packet::parse(&dg.payload);
        report.observe(dg, &parsed);
        if args.quiet {
            continue;
        }
        if let Some(line) = report::format(dg, &parsed, base, &rc, args.max_extra, args.all) {
            println!("{line}");
        }
    }

    if !args.quiet {
        println!();
    }
    report.print(frames.len(), skipped);
    ExitCode::SUCCESS
}

/// Telemetry only ever flows RC -> DS, so its sender is the Robot Controller.
/// Falls back to senders of `CMD_NOTIFY_*`, then to no labelling at all. A set
/// rather than one endpoint so a capture spanning several sessions (each on its
/// own ephemeral port) still labels every one of them.
fn infer_rc(datagrams: &[net::Datagram]) -> BTreeSet<report::Endpoint> {
    let mut telemetry_senders = BTreeSet::new();
    let mut notify_senders = BTreeSet::new();
    for dg in datagrams {
        match Packet::parse(&dg.payload) {
            Ok(Packet::Telemetry(_)) => {
                telemetry_senders.insert((dg.src, dg.sport));
            }
            Ok(Packet::Command(c)) if c.name.starts_with("CMD_NOTIFY") => {
                notify_senders.insert((dg.src, dg.sport));
            }
            _ => {}
        }
    }
    if telemetry_senders.is_empty() {
        notify_senders
    } else {
        telemetry_senders
    }
}
