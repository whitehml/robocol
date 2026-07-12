//! End-to-end client tests against a fake RC on localhost: discovery
//! handshake, auto-requests, command ack both ways, telemetry, config CRUD.

use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use robocol::client::{ClientConfig, Event, RobocolClient};
use robocol::cmd::{self, ConfigMeta};
use robocol::packets::{Command, Packet, PeerDiscovery, Telemetry, BATTERY_LEVEL_KEY};
use robocol::types::RobotState;

fn wait_for<T>(rx: &Receiver<Event>, mut pick: impl FnMut(Event) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for event");
        let event = rx.recv_timeout(remaining).expect("event channel");
        if let Some(v) = pick(event) {
            return v;
        }
    }
}

/// Binds a fake RC and a client pointed at it, runs the discovery
/// handshake, and acks the three connect-time auto-requests (active
/// config, saved configs, OpMode list — matches the reference DS).
fn connect() -> (RobocolClient, Receiver<Event>, UdpSocket, SocketAddr) {
    let rc = UdpSocket::bind("127.0.0.1:0").unwrap();
    rc.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let rc_port = rc.local_addr().unwrap().port();

    let config = ClientConfig {
        bind_port: 0,
        peer_addrs: vec!["127.0.0.1".parse().unwrap()],
        peer_port: rc_port,
        ..Default::default()
    };
    let (client, events) = RobocolClient::start(config).unwrap();

    let mut buf = [0u8; 4096];
    let ds_addr = loop {
        let (n, from) = rc.recv_from(&mut buf).unwrap();
        if let Ok(Packet::PeerDiscovery(_)) = Packet::parse(&buf[..n]) {
            rc.send_to(&PeerDiscovery::default().serialize(), from)
                .unwrap();
            break from;
        }
    };

    wait_for(&events, |e| match e {
        Event::Connected { .. } => Some(()),
        _ => None,
    });

    let mut expected: std::collections::HashSet<&str> = [
        cmd::REQUEST_ACTIVE_CONFIG,
        cmd::REQUEST_CONFIGURATIONS,
        cmd::REQUEST_OP_MODE_LIST,
    ]
    .into_iter()
    .collect();
    while !expected.is_empty() {
        let (n, _) = rc.recv_from(&mut buf).unwrap();
        let Ok(Packet::Command(c)) = Packet::parse(&buf[..n]) else {
            continue;
        };
        if c.acknowledged || !expected.remove(c.name.as_str()) {
            continue;
        }
        rc.send_to(&Command::ack_of(&c).serialize(), ds_addr)
            .unwrap();
    }

    (client, events, rc, ds_addr)
}

#[test]
fn full_handshake_and_traffic() {
    let (client, events, rc, ds_addr) = connect();
    let mut buf = [0u8; 4096];

    let notify = Command::new(
        cmd::NOTIFY_OP_MODE_LIST,
        r#"[{"name":"Duo","flavor":"TELEOP","group":"drive"}]"#,
        777,
    );
    rc.send_to(&notify.serialize(), ds_addr).unwrap();

    let opmodes = wait_for(&events, |e| match e {
        Event::OpModeList(list) => Some(list),
        _ => None,
    });
    assert_eq!(opmodes.len(), 1);
    assert_eq!(opmodes[0].name, "Duo");

    // Client must have acked our notify.
    loop {
        let (n, _) = rc.recv_from(&mut buf).unwrap();
        if let Ok(Packet::Command(c)) = Packet::parse(&buf[..n]) {
            if c.acknowledged {
                assert_eq!(c.name, cmd::NOTIFY_OP_MODE_LIST);
                assert_eq!(c.timestamp, 777);
                break;
            }
        }
    }

    // Telemetry flows through as an event, with a RobotState change first.
    let mut telemetry = Telemetry {
        robot_state: RobotState::Running,
        tag: "TELEMETRY_DATA".to_string(),
        ..Default::default()
    };
    telemetry.numbers.insert("flywheel_rpm".into(), 2810.0);
    rc.send_to(&telemetry.serialize(), ds_addr).unwrap();

    wait_for(&events, |e| match e {
        Event::RobotState(RobotState::Running) => Some(()),
        _ => None,
    });
    let received = wait_for(&events, |e| match e {
        Event::Telemetry(t) => Some(t),
        _ => None,
    });
    assert_eq!(received.numbers["flywheel_rpm"], 2810.0);

    // An init command from the API surfaces on the RC side with a raw
    // (unquoted) OpMode name payload.
    client.init_opmode("Duo");
    loop {
        let (n, _) = rc.recv_from(&mut buf).unwrap();
        if let Ok(Packet::Command(c)) = Packet::parse(&buf[..n]) {
            if !c.acknowledged && c.name == cmd::INIT_OP_MODE {
                assert_eq!(c.extra, "Duo");
                break;
            }
        }
    }

    drop(client);
}

#[test]
fn battery_voltage_arrives_over_telemetry() {
    let (_client, events, rc, ds_addr) = connect();

    let mut telemetry = Telemetry {
        robot_state: RobotState::Running,
        tag: "".to_string(),
        ..Default::default()
    };
    telemetry
        .strings
        .push((BATTERY_LEVEL_KEY.to_string(), "12.06".to_string()));
    rc.send_to(&telemetry.serialize(), ds_addr).unwrap();

    let received = wait_for(&events, |e| match e {
        Event::Telemetry(t) if t.battery_voltage().is_some() => Some(t),
        _ => None,
    });
    assert_eq!(received.battery_voltage(), Some(12.06));
}

#[test]
fn activate_configuration_round_trips_meta_and_triggers_restart() {
    let (client, events, rc, ds_addr) = connect();
    let mut buf = [0u8; 4096];

    let meta = ConfigMeta {
        is_dirty: false,
        location: "RESOURCE".to_string(),
        name: "jimmy".to_string(),
        resource_id: 2132017159,
    };
    client.activate_configuration(&meta);

    // The RC should see the full ConfigMeta JSON as the command's extra —
    // not a bare name (this was the original bug: the RC silently ignores
    // an activate/request/delete keyed by name alone).
    let activate = loop {
        let (n, _) = rc.recv_from(&mut buf).unwrap();
        if let Ok(Packet::Command(c)) = Packet::parse(&buf[..n]) {
            if !c.acknowledged && c.name == cmd::ACTIVATE_CONFIGURATION {
                break c;
            }
        }
    };
    let sent: ConfigMeta = serde_json::from_str(&activate.extra).unwrap();
    assert_eq!(sent, meta);

    rc.send_to(&Command::ack_of(&activate).serialize(), ds_addr)
        .unwrap();

    // Activation only takes effect after a restart; the client sends one
    // automatically as soon as the activate is acked.
    loop {
        let (n, _) = rc.recv_from(&mut buf).unwrap();
        if let Ok(Packet::Command(c)) = Packet::parse(&buf[..n]) {
            if !c.acknowledged && c.name == cmd::RESTART_ROBOT {
                break;
            }
        }
    }

    // The RC's resulting active-config notification surfaces as a typed
    // event with the same ConfigMeta shape.
    let notify = Command::new(
        cmd::NOTIFY_ACTIVE_CONFIGURATION,
        &serde_json::to_string(&meta).unwrap(),
        42,
    );
    rc.send_to(&notify.serialize(), ds_addr).unwrap();
    let active = wait_for(&events, |e| match e {
        Event::ActiveConfiguration(extra) => Some(extra),
        _ => None,
    });
    assert_eq!(serde_json::from_str::<ConfigMeta>(&active).unwrap(), meta);

    drop(client);
}

#[test]
fn save_configuration_does_not_restart_robot() {
    let (client, _events, rc, ds_addr) = connect();
    let mut buf = [0u8; 4096];

    let meta = ConfigMeta {
        is_dirty: false,
        location: "LOCAL_STORAGE".to_string(),
        name: "practice_bot".to_string(),
        resource_id: 0,
    };
    let payload = format!("{};{}", serde_json::to_string(&meta).unwrap(), "<Robot/>");
    client.save_configuration(&payload);

    let save = loop {
        let (n, _) = rc.recv_from(&mut buf).unwrap();
        if let Ok(Packet::Command(c)) = Packet::parse(&buf[..n]) {
            if !c.acknowledged && c.name == cmd::SAVE_CONFIGURATION {
                break c;
            }
        }
    };
    rc.send_to(&Command::ack_of(&save).serialize(), ds_addr)
        .unwrap();

    // Unlike the reference DS, a save must NOT trigger a restart — watch a
    // short window and fail if one shows up.
    rc.set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_millis(600);
    while Instant::now() < deadline {
        let Ok((n, _)) = rc.recv_from(&mut buf) else {
            continue;
        };
        if let Ok(Packet::Command(c)) = Packet::parse(&buf[..n]) {
            assert_ne!(
                c.name,
                cmd::RESTART_ROBOT,
                "save must not restart the robot"
            );
        }
    }

    // Positive control: an explicit activate still restarts, proving the
    // window above was long enough to have caught one.
    rc.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    client.activate_configuration(&meta);
    let activate = loop {
        let (n, _) = rc.recv_from(&mut buf).unwrap();
        if let Ok(Packet::Command(c)) = Packet::parse(&buf[..n]) {
            if !c.acknowledged && c.name == cmd::ACTIVATE_CONFIGURATION {
                break c;
            }
        }
    };
    rc.send_to(&Command::ack_of(&activate).serialize(), ds_addr)
        .unwrap();
    loop {
        let (n, _) = rc.recv_from(&mut buf).unwrap();
        if let Ok(Packet::Command(c)) = Packet::parse(&buf[..n]) {
            if !c.acknowledged && c.name == cmd::RESTART_ROBOT {
                break;
            }
        }
    }

    drop(client);
}
