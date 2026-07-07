# robocol

The FTC Robocol (Driver Station <-> Robot Controller, UDP/20884) protocol
stack, in pure Rust. Ported from the
[Epiteugma/librobocol](https://github.com/Epiteugma/librobocol) TypeScript
implementation, with a few deviations.

## Workspace

| crate | kind | what it is |
|---|---|---|
| `robocol/` | lib | the protocol crate — six packet codecs, a threaded client (discovery -> heartbeat -> command ack/retransmit -> reconnection on one background thread, no async runtime), MJPEG camera decode. `std` + `serde` only. |
| `fake_rc/` | bin | a standalone fake Robot Controller for hardware-free testing. |
| `ds_cli/` | bin | an interactive Robocol client for driving a real Control Hub from a terminal. |

## Build, test, run

```sh
cargo build --workspace
cargo test -p robocol
cargo test -p robocol --test loopback -- <name>

cargo run -p fake_rc [port]           # standalone fake Robot Controller (default 20884)
cargo run -p ds_cli  [rc-ip]          # interactive CLI against a real Control Hub
```

`tests/loopback.rs` drives the real `RobocolClient` against an in-process
`UdpSocket` fake — discovery, acks both ways, telemetry, config CRUD — so the
end-to-end test needs no external binary.

## Supported Robot Controller versions

**`11.0`–`11.1`**

> [!NOTE]
> **This stack first targeted FTC Robot Controller v11.0.** The wire formats
> were confirmed against a live capture — not against a spec. Older FTC SDK
> releases may use different packet layouts, command vocabularies, or defaults
> and are **not supported**. They may still work, but you are on your own.

## Packet capture

`capture/` holds the packet-capture + triage tooling used to observe and
confirm the protocol.
