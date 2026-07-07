//! Well-known Command names and payload helpers, as used by the stock DS
//! (via the Epiteugma/FtcDriverStation reference implementation).

use serde::{Deserialize, Serialize};

// DS -> RC requests.
pub const REQUEST_OP_MODE_LIST: &str = "CMD_REQUEST_OP_MODE_LIST";
pub const INIT_OP_MODE: &str = "CMD_INIT_OP_MODE"; // extra: OpMode name (raw string)
pub const RUN_OP_MODE: &str = "CMD_RUN_OP_MODE"; // extra: OpMode name (raw string)
pub const REQUEST_ACTIVE_CONFIG: &str = "CMD_REQUEST_ACTIVE_CONFIG";
pub const REQUEST_CONFIGURATIONS: &str = "CMD_REQUEST_CONFIGURATIONS";
pub const REQUEST_PARTICULAR_CONFIGURATION: &str = "CMD_REQUEST_PARTICULAR_CONFIGURATION";
pub const REQUEST_USER_DEVICE_TYPES: &str = "CMD_REQUEST_USER_DEVICE_TYPES";
pub const SAVE_CONFIGURATION: &str = "CMD_SAVE_CONFIGURATION"; // extra: configuration JSON
pub const ACTIVATE_CONFIGURATION: &str = "CMD_ACTIVATE_CONFIGURATION"; // extra: ConfigMeta JSON
pub const DELETE_CONFIGURATION: &str = "CMD_DELETE_CONFIGURATION"; // extra: ConfigMeta JSON
pub const RESTART_ROBOT: &str = "CMD_RESTART_ROBOT";
pub const SCAN: &str = "CMD_SCAN";
pub const DISCOVER_LYNX_MODULES: &str = "CMD_DISCOVER_LYNX_MODULES";

// RC -> DS notifications/responses.
pub const NOTIFY_OP_MODE_LIST: &str = "CMD_NOTIFY_OP_MODE_LIST"; // extra: OpMode[] JSON
pub const NOTIFY_INIT_OP_MODE: &str = "CMD_NOTIFY_INIT_OP_MODE"; // extra: OpMode name
pub const NOTIFY_RUN_OP_MODE: &str = "CMD_NOTIFY_RUN_OP_MODE"; // extra: OpMode name
pub const NOTIFY_ACTIVE_CONFIGURATION: &str = "CMD_NOTIFY_ACTIVE_CONFIGURATION";
pub const NOTIFY_USER_DEVICE_LIST: &str = "CMD_NOTIFY_USER_DEVICE_LIST";
pub const REQUEST_CONFIGURATIONS_RESP: &str = "CMD_REQUEST_CONFIGURATIONS_RESP";
pub const REQUEST_PARTICULAR_CONFIGURATION_RESP: &str = "CMD_REQUEST_PARTICULAR_CONFIGURATION_RESP";
pub const SCAN_RESP: &str = "CMD_SCAN_RESP";
pub const DISCOVER_LYNX_MODULES_RESP: &str = "CMD_DISCOVER_LYNX_MODULES_RESP";
pub const SHOW_STACKTRACE: &str = "CMD_SHOW_STACKTRACE"; // extra: stacktrace text

/// The SDK's built-in idle OpMode (OpModeManager.DEFAULT_OP_MODE_NAME).
/// The stock DS "stops" a running OpMode by initing this one.
pub const DEFAULT_OP_MODE: &str = "$Stop$Robot$";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OpModeMeta {
    pub name: String,
    #[serde(default)]
    pub flavor: String,
    #[serde(default)]
    pub group: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigMeta {
    #[serde(rename = "isDirty")]
    pub is_dirty: bool,
    pub location: String,
    pub name: String,
    #[serde(rename = "resourceId", default)]
    pub resource_id: i64,
}

pub fn parse_config_list(extra: &str) -> Vec<ConfigMeta> {
    serde_json::from_str(extra).unwrap_or_default()
}

/// Parses the CMD_NOTIFY_OP_MODE_LIST `extra` payload. Tolerates both the
/// object form (`[{"name": ..., "flavor": ..., "group": ...}]`) and a plain
/// string array, since the exact shape varies by SDK version.
pub fn parse_opmode_list(extra: &str) -> Vec<OpModeMeta> {
    if let Ok(metas) = serde_json::from_str::<Vec<OpModeMeta>>(extra) {
        return metas;
    }
    if let Ok(names) = serde_json::from_str::<Vec<String>>(extra) {
        return names
            .into_iter()
            .map(|name| OpModeMeta {
                name,
                flavor: String::new(),
                group: String::new(),
            })
            .collect();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_object_form() {
        let extra = r#"[{"name":"Duo","flavor":"TELEOP","group":"drive"},{"name":"Auto"}]"#;
        let list = parse_opmode_list(extra);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "Duo");
        assert_eq!(list[0].flavor, "TELEOP");
        assert_eq!(list[1].group, "");
    }

    #[test]
    fn parses_string_array_form() {
        let list = parse_opmode_list(r#"["Duo","Solo"]"#);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].name, "Solo");
    }

    #[test]
    fn garbage_yields_empty() {
        assert!(parse_opmode_list("not json").is_empty());
    }
}
