//! Client-side filtering of `watch` event lines. The daemon streams every
//! event; which ones an operator or agent wants to see is presentation, so
//! each front end narrows the stream itself.

use serde_json::Value;

/// Every event type a `watch` stream can carry.
pub const EVENT_TYPES: &[&str] = &[
    "connected",
    "disconnected",
    "peer_busy",
    "state",
    "telemetry",
    "stacktrace",
    "command_dropped",
    "protocol_error",
    "webcam_available",
    "gamepad_owner",
    "opmode_inited",
    "opmode_running",
    "command",
];

/// Which watch events to show. A `None` field places no restriction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WatchFilter {
    /// Keep only these event types.
    pub types: Option<Vec<String>>,
    /// Keep only telemetry events with one of these tags; other events pass.
    pub tags: Option<Vec<String>>,
    /// In telemetry events keep only these lines and numbers, and drop the
    /// event when none remain; other events pass. A number matches by name.
    /// A line matches by its caption (an ordinary line's text is
    /// `"Caption : Data"`, keyed on a real RC by an opaque sort tag) or by
    /// its key, which only reserved lines such as `$Robot$Battery$Level$`
    /// have.
    pub keys: Option<Vec<String>>,
}

impl WatchFilter {
    /// Parses `[--types a,b] [--tag t,...] [--key k,...]`.
    pub fn parse(args: &[&str]) -> Result<WatchFilter, String> {
        let mut filter = WatchFilter::default();
        let mut i = 0;
        while i < args.len() {
            let flag = args[i];
            let value = args
                .get(i + 1)
                .ok_or_else(|| format!("missing value for {flag}"))?;
            let list: Vec<String> = value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if list.is_empty() {
                return Err(format!("{flag} needs at least one value"));
            }
            match flag {
                "--types" => {
                    if let Some(bad) = list.iter().find(|t| !EVENT_TYPES.contains(&t.as_str())) {
                        return Err(format!(
                            "unknown event type {bad:?}; expected one of: {}",
                            EVENT_TYPES.join(", ")
                        ));
                    }
                    filter.types = Some(list);
                }
                "--tag" => filter.tags = Some(list),
                "--key" => filter.keys = Some(list),
                other => return Err(format!("unknown watch flag: {other}")),
            }
            i += 2;
        }
        Ok(filter)
    }

    /// The event as it should be shown, or `None` to hide it.
    pub fn apply(&self, event: &Value) -> Option<Value> {
        let kind = event["event"].as_str().unwrap_or_default();
        if let Some(types) = &self.types
            && !types.iter().any(|t| t == kind)
        {
            return None;
        }
        if kind != "telemetry" {
            return Some(event.clone());
        }
        if let Some(tags) = &self.tags {
            let tag = event["tag"].as_str().unwrap_or_default();
            if !tags.iter().any(|t| t == tag) {
                return None;
            }
        }
        let Some(keys) = &self.keys else {
            return Some(event.clone());
        };
        let wanted = |name: &str| keys.iter().any(|want| want == name);
        let mut event = event.clone();
        let mut kept = 0;
        if let Some(lines) = event.get_mut("strings").and_then(Value::as_array_mut) {
            lines.retain(|line| {
                let key = line[0].as_str().unwrap_or_default();
                let text = line[1].as_str().unwrap_or_default();
                wanted(key) || caption(text).is_some_and(wanted)
            });
            kept += lines.len();
        }
        if let Some(numbers) = event.get_mut("numbers").and_then(Value::as_object_mut) {
            numbers.retain(|name, _| wanted(name));
            kept += numbers.len();
        }
        (kept > 0).then_some(event)
    }
}

/// The caption of an ordinary telemetry line, `"Caption : Data"`.
fn caption(text: &str) -> Option<&str> {
    text.split_once(" : ").map(|(caption, _)| caption.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Telemetry as a real RC sends it: ordinary lines keyed by opaque sort
    /// tags with "Caption : Data" as their text, reserved lines keyed by name.
    fn telemetry() -> Value {
        json!({
            "event": "telemetry",
            "tag": "TELEMETRY_DATA",
            "strings": [
                ["$Robot$Battery$Level$", "12.80"],
                ["\u{1}", "Alliance : RED"],
                ["\u{2}", "Start Position : LEFT"],
                ["\u{3}", "a line with no caption"],
            ],
            "numbers": {"heading_deg": 12.5, "gamepad2_left_stick_y": -0.5},
        })
    }

    #[test]
    fn empty_filter_passes_everything() {
        let filter = WatchFilter::default();
        assert_eq!(filter.apply(&telemetry()), Some(telemetry()));
        let disconnected = json!({"event": "disconnected"});
        assert_eq!(filter.apply(&disconnected), Some(disconnected.clone()));
    }

    #[test]
    fn types_keep_only_listed_events() {
        let filter = WatchFilter::parse(&["--types", "disconnected,state"]).unwrap();
        assert!(filter.apply(&telemetry()).is_none());
        assert!(filter.apply(&json!({"event": "disconnected"})).is_some());
    }

    #[test]
    fn unknown_event_type_is_rejected() {
        let err = WatchFilter::parse(&["--types", "telemetri"]).unwrap_err();
        assert!(err.contains("telemetri"), "{err}");
    }

    #[test]
    fn tag_narrows_telemetry_only() {
        let filter = WatchFilter::parse(&["--tag", "OTHER"]).unwrap();
        assert!(filter.apply(&telemetry()).is_none());
        assert!(
            filter
                .apply(&json!({"event": "state", "state": "Running"}))
                .is_some()
        );
        let filter = WatchFilter::parse(&["--tag", "TELEMETRY_DATA"]).unwrap();
        assert!(filter.apply(&telemetry()).is_some());
    }

    #[test]
    fn keys_pick_lines_by_caption_and_numbers_by_name() {
        let filter = WatchFilter::parse(&["--key", "heading_deg,Start Position"]).unwrap();
        let shown = filter.apply(&telemetry()).unwrap();
        assert_eq!(
            shown["strings"],
            json!([["\u{2}", "Start Position : LEFT"]])
        );
        assert_eq!(shown["numbers"], json!({"heading_deg": 12.5}));
    }

    #[test]
    fn keys_pick_reserved_lines_by_key() {
        let filter = WatchFilter::parse(&["--key", "$Robot$Battery$Level$"]).unwrap();
        let shown = filter.apply(&telemetry()).unwrap();
        assert_eq!(
            shown["strings"],
            json!([["$Robot$Battery$Level$", "12.80"]])
        );
        assert_eq!(shown["numbers"], json!({}));
    }

    #[test]
    fn an_event_with_nothing_left_is_dropped() {
        let filter = WatchFilter::parse(&["--key", "nothing_like_this"]).unwrap();
        assert!(filter.apply(&telemetry()).is_none());
    }

    #[test]
    fn flags_combine() {
        let filter = WatchFilter::parse(&[
            "--types",
            "telemetry",
            "--tag",
            "TELEMETRY_DATA",
            "--key",
            "gamepad2_left_stick_y",
        ])
        .unwrap();
        let shown = filter.apply(&telemetry()).unwrap();
        assert_eq!(shown["numbers"], json!({"gamepad2_left_stick_y": -0.5}));
        assert_eq!(shown["strings"], json!([]));
        assert!(filter.apply(&json!({"event": "state"})).is_none());
    }

    #[test]
    fn malformed_flags_are_rejected() {
        assert!(WatchFilter::parse(&["--tag"]).is_err());
        assert!(WatchFilter::parse(&["--colour", "red"]).is_err());
        assert!(WatchFilter::parse(&["--key", ","]).is_err());
    }
}
