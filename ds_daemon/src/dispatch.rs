//! Turns one `ds_proto::Request` into `robocol` client calls and a JSON
//! reply, using `DaemonState` for instant answers (`status`) and name
//! lookups, and `WaiterRegistry` to correlate a request with the `Event` that
//! answers it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ds_proto::Request;
use robocol::client::{Event, RobocolClient};
use robocol::cmd::parse_config_list;
use robocol::packets::Gamepad;

use crate::gamepad::to_packet;
use crate::state::DaemonState;
use crate::waiters::{WaiterKind, WaiterPredicate, WaiterRegistry};

fn err(code: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({"ok": false, "error": code, "message": message.into()})
}

/// Every command except `ping`/`status` fails fast with `not_connected`
/// when the daemon has never seen `Event::Connected`, rather than waiting
/// out a timeout or silently dropping a packet that has no peer to go to.
fn ensure_connected(state: &Arc<Mutex<DaemonState>>) -> Result<(), serde_json::Value> {
    if state.lock().expect("state mutex poisoned").connected {
        Ok(())
    } else {
        Err(err("not_connected", "robot controller not connected yet"))
    }
}

/// Predicate for commands whose response carries nothing to match on.
fn any_event() -> WaiterPredicate {
    Box::new(|_| true)
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

/// Registers a waiter for `kind` events accepted by `pred`, fires `send`
/// (which must issue exactly one `robocol` request), and blocks up to
/// `timeout` for the matching event. Callers must have already passed
/// `ensure_connected`.
fn wait_for_event(
    waiters: &Arc<Mutex<WaiterRegistry>>,
    kind: WaiterKind,
    pred: WaiterPredicate,
    timeout: Duration,
    send: impl FnOnce(),
) -> Result<Event, serde_json::Value> {
    let (id, rx) = waiters
        .lock()
        .expect("waiters mutex poisoned")
        .register(kind, pred);
    send();
    rx.recv_timeout(timeout).map_err(|_| {
        // A payload-matched waiter (`init Foo`) would otherwise linger until
        // a matching event that may never arrive.
        waiters
            .lock()
            .expect("waiters mutex poisoned")
            .unregister(id);
        err("timeout", "robot controller did not respond in time")
    })
}

pub fn handle_request(
    req: Request,
    client: &Arc<Mutex<RobocolClient>>,
    state: &Arc<Mutex<DaemonState>>,
    waiters: &Arc<Mutex<WaiterRegistry>>,
    timeout: Duration,
) -> serde_json::Value {
    match &req {
        Request::Ping | Request::Status => {}
        _ => {
            if let Err(e) = ensure_connected(state) {
                return e;
            }
        }
    }
    match req {
        Request::Ping => serde_json::json!({"ok": true}),
        Request::Status => state.lock().expect("state mutex poisoned").to_json(),
        Request::List => {
            let client = client.clone();
            match wait_for_event(
                waiters,
                WaiterKind::OpModeList,
                any_event(),
                timeout,
                move || {
                    client
                        .lock()
                        .expect("client mutex poisoned")
                        .request_opmode_list();
                },
            ) {
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
            let want = name.clone();
            match wait_for_event(
                waiters,
                WaiterKind::OpModeInited,
                Box::new(move |e| matches!(e, Event::OpModeInited(n) if *n == want)),
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
            let want = name.clone();
            match wait_for_event(
                waiters,
                WaiterKind::OpModeRunning,
                Box::new(move |e| matches!(e, Event::OpModeRunning(n) if *n == want)),
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
                WaiterKind::ActiveConfiguration,
                any_event(),
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
                WaiterKind::ConfigurationList,
                any_event(),
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
                WaiterKind::Configuration,
                any_event(),
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
                WaiterKind::UserDeviceList,
                any_event(),
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
            match wait_for_event(
                waiters,
                WaiterKind::ScanResult,
                any_event(),
                timeout,
                move || {
                    client.lock().expect("client mutex poisoned").scan();
                },
            ) {
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
            // First-match only: see the `WaiterKind::LynxModules` doc.
            match wait_for_event(
                waiters,
                WaiterKind::LynxModules,
                any_event(),
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
        Request::Gamepad(input) => {
            let gamepad = to_packet(&input);
            client
                .lock()
                .expect("client mutex poisoned")
                .send_gamepad(gamepad);
            if let Some(ms) = input.duration_ms {
                std::thread::sleep(Duration::from_millis(ms));
                client
                    .lock()
                    .expect("client mutex poisoned")
                    .send_gamepad(Gamepad::default());
            }
            serde_json::json!({"ok": true})
        }
        // Watch and Shutdown are handled by the connection layer, which
        // needs the raw stream to push a live stream / exit the process;
        // they never reach this dispatcher.
        Request::Watch | Request::Shutdown => {
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
        // after the request goes out, the way the real event-pump thread in
        // `main.rs` does.
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
    fn gamepad_fails_fast_with_not_connected() {
        let (state, waiters) = deps();
        let client = unreachable_client();
        let reply = handle_request(
            Request::Gamepad(ds_proto::GamepadInput::default()),
            &client,
            &state,
            &waiters,
            Duration::from_millis(50),
        );
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "not_connected");
    }

    #[test]
    fn init_ignores_a_stale_stop_robot_inited_event() {
        // Mirrors `ds_agent stop; ds_agent init Foo`: the RC's late
        // `NOTIFY_INIT_OP_MODE $Stop$Robot$` lands while the `init Foo`
        // waiter is registered and must not resolve it.
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
                name: "Foo".into(),
                flavor: "TELEOP".into(),
                group: "drive".into(),
            }]));
        let client = unreachable_client();

        let waiters_bg = waiters.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            ready_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            waiters_bg
                .lock()
                .unwrap()
                .resolve(&Event::OpModeInited(robocol::cmd::DEFAULT_OP_MODE.into()));
            std::thread::sleep(Duration::from_millis(50));
            waiters_bg
                .lock()
                .unwrap()
                .resolve(&Event::OpModeInited("Foo".into()));
        });

        ready_tx.send(()).unwrap();
        let reply = handle_request(
            Request::Init { name: "Foo".into() },
            &client,
            &state,
            &waiters,
            Duration::from_secs(1),
        );
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["name"], "Foo");
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

    #[test]
    fn timed_out_init_waiter_is_unregistered() {
        let (state, waiters) = deps();
        let peer: SocketAddr = "127.0.0.1:20884".parse().unwrap();
        state
            .lock()
            .unwrap()
            .apply_event(&Event::Connected { peer });
        let client = unreachable_client();
        let reply = handle_request(
            Request::Init {
                name: "Never".into(),
            },
            &client,
            &state,
            &waiters,
            Duration::from_millis(50),
        );
        assert_eq!(reply["error"], "timeout");
        // A name-matched waiter would otherwise linger until an
        // `OpModeInited("Never")` that may never arrive.
        assert_eq!(waiters.lock().unwrap().len(), 0);
    }
}
