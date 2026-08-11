# Packet capture (capture host <-> Control Hub)

Everything here captures on the same machine's own Wi-Fi interface while it's
joined to the Control Hub's AP — no phone-side capture and no WPA2 decryption,
because that machine is one end of the traffic.

## The one command

Run this **before** connecting a Driver Station, drive a session, then Ctrl-C:

```sh
./capture/session.sh                        # RC at 192.168.43.1
./capture/session.sh --rc 192.168.49.1      # RC phone
./capture/session.sh --cli                  # also runs ds_cli here, logged
./capture/session.sh --label sdk-11.2       # names the output directory
```

It picks the interface that actually routes to the RC, captures to
`capture/captures/<timestamp>-<label>/`, and on exit decodes the pcap and prints
a report of everything the `robocol` crate does not already understand:

- Command names absent from `robocol::cmd` (marked `*`)
- OpMode JSON fields `cmd::OpModeMeta` drops, and flavors beyond TELEOP /
  AUTONOMOUS / SYSTEM
- entries lost by `cmd::parse_opmode_list` — it fails **closed**, returning an
  empty list rather than an error, so this is the check that catches a changed
  list shape
- the versions each peer advertised vs. the ones we hardcode in
  `PeerDiscovery::default()`

It uses `dumpcap`, which carries `cap_net_raw`, so **no sudo** — provided you're
in the `wireshark` group:

```sh
getcap "$(command -v dumpcap)" && id -nG | tr ' ' '\n' | grep -x wireshark
```

If you'd rather not use the script, `sudo tcpdump -i <iface> -w out.pcap 'udp
port 20884'` produces an equivalent pcap.

## Decoding a pcap you already have

```sh
cargo run -q -p pcap_decode -- capture.pcap             # transcript + report
cargo run -q -p pcap_decode -- capture.pcap --quiet     # report only
cargo run -q -p pcap_decode -- capture.pcap --all       # include heartbeats,
                                                        # gamepads, telemetry,
                                                        # webcam frame chunks
cargo run -q -p pcap_decode -- capture.pcap --port 0    # ephemeral ports:
                                                        # keep any UDP that
                                                        # parses as Robocol
```

It runs payloads through the crate's own `Packet::parse`, so the decode is
exactly what the client would do with the same bytes — a divergence in the
report is a divergence in the client. It reassembles IPv4 fragments, which
matters because a full OpMode list, a config XML response, and every 4 KiB
webcam frame chunk all exceed the MTU.

`analyze.sh` remains for the HTTP side (it lists Limelight MJPEG endpoints); for
Robocol it only ever printed packet *sizes*, so prefer `pcap_decode`.

## Camera streams

The camera HTTP servers are reachable straight from the capture host, so you
capture a feed by pointing any MJPEG consumer (browser, `curl`, VLC, ffmpeg) at
its URL and sniffing locally — you need a client that actually *pulls* the
stream for packets to flow. `session.sh` already captures the whole RC host, so
a Limelight pull during the session is included.

- **Limelight**: its reachable host depends on what it's plugged into —
  `limelight.local` (mDNS) when it's wired straight to this machine,
  `192.168.43.1` behind a Control Hub, or `192.168.49.1` behind an RC phone. It
  serves two always-on MJPEG streams at the root path: the annotated feed on
  **5800** and the raw feed on **5802**. Confirmed via
  `curl -s --max-time 2 -D - -o /dev/null http://<limelight-ip>:5800/` (look for
  `Content-Type: multipart/x-mixed-replace;boundary=...` and
  `Server: CameraServer/1.0`):

  ```sh
  curl "$DECK_LIMELIGHT_STREAM" -o /dev/null    # Ctrl-C after a few frames
  ```

- **RC webcam**: not HTTP — it isn't reachable at any port at all. It rides
  Robocol itself (`CMD_REQUEST_FRAME` / `CMD_STREAM_CHANGE` /
  `CMD_RECEIVE_FRAME_*`, chunked JPEGs reassembled in `robocol::client`), so
  `pcap_decode --all` shows it.

Inspect a Limelight capture with `analyze.sh` (it flags MJPEG multipart
responses and their request URIs), or open the `.pcap` in Wireshark and
`Follow -> HTTP Stream`.
