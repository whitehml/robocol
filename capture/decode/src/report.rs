//! Per-packet formatting and the end-of-run report.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use robocol::cmd;
use robocol::packets::{Packet, PeerDiscovery};
use robocol::wire::Error as WireError;

use crate::net::Datagram;

pub type Endpoint = (IpAddr, u16);

/// Every Command name `robocol::cmd` declares. Anything outside this set is
/// new to us and is what a capture against a newer SDK exists to surface.
const KNOWN_COMMANDS: &[&str] = &[
    cmd::REQUEST_OP_MODE_LIST,
    cmd::INIT_OP_MODE,
    cmd::RUN_OP_MODE,
    cmd::REQUEST_ACTIVE_CONFIG,
    cmd::REQUEST_CONFIGURATIONS,
    cmd::REQUEST_PARTICULAR_CONFIGURATION,
    cmd::REQUEST_USER_DEVICE_TYPES,
    cmd::SAVE_CONFIGURATION,
    cmd::ACTIVATE_CONFIGURATION,
    cmd::DELETE_CONFIGURATION,
    cmd::RESTART_ROBOT,
    cmd::SCAN,
    cmd::DISCOVER_LYNX_MODULES,
    cmd::REQUEST_FRAME,
    cmd::NOTIFY_OP_MODE_LIST,
    cmd::NOTIFY_INIT_OP_MODE,
    cmd::NOTIFY_RUN_OP_MODE,
    cmd::NOTIFY_ACTIVE_CONFIGURATION,
    cmd::NOTIFY_USER_DEVICE_LIST,
    cmd::REQUEST_CONFIGURATIONS_RESP,
    cmd::REQUEST_PARTICULAR_CONFIGURATION_RESP,
    cmd::SCAN_RESP,
    cmd::DISCOVER_LYNX_MODULES_RESP,
    cmd::SHOW_STACKTRACE,
    cmd::STREAM_CHANGE,
    cmd::RECEIVE_FRAME_BEGIN,
    cmd::RECEIVE_FRAME_CHUNK,
];

/// The fields `cmd::OpModeMeta` keeps. Anything else in the RC's OpMode JSON
/// is silently dropped today.
const KNOWN_OPMODE_FIELDS: &[&str] = &["name", "flavor", "group"];

fn is_known(name: &str) -> bool {
    KNOWN_COMMANDS.contains(&name)
}

fn noisy(packet: &Packet) -> bool {
    match packet {
        Packet::Heartbeat(_) | Packet::Gamepad(_) | Packet::KeepAlive(_) | Packet::Telemetry(_) => {
            true
        }
        Packet::Command(c) => {
            c.name == cmd::RECEIVE_FRAME_CHUNK || c.name == cmd::RECEIVE_FRAME_BEGIN
        }
        Packet::PeerDiscovery(_) => false,
    }
}

/// Matched on the full endpoint, not the address: on loopback (fake_rc, the
/// mock harness) both peers share 127.0.0.1 and only the port tells them apart.
fn dir(dg: &Datagram, rc: &BTreeSet<Endpoint>) -> String {
    if rc.contains(&(dg.src, dg.sport)) {
        "RC->DS".into()
    } else if rc.contains(&(dg.dst, dg.dport)) {
        "DS->RC".into()
    } else {
        format!("{}:{}->{}:{}", dg.src, dg.sport, dg.dst, dg.dport)
    }
}

fn one_line(s: &str) -> String {
    s.replace('\n', "\\n").replace('\r', "")
}

fn clip(s: &str, max: usize) -> String {
    let s = one_line(s);
    if max == 0 || s.chars().count() <= max {
        return s;
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}... [{} chars total]", s.chars().count())
}

pub fn format(
    dg: &Datagram,
    parsed: &Result<Packet, WireError>,
    base: f64,
    rc: &BTreeSet<Endpoint>,
    max_extra: usize,
    all: bool,
) -> Option<String> {
    let t = dg.ts - base;
    let d = dir(dg, rc);
    let frag = if dg.fragmented { " [reassembled]" } else { "" };

    let packet = match parsed {
        Ok(packet) => packet,
        Err(err) => {
            let head: Vec<String> = dg
                .payload
                .iter()
                .take(24)
                .map(|b| format!("{b:02x}"))
                .collect();
            return Some(format!(
                "{t:8.3} {d}  !! undecodable ({err}), {} bytes: {}",
                dg.payload.len(),
                head.join(" ")
            ));
        }
    };
    if !all && noisy(packet) {
        return None;
    }

    let body = match packet {
        Packet::Command(c) => {
            let tag = if c.acknowledged {
                "ack"
            } else if is_known(&c.name) {
                "cmd"
            } else {
                "cmd NEW"
            };
            // Never hide the two payloads a capture is usually taken for.
            let limit = if c.name == cmd::NOTIFY_OP_MODE_LIST || !is_known(&c.name) {
                0
            } else {
                max_extra
            };
            if c.extra.is_empty() {
                format!("{tag} {} #{}", c.name, c.seq)
            } else {
                format!("{tag} {} #{}: {}", c.name, c.seq, clip(&c.extra, limit))
            }
        }
        Packet::PeerDiscovery(p) => format!(
            "peer-discovery robocol={} sdk={}.{} build={}/{} type={:?}",
            p.robocol_version,
            p.sdk_major_version,
            p.sdk_minor_version,
            p.sdk_build_month,
            p.sdk_build_year,
            p.peer_type
        ),
        Packet::Heartbeat(h) => format!("heartbeat #{} state={:?}", h.seq, h.robot_state),
        Packet::Telemetry(t) => format!(
            "telemetry #{} state={:?} tag={:?} {} strings {} numbers",
            t.seq,
            t.robot_state,
            t.tag,
            t.strings.len(),
            t.numbers.len()
        ),
        Packet::Gamepad(g) => format!("gamepad #{}", g.seq),
        Packet::KeepAlive(_) => "keep-alive".to_string(),
    };
    Some(format!("{t:8.3} {d}  {body}{frag}"))
}

#[derive(Default)]
struct OpModeFindings {
    lists: usize,
    flavors: BTreeSet<String>,
    groups: BTreeSet<String>,
    dropped_fields: BTreeSet<String>,
    json_entries: usize,
    parsed_entries: usize,
    unparsable: Vec<String>,
}

pub struct Report {
    rc: BTreeSet<Endpoint>,
    commands: BTreeMap<(String, String), usize>,
    peers: BTreeMap<String, (String, usize)>,
    opmodes: OpModeFindings,
    inited: BTreeSet<String>,
    ran: BTreeSet<String>,
    packets: usize,
    reassembled: usize,
    undecodable: BTreeMap<String, usize>,
}

impl Report {
    pub fn new(rc: BTreeSet<Endpoint>) -> Report {
        Report {
            rc,
            commands: BTreeMap::new(),
            peers: BTreeMap::new(),
            opmodes: OpModeFindings::default(),
            inited: BTreeSet::new(),
            ran: BTreeSet::new(),
            packets: 0,
            reassembled: 0,
            undecodable: BTreeMap::new(),
        }
    }

    pub fn observe(&mut self, dg: &Datagram, parsed: &Result<Packet, WireError>) {
        self.packets += 1;
        if dg.fragmented {
            self.reassembled += 1;
        }
        let packet = match parsed {
            Ok(packet) => packet,
            Err(err) => {
                *self.undecodable.entry(err.to_string()).or_default() += 1;
                return;
            }
        };
        match packet {
            Packet::Command(c) if !c.acknowledged => {
                *self
                    .commands
                    .entry((c.name.clone(), dir(dg, &self.rc)))
                    .or_default() += 1;
                match c.name.as_str() {
                    cmd::NOTIFY_OP_MODE_LIST => self.observe_opmode_list(&c.extra),
                    cmd::NOTIFY_INIT_OP_MODE => {
                        self.inited.insert(c.extra.clone());
                    }
                    cmd::NOTIFY_RUN_OP_MODE => {
                        self.ran.insert(c.extra.clone());
                    }
                    _ => {}
                }
            }
            Packet::PeerDiscovery(p) => {
                let key = format!(
                    "robocol={} sdk={}.{} build={}/{} type={:?}",
                    p.robocol_version,
                    p.sdk_major_version,
                    p.sdk_minor_version,
                    p.sdk_build_month,
                    p.sdk_build_year,
                    p.peer_type
                );
                let entry = self.peers.entry(key).or_insert((dir(dg, &self.rc), 0));
                entry.1 += 1;
            }
            _ => {}
        }
    }

    fn observe_opmode_list(&mut self, extra: &str) {
        let f = &mut self.opmodes;
        f.lists += 1;
        f.parsed_entries = f.parsed_entries.max(cmd::parse_opmode_list(extra).len());

        let Ok(value) = serde_json::from_str::<serde_json::Value>(extra) else {
            f.unparsable.push(clip(extra, 200));
            return;
        };
        let Some(items) = value.as_array() else {
            f.unparsable.push(clip(extra, 200));
            return;
        };
        f.json_entries = f.json_entries.max(items.len());
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            for (key, val) in obj {
                match key.as_str() {
                    "flavor" => {
                        f.flavors.insert(render(val));
                    }
                    "group" => {
                        f.groups.insert(render(val));
                    }
                    _ if !KNOWN_OPMODE_FIELDS.contains(&key.as_str()) => {
                        f.dropped_fields
                            .insert(format!("{key} (e.g. {})", render(val)));
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn print(&self, frames: usize, skipped: usize) {
        println!("=== capture ===");
        println!(
            "{frames} frames, {} Robocol packets ({} reassembled from IP fragments), \
             {skipped} non-IP/unsupported frames",
            self.packets, self.reassembled
        );
        if self.rc.is_empty() {
            println!(
                "Robot Controller NOT inferred (no telemetry seen) — directions shown as endpoints"
            );
        } else {
            let endpoints: Vec<String> = self
                .rc
                .iter()
                .map(|(ip, port)| format!("{ip}:{port}"))
                .collect();
            println!("Robot Controller endpoint(s): {}", endpoints.join(", "));
        }

        println!("\n=== peer discovery (advertised versions) ===");
        if self.peers.is_empty() {
            println!("  none seen");
        }
        for (key, (direction, count)) in &self.peers {
            println!("  {direction:<7} {key}  x{count}");
        }
        let ours = PeerDiscovery::default();
        println!(
            "  we advertise: robocol={} sdk={}.{} build={}/{}  <- peer_discovery.rs, hardcoded",
            ours.robocol_version,
            ours.sdk_major_version,
            ours.sdk_minor_version,
            ours.sdk_build_month,
            ours.sdk_build_year
        );

        println!("\n=== commands ===");
        let mut new_commands = BTreeSet::new();
        for ((name, direction), count) in &self.commands {
            let mark = if is_known(name) {
                " "
            } else {
                new_commands.insert(name.clone());
                "*"
            };
            println!("  {mark} {direction:<7} {name:<45} x{count}");
        }
        if self.commands.is_empty() {
            println!("  none seen");
        }
        if !new_commands.is_empty() {
            println!("\n  * NEW — not declared in robocol::cmd:");
            for name in &new_commands {
                println!("      {name}");
            }
        }

        println!("\n=== opmodes ===");
        let f = &self.opmodes;
        if f.lists == 0 {
            println!("  no CMD_NOTIFY_OP_MODE_LIST seen — did you run `opmodes` in ds_cli?");
        } else {
            println!(
                "  {} list(s); {} entries in JSON, {} survived cmd::parse_opmode_list",
                f.lists, f.json_entries, f.parsed_entries
            );
            println!("  flavors seen: {}", join(&f.flavors));
            println!("  groups seen:  {}", join(&f.groups));
            if !self.inited.is_empty() {
                println!("  inited: {}", join(&self.inited));
            }
            if !self.ran.is_empty() {
                println!("  ran:    {}", join(&self.ran));
            }
        }

        println!("\n=== action items ===");
        let mut actions = 0;
        let mut action = |msg: String| {
            actions += 1;
            println!("  {actions}. {msg}");
        };

        if f.lists > 0 && f.parsed_entries < f.json_entries {
            action(format!(
                "OpMode list PARSE LOSS: {} of {} entries dropped by cmd::parse_opmode_list \
                 (cmd.rs) — the JSON shape changed.",
                f.json_entries - f.parsed_entries,
                f.json_entries
            ));
        }
        if !f.unparsable.is_empty() {
            action(format!(
                "OpMode list payload is not a JSON array; raw head: {}",
                f.unparsable[0]
            ));
        }
        if !f.dropped_fields.is_empty() {
            action(format!(
                "OpMode JSON has fields OpModeMeta drops: {}",
                join(&f.dropped_fields)
            ));
        }
        let unknown_flavors: BTreeSet<_> = f
            .flavors
            .iter()
            .filter(|fl| !matches!(fl.as_str(), "\"TELEOP\"" | "\"AUTONOMOUS\"" | "\"SYSTEM\""))
            .collect();
        if !unknown_flavors.is_empty() {
            action(format!(
                "New OpMode flavor(s) {} — opmodes_page.gd:168 treats anything != AUTONOMOUS \
                 as TeleOp.",
                unknown_flavors
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !new_commands.is_empty() {
            action(format!(
                "{} command name(s) not in robocol::cmd — see the * rows above.",
                new_commands.len()
            ));
        }
        for (peer, (direction, _)) in &self.peers {
            if direction == "RC->DS" {
                let ours_str = format!(
                    "robocol={} sdk={}.{}",
                    ours.robocol_version, ours.sdk_major_version, ours.sdk_minor_version
                );
                if !peer.starts_with(&ours_str) {
                    action(format!(
                        "RC advertises {peer}; we advertise {ours_str} — consider bumping \
                         PeerDiscovery::default()."
                    ));
                }
            }
        }
        if !self.undecodable.is_empty() {
            let detail: Vec<String> = self
                .undecodable
                .iter()
                .map(|(err, n)| format!("{err} x{n}"))
                .collect();
            action(format!(
                "Packets the wire codecs rejected: {}",
                detail.join(", ")
            ));
        }
        if actions == 0 {
            println!("  none — nothing in this capture is unknown to the current crate.");
        }
    }
}

fn render(value: &serde_json::Value) -> String {
    value.to_string()
}

fn join(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        return "(none)".into();
    }
    set.iter().cloned().collect::<Vec<_>>().join(", ")
}
