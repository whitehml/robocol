//! In-memory cache of everything the robot controller has told us, built
//! from the `robocol::Event` stream. Pure data — no sockets, no threads —
//! so `status` can answer instantly and name-lookups (opmode/config) don't
//! need to round-trip the robot.

use std::collections::HashMap;

use robocol::client::Event;
use robocol::cmd::{ConfigMeta, OpModeMeta, parse_config_list};
use robocol::packets::Telemetry;
use robocol::types::RobotState;

pub struct DaemonState {
    pub connected: bool,
    pub peer: Option<String>,
    /// The RC that answered discovery as already owned by another driver
    /// station. Discovery stays paused until a `connect` request.
    pub peer_busy: Option<String>,
    /// Discovery was stopped by `disconnect`, and stays stopped until
    /// `connect`.
    pub paused: bool,
    /// The opmode inited or running on the RC; `None` once it stops
    /// (`$Stop$Robot$` is inited) or the RC goes away.
    pub opmode: Option<String>,
    pub robot_state: Option<RobotState>,
    pub opmodes: Vec<OpModeMeta>,
    pub active_config: Option<String>,
    pub configs: Vec<ConfigMeta>,
    pub device_types: Option<String>,
    pub last_scan: Option<String>,
    /// Most recent `CMD_DISCOVER_LYNX_MODULES_RESP` payload. Not keyed by
    /// serial: `Event::LynxModules` only carries the RC's raw response, not
    /// the serial it was requested for, so per-serial caching isn't possible
    /// from this event alone — the `lynx-modules` command itself still
    /// returns the right answer, via a fresh wait on this event, per call.
    pub last_lynx_modules: Option<String>,
    pub telemetry: HashMap<String, Telemetry>,
}

impl DaemonState {
    pub fn new() -> Self {
        DaemonState {
            connected: false,
            peer: None,
            peer_busy: None,
            paused: false,
            opmode: None,
            robot_state: None,
            opmodes: Vec::new(),
            active_config: None,
            configs: Vec::new(),
            device_types: None,
            last_scan: None,
            last_lynx_modules: None,
            telemetry: HashMap::new(),
        }
    }

    pub fn apply_event(&mut self, event: &Event) {
        match event {
            Event::Connected { peer } => {
                self.connected = true;
                self.peer = Some(peer.to_string());
                self.peer_busy = None;
            }
            Event::PeerBusy { peer } => {
                self.connected = false;
                self.peer = None;
                self.peer_busy = Some(peer.to_string());
                self.opmode = None;
            }
            Event::Disconnected => {
                self.connected = false;
                self.peer = None;
                self.robot_state = None;
                self.opmode = None;
            }
            Event::OpModeInited(name) => {
                self.opmode = (name != robocol::cmd::DEFAULT_OP_MODE).then(|| name.clone());
            }
            Event::OpModeRunning(name) => self.opmode = Some(name.clone()),
            Event::RobotState(state) => self.robot_state = Some(*state),
            Event::OpModeList(list) => self.opmodes = list.clone(),
            Event::ActiveConfiguration(extra) => self.active_config = Some(extra.clone()),
            Event::ConfigurationList(extra) => self.configs = parse_config_list(extra),
            Event::UserDeviceList(extra) => self.device_types = Some(extra.clone()),
            Event::ScanResult(extra) => self.last_scan = Some(extra.clone()),
            Event::LynxModules(extra) => self.last_lynx_modules = Some(extra.clone()),
            Event::Telemetry(t) => {
                self.telemetry.insert(t.tag.clone(), t.clone());
            }
            _ => {}
        }
    }

    pub fn find_config(&self, name: &str) -> Option<ConfigMeta> {
        self.configs.iter().find(|c| c.name == name).cloned()
    }

    pub fn find_opmode(&self, name: &str) -> bool {
        self.opmodes.iter().any(|m| m.name == name)
    }

    pub fn to_json(&self) -> serde_json::Value {
        let opmodes: Vec<serde_json::Value> = self
            .opmodes
            .iter()
            .map(|m| serde_json::json!({"name": m.name, "flavor": m.flavor, "group": m.group}))
            .collect();
        let telemetry: serde_json::Map<String, serde_json::Value> = self
            .telemetry
            .iter()
            .map(|(tag, t)| (tag.clone(), crate::watch::telemetry_json(t)))
            .collect();
        serde_json::json!({
            "ok": true,
            "connected": self.connected,
            "peer": self.peer,
            "peer_busy": self.peer_busy,
            "paused": self.paused,
            "opmode": self.opmode,
            "robot_state": self.robot_state.map(|s| format!("{s:?}")),
            "opmodes": opmodes,
            "active_config": self.active_config,
            "configs": self.configs,
            "device_types": self.device_types,
            "last_scan": self.last_scan,
            "last_lynx_modules": self.last_lynx_modules,
            "telemetry": telemetry,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn starts_disconnected_with_empty_caches() {
        let state = DaemonState::new();
        assert!(!state.connected);
        assert!(state.opmodes.is_empty());
        assert!(state.find_config("anything").is_none());
        assert!(!state.find_opmode("anything"));
    }

    #[test]
    fn connected_then_disconnected_clears_robot_state() {
        let mut state = DaemonState::new();
        let peer: SocketAddr = "127.0.0.1:20884".parse().unwrap();
        state.apply_event(&Event::Connected { peer });
        state.apply_event(&Event::RobotState(RobotState::Running));
        assert!(state.connected);
        assert_eq!(state.robot_state, Some(RobotState::Running));

        state.apply_event(&Event::Disconnected);
        assert!(!state.connected);
        assert_eq!(state.robot_state, None);
        assert_eq!(state.peer, None);
    }

    #[test]
    fn a_busy_rc_is_remembered_until_a_connection() {
        let mut state = DaemonState::new();
        let peer: SocketAddr = "192.168.43.1:20884".parse().unwrap();
        state.apply_event(&Event::PeerBusy { peer });
        assert!(!state.connected);
        assert_eq!(state.peer_busy.as_deref(), Some("192.168.43.1:20884"));
        assert_eq!(state.to_json()["peer_busy"], "192.168.43.1:20884");

        state.apply_event(&Event::Connected { peer });
        assert!(state.connected);
        assert_eq!(state.peer_busy, None);
    }

    #[test]
    fn the_current_opmode_follows_init_run_and_stop() {
        let mut state = DaemonState::new();
        state.apply_event(&Event::OpModeInited("Duo (TeleOp)".into()));
        assert_eq!(state.to_json()["opmode"], "Duo (TeleOp)");
        state.apply_event(&Event::OpModeRunning("Duo (TeleOp)".into()));
        assert_eq!(state.opmode.as_deref(), Some("Duo (TeleOp)"));
        state.apply_event(&Event::OpModeInited(
            robocol::cmd::DEFAULT_OP_MODE.to_string(),
        ));
        assert_eq!(state.opmode, None);
    }

    #[test]
    fn opmode_list_populates_find_opmode() {
        let mut state = DaemonState::new();
        state.apply_event(&Event::OpModeList(vec![OpModeMeta {
            name: "Duo (TeleOp)".into(),
            flavor: "TELEOP".into(),
            group: "drive".into(),
        }]));
        assert!(state.find_opmode("Duo (TeleOp)"));
        assert!(!state.find_opmode("Nonexistent"));
    }

    #[test]
    fn configuration_list_populates_find_config() {
        let mut state = DaemonState::new();
        let json = r#"[{"isDirty":false,"location":"Local","name":"my_config","resourceId":1}]"#;
        state.apply_event(&Event::ConfigurationList(json.to_string()));
        let meta = state.find_config("my_config").expect("config found");
        assert_eq!(meta.location, "Local");
        assert_eq!(meta.resource_id, 1);
    }

    #[test]
    fn telemetry_keeps_latest_per_tag() {
        let mut state = DaemonState::new();
        let first = Telemetry {
            tag: "auto".into(),
            strings: vec![("phase".into(), "start".into())],
            ..Default::default()
        };
        state.apply_event(&Event::Telemetry(first));

        let second = Telemetry {
            tag: "auto".into(),
            strings: vec![("phase".into(), "end".into())],
            ..Default::default()
        };
        state.apply_event(&Event::Telemetry(second));

        assert_eq!(state.telemetry.len(), 1);
        assert_eq!(state.telemetry["auto"].strings[0].1, "end");
    }

    #[test]
    fn to_json_reports_ok_true_and_connection_state() {
        let state = DaemonState::new();
        let json = state.to_json();
        assert_eq!(json["ok"], true);
        assert_eq!(json["connected"], false);
        assert!(json["opmodes"].as_array().unwrap().is_empty());
    }
}
