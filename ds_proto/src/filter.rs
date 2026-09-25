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
];

/// Which watch events to show. A `None` field places no restriction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WatchFilter {
    /// Keep only these event types.
    pub types: Option<Vec<String>>,
    /// Keep only telemetry events with one of these tags; other events pass.
    pub tags: Option<Vec<String>>,
    /// In telemetry events keep only these keys, and drop the event when none
    /// remain; other events pass.
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
        let mut event = event.clone();
        let mut kept = 0;
        for section in ["strings", "numbers"] {
            if let Some(map) = event.get_mut(section).and_then(Value::as_object_mut) {
                map.retain(|key, _| keys.iter().any(|want| want == key));
                kept += map.len();
            }
        }
        (kept > 0).then_some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn telemetry() -> Value {
        json!({
            "event": "telemetry",
            "tag": "TELEMETRY_DATA",
            "strings": {"Alliance": "RED", "Start Position": "LEFT"},
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
    fn keys_trim_telemetry_and_drop_events_left_empty() {
        let filter = WatchFilter::parse(&["--key", "heading_deg,Start Position"]).unwrap();
        let shown = filter.apply(&telemetry()).unwrap();
        assert_eq!(shown["strings"], json!({"Start Position": "LEFT"}));
        assert_eq!(shown["numbers"], json!({"heading_deg": 12.5}));

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
        assert_eq!(shown["strings"], json!({}));
        assert!(filter.apply(&json!({"event": "state"})).is_none());
    }

    #[test]
    fn malformed_flags_are_rejected() {
        assert!(WatchFilter::parse(&["--tag"]).is_err());
        assert!(WatchFilter::parse(&["--colour", "red"]).is_err());
        assert!(WatchFilter::parse(&["--key", ","]).is_err());
    }
}
