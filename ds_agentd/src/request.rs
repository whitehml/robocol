//! The daemon's JSON protocol: parsing one request off the wire and turning
//! it into a `robocol` client call plus a reply, using `DaemonState` for
//! instant answers (`status`) and name lookups, and `WaiterRegistry` to
//! correlate a request with the `Event` that answers it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use robocol::client::{Event, RobocolClient};
use robocol::cmd::parse_config_list;
use robocol::packets::Gamepad;
use serde::Deserialize;

use crate::state::DaemonState;
use crate::waiters::{WaiterKind, WaiterRegistry};

// Not yet constructed outside tests: Task 5's connection handling is what
// will call `serde_json::from_str::<Request>` on incoming socket lines.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    Ping,
    Status,
    List,
    Init {
        name: String,
    },
    Run {
        name: Option<String>,
    },
    Stop,
    Restart,
    ActiveConfig,
    Configs,
    Config {
        name: String,
    },
    SaveConfig {
        json: String,
    },
    ActivateConfig {
        name: String,
    },
    DeleteConfig {
        name: String,
    },
    DeviceTypes,
    Scan,
    LynxModules {
        serial: String,
    },
    Gamepad {
        #[serde(default)]
        left_stick_x: Option<f32>,
        #[serde(default)]
        left_stick_y: Option<f32>,
        #[serde(default)]
        right_stick_x: Option<f32>,
        #[serde(default)]
        right_stick_y: Option<f32>,
        #[serde(default)]
        left_trigger: Option<f32>,
        #[serde(default)]
        right_trigger: Option<f32>,
        #[serde(default)]
        dpad_up: Option<bool>,
        #[serde(default)]
        dpad_down: Option<bool>,
        #[serde(default)]
        dpad_left: Option<bool>,
        #[serde(default)]
        dpad_right: Option<bool>,
        #[serde(default)]
        a: Option<bool>,
        #[serde(default)]
        b: Option<bool>,
        #[serde(default)]
        x: Option<bool>,
        #[serde(default)]
        y: Option<bool>,
        #[serde(default)]
        start: Option<bool>,
        #[serde(default)]
        back: Option<bool>,
        #[serde(default)]
        left_bumper: Option<bool>,
        #[serde(default)]
        right_bumper: Option<bool>,
        #[serde(default)]
        left_stick_button: Option<bool>,
        #[serde(default)]
        right_stick_button: Option<bool>,
        #[serde(default)]
        duration_ms: Option<u64>,
    },
    Watch {
        #[serde(default)]
        types: Option<Vec<String>>,
    },
    Shutdown,
}

// The functions below are only reachable via `handle_request`, which is
// itself only wired up to a real socket by Task 5 — allow dead_code until
// then, as Tasks 2/3 did for `state`/`waiters`.
fn err(code: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({"ok": false, "error": code, "message": message.into()})
}

fn not_connected() -> serde_json::Value {
    err("not_connected", "robot controller not connected yet")
}

fn check_known_opmode(state: &Arc<Mutex<DaemonState>>, name: &str) -> Option<serde_json::Value> {
    let state = state.lock().expect("state mutex poisoned");
    if state.opmodes.is_empty() || state.find_opmode(name) {
        None
    } else {
        Some(err(
            "unknown_opmode",
            format!("no opmode named {name:?}; run `list` first"),
        ))
    }
}

/// Registers a waiter, fires `send` (which must issue exactly one
/// `robocol` request), and blocks up to `timeout` for the matching event.
/// Fails fast with `not_connected` rather than waiting out the timeout when
/// the daemon has never seen `Event::Connected`.
fn wait_for_event(
    waiters: &Arc<Mutex<WaiterRegistry>>,
    state: &Arc<Mutex<DaemonState>>,
    kind: WaiterKind,
    timeout: Duration,
    send: impl FnOnce(),
) -> Result<Event, serde_json::Value> {
    if !state.lock().expect("state mutex poisoned").connected {
        return Err(not_connected());
    }
    let rx = waiters
        .lock()
        .expect("waiters mutex poisoned")
        .register(kind);
    send();
    rx.recv_timeout(timeout)
        .map_err(|_| err("timeout", "robot controller did not respond in time"))
}

#[allow(dead_code, clippy::too_many_arguments)]
fn build_gamepad(
    left_stick_x: Option<f32>,
    left_stick_y: Option<f32>,
    right_stick_x: Option<f32>,
    right_stick_y: Option<f32>,
    left_trigger: Option<f32>,
    right_trigger: Option<f32>,
    dpad_up: Option<bool>,
    dpad_down: Option<bool>,
    dpad_left: Option<bool>,
    dpad_right: Option<bool>,
    a: Option<bool>,
    b: Option<bool>,
    x: Option<bool>,
    y: Option<bool>,
    start: Option<bool>,
    back: Option<bool>,
    left_bumper: Option<bool>,
    right_bumper: Option<bool>,
    left_stick_button: Option<bool>,
    right_stick_button: Option<bool>,
) -> Gamepad {
    Gamepad {
        left_stick_x: left_stick_x.unwrap_or(0.0),
        left_stick_y: left_stick_y.unwrap_or(0.0),
        right_stick_x: right_stick_x.unwrap_or(0.0),
        right_stick_y: right_stick_y.unwrap_or(0.0),
        left_trigger: left_trigger.unwrap_or(0.0),
        right_trigger: right_trigger.unwrap_or(0.0),
        dpad_up: dpad_up.unwrap_or(false),
        dpad_down: dpad_down.unwrap_or(false),
        dpad_left: dpad_left.unwrap_or(false),
        dpad_right: dpad_right.unwrap_or(false),
        a: a.unwrap_or(false),
        b: b.unwrap_or(false),
        x: x.unwrap_or(false),
        y: y.unwrap_or(false),
        start: start.unwrap_or(false),
        back: back.unwrap_or(false),
        left_bumper: left_bumper.unwrap_or(false),
        right_bumper: right_bumper.unwrap_or(false),
        left_stick_button: left_stick_button.unwrap_or(false),
        right_stick_button: right_stick_button.unwrap_or(false),
        ..Gamepad::default()
    }
}

pub fn handle_request(
    req: Request,
    client: &Arc<Mutex<RobocolClient>>,
    state: &Arc<Mutex<DaemonState>>,
    waiters: &Arc<Mutex<WaiterRegistry>>,
    timeout: Duration,
) -> serde_json::Value {
    match req {
        Request::Ping => serde_json::json!({"ok": true}),
        Request::Status => state.lock().expect("state mutex poisoned").to_json(),
        Request::List => {
            let client = client.clone();
            match wait_for_event(waiters, state, WaiterKind::OpModeList, timeout, move || {
                client
                    .lock()
                    .expect("client mutex poisoned")
                    .request_opmode_list();
            }) {
                Ok(Event::OpModeList(list)) => serde_json::json!({
                    "ok": true,
                    "opmodes": list.iter().map(|m| serde_json::json!({
                        "name": m.name, "flavor": m.flavor, "group": m.group,
                    })).collect::<Vec<_>>(),
                }),
                Ok(_) => {
                    unreachable!("WaiterKind::OpModeList only resolves with Event::OpModeList")
                }
                Err(e) => e,
            }
        }
        Request::Init { name } => {
            if let Some(e) = check_known_opmode(state, &name) {
                return e;
            }
            let client = client.clone();
            let send_name = name.clone();
            match wait_for_event(
                waiters,
                state,
                WaiterKind::OpModeInited,
                timeout,
                move || {
                    client
                        .lock()
                        .expect("client mutex poisoned")
                        .init_opmode(&send_name);
                },
            ) {
                Ok(Event::OpModeInited(n)) => serde_json::json!({"ok": true, "name": n}),
                Ok(_) => {
                    unreachable!("WaiterKind::OpModeInited only resolves with Event::OpModeInited")
                }
                Err(e) => e,
            }
        }
        Request::Run { name } => {
            let name = match name.or_else(|| {
                state
                    .lock()
                    .expect("state mutex poisoned")
                    .last_inited
                    .clone()
            }) {
                Some(n) => n,
                None => {
                    return err(
                        "bad_args",
                        "no opmode inited; call `init <name>` first or pass a name",
                    );
                }
            };
            if let Some(e) = check_known_opmode(state, &name) {
                return e;
            }
            let client = client.clone();
            let send_name = name.clone();
            match wait_for_event(
                waiters,
                state,
                WaiterKind::OpModeRunning,
                timeout,
                move || {
                    client
                        .lock()
                        .expect("client mutex poisoned")
                        .run_opmode(&send_name);
                },
            ) {
                Ok(Event::OpModeRunning(n)) => serde_json::json!({"ok": true, "name": n}),
                Ok(_) => unreachable!(
                    "WaiterKind::OpModeRunning only resolves with Event::OpModeRunning"
                ),
                Err(e) => e,
            }
        }
        Request::Stop => {
            client.lock().expect("client mutex poisoned").stop_opmode();
            serde_json::json!({"ok": true})
        }
        Request::Restart => {
            client
                .lock()
                .expect("client mutex poisoned")
                .restart_robot();
            serde_json::json!({"ok": true})
        }
        Request::ActiveConfig => {
            let client = client.clone();
            match wait_for_event(
                waiters,
                state,
                WaiterKind::ActiveConfiguration,
                timeout,
                move || {
                    client
                        .lock()
                        .expect("client mutex poisoned")
                        .request_active_config();
                },
            ) {
                Ok(Event::ActiveConfiguration(extra)) => {
                    serde_json::json!({"ok": true, "config": extra})
                }
                Ok(_) => unreachable!(
                    "WaiterKind::ActiveConfiguration only resolves with Event::ActiveConfiguration"
                ),
                Err(e) => e,
            }
        }
        Request::Configs => {
            let client = client.clone();
            match wait_for_event(
                waiters,
                state,
                WaiterKind::ConfigurationList,
                timeout,
                move || {
                    client
                        .lock()
                        .expect("client mutex poisoned")
                        .request_configurations();
                },
            ) {
                Ok(Event::ConfigurationList(extra)) => {
                    serde_json::json!({"ok": true, "configs": parse_config_list(&extra)})
                }
                Ok(_) => unreachable!(
                    "WaiterKind::ConfigurationList only resolves with Event::ConfigurationList"
                ),
                Err(e) => e,
            }
        }
        Request::Config { name } => {
            let Some(meta) = state
                .lock()
                .expect("state mutex poisoned")
                .find_config(&name)
            else {
                return err(
                    "unknown_config",
                    format!("unknown config {name:?}; run `configs` first"),
                );
            };
            let client = client.clone();
            match wait_for_event(
                waiters,
                state,
                WaiterKind::Configuration,
                timeout,
                move || {
                    client
                        .lock()
                        .expect("client mutex poisoned")
                        .request_particular_configuration(&meta);
                },
            ) {
                Ok(Event::Configuration(extra)) => serde_json::json!({"ok": true, "config": extra}),
                Ok(_) => unreachable!(
                    "WaiterKind::Configuration only resolves with Event::Configuration"
                ),
                Err(e) => e,
            }
        }
        Request::SaveConfig { json } => {
            client
                .lock()
                .expect("client mutex poisoned")
                .save_configuration(&json);
            serde_json::json!({"ok": true})
        }
        Request::ActivateConfig { name } => {
            let Some(meta) = state
                .lock()
                .expect("state mutex poisoned")
                .find_config(&name)
            else {
                return err(
                    "unknown_config",
                    format!("unknown config {name:?}; run `configs` first"),
                );
            };
            client
                .lock()
                .expect("client mutex poisoned")
                .activate_configuration(&meta);
            serde_json::json!({"ok": true})
        }
        Request::DeleteConfig { name } => {
            let Some(meta) = state
                .lock()
                .expect("state mutex poisoned")
                .find_config(&name)
            else {
                return err(
                    "unknown_config",
                    format!("unknown config {name:?}; run `configs` first"),
                );
            };
            client
                .lock()
                .expect("client mutex poisoned")
                .delete_configuration(&meta);
            serde_json::json!({"ok": true})
        }
        Request::DeviceTypes => {
            let client = client.clone();
            match wait_for_event(
                waiters,
                state,
                WaiterKind::UserDeviceList,
                timeout,
                move || {
                    client
                        .lock()
                        .expect("client mutex poisoned")
                        .request_user_device_types();
                },
            ) {
                Ok(Event::UserDeviceList(extra)) => {
                    serde_json::json!({"ok": true, "device_types": extra})
                }
                Ok(_) => unreachable!(
                    "WaiterKind::UserDeviceList only resolves with Event::UserDeviceList"
                ),
                Err(e) => e,
            }
        }
        Request::Scan => {
            let client = client.clone();
            match wait_for_event(waiters, state, WaiterKind::ScanResult, timeout, move || {
                client.lock().expect("client mutex poisoned").scan();
            }) {
                Ok(Event::ScanResult(extra)) => serde_json::json!({"ok": true, "scan": extra}),
                Ok(_) => {
                    unreachable!("WaiterKind::ScanResult only resolves with Event::ScanResult")
                }
                Err(e) => e,
            }
        }
        Request::LynxModules { serial } => {
            let client = client.clone();
            let send_serial = serial.clone();
            match wait_for_event(
                waiters,
                state,
                WaiterKind::LynxModules,
                timeout,
                move || {
                    client
                        .lock()
                        .expect("client mutex poisoned")
                        .discover_lynx_modules(&send_serial);
                },
            ) {
                Ok(Event::LynxModules(extra)) => {
                    serde_json::json!({"ok": true, "serial": serial, "modules": extra})
                }
                Ok(_) => {
                    unreachable!("WaiterKind::LynxModules only resolves with Event::LynxModules")
                }
                Err(e) => e,
            }
        }
        Request::Gamepad {
            left_stick_x,
            left_stick_y,
            right_stick_x,
            right_stick_y,
            left_trigger,
            right_trigger,
            dpad_up,
            dpad_down,
            dpad_left,
            dpad_right,
            a,
            b,
            x,
            y,
            start,
            back,
            left_bumper,
            right_bumper,
            left_stick_button,
            right_stick_button,
            duration_ms,
        } => {
            let gamepad = build_gamepad(
                left_stick_x,
                left_stick_y,
                right_stick_x,
                right_stick_y,
                left_trigger,
                right_trigger,
                dpad_up,
                dpad_down,
                dpad_left,
                dpad_right,
                a,
                b,
                x,
                y,
                start,
                back,
                left_bumper,
                right_bumper,
                left_stick_button,
                right_stick_button,
            );
            client
                .lock()
                .expect("client mutex poisoned")
                .send_gamepad(gamepad);
            if let Some(ms) = duration_ms {
                std::thread::sleep(Duration::from_millis(ms));
                client
                    .lock()
                    .expect("client mutex poisoned")
                    .send_gamepad(Gamepad::default());
            }
            serde_json::json!({"ok": true})
        }
        // Watch and Shutdown are handled by the connection layer (Task 5),
        // which needs the raw stream to push a live stream / exit the
        // process; they never reach this dispatcher.
        Request::Watch { .. } | Request::Shutdown => {
            unreachable!("Watch and Shutdown are intercepted before handle_request is called")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::mpsc;

    fn deps() -> (Arc<Mutex<DaemonState>>, Arc<Mutex<WaiterRegistry>>) {
        (
            Arc::new(Mutex::new(DaemonState::new())),
            Arc::new(Mutex::new(WaiterRegistry::new())),
        )
    }

    fn unreachable_client() -> Arc<Mutex<RobocolClient>> {
        // Binds a real socket but points at an address nothing answers, so
        // it never emits Event::Connected — exactly the "not connected" case.
        let config = robocol::ClientConfig {
            bind_port: 0,
            peer_addrs: vec!["127.0.0.1".parse().unwrap()],
            peer_port: 1, // nothing listens on port 1
            ..Default::default()
        };
        let (client, _events) = RobocolClient::start(config).expect("bind UDP socket");
        Arc::new(Mutex::new(client))
    }

    #[test]
    fn status_answers_without_touching_the_client_or_waiters() {
        let (state, waiters) = deps();
        let client = unreachable_client();
        let reply = handle_request(
            Request::Status,
            &client,
            &state,
            &waiters,
            Duration::from_millis(50),
        );
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["connected"], false);
    }

    #[test]
    fn list_fails_fast_with_not_connected() {
        let (state, waiters) = deps();
        let client = unreachable_client();
        let reply = handle_request(
            Request::List,
            &client,
            &state,
            &waiters,
            Duration::from_secs(5),
        );
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "not_connected");
    }

    #[test]
    fn init_rejects_unknown_opmode_without_waiting() {
        let (state, waiters) = deps();
        let peer: SocketAddr = "127.0.0.1:20884".parse().unwrap();
        state
            .lock()
            .unwrap()
            .apply_event(&Event::Connected { peer });
        state
            .lock()
            .unwrap()
            .apply_event(&Event::OpModeList(vec![robocol::cmd::OpModeMeta {
                name: "Duo (TeleOp)".into(),
                flavor: "TELEOP".into(),
                group: "drive".into(),
            }]));
        let client = unreachable_client();
        let reply = handle_request(
            Request::Init {
                name: "Nonexistent".into(),
            },
            &client,
            &state,
            &waiters,
            Duration::from_millis(50),
        );
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "unknown_opmode");
    }

    #[test]
    fn list_resolves_once_the_matching_event_arrives() {
        let (state, waiters) = deps();
        let peer: SocketAddr = "127.0.0.1:20884".parse().unwrap();
        state
            .lock()
            .unwrap()
            .apply_event(&Event::Connected { peer });
        let client = unreachable_client();

        // Simulate the robot's reply landing on the waiter registry shortly
        // after the request goes out, the way the real event-pump thread
        // would (Task 6 wires that thread up for real).
        let waiters_bg = waiters.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            ready_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            waiters_bg.lock().unwrap().resolve(&Event::OpModeList(vec![
                robocol::cmd::OpModeMeta {
                    name: "Solo (TeleOp)".into(),
                    flavor: "TELEOP".into(),
                    group: "drive".into(),
                },
            ]));
        });

        // handle_request's `send` closure runs before recv_timeout blocks,
        // so signal the background thread right as we call in.
        ready_tx.send(()).unwrap();
        let reply = handle_request(
            Request::List,
            &client,
            &state,
            &waiters,
            Duration::from_secs(1),
        );
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["opmodes"][0]["name"], "Solo (TeleOp)");
    }

    #[test]
    fn list_times_out_when_nothing_resolves_it() {
        let (state, waiters) = deps();
        let peer: SocketAddr = "127.0.0.1:20884".parse().unwrap();
        state
            .lock()
            .unwrap()
            .apply_event(&Event::Connected { peer });
        let client = unreachable_client();
        let reply = handle_request(
            Request::List,
            &client,
            &state,
            &waiters,
            Duration::from_millis(50),
        );
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "timeout");
    }
}
