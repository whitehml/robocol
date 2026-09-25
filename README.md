# robocol

The FTC Robocol (Driver Station <-> Robot Controller, UDP/20884) protocol
stack, in pure Rust. Ported from the
[Epiteugma/librobocol](https://github.com/Epiteugma/librobocol) TypeScript
implementation, with a few deviations.

## Workspace

| crate | kind | what |
|---|---|---|
| `robocol/` | lib | the protocol crate — six packet codecs, a threaded client (discovery -> heartbeat -> command ack/retransmit -> reconnection on one background thread, no async runtime), MJPEG camera decode. `std` + `serde` only. |
| `fake_rc/` | bin | a standalone fake Robot Controller for hardware-free testing. |
| `ds_daemon/` | bin | holds the one Robocol connection to the Robot Controller and serves the two front ends below. |
| `ds_cli/` | bin | interactive front end for a person: text output, live events. |
| `ds_agent/` | bin | one-command-per-run front end for scripts and AI agents: JSON output. |
| `ds_proto/` | lib | what the three `ds_*` binaries share: the command table, wire protocol, transport. |

## Build, test, run

```sh
cargo build --workspace
cargo test -p robocol
cargo test -p robocol --test loopback -- <name>

cargo run -p fake_rc [port]           # standalone fake Robot Controller (default 20884)
cargo run -p ds_cli                   # interactive CLI against a real Control Hub
```

`tests/loopback.rs` drives the real `RobocolClient` against an in-process
`UdpSocket` fake — discovery, acks both ways, telemetry, config CRUD — so the
end-to-end test needs no external binary.

## Driving the robot from a terminal or an agent

`ds_daemon` keeps the connection to the Robot Controller. `ds_cli` (for a
person) and `ds_agent` (for a script or AI agent) both send it commands, and
accept the same ones. Either starts the daemon if it is not running; the
daemon keeps running after they exit. macOS and Linux only.

Join the Control Hub's Wi-Fi. Close Deck Station first: it uses the same
UDP port (20884) as the daemon.

```sh
cargo build -p ds_daemon -p ds_cli -p ds_agent
```

### By hand: `ds_cli`

```sh
target/debug/ds_cli                      # default RC addresses
target/debug/ds_cli --peer 10.0.0.5      # another RC address (used when this starts the daemon)
```

At the `>` prompt:

```text
> list
> init Duo (TeleOp)
> run
> stop
> claim gamepad1
> gamepad --left-stick-y -0.5 --duration 1s
> release
> watch --tag TELEMETRY_DATA --key heading_deg
> help
> quit
```

`watch` here only narrows the live events printed above the prompt.
`--json` prints raw JSON lines instead of text.

### From a script or agent: `ds_agent`

One command per run. It prints one JSON line and exits 0 if the reply has
`"ok": true`, 1 if not, 2 for a usage error.

```sh
target/debug/ds_agent status
target/debug/ds_agent list
target/debug/ds_agent init "Duo (TeleOp)"
target/debug/ds_agent run "Duo (TeleOp)"     # name it: each run is a new connection
target/debug/ds_agent stop
target/debug/ds_agent claim gamepad2
target/debug/ds_agent gamepad --user 2 --left-stick-y -0.5 --duration 500ms
target/debug/ds_agent release
target/debug/ds_agent watch --types telemetry --key heading_deg
target/debug/ds_agent --pretty status        # text instead of JSON
```

`watch` flags narrow the stream: `--types` (event types), `--tag`
(telemetry tag), `--key` (a telemetry line's caption, the part before ` : `
as in `telemetry.addData("Heading", ...)`, or a number's name).

Errors look like `{"ok":false,"error":"<code>","message":"..."}`.

### Gamepads

Claim a gamepad before sending to it. `ds_cli` is the operator and
`ds_agent` is the agent. The operator's claim always wins and cuts the agent
off; the agent is told `preempted`. The operator's pads come back when it
releases them or its last `ds_cli` quits.

Each `gamepad` command is a pulse: the input is sent 50 times a second for
`--duration` (default 300 ms, at most 10 s, at least 3 packets), then the pad
returns to neutral. A pulse stops early, back at neutral, when the operator
takes the pad or the command that asked for it is interrupted.

Gamepad packets are not acknowledged, so every way back to rest (a pulse
ending, a claim, a release) sends a burst of neutral packets, not one.
Nobody takes over a pushed stick, and claiming again clears a stuck one.

### Watching

`watch` prints one line per event: `connected`, `disconnected`,
`peer_busy`, `state`, `telemetry`, `opmode_inited`, `opmode_running`,
`command`, `stacktrace`, `command_dropped`, `protocol_error`,
`webcam_available`, `gamepad_owner`. `status` has the latest of everything,
including the current `opmode` and who holds each gamepad.

### Errors

| error | meaning | what to do |
|---|---|---|
| `not_connected` | no Robot Controller found yet, or `disconnect` was run | join its Wi-Fi and check `status`; after `disconnect`, run `connect` |
| `peer_busy` | another driver station owns the RC | close it, then run `connect` |
| `preempted` | the operator holds that gamepad | wait until it releases it or quits `ds_cli` |
| `not_claimed` | you have not claimed that gamepad | run `claim gamepad1` |
| `daemon_unreachable` | the daemon is not running and could not start | read the message; it quotes the daemon's log |
| `timeout` | the RC did not answer in time | check `status`, try again |
| `bad_args` | the command or its arguments are wrong | check the usage `ds_agent` prints, or `help` in `ds_cli` |

Other commands: `connect`, `disconnect`, `ping` (is a daemon running? never
starts one), `shutdown` (stop the daemon; pads it holds go back to neutral).

The daemon keeps running across rebuilds: after rebuilding, run
`ds_agent shutdown` so the next command starts the new one.

| variable | effect |
|---|---|
| `DS_DAEMON_ARGS` | extra flags for a daemon that a front end starts, e.g. `--peer 10.0.0.5` |
| `DS_DAEMON_TIMEOUT_MS` | how long the daemon waits for the RC to answer (default 5000) |

The socket is `$XDG_RUNTIME_DIR/ds_daemon.sock`; on macOS, which has no
`XDG_RUNTIME_DIR`, `$TMPDIR/ds_daemon.sock`; otherwise
`/tmp/ds_daemon-<uid>.sock`. The daemon's log is the same path ending in
`.log`.

## Lint

```sh
./scripts/lint.sh            # fmt --check, clippy, test
./scripts/lint.sh --fix      # cargo fmt first, then the checks
./scripts/lint.sh --no-test  # lint only, skip the test step
```

## Supported Robot Controller versions

**`11.0`–`11.1`**

> [!NOTE]
> **This stack first targeted FTC Robot Controller v11.0.** The wire formats
> were confirmed against a live capture — not against a spec. Older FTC SDK
> releases may use different packet layouts, command vocabularies, or defaults
> and are **not supported**. They may still work, but you are on your own.

## Packet capture

`capture/` holds the packet-capture + inspection tooling used to observe and
confirm the protocol.
