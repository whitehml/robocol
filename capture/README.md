# Packet capture (capture host <-> Control Hub)

Everything here captures on the same machine's own Wi-Fi interface while it's
joined to the Control Hub's AP — no phone-side capture and no WPA2 decryption,
because that machine is one end of the traffic. Find your interface first
(`ip link` / `nmcli device status`; often `wlan0` or `wlp3s0`) and substitute it
below.

## Robocol (`ds_cli`)

Confirms our Robocol UDP layouts (the `LIBROBOCOL DEVIATION` ones) against a real
RC.

1. Start the capture:

   ```sh
   sudo tcpdump -i wlp3s0 -w capture/captures/ds_cli_session.pcap 'udp port 20884'
   ```

2. In another terminal, run a `ds_cli` session (see the repo README): connect,
   INIT / START / STOP an OpMode, and exercise config CRUD.
3. Ctrl-C the tcpdump, then inspect it:

   ```sh
   ./capture/analyze.sh capture/captures/ds_cli_session.pcap
   ```

## Camera streams

The camera HTTP servers are reachable straight from the capture host, so you
capture a feed by pointing any MJPEG consumer (browser, `curl`, VLC, ffmpeg) at
its URL and sniffing locally — you need a client that actually *pulls* the stream
for packets to flow. Capture by the camera's **host**, not a port, so you catch
whatever port/path it really uses:

```sh
sudo tcpdump -i wlp3s0 -w capture/captures/camera.pcap 'host <camera-ip>'
```

- **Limelight**: its reachable host depends on what it's plugged 
  into — `limelight.local` (mDNS) when it's wired straight to this machine,
  `192.168.43.1` behind a Control Hub, or `192.168.49.1` behind an RC phone.
  It serves two always-on MJPEG streams at the root path: the annotated 
  feed on **5800** and the raw feed on **5802**. Confirmed via
  `curl -s --max-time 2 -D - -o /dev/null http://<limelight-ip>:5800/`
  (look for `Content-Type: multipart/x-mixed-replace;boundary=...` and
  `Server: CameraServer/1.0`):

  ```sh
  curl "$DECK_LIMELIGHT_STREAM" -o /dev/null    # Ctrl-C after a few frames
  ```

- **RC webcam**: not HTTP — it isn't reachable at any port at all. It rides
  Robocol itself (`CMD_REQUEST_FRAME` / `CMD_STREAM_CHANGE` /
  `CMD_RECEIVE_FRAME_*` over the same UDP/20884 traffic `ds_cli` capture above
  already gets), chunked JPEGs reassembled in `robocol::client`.

Inspect the Limelight capture with `analyze.sh` (it flags MJPEG multipart
responses and their request URIs), or open the `.pcap` in Wireshark and
`Follow -> HTTP Stream`.
