//! Interactive test client for a real Control Hub.
//!
//! Join the Control Hub's WiFi AP, then:
//! ```sh
//! cargo run -p ds_cli            # tries 192.168.43.1 / 192.168.49.1
//! cargo run -p ds_cli 10.0.0.5   # explicit RC address
//! ```
//! Commands: `list`, `init <name>`, `run`, `stop`, `restart`, `quit`,
//! `active-config`, `configs`, `config <name>`, `save-config <json>`,
//! `activate-config <name>`, `delete-config <name>`, `device-types`,
//! `scan`, `lynx-modules <serial>`, `w` (pulse gamepad1 forward stick).
//!

use std::sync::{Arc, Mutex};
use std::time::Duration;

use robocol::client::{ClientConfig, Event, RobocolClient};
use robocol::cmd::{self, ConfigMeta};
use robocol::packets::Gamepad;
use rustyline::DefaultEditor;
use rustyline::ExternalPrinter;

const FORWARD_STICK: f32 = -0.5;
const PULSE: Duration = Duration::from_millis(300);

fn find_config(known: &Arc<Mutex<Vec<ConfigMeta>>>, name: &str) -> Option<ConfigMeta> {
    known
        .lock()
        .unwrap()
        .iter()
        .find(|c| c.name == name)
        .cloned()
}

fn main() {
    let mut config = ClientConfig::default();
    if let Some(addr) = std::env::args().nth(1) {
        config.peer_addrs = vec![addr.parse().expect("invalid IP address")];
    }

    let mut rl = DefaultEditor::new().expect("init line editor");
    let mut printer = rl
        .create_external_printer()
        .expect("create external printer");
    printer
        .print(format!(
            "robocol ds_cli — discovering RC at {:?}...\n",
            config.peer_addrs
        ))
        .ok();

    let (client, events) = RobocolClient::start(config).expect("bind UDP socket");

    // Configs are identified by (location, resourceId), not by name alone,
    // so `config <name>` / `activate-config <name>` / `delete-config <name>`
    // look up the full ConfigMeta here rather than sending the bare name.
    let known_configs: Arc<Mutex<Vec<ConfigMeta>>> = Arc::new(Mutex::new(Vec::new()));
    let known_configs_bg = known_configs.clone();

    std::thread::spawn(move || {
        let mut last_telemetry = String::new();
        for event in events {
            let line = match event {
                Event::Connected { peer } => format!("<< connected to {peer}"),
                Event::Disconnected => "<< disconnected, rediscovering...".to_string(),
                Event::RobotState(state) => format!("<< robot state: {state:?}"),
                Event::OpModeList(list) => {
                    let mut s = "<< opmodes:".to_string();
                    for m in list {
                        s.push_str(&format!("\n     {} [{} / {}]", m.name, m.flavor, m.group));
                    }
                    s
                }
                Event::OpModeInited(name) => format!("<< INIT: {name}"),
                Event::OpModeRunning(name) => format!("<< RUNNING: {name}"),
                Event::ActiveConfiguration(extra) => format!("<< active config: {extra}"),
                Event::ConfigurationList(extra) => {
                    *known_configs_bg.lock().unwrap() = cmd::parse_config_list(&extra);
                    format!("<< configs: {extra}")
                }
                Event::Configuration(extra) => format!("<< config: {extra}"),
                Event::UserDeviceList(extra) => format!("<< device types: {extra}"),
                Event::ScanResult(extra) => format!("<< scan: {extra}"),
                Event::LynxModules(extra) => format!("<< lynx modules: {extra}"),
                Event::Telemetry(t) => {
                    let strings: Vec<String> =
                        t.strings.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    let numbers: Vec<String> =
                        t.numbers.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    let line = format!(
                        "<< telemetry [{}] {} {}",
                        t.tag,
                        strings.join(" "),
                        numbers.join(" ")
                    );
                    if line == last_telemetry {
                        continue;
                    }
                    last_telemetry = line.clone();
                    line
                }
                Event::Stacktrace(trace) => format!("<< STACKTRACE:\n{trace}"),
                Event::Command { name, extra } => format!("<< command {name}: {extra}"),
                Event::CommandDropped { name } => format!("!! command {name} never acked"),
                Event::ProtocolError(e) => format!("!! protocol error: {e}"),
                Event::WebcamAvailable(available) => format!("<< webcam available: {available}"),
                Event::WebcamFrame(jpeg) => format!("<< webcam frame: {} bytes", jpeg.len()),
            };
            printer.print(format!("{line}\n")).ok();
        }
    });

    let mut last_inited = String::new();
    while let Ok(line) = rl.readline("> ") {
        rl.add_history_entry(line.as_str()).ok();
        let mut parts = line.trim().splitn(2, ' ');
        match (parts.next().unwrap_or(""), parts.next()) {
            ("list", _) => client.request_opmode_list(),
            ("init", Some(name)) => {
                last_inited = name.to_string();
                client.init_opmode(name);
            }
            ("run", name) => {
                let name = name.unwrap_or(&last_inited);
                if name.is_empty() {
                    println!("!! nothing inited; use: init <name>");
                } else {
                    client.run_opmode(name);
                }
            }
            ("stop", _) => client.stop_opmode(),
            ("restart", _) => client.restart_robot(),
            ("active-config", _) => client.request_active_config(),
            ("configs", _) => client.request_configurations(),
            ("config", Some(name)) => match find_config(&known_configs, name) {
                Some(meta) => client.request_particular_configuration(&meta),
                None => println!("!! unknown config {name:?}; run `configs` first"),
            },
            ("save-config", Some(json)) => client.save_configuration(json),
            ("activate-config", Some(name)) => match find_config(&known_configs, name) {
                Some(meta) => client.activate_configuration(&meta),
                None => println!("!! unknown config {name:?}; run `configs` first"),
            },
            ("delete-config", Some(name)) => match find_config(&known_configs, name) {
                Some(meta) => client.delete_configuration(&meta),
                None => println!("!! unknown config {name:?}; run `configs` first"),
            },
            ("config" | "save-config" | "activate-config" | "delete-config", None) => {
                println!("!! missing argument")
            }
            ("device-types", _) => client.request_user_device_types(),
            ("scan", _) => client.scan(),
            ("lynx-modules", Some(serial)) => client.discover_lynx_modules(serial),
            ("lynx-modules", None) => {
                println!("!! missing argument (LynxUsbDevice serial from `scan`)")
            }
            ("w", _) => {
                client.send_gamepad(Gamepad {
                    left_stick_y: FORWARD_STICK,
                    ..Default::default()
                });
                std::thread::sleep(PULSE);
                client.send_gamepad(Gamepad::default());
            }
            ("quit" | "exit", _) => break,
            ("", _) => {}
            (other, _) => println!("!! unknown command: {other}"),
        }
    }
}
