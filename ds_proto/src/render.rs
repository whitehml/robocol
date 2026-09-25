//! Human-readable text for daemon replies and watch events, shared by
//! `ds_cli` (its default) and `ds_agent --pretty`.

use serde_json::Value;

/// One reply or event as text. Errors and warnings start with `!!`, live
/// events with `<<`.
pub fn render(value: &Value) -> String {
    if let Some(kind) = value["event"].as_str() {
        return render_event(kind, value);
    }
    if value["ok"] == false {
        return format!("!! {}: {}", text(&value["error"]), text(&value["message"]));
    }
    render_reply(value)
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "none".to_string(),
        other => other.to_string(),
    }
}

fn owner(value: &Value) -> &str {
    value.as_str().unwrap_or("nobody")
}

fn render_event(kind: &str, e: &Value) -> String {
    match kind {
        "connected" => format!("<< connected to {}", text(&e["peer"])),
        "disconnected" => "<< disconnected".to_string(),
        "peer_busy" => format!(
            "!! {} is connected to another driver station; close it, then run `connect`",
            text(&e["peer"])
        ),
        "state" => format!("<< robot state: {}", text(&e["state"])),
        "telemetry" => {
            let mut line = format!("<< telemetry [{}]", text(&e["tag"]));
            for section in ["strings", "numbers"] {
                if let Some(map) = e[section].as_object() {
                    for (key, value) in map {
                        line.push_str(&format!(" {key}={}", text(value)));
                    }
                }
            }
            line
        }
        "stacktrace" => format!("<< STACKTRACE:\n{}", text(&e["trace"])),
        "command_dropped" => format!("!! command {} never acked", text(&e["name"])),
        "protocol_error" => format!("!! protocol error: {}", text(&e["message"])),
        "webcam_available" => format!("<< webcam available: {}", text(&e["available"])),
        "gamepad_owner" => format!(
            "<< gamepad{}: {} -> {}",
            text(&e["user"]),
            owner(&e["previous"]),
            owner(&e["owner"])
        ),
        other => format!("<< {other} {e}"),
    }
}

fn render_reply(reply: &Value) -> String {
    let Some(fields) = reply.as_object() else {
        return reply.to_string();
    };
    let mut lines = Vec::new();
    for (key, value) in fields {
        match (key.as_str(), value) {
            ("ok", _) => {}
            ("opmodes", Value::Array(list)) => {
                lines.push("opmodes:".to_string());
                for m in list {
                    lines.push(format!(
                        "  {} [{} / {}]",
                        text(&m["name"]),
                        text(&m["flavor"]),
                        text(&m["group"])
                    ));
                }
            }
            ("configs", Value::Array(list)) => {
                lines.push("configs:".to_string());
                for c in list {
                    lines.push(format!("  {} ({})", text(&c["name"]), text(&c["location"])));
                }
            }
            _ => lines.push(format!("{key}: {}", text(value))),
        }
    }
    if lines.is_empty() {
        "ok".to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn errors_name_the_code_and_message() {
        let reply = json!({"ok": false, "error": "not_connected", "message": "no RC yet"});
        assert_eq!(render(&reply), "!! not_connected: no RC yet");
    }

    #[test]
    fn a_bare_ok_is_just_ok() {
        assert_eq!(render(&json!({"ok": true})), "ok");
    }

    #[test]
    fn replies_list_their_fields() {
        let reply = json!({"ok": true, "name": "Duo (TeleOp)", "connected": true});
        assert_eq!(render(&reply), "connected: true\nname: Duo (TeleOp)");
    }

    #[test]
    fn opmodes_are_one_per_line() {
        let reply = json!({"ok": true, "opmodes": [
            {"name": "Duo (TeleOp)", "flavor": "TELEOP", "group": "drive"},
        ]});
        assert_eq!(render(&reply), "opmodes:\n  Duo (TeleOp) [TELEOP / drive]");
    }

    #[test]
    fn telemetry_is_one_line_of_key_values() {
        let event = json!({"event": "telemetry", "tag": "T",
            "strings": {"Alliance": "RED"}, "numbers": {"heading_deg": 12.5}});
        assert_eq!(
            render(&event),
            "<< telemetry [T] Alliance=RED heading_deg=12.5"
        );
    }

    #[test]
    fn gamepad_owner_shows_the_handover() {
        let event =
            json!({"event": "gamepad_owner", "user": 2, "owner": "operator", "previous": "agent"});
        assert_eq!(render(&event), "<< gamepad2: agent -> operator");
        let released =
            json!({"event": "gamepad_owner", "user": 1, "owner": null, "previous": "operator"});
        assert_eq!(render(&released), "<< gamepad1: operator -> nobody");
    }

    #[test]
    fn a_busy_rc_says_what_to_do() {
        let event = json!({"event": "peer_busy", "peer": "192.168.43.1:20884"});
        assert!(render(&event).contains("run `connect`"));
    }

    #[test]
    fn unknown_events_still_show() {
        let event = json!({"event": "novel", "x": 1});
        assert!(render(&event).starts_with("<< novel "));
    }
}
