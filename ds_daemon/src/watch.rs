//! The JSON shape of each `robocol` event on a `watch` stream. Built once in
//! the event pump and fanned out to every watcher; which events to show is
//! up to each client (`ds_proto::WatchFilter`).

use robocol::client::Event;
use robocol::packets::Telemetry;
use serde_json::{Map, Value, json};

/// A telemetry packet's lines and numbers as JSON, for `watch` and
/// `status`. `strings` stays an ordered list of `[key, value]` pairs: a real
/// RC keys each ordinary line by an opaque sort tag (and `fake_rc` by an
/// empty key), with "Caption : Data" in the value, so a map would drop and
/// re-sort lines. `numbers` are keyed by real names.
pub fn telemetry_json(t: &Telemetry) -> Value {
    let strings: Vec<Value> = t.strings.iter().map(|(k, v)| json!([k, v])).collect();
    let numbers: Map<String, Value> = t
        .numbers
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    json!({"strings": strings, "numbers": numbers})
}

/// `None` for events with no watch representation: request replies such as
/// `OpModeList`, and raw webcam frames.
pub fn watch_json(event: &Event) -> Option<Value> {
    Some(match event {
        Event::Connected { peer } => json!({"event": "connected", "peer": peer.to_string()}),
        Event::Disconnected => json!({"event": "disconnected"}),
        Event::PeerBusy { peer } => json!({"event": "peer_busy", "peer": peer.to_string()}),
        Event::RobotState(s) => json!({"event": "state", "state": format!("{s:?}")}),
        Event::Telemetry(t) => {
            let mut event = telemetry_json(t);
            event["event"] = json!("telemetry");
            event["tag"] = json!(t.tag);
            event
        }
        Event::Stacktrace(trace) => json!({"event": "stacktrace", "trace": trace}),
        Event::CommandDropped { name } => json!({"event": "command_dropped", "name": name}),
        Event::ProtocolError(message) => json!({"event": "protocol_error", "message": message}),
        Event::WebcamAvailable(available) => {
            json!({"event": "webcam_available", "available": available})
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use robocol::packets::Telemetry;

    #[test]
    fn telemetry_keeps_every_line_in_order() {
        let mut t = Telemetry {
            tag: "TELEMETRY_DATA".into(),
            strings: vec![
                ("$Robot$Battery$Level$".into(), "12.80".into()),
                (String::new(), "OpMode : Duo".into()),
                (String::new(), "Alliance : RED".into()),
            ],
            ..Default::default()
        };
        t.numbers.insert("heading_deg".into(), 12.5);
        let json = watch_json(&Event::Telemetry(t)).unwrap();
        assert_eq!(
            json,
            json!({
                "event": "telemetry",
                "tag": "TELEMETRY_DATA",
                "strings": [
                    ["$Robot$Battery$Level$", "12.80"],
                    ["", "OpMode : Duo"],
                    ["", "Alliance : RED"],
                ],
                "numbers": {"heading_deg": 12.5},
            })
        );
    }

    #[test]
    fn a_busy_rc_is_a_watch_event() {
        let peer = "192.168.43.1:20884".parse().unwrap();
        assert_eq!(
            watch_json(&Event::PeerBusy { peer }),
            Some(json!({"event": "peer_busy", "peer": "192.168.43.1:20884"}))
        );
    }

    #[test]
    fn request_replies_are_not_watch_events() {
        assert!(watch_json(&Event::OpModeList(vec![])).is_none());
        assert_eq!(
            watch_json(&Event::Disconnected),
            Some(json!({"event": "disconnected"}))
        );
    }
}
