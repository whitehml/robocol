//! The one command surface every front end shares: what a request is on the
//! wire (`Request`), what each CLI verb is called (`Request::verb`), and how
//! a verb's words become a `Request` (`PARSE_TABLE`, `parse`, `split_line`).
//!
//! `Request` holds only plain data. Turning it into `robocol` calls is
//! `ds_daemon`'s job, which keeps `robocol` out of the front ends.

use serde::{Deserialize, Serialize};

use crate::filter::WatchFilter;

/// Stick, trigger and button values for one gamepad pulse. Anything left
/// `None` is sent as zero / released.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GamepadInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_stick_x: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_stick_y: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_stick_x: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_stick_y: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_trigger: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_trigger: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpad_up: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpad_down: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpad_left: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpad_right: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub back: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_bumper: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_bumper: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_stick_button: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_stick_button: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl GamepadInput {
    /// The field behind a stick/trigger flag such as `left-stick-y`.
    fn axis(&mut self, flag: &str) -> Option<&mut Option<f32>> {
        Some(match flag {
            "left-stick-x" => &mut self.left_stick_x,
            "left-stick-y" => &mut self.left_stick_y,
            "right-stick-x" => &mut self.right_stick_x,
            "right-stick-y" => &mut self.right_stick_y,
            "left-trigger" => &mut self.left_trigger,
            "right-trigger" => &mut self.right_trigger,
            _ => return None,
        })
    }

    /// The field behind a button flag such as `dpad-up` or `a`.
    fn button(&mut self, flag: &str) -> Option<&mut Option<bool>> {
        Some(match flag {
            "dpad-up" => &mut self.dpad_up,
            "dpad-down" => &mut self.dpad_down,
            "dpad-left" => &mut self.dpad_left,
            "dpad-right" => &mut self.dpad_right,
            "a" => &mut self.a,
            "b" => &mut self.b,
            "x" => &mut self.x,
            "y" => &mut self.y,
            "start" => &mut self.start,
            "back" => &mut self.back,
            "left-bumper" => &mut self.left_bumper,
            "right-bumper" => &mut self.right_bumper,
            "left-stick-button" => &mut self.left_stick_button,
            "right-stick-button" => &mut self.right_stick_button,
            _ => return None,
        })
    }
}

/// One request from a front end to `ds_daemon`, as it travels on the wire:
/// `{"cmd": "<verb>", ...fields}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    Ping,
    Status,
    List,
    Init {
        name: String,
    },
    Run {
        #[serde(default)]
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
    Gamepad(GamepadInput),
    Watch,
    Shutdown,
}

impl Request {
    /// The CLI verb for this request. The match is exhaustive on purpose: a
    /// new variant does not compile until it is given a verb, and the parity
    /// test then checks that verb is in `PARSE_TABLE`.
    pub fn verb(&self) -> &'static str {
        match self {
            Request::Ping => "ping",
            Request::Status => "status",
            Request::List => "list",
            Request::Init { .. } => "init",
            Request::Run { .. } => "run",
            Request::Stop => "stop",
            Request::Restart => "restart",
            Request::ActiveConfig => "active-config",
            Request::Configs => "configs",
            Request::Config { .. } => "config",
            Request::SaveConfig { .. } => "save-config",
            Request::ActivateConfig { .. } => "activate-config",
            Request::DeleteConfig { .. } => "delete-config",
            Request::DeviceTypes => "device-types",
            Request::Scan => "scan",
            Request::LynxModules { .. } => "lynx-modules",
            Request::Gamepad(_) => "gamepad",
            Request::Watch => "watch",
            Request::Shutdown => "shutdown",
        }
    }
}

/// Parses the words after a verb.
pub type ParseFn = fn(&[&str]) -> Result<Request, String>;

/// One row of the command table.
pub struct Verb {
    pub name: &'static str,
    /// One-line usage, shown by `help` and in errors.
    pub usage: &'static str,
    pub parse: ParseFn,
    /// A valid argument list for `parse`, walked by the parity test.
    pub example: &'static [&'static str],
    /// Takes a single free-text argument (an opmode name, a config name,
    /// JSON) that `split_line` passes through verbatim instead of splitting.
    pub text_arg: bool,
}

/// Every verb either front end accepts, and how to parse it. The only place
/// verbs are defined: `ds_cli` and `ds_agent` both go through `parse`.
pub const PARSE_TABLE: &[Verb] = &[
    Verb {
        name: "ping",
        usage: "ping",
        parse: |a| no_args("ping", a, Request::Ping),
        example: &[],
        text_arg: false,
    },
    Verb {
        name: "status",
        usage: "status",
        parse: |a| no_args("status", a, Request::Status),
        example: &[],
        text_arg: false,
    },
    Verb {
        name: "list",
        usage: "list",
        parse: |a| no_args("list", a, Request::List),
        example: &[],
        text_arg: false,
    },
    Verb {
        name: "init",
        usage: "init <opmode>",
        parse: |a| {
            Ok(Request::Init {
                name: text("init <opmode>", a)?,
            })
        },
        example: &["Duo (TeleOp)"],
        text_arg: true,
    },
    Verb {
        name: "run",
        usage: "run [opmode]",
        parse: |a| {
            let name = a.join(" ").trim().to_string();
            Ok(Request::Run {
                name: (!name.is_empty()).then_some(name),
            })
        },
        example: &["Duo (TeleOp)"],
        text_arg: true,
    },
    Verb {
        name: "stop",
        usage: "stop",
        parse: |a| no_args("stop", a, Request::Stop),
        example: &[],
        text_arg: false,
    },
    Verb {
        name: "restart",
        usage: "restart",
        parse: |a| no_args("restart", a, Request::Restart),
        example: &[],
        text_arg: false,
    },
    Verb {
        name: "active-config",
        usage: "active-config",
        parse: |a| no_args("active-config", a, Request::ActiveConfig),
        example: &[],
        text_arg: false,
    },
    Verb {
        name: "configs",
        usage: "configs",
        parse: |a| no_args("configs", a, Request::Configs),
        example: &[],
        text_arg: false,
    },
    Verb {
        name: "config",
        usage: "config <name>",
        parse: |a| {
            Ok(Request::Config {
                name: text("config <name>", a)?,
            })
        },
        example: &["practice_bot"],
        text_arg: true,
    },
    Verb {
        name: "save-config",
        usage: "save-config <json>",
        parse: |a| {
            Ok(Request::SaveConfig {
                json: text("save-config <json>", a)?,
            })
        },
        example: &[r#"{"name": "practice_bot"}"#],
        text_arg: true,
    },
    Verb {
        name: "activate-config",
        usage: "activate-config <name>",
        parse: |a| {
            Ok(Request::ActivateConfig {
                name: text("activate-config <name>", a)?,
            })
        },
        example: &["practice_bot"],
        text_arg: true,
    },
    Verb {
        name: "delete-config",
        usage: "delete-config <name>",
        parse: |a| {
            Ok(Request::DeleteConfig {
                name: text("delete-config <name>", a)?,
            })
        },
        example: &["practice_bot"],
        text_arg: true,
    },
    Verb {
        name: "device-types",
        usage: "device-types",
        parse: |a| no_args("device-types", a, Request::DeviceTypes),
        example: &[],
        text_arg: false,
    },
    Verb {
        name: "scan",
        usage: "scan",
        parse: |a| no_args("scan", a, Request::Scan),
        example: &[],
        text_arg: false,
    },
    Verb {
        name: "lynx-modules",
        usage: "lynx-modules <serial>",
        parse: |a| {
            Ok(Request::LynxModules {
                serial: text("lynx-modules <serial>", a)?,
            })
        },
        example: &["DQ12345"],
        text_arg: true,
    },
    Verb {
        name: "gamepad",
        usage: "gamepad [--left-stick-y -0.5 ...] [--a true ...] [--duration 300ms]",
        parse: parse_gamepad,
        example: &[
            "--left-stick-y",
            "-0.5",
            "--a",
            "true",
            "--duration",
            "300ms",
        ],
        text_arg: false,
    },
    Verb {
        name: "watch",
        usage: "watch [--types a,b] [--tag t,...] [--key k,...]",
        parse: |a| WatchFilter::parse(a).map(|_| Request::Watch),
        example: &[
            "--types",
            "telemetry",
            "--tag",
            "TELEMETRY_DATA",
            "--key",
            "heading_deg",
        ],
        text_arg: false,
    },
    Verb {
        name: "shutdown",
        usage: "shutdown",
        parse: |a| no_args("shutdown", a, Request::Shutdown),
        example: &[],
        text_arg: false,
    },
];

/// Parses `words` (a verb, then its arguments) into a `Request`.
pub fn parse(words: &[&str]) -> Result<Request, String> {
    let (verb, args) = words.split_first().ok_or("empty command")?;
    let row = PARSE_TABLE
        .iter()
        .find(|v| v.name == *verb)
        .ok_or_else(|| format!("unknown command: {verb} (try `help`)"))?;
    (row.parse)(args)
}

/// Splits a typed line (the REPL) into the words `parse` takes: the verb,
/// then either its free-text argument verbatim (`init Duo (TeleOp)`,
/// `save-config {...}`) or its whitespace-separated words, where "double
/// quotes" group a word (`watch --key "Start Position"`).
pub fn split_line(line: &str) -> Result<Vec<String>, String> {
    let line = line.trim();
    let (verb, rest) = match line.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (line, ""),
    };
    if verb.is_empty() {
        return Ok(Vec::new());
    }
    let mut words = vec![verb.to_string()];
    let text_arg = PARSE_TABLE.iter().any(|v| v.name == verb && v.text_arg);
    if text_arg {
        if !rest.is_empty() {
            words.push(rest.to_string());
        }
    } else {
        words.extend(split_words(rest)?);
    }
    Ok(words)
}

fn split_words(s: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                in_word = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            c => {
                current.push(c);
                in_word = true;
            }
        }
    }
    if quoted {
        return Err("unterminated \" in command".into());
    }
    if in_word {
        words.push(current);
    }
    Ok(words)
}

fn no_args(verb: &str, args: &[&str], request: Request) -> Result<Request, String> {
    if args.is_empty() {
        Ok(request)
    } else {
        Err(format!("{verb} takes no arguments"))
    }
}

/// One free-text argument. Words are re-joined with single spaces, so an
/// unquoted `ds_agent init Duo (TeleOp)` means the same as the quoted form.
fn text(usage: &str, args: &[&str]) -> Result<String, String> {
    let joined = args.join(" ");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        Err(format!("usage: {usage}"))
    } else {
        Ok(trimmed.to_string())
    }
}

fn parse_gamepad(args: &[&str]) -> Result<Request, String> {
    let mut input = GamepadInput::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i]
            .strip_prefix("--")
            .ok_or_else(|| format!("expected a --flag, got {:?}", args[i]))?
            .replace('_', "-");
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("missing value for --{flag}"))?;
        if flag == "duration" {
            input.duration_ms = Some(parse_duration_ms(value)?);
        } else if let Some(slot) = input.axis(&flag) {
            *slot = Some(
                value
                    .parse()
                    .map_err(|_| format!("--{flag} expects a number, got {value:?}"))?,
            );
        } else if let Some(slot) = input.button(&flag) {
            *slot = Some(
                value
                    .parse()
                    .map_err(|_| format!("--{flag} expects true or false, got {value:?}"))?,
            );
        } else {
            return Err(format!("unknown gamepad flag: --{flag}"));
        }
        i += 2;
    }
    Ok(Request::Gamepad(input))
}

/// `300ms`, `1.5s`, or a bare number of milliseconds.
pub fn parse_duration_ms(value: &str) -> Result<u64, String> {
    let invalid = || format!("invalid duration: {value}");
    if let Some(ms) = value.strip_suffix("ms") {
        ms.parse().map_err(|_| invalid())
    } else if let Some(secs) = value.strip_suffix('s') {
        match secs.parse::<f64>() {
            Ok(secs) if secs.is_finite() && secs >= 0.0 => Ok((secs * 1000.0) as u64),
            _ => Err(invalid()),
        }
    } else {
        value.parse().map_err(|_| invalid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The parity test. Every row's example parses, the resulting request
    /// names the verb it came from, it survives the wire, and the typed-line
    /// path (`ds_cli`) yields the same request as the argv path (`ds_agent`).
    #[test]
    fn every_verb_parses_to_itself_on_both_paths() {
        for row in PARSE_TABLE {
            let request = (row.parse)(row.example)
                .unwrap_or_else(|e| panic!("{}: example did not parse: {e}", row.name));
            assert_eq!(
                request.verb(),
                row.name,
                "{} parsed to the wrong variant",
                row.name
            );

            let mut argv = vec![row.name];
            argv.extend_from_slice(row.example);
            assert_eq!(
                parse(&argv).as_ref(),
                Ok(&request),
                "argv path for {}",
                row.name
            );

            let quoted: Vec<String> = row
                .example
                .iter()
                .map(|w| {
                    if row.text_arg || !w.contains(' ') {
                        w.to_string()
                    } else {
                        format!("\"{w}\"")
                    }
                })
                .collect();
            let line = format!("{} {}", row.name, quoted.join(" "));
            let words = split_line(&line).unwrap();
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            assert_eq!(
                parse(&words).as_ref(),
                Ok(&request),
                "typed-line path for {line:?}"
            );

            let wire = serde_json::to_value(&request).unwrap();
            assert_eq!(wire["cmd"], row.name, "wire tag for {}", row.name);
            let back: Request = serde_json::from_value(wire).unwrap();
            assert_eq!(back, request, "{} did not survive the wire", row.name);
        }
    }

    #[test]
    fn verbs_are_unique() {
        let mut names: Vec<&str> = PARSE_TABLE.iter().map(|v| v.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "a verb appears twice in PARSE_TABLE");
    }

    #[test]
    fn unknown_and_empty_commands_are_errors() {
        assert!(parse(&[]).is_err());
        let err = parse(&["fly"]).unwrap_err();
        assert!(err.contains("help"), "{err}");
    }

    #[test]
    fn verbs_without_arguments_reject_extra_words() {
        assert!(parse(&["list", "now"]).is_err());
        assert_eq!(parse(&["list"]), Ok(Request::List));
    }

    #[test]
    fn text_arguments_are_required_and_rejoined() {
        assert!(parse(&["init"]).is_err());
        assert_eq!(
            parse(&["init", "Duo", "(TeleOp)"]),
            Ok(Request::Init {
                name: "Duo (TeleOp)".into()
            })
        );
        assert_eq!(parse(&["run"]), Ok(Request::Run { name: None }));
    }

    #[test]
    fn split_line_keeps_text_arguments_verbatim() {
        let json = r#"save-config {"name": "two  spaces"}"#;
        assert_eq!(
            split_line(json).unwrap(),
            vec!["save-config", r#"{"name": "two  spaces"}"#]
        );
        assert_eq!(
            split_line("  init Duo (TeleOp) ").unwrap(),
            vec!["init", "Duo (TeleOp)"]
        );
        assert_eq!(split_line("   ").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn split_line_groups_double_quoted_words() {
        assert_eq!(
            split_line(r#"watch --key "Start Position,heading_deg""#).unwrap(),
            vec!["watch", "--key", "Start Position,heading_deg"]
        );
        assert!(split_line(r#"watch --key "Start"#).is_err());
    }

    #[test]
    fn gamepad_flags_parse_and_accept_underscores() {
        let request = parse(&[
            "gamepad",
            "--left_stick_y",
            "-0.5",
            "--dpad-up",
            "true",
            "--duration",
            "1.5s",
        ])
        .unwrap();
        let Request::Gamepad(input) = request else {
            panic!("not a gamepad request")
        };
        assert_eq!(input.left_stick_y, Some(-0.5));
        assert_eq!(input.dpad_up, Some(true));
        assert_eq!(input.duration_ms, Some(1500));
        assert_eq!(input.a, None);
    }

    #[test]
    fn gamepad_rejects_bad_flags_and_values() {
        assert!(parse(&["gamepad", "--not-a-flag", "1"]).is_err());
        assert!(parse(&["gamepad", "--left-stick-y", "fast"]).is_err());
        assert!(parse(&["gamepad", "--a", "yes"]).is_err());
        assert!(parse(&["gamepad", "--a"]).is_err());
        assert!(parse(&["gamepad", "a", "true"]).is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("300ms"), Ok(300));
        assert_eq!(parse_duration_ms("1.5s"), Ok(1500));
        assert_eq!(parse_duration_ms("250"), Ok(250));
        assert!(parse_duration_ms("-1s").is_err());
        assert!(parse_duration_ms("nans").is_err());
        assert!(parse_duration_ms("infs").is_err());
        assert!(parse_duration_ms("soon").is_err());
    }

    #[test]
    fn wire_shape_is_flat_and_omits_unset_gamepad_fields() {
        assert_eq!(
            serde_json::to_value(Request::Run { name: None }).unwrap(),
            json!({"cmd": "run", "name": null})
        );
        let request = Request::Gamepad(GamepadInput {
            left_stick_y: Some(-0.5),
            ..Default::default()
        });
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({"cmd": "gamepad", "left_stick_y": -0.5})
        );
        let back: Request = serde_json::from_value(json!({"cmd": "gamepad"})).unwrap();
        assert_eq!(back, Request::Gamepad(GamepadInput::default()));
    }
}
