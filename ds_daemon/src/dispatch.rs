//! Turns one `ds_proto::Request` into `robocol` client calls and a JSON
//! reply. `Daemon` is what every connection shares; `Session` is what
//! belongs to one connection. Nothing here knows about sockets, so any
//! transport can drive `handle_request`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use ds_proto::{Pad, Request, Role};
use robocol::client::{Event, RobocolClient};
use robocol::cmd::{ConfigMeta, parse_config_list};
use serde_json::{Value, json};

use crate::gamepad::{
    self, Arbiter, DEFAULT_PULSE_MS, MAX_PULSE_MS, OwnerChange, PulseEnd, Refusal, check_input,
    to_packet,
};
use crate::state::DaemonState;
use crate::waiters::{Subscribers, WAITER_KINDS, WaiterKind, WaiterPredicate, WaiterRegistry};

/// Everything a request can touch, shared by every connection.
pub struct Daemon {
    pub client: Mutex<RobocolClient>,
    pub state: Mutex<DaemonState>,
    pub waiters: Mutex<WaiterRegistry>,
    pub subscribers: Mutex<Subscribers>,
    pub gamepads: Mutex<Arbiter>,
    /// How many connections currently speak as the operator. Pads belong to
    /// a role, not a connection, so when the last operator leaves its pads
    /// are released; otherwise they would lock the agent out with nobody
    /// left to release them.
    operators: Mutex<usize>,
    /// One request in flight per kind of reply, by `WaiterKind as usize`.
    /// Most replies carry nothing that says which request they answer, so
    /// two `config` requests (say, the operator's and the agent's) in flight
    /// at once could each get the other's reply.
    turns: [Mutex<()>; WAITER_KINDS],
    /// How long a request waits for the RC's answer.
    pub timeout: Duration,
}

impl Daemon {
    pub fn new(client: RobocolClient, timeout: Duration) -> Daemon {
        Daemon {
            client: Mutex::new(client),
            state: Mutex::new(DaemonState::new()),
            waiters: Mutex::new(WaiterRegistry::new()),
            subscribers: Mutex::new(Subscribers::new()),
            gamepads: Mutex::new(Arbiter::default()),
            operators: Mutex::new(0),
            turns: std::array::from_fn(|_| Mutex::new(())),
            timeout,
        }
    }

    fn client(&self) -> MutexGuard<'_, RobocolClient> {
        self.client.lock().expect("client mutex poisoned")
    }

    fn state(&self) -> MutexGuard<'_, DaemonState> {
        self.state.lock().expect("state mutex poisoned")
    }

    fn waiters(&self) -> MutexGuard<'_, WaiterRegistry> {
        self.waiters.lock().expect("waiters mutex poisoned")
    }

    fn gamepads(&self) -> MutexGuard<'_, Arbiter> {
        self.gamepads.lock().expect("gamepads mutex poisoned")
    }

    /// Sends one gamepad packet. Called with the arbiter lock held, which is
    /// always taken before the client lock.
    fn send_gamepad(&self, packet: &robocol::packets::Gamepad) {
        self.client().send_gamepad(packet.clone());
    }

    /// Tells every watcher a pad changed hands. Called with the arbiter lock
    /// held, which is always taken before the subscribers lock.
    fn announce(&self, change: &OwnerChange) {
        self.subscribers
            .lock()
            .expect("subscribers mutex poisoned")
            .broadcast(&change.to_json());
    }

    fn operator_joined(&self) {
        *self.operators.lock().expect("operators mutex poisoned") += 1;
    }

    /// One operator connection is gone; if it was the last, give back the
    /// operator's pads (neutral burst and `gamepad_owner` included). The
    /// count stays locked until then, so an operator arriving meanwhile
    /// cannot have its fresh claim released.
    fn operator_left(&self) {
        let mut operators = self.operators.lock().expect("operators mutex poisoned");
        *operators = operators.saturating_sub(1);
        if *operators == 0 {
            gamepad::release(
                &self.gamepads,
                Role::Operator,
                &Pad::ALL,
                |change| self.announce(change),
                |packet| self.send_gamepad(packet),
            );
        }
    }

    /// Stops the daemon for `shutdown`: returns every held pad to neutral,
    /// removes the socket, and exits. The arbiter lock is kept from here on,
    /// so no pulse can send another packet.
    pub fn shut_down(&self) -> ! {
        let arbiter = self.gamepads();
        let held: Vec<Pad> = Pad::ALL
            .into_iter()
            .filter(|pad| arbiter.owner(*pad).is_some())
            .collect();
        for _ in 0..gamepad::TAIL_PACKETS {
            for &pad in &held {
                self.send_gamepad(&gamepad::neutral(pad));
            }
            std::thread::sleep(Duration::from_millis(gamepad::PULSE_PERIOD_MS));
        }
        let _ = std::fs::remove_file(ds_proto::socket_path());
        // robocol sends from its own thread: give it a moment to put the
        // last packets on the wire.
        std::thread::sleep(Duration::from_millis(50));
        std::process::exit(0);
    }

    /// Called by the connection layer once a session is over: its client
    /// hung up, or the connection became a `watch` stream.
    pub fn end_session(&self, session: &Session) {
        if session.role == Role::Operator {
            self.operator_left();
        }
    }
}

/// What belongs to one connection.
pub struct Session {
    /// Who is on the other end; set by `hello`, `Agent` until then.
    pub role: Role,
    /// The opmode this connection last inited, for a bare `run`.
    pub last_inited: Option<String>,
    /// Set by the connection layer when the client goes away, so a pulse it
    /// asked for stops early instead of driving on for nobody.
    pub hung_up: Arc<AtomicBool>,
}

impl Session {
    pub fn new() -> Session {
        Session {
            role: Role::Agent,
            last_inited: None,
            hung_up: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_hung_up(&self) -> bool {
        self.hung_up.load(Ordering::Relaxed)
    }
}

fn err(code: &str, message: impl Into<String>) -> Value {
    json!({"ok": false, "error": code, "message": message.into()})
}

/// Requests that talk to the robot. The rest are answered by the daemon
/// alone and work with no RC connected.
fn needs_rc(request: &Request) -> bool {
    !matches!(
        request,
        Request::Hello { .. }
            | Request::Ping
            | Request::Status
            | Request::Claim { .. }
            | Request::Release { .. }
            | Request::Connect
            | Request::Disconnect
            | Request::Watch
            | Request::Shutdown
    )
}

/// Fails fast before any request that needs the RC, rather than waiting out
/// a timeout or silently dropping a packet that has no peer to go to:
/// `peer_busy` when the RC is owned by another driver station, else
/// `not_connected`.
fn ensure_connected(daemon: &Daemon) -> Result<(), Value> {
    let state = daemon.state();
    if state.connected {
        return Ok(());
    }
    Err(match &state.peer_busy {
        Some(peer) => err(
            "peer_busy",
            format!("{peer} is connected to another driver station; close it, then run `connect`"),
        ),
        None if state.paused => err(
            "not_connected",
            "disconnected by `disconnect`; run `connect` to reconnect",
        ),
        None => err("not_connected", "robot controller not connected yet"),
    })
}

/// Predicate for requests whose answer carries nothing to match on.
fn any_event() -> WaiterPredicate {
    Box::new(|_| true)
}

/// A waiter only ever resolves with an event of its own kind.
fn wrong_kind(event: Event) -> ! {
    unreachable!("waiter resolved with an event of another kind: {event:?}")
}

fn check_known_opmode(daemon: &Daemon, name: &str) -> Result<(), Value> {
    let state = daemon.state();
    if state.opmodes.is_empty() || state.find_opmode(name) {
        Ok(())
    } else {
        Err(err(
            "unknown_opmode",
            format!("no opmode named {name:?}; run `list` first"),
        ))
    }
}

fn known_config(daemon: &Daemon, name: &str) -> Result<ConfigMeta, Value> {
    daemon.state().find_config(name).ok_or_else(|| {
        err(
            "unknown_config",
            format!("unknown config {name:?}; run `configs` first"),
        )
    })
}

/// Waits for this kind of request's turn, registers a waiter for `kind`
/// events accepted by `pred`, runs `send` (which must issue exactly one
/// `robocol` request), and blocks up to the daemon's timeout for the
/// matching event. Only the turn is held while waiting: it serializes
/// requests of one kind and nothing else.
fn wait_for_event(
    daemon: &Daemon,
    kind: WaiterKind,
    pred: WaiterPredicate,
    send: impl FnOnce(&RobocolClient),
) -> Result<Event, Value> {
    let _turn = daemon.turns[kind as usize]
        .lock()
        .expect("turn mutex poisoned");
    let (id, rx) = daemon.waiters().register(kind, pred);
    send(&daemon.client());
    rx.recv_timeout(daemon.timeout).map_err(|_| {
        // A payload-matched waiter (`init Foo`) would otherwise linger until
        // a matching event that may never arrive.
        daemon.waiters().unregister(id);
        err("timeout", "robot controller did not respond in time")
    })
}

pub fn handle_request(request: Request, session: &mut Session, daemon: &Daemon) -> Value {
    if needs_rc(&request)
        && let Err(e) = ensure_connected(daemon)
    {
        return e;
    }
    match request {
        Request::Hello { role } => {
            match (session.role, role) {
                (Role::Agent, Role::Operator) => daemon.operator_joined(),
                (Role::Operator, Role::Agent) => daemon.operator_left(),
                _ => {}
            }
            session.role = role;
            json!({"ok": true, "role": role})
        }
        Request::Ping => json!({"ok": true}),
        Request::Status => {
            let mut status = daemon.state().to_json();
            status["gamepads"] = daemon.gamepads().to_json();
            status
        }
        Request::List => match wait_for_event(daemon, WaiterKind::OpModeList, any_event(), |c| {
            c.request_opmode_list()
        }) {
            Ok(Event::OpModeList(list)) => json!({
                "ok": true,
                "opmodes": list.iter().map(|m| json!({
                    "name": m.name, "flavor": m.flavor, "group": m.group,
                })).collect::<Vec<_>>(),
            }),
            Ok(other) => wrong_kind(other),
            Err(e) => e,
        },
        Request::Init { name } => {
            if let Err(e) = check_known_opmode(daemon, &name) {
                return e;
            }
            let want = name.clone();
            match wait_for_event(
                daemon,
                WaiterKind::OpModeInited,
                Box::new(move |e| matches!(e, Event::OpModeInited(n) if *n == want)),
                |c| c.init_opmode(&name),
            ) {
                Ok(Event::OpModeInited(n)) => {
                    session.last_inited = Some(n.clone());
                    json!({"ok": true, "name": n})
                }
                Ok(other) => wrong_kind(other),
                Err(e) => e,
            }
        }
        Request::Run { name } => {
            let Some(name) = name.or_else(|| session.last_inited.clone()) else {
                return err(
                    "bad_args",
                    "nothing inited on this connection; use `run <name>`",
                );
            };
            if let Err(e) = check_known_opmode(daemon, &name) {
                return e;
            }
            let want = name.clone();
            match wait_for_event(
                daemon,
                WaiterKind::OpModeRunning,
                Box::new(move |e| matches!(e, Event::OpModeRunning(n) if *n == want)),
                |c| c.run_opmode(&name),
            ) {
                Ok(Event::OpModeRunning(n)) => json!({"ok": true, "name": n}),
                Ok(other) => wrong_kind(other),
                Err(e) => e,
            }
        }
        Request::Stop => {
            daemon.client().stop_opmode();
            json!({"ok": true})
        }
        Request::Restart => {
            daemon.client().restart_robot();
            json!({"ok": true})
        }
        Request::ActiveConfig => {
            match wait_for_event(daemon, WaiterKind::ActiveConfiguration, any_event(), |c| {
                c.request_active_config()
            }) {
                Ok(Event::ActiveConfiguration(extra)) => json!({"ok": true, "config": extra}),
                Ok(other) => wrong_kind(other),
                Err(e) => e,
            }
        }
        Request::Configs => {
            match wait_for_event(daemon, WaiterKind::ConfigurationList, any_event(), |c| {
                c.request_configurations()
            }) {
                Ok(Event::ConfigurationList(extra)) => {
                    json!({"ok": true, "configs": parse_config_list(&extra)})
                }
                Ok(other) => wrong_kind(other),
                Err(e) => e,
            }
        }
        Request::Config { name } => {
            let meta = match known_config(daemon, &name) {
                Ok(meta) => meta,
                Err(e) => return e,
            };
            match wait_for_event(daemon, WaiterKind::Configuration, any_event(), |c| {
                c.request_particular_configuration(&meta)
            }) {
                Ok(Event::Configuration(extra)) => json!({"ok": true, "config": extra}),
                Ok(other) => wrong_kind(other),
                Err(e) => e,
            }
        }
        Request::SaveConfig { json } => {
            daemon.client().save_configuration(&json);
            json!({"ok": true})
        }
        Request::ActivateConfig { name } => match known_config(daemon, &name) {
            Ok(meta) => {
                daemon.client().activate_configuration(&meta);
                json!({"ok": true})
            }
            Err(e) => e,
        },
        Request::DeleteConfig { name } => match known_config(daemon, &name) {
            Ok(meta) => {
                daemon.client().delete_configuration(&meta);
                json!({"ok": true})
            }
            Err(e) => e,
        },
        Request::DeviceTypes => {
            match wait_for_event(daemon, WaiterKind::UserDeviceList, any_event(), |c| {
                c.request_user_device_types()
            }) {
                Ok(Event::UserDeviceList(extra)) => json!({"ok": true, "device_types": extra}),
                Ok(other) => wrong_kind(other),
                Err(e) => e,
            }
        }
        Request::Scan => {
            match wait_for_event(daemon, WaiterKind::ScanResult, any_event(), |c| c.scan()) {
                Ok(Event::ScanResult(extra)) => json!({"ok": true, "scan": extra}),
                Ok(other) => wrong_kind(other),
                Err(e) => e,
            }
        }
        Request::LynxModules { serial } => {
            // First-match only: see the `WaiterKind::LynxModules` doc.
            match wait_for_event(daemon, WaiterKind::LynxModules, any_event(), |c| {
                c.discover_lynx_modules(&serial)
            }) {
                Ok(Event::LynxModules(extra)) => {
                    json!({"ok": true, "serial": serial, "modules": extra})
                }
                Ok(other) => wrong_kind(other),
                Err(e) => e,
            }
        }
        Request::Connect => {
            {
                let mut state = daemon.state();
                state.peer_busy = None;
                state.paused = false;
            }
            daemon.client().connect();
            json!({"ok": true})
        }
        Request::Disconnect => {
            daemon.state().paused = true;
            daemon.client().disconnect();
            json!({"ok": true})
        }
        Request::Claim { pads } => {
            if pads.is_empty() {
                return err("bad_args", "claim needs gamepad1, gamepad2 or both");
            }
            match gamepad::claim(
                &daemon.gamepads,
                session.role,
                &pads,
                |change| daemon.announce(change),
                |packet| daemon.send_gamepad(packet),
            ) {
                Ok(_) => json!({"ok": true, "held": daemon.gamepads().held_by(session.role)}),
                Err(refusal) => refusal.to_json(),
            }
        }
        Request::Release { pads } => {
            let pads = pads.unwrap_or_else(|| Pad::ALL.to_vec());
            let changes = gamepad::release(
                &daemon.gamepads,
                session.role,
                &pads,
                |change| daemon.announce(change),
                |packet| daemon.send_gamepad(packet),
            );
            let released: Vec<Pad> = changes.iter().map(|c| c.pad).collect();
            json!({"ok": true, "released": released})
        }
        Request::Gamepad(input) => {
            let user = input.user.unwrap_or(1);
            let Some(pad) = Pad::from_user(user) else {
                return err("bad_args", format!("user must be 1 or 2, got {user}"));
            };
            let duration_ms = input.duration_ms.unwrap_or(DEFAULT_PULSE_MS);
            if duration_ms > MAX_PULSE_MS {
                return err(
                    "bad_args",
                    format!("a pulse can last at most {MAX_PULSE_MS} ms, not {duration_ms}"),
                );
            }
            if let Err(message) = check_input(&input) {
                return err("bad_args", message);
            }
            let packet = to_packet(&input, pad);
            match gamepad::pulse(
                &daemon.gamepads,
                session.role,
                pad,
                &packet,
                duration_ms,
                || session.is_hung_up(),
                |g| daemon.send_gamepad(g),
            ) {
                Err(refusal) => refusal.to_json(),
                Ok(PulseEnd::Completed { packets }) => {
                    json!({"ok": true, "user": user, "packets": packets})
                }
                Ok(PulseEnd::Superseded { packets }) => {
                    json!({"ok": true, "user": user, "packets": packets, "superseded": true})
                }
                // Nobody is left to read this reply; it only keeps the match whole.
                Ok(PulseEnd::Abandoned { packets }) => {
                    json!({"ok": true, "user": user, "packets": packets, "abandoned": true})
                }
                Ok(PulseEnd::Lost {
                    owner: Some(Role::Operator),
                    ..
                }) if session.role == Role::Agent => Refusal::Preempted(pad).to_json(),
                Ok(PulseEnd::Lost { .. }) => err(
                    "not_claimed",
                    format!("{} was released while the pulse ran", pad.name()),
                ),
            }
        }
        // The connection layer handles these: `watch` takes over the stream
        // and `shutdown` exits the process, so neither reaches dispatch.
        Request::Watch | Request::Shutdown => {
            unreachable!("watch and shutdown are handled by the connection layer")
        }
    }
}

#[cfg(test)]
pub(crate) fn test_daemon(timeout: Duration) -> std::sync::Arc<Daemon> {
    // A real UDP socket pointed at a port nothing answers, so the daemon
    // never sees `Event::Connected` unless a test applies one.
    let config = robocol::ClientConfig {
        bind_port: 0,
        peer_addrs: vec!["127.0.0.1".parse().unwrap()],
        peer_port: 1,
        ..Default::default()
    };
    let (client, _events) = RobocolClient::start(config).expect("bind UDP socket");
    std::sync::Arc::new(Daemon::new(client, timeout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    fn connected(daemon: &Daemon) {
        let peer = "127.0.0.1:20884".parse().unwrap();
        daemon.state().apply_event(&Event::Connected { peer });
    }

    fn with_opmodes(daemon: &Daemon, names: &[&str]) {
        let list = names
            .iter()
            .map(|n| robocol::cmd::OpModeMeta {
                name: n.to_string(),
                flavor: "TELEOP".into(),
                group: "drive".into(),
            })
            .collect();
        daemon.state().apply_event(&Event::OpModeList(list));
    }

    /// Plays the RC from another thread: waits for a request to register its
    /// waiter, then resolves `events` in order, 20 ms apart.
    fn answer(daemon: &Arc<Daemon>, events: Vec<Event>) {
        let daemon = daemon.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while daemon.waiters().len() == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            for event in events {
                daemon.waiters().resolve(&event);
                std::thread::sleep(Duration::from_millis(20));
            }
        });
    }

    #[test]
    fn status_answers_without_an_rc() {
        let daemon = test_daemon(Duration::from_millis(50));
        let reply = handle_request(Request::Status, &mut Session::new(), &daemon);
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["connected"], false);
    }

    #[test]
    fn hello_sets_the_session_role() {
        let daemon = test_daemon(Duration::from_millis(50));
        let mut session = Session::new();
        assert_eq!(session.role, Role::Agent);
        let reply = handle_request(
            Request::Hello {
                role: Role::Operator,
            },
            &mut session,
            &daemon,
        );
        assert_eq!(reply, json!({"ok": true, "role": "operator"}));
        assert_eq!(session.role, Role::Operator);
    }

    #[test]
    fn a_busy_rc_fails_requests_with_peer_busy_until_connect() {
        let daemon = test_daemon(Duration::from_millis(50));
        let peer = "192.168.43.1:20884".parse().unwrap();
        daemon.state().apply_event(&Event::PeerBusy { peer });

        let reply = handle_request(Request::List, &mut Session::new(), &daemon);
        assert_eq!(reply["error"], "peer_busy");
        let message = reply["message"].as_str().unwrap();
        assert!(message.contains("192.168.43.1:20884") && message.contains("connect"));

        let reply = handle_request(Request::Connect, &mut Session::new(), &daemon);
        assert_eq!(reply, json!({"ok": true}));
        assert_eq!(daemon.state().peer_busy, None);
        let reply = handle_request(Request::List, &mut Session::new(), &daemon);
        assert_eq!(reply["error"], "not_connected");
    }

    #[test]
    fn disconnect_needs_no_rc_and_says_how_to_undo_it() {
        let daemon = test_daemon(Duration::from_millis(50));
        let reply = handle_request(Request::Disconnect, &mut Session::new(), &daemon);
        assert_eq!(reply, json!({"ok": true}));
        let status = handle_request(Request::Status, &mut Session::new(), &daemon);
        assert_eq!(status["paused"], true);
        let list = handle_request(Request::List, &mut Session::new(), &daemon);
        assert!(
            list["message"].as_str().unwrap().contains("run `connect`"),
            "{list}"
        );

        handle_request(Request::Connect, &mut Session::new(), &daemon);
        let status = handle_request(Request::Status, &mut Session::new(), &daemon);
        assert_eq!(status["paused"], false);
    }

    #[test]
    fn list_fails_fast_with_not_connected() {
        let daemon = test_daemon(Duration::from_secs(5));
        let started = Instant::now();
        let reply = handle_request(Request::List, &mut Session::new(), &daemon);
        assert_eq!(reply["error"], "not_connected");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn gamepad_fails_fast_with_not_connected() {
        let daemon = test_daemon(Duration::from_millis(50));
        let reply = handle_request(
            Request::Gamepad(ds_proto::GamepadInput::default()),
            &mut Session::new(),
            &daemon,
        );
        assert_eq!(reply["error"], "not_connected");
    }

    #[test]
    fn init_rejects_unknown_opmode_without_waiting() {
        let daemon = test_daemon(Duration::from_millis(50));
        connected(&daemon);
        with_opmodes(&daemon, &["Duo (TeleOp)"]);
        let reply = handle_request(
            Request::Init {
                name: "Nonexistent".into(),
            },
            &mut Session::new(),
            &daemon,
        );
        assert_eq!(reply["error"], "unknown_opmode");
    }

    #[test]
    fn list_resolves_once_the_matching_event_arrives() {
        let daemon = test_daemon(Duration::from_secs(1));
        connected(&daemon);
        answer(
            &daemon,
            vec![Event::OpModeList(vec![robocol::cmd::OpModeMeta {
                name: "Solo (TeleOp)".into(),
                flavor: "TELEOP".into(),
                group: "drive".into(),
            }])],
        );
        let reply = handle_request(Request::List, &mut Session::new(), &daemon);
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["opmodes"][0]["name"], "Solo (TeleOp)");
    }

    #[test]
    fn init_ignores_a_stale_stop_robot_inited_event() {
        // Mirrors `stop` then `init Foo`: the RC's late
        // `NOTIFY_INIT_OP_MODE $Stop$Robot$` lands while the `init Foo`
        // waiter is registered and must not resolve it.
        let daemon = test_daemon(Duration::from_secs(1));
        connected(&daemon);
        with_opmodes(&daemon, &["Foo"]);
        answer(
            &daemon,
            vec![
                Event::OpModeInited(robocol::cmd::DEFAULT_OP_MODE.into()),
                Event::OpModeInited("Foo".into()),
            ],
        );
        let mut session = Session::new();
        let reply = handle_request(Request::Init { name: "Foo".into() }, &mut session, &daemon);
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["name"], "Foo");
        assert_eq!(session.last_inited.as_deref(), Some("Foo"));
    }

    #[test]
    fn bare_run_ignores_another_connections_init() {
        // Two clients: the first inits Foo; the second's bare `run` must not
        // pick that up (review item 4).
        let daemon = test_daemon(Duration::from_secs(1));
        connected(&daemon);
        with_opmodes(&daemon, &["Foo"]);
        answer(&daemon, vec![Event::OpModeInited("Foo".into())]);
        let mut first = Session::new();
        let inited = handle_request(Request::Init { name: "Foo".into() }, &mut first, &daemon);
        assert_eq!(inited["ok"], true);

        let reply = handle_request(Request::Run { name: None }, &mut Session::new(), &daemon);
        assert_eq!(reply["error"], "bad_args");
        assert!(reply["message"].as_str().unwrap().contains("run <name>"));
    }

    #[test]
    fn bare_run_uses_this_sessions_init() {
        let daemon = test_daemon(Duration::from_secs(1));
        connected(&daemon);
        with_opmodes(&daemon, &["Foo", "Bar"]);
        answer(&daemon, vec![Event::OpModeRunning("Foo".into())]);
        let mut session = Session::new();
        session.last_inited = Some("Foo".into());
        let reply = handle_request(Request::Run { name: None }, &mut session, &daemon);
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["name"], "Foo");
    }

    /// A session that has said `hello operator`, counted as the daemon's
    /// connections are.
    fn operator(daemon: &Daemon) -> Session {
        let mut session = Session::new();
        let hello = Request::Hello {
            role: Role::Operator,
        };
        handle_request(hello, &mut session, daemon);
        session
    }

    fn status_gamepads(daemon: &Daemon) -> Value {
        handle_request(Request::Status, &mut Session::new(), daemon)["gamepads"].clone()
    }

    #[test]
    fn claim_needs_no_rc_and_announces_the_new_owner() {
        let daemon = test_daemon(Duration::from_millis(50));
        let events = daemon.subscribers.lock().unwrap().subscribe();
        let reply = handle_request(
            Request::Claim {
                pads: vec![Pad::Gamepad1],
            },
            &mut Session::new(),
            &daemon,
        );
        assert_eq!(reply, json!({"ok": true, "held": ["gamepad1"]}));
        assert_eq!(
            events.try_recv().unwrap(),
            json!({"event": "gamepad_owner", "user": 1, "owner": "agent", "previous": null})
        );
        assert_eq!(
            status_gamepads(&daemon),
            json!({"gamepad1": "agent", "gamepad2": null})
        );
    }

    #[test]
    fn an_agent_cannot_claim_what_the_operator_holds() {
        let daemon = test_daemon(Duration::from_millis(50));
        let claim = |pads: Vec<Pad>, session: &mut Session| {
            handle_request(Request::Claim { pads }, session, &daemon)
        };
        assert_eq!(
            claim(vec![Pad::Gamepad2], &mut operator(&daemon))["ok"],
            true
        );
        let reply = claim(Pad::ALL.to_vec(), &mut Session::new());
        assert_eq!(reply["error"], "preempted");
        assert_eq!(
            status_gamepads(&daemon),
            json!({"gamepad1": null, "gamepad2": "operator"})
        );
    }

    #[test]
    fn release_defaults_to_every_pad_you_hold() {
        let daemon = test_daemon(Duration::from_millis(50));
        let mut agent = Session::new();
        let claim = Request::Claim {
            pads: Pad::ALL.to_vec(),
        };
        handle_request(claim, &mut agent, &daemon);
        let reply = handle_request(Request::Release { pads: None }, &mut agent, &daemon);
        assert_eq!(
            reply,
            json!({"ok": true, "released": ["gamepad1", "gamepad2"]})
        );
        assert_eq!(
            status_gamepads(&daemon),
            json!({"gamepad1": null, "gamepad2": null})
        );
    }

    #[test]
    fn the_last_operator_leaving_releases_its_pads() {
        let daemon = test_daemon(Duration::from_millis(50));
        let events = daemon.subscribers.lock().unwrap().subscribe();
        let first = operator(&daemon);
        let mut second = operator(&daemon);
        let claim = Request::Claim {
            pads: vec![Pad::Gamepad1],
        };
        handle_request(claim, &mut second, &daemon);

        daemon.end_session(&second);
        assert_eq!(
            status_gamepads(&daemon),
            json!({"gamepad1": "operator", "gamepad2": null}),
            "another operator is still connected"
        );
        daemon.end_session(&first);
        assert_eq!(
            status_gamepads(&daemon),
            json!({"gamepad1": null, "gamepad2": null})
        );
        let released: Vec<Value> = events.try_iter().collect();
        assert_eq!(
            released.last(),
            Some(
                &json!({"event": "gamepad_owner", "user": 1, "owner": null, "previous": "operator"})
            )
        );

        // The agent is no longer locked out.
        let reply = handle_request(
            Request::Claim {
                pads: vec![Pad::Gamepad1],
            },
            &mut Session::new(),
            &daemon,
        );
        assert_eq!(reply["ok"], true);
    }

    #[test]
    fn a_pulse_stops_once_its_client_hangs_up() {
        let daemon = test_daemon(Duration::from_millis(50));
        connected(&daemon);
        let mut agent = Session::new();
        let claim = Request::Claim {
            pads: vec![Pad::Gamepad1],
        };
        handle_request(claim, &mut agent, &daemon);
        let hung_up = agent.hung_up.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            hung_up.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let request = Request::Gamepad(ds_proto::GamepadInput {
            left_stick_y: Some(0.5),
            duration_ms: Some(5_000),
            ..Default::default()
        });
        let reply = handle_request(request, &mut agent, &daemon);
        assert_eq!(reply["abandoned"], true, "{reply}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn gamepad_values_are_range_checked() {
        let daemon = test_daemon(Duration::from_millis(50));
        connected(&daemon);
        let mut agent = Session::new();
        let claim = Request::Claim {
            pads: vec![Pad::Gamepad1],
        };
        handle_request(claim, &mut agent, &daemon);
        let request = Request::Gamepad(ds_proto::GamepadInput {
            left_stick_y: Some(2.0),
            ..Default::default()
        });
        assert_eq!(
            handle_request(request, &mut agent, &daemon)["error"],
            "bad_args"
        );
    }

    #[test]
    fn an_empty_claim_is_bad_args() {
        let daemon = test_daemon(Duration::from_millis(50));
        let reply = handle_request(
            Request::Claim { pads: vec![] },
            &mut Session::new(),
            &daemon,
        );
        assert_eq!(reply["error"], "bad_args");
    }

    #[test]
    fn gamepad_needs_a_claim() {
        let daemon = test_daemon(Duration::from_millis(50));
        connected(&daemon);
        let reply = handle_request(
            Request::Gamepad(ds_proto::GamepadInput::default()),
            &mut Session::new(),
            &daemon,
        );
        assert_eq!(reply["error"], "not_claimed");
    }

    #[test]
    fn gamepad_checks_its_user_and_duration() {
        let daemon = test_daemon(Duration::from_millis(50));
        connected(&daemon);
        let mut agent = Session::new();
        let claim = Request::Claim {
            pads: Pad::ALL.to_vec(),
        };
        handle_request(claim, &mut agent, &daemon);
        let input = |user, duration_ms| {
            Request::Gamepad(ds_proto::GamepadInput {
                user: Some(user),
                duration_ms: Some(duration_ms),
                ..Default::default()
            })
        };
        assert_eq!(
            handle_request(input(3, 20), &mut agent, &daemon)["error"],
            "bad_args"
        );
        assert_eq!(
            handle_request(input(1, MAX_PULSE_MS + 1), &mut agent, &daemon)["error"],
            "bad_args"
        );
    }

    #[test]
    fn gamepad_pulses_a_claimed_pad() {
        let daemon = test_daemon(Duration::from_millis(50));
        connected(&daemon);
        let mut agent = Session::new();
        let claim = Request::Claim {
            pads: vec![Pad::Gamepad2],
        };
        handle_request(claim, &mut agent, &daemon);
        let request = Request::Gamepad(ds_proto::GamepadInput {
            user: Some(2),
            left_stick_y: Some(-0.5),
            duration_ms: Some(40),
            ..Default::default()
        });
        assert_eq!(
            handle_request(request, &mut agent, &daemon),
            json!({"ok": true, "user": 2, "packets": 3})
        );
    }

    #[test]
    fn same_kind_requests_take_turns_and_get_their_own_replies() {
        let daemon = test_daemon(Duration::from_secs(2));
        connected(&daemon);
        let asks: Vec<_> = (0..2)
            .map(|_| {
                let daemon = daemon.clone();
                std::thread::spawn(move || {
                    handle_request(Request::ActiveConfig, &mut Session::new(), &daemon)
                })
            })
            .collect();
        // Play the RC: answer each request as it registers. Two in flight at
        // once would both take the first answer.
        for config in ["A", "B"] {
            let deadline = Instant::now() + Duration::from_secs(2);
            while daemon.waiters().len() == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(daemon.waiters().len(), 1, "requests of one kind overlapped");
            daemon
                .waiters()
                .resolve(&Event::ActiveConfiguration(config.into()));
        }
        let mut configs: Vec<Value> = asks
            .into_iter()
            .map(|ask| ask.join().unwrap()["config"].clone())
            .collect();
        configs.sort_by_key(|c| c.to_string());
        assert_eq!(configs, vec![json!("A"), json!("B")]);
    }

    #[test]
    fn list_times_out_when_nothing_resolves_it() {
        let daemon = test_daemon(Duration::from_millis(50));
        connected(&daemon);
        let reply = handle_request(Request::List, &mut Session::new(), &daemon);
        assert_eq!(reply["error"], "timeout");
    }

    #[test]
    fn timed_out_init_waiter_is_unregistered() {
        let daemon = test_daemon(Duration::from_millis(50));
        connected(&daemon);
        let reply = handle_request(
            Request::Init {
                name: "Never".into(),
            },
            &mut Session::new(),
            &daemon,
        );
        assert_eq!(reply["error"], "timeout");
        // A name-matched waiter would otherwise linger until an
        // `OpModeInited("Never")` that may never arrive.
        assert_eq!(daemon.waiters().len(), 0);
    }
}
