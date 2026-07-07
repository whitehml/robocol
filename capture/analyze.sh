#!/usr/bin/env bash
# Quick triage of a capture. Two things we still want off a real DS session:
#   - Robocol UDP (20884): cross-check our packet layouts against the real DS.
#   - Camera + RobotServer HTTP: two sources at known ports — a Limelight on 5800
#     and the RC webcam served by RobotServer on ????.

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <capture.pcap>" >&2
    exit 1
fi

pcap="$1"
if [[ ! -f "$pcap" ]]; then
    echo "error: no such file: $pcap" >&2
    exit 1
fi

# Limelight stream + RC webcam/RobotServer. Add ports here if a source moves.
cam_ports="tcp.port == 5800 || tcp.port == 8080"

if command -v tshark >/dev/null 2>&1; then
    echo "=== Robocol (udp/20884) packet sizes ==="
    tshark -r "$pcap" -Y "udp.port == 20884" -T fields -e frame.time_relative -e ip.src -e ip.dst -e udp.length

    echo
    echo "=== Camera/RobotServer HTTP requests (5800 Limelight, 8080 webcam) ==="
    echo "    the request URIs here are the candidate stream/health endpoints"
    tshark -r "$pcap" -Y "($cam_ports) && http.request" -T fields -e frame.time_relative -e ip.src -e ip.dst -e tcp.dstport -e http.request.method -e http.request.uri

    echo
    echo "=== ...their responses (content-type tells stream vs health) ==="
    tshark -r "$pcap" -Y "($cam_ports) && http.response" -T fields -e frame.time_relative -e ip.src -e ip.dst -e tcp.srcport -e http.response.code -e http.content_type

    echo
    echo "=== MJPEG multipart responses (confirms video.rs's framing guess) ==="
    echo "    match a boundary here back to its request URI above = the feed's URL"
    tshark -r "$pcap" -Y "($cam_ports) && http.content_type contains \"multipart\"" -T fields -e frame.time_relative -e ip.src -e ip.dst -e tcp.srcport -e http.content_type

    echo
    echo "=== non-stream RobotServer responses on 8080 (health: current, RSSI) ==="
    tshark -r "$pcap" -Y "tcp.port == 8080 && http.response && !(http.content_type contains \"multipart\")" -T fields -e frame.time_relative -e http.response.code -e http.content_type -e http.request.uri
else
    echo "tshark not found, falling back to tcpdump summaries (install tshark/wireshark for the detailed view)" >&2
    echo
    echo "=== Robocol (udp/20884) ==="
    tcpdump -r "$pcap" -nn 'udp port 20884'
    echo
    echo "=== Limelight (tcp/5800) ==="
    tcpdump -r "$pcap" -nn -A 'tcp port 5800'
    echo
    echo "=== RobotServer / webcam (tcp/8080) ==="
    tcpdump -r "$pcap" -nn -A 'tcp port 8080'
fi
