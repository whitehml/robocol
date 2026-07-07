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

- **Limelight** (its own device, port 5800): the URL is known, so pull it while
  capturing to confirm the framing against `video.rs`:

  ```sh
  curl "$DECK_LIMELIGHT_STREAM" -o /dev/null    # Ctrl-C after a few frames
  ```

- **RC webcam** (served by RobotServer): the endpoint URL is still unknown, so
  the capture is how you find it. Open the RC's web console at
  `http://<rc-ip>:8080/` in a browser and start its camera stream; the captured
  request URI is the endpoint for `DECK_WEBCAM_STREAM`, and its `Content-Type`
  confirms or refutes the `multipart/x-mixed-replace` MJPEG framing `video.rs`
  assumes.

Inspect with `analyze.sh` (it flags MJPEG multipart responses and their request
URIs), or open the `.pcap` in Wireshark and `Follow -> HTTP Stream`.
