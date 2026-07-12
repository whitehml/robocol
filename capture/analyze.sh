#!/usr/bin/env bash
# Quick inspection of a capture. What we check against a real DS session:
#   - Robocol UDP (20884): cross-check our packet layouts against the real DS.
#     The RC webcam also rides this port (CMD_REQUEST_FRAME / CMD_STREAM_CHANGE /
#     CMD_RECEIVE_FRAME_*, chunked JPEGs) — it is NOT an HTTP stream.
#   - Limelight HTTP: two always-on CameraServer MJPEG streams (annotated on
#     5800, raw on 5802).

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <capture.pcap>" >&2
    exit 1
fi

pcap="$1"
if [[ ! -f "$pcap" ]]; then
    echo "error: no such file: $pcap" >&2
    exit 1
fi

# Known Limelight streams (5800/5802). The RC webcam is not here — it rides
# Robocol UDP. Add ports here if a known HTTP source moves.
http_ports="tcp.port == 5800 || tcp.port == 5802"

if command -v tshark >/dev/null 2>&1; then
    echo "=== Robocol (udp/20884) packet sizes ==="
    tshark -r "$pcap" -Y "udp.port == 20884" -T fields -e frame.time_relative -e ip.src -e ip.dst -e udp.length

    echo
    echo "=== Limelight HTTP requests (5800/5802) ==="
    echo "    the request URIs here are the candidate stream endpoints"
    tshark -r "$pcap" -Y "($http_ports) && http.request" -T fields -e frame.time_relative -e ip.src -e ip.dst -e tcp.dstport -e http.request.method -e http.request.uri

    echo
    echo "=== ...their responses ==="
    tshark -r "$pcap" -Y "($http_ports) && http.response" -T fields -e frame.time_relative -e ip.src -e ip.dst -e tcp.srcport -e http.response.code -e http.content_type

    echo
    echo "=== Limelight MJPEG multipart responses (confirms the 5800/5802 framing) ==="
    echo "    match a boundary here back to its request URI above = the feed's URL"
    tshark -r "$pcap" -Y "($http_ports) && http.content_type contains \"multipart\"" -T fields -e frame.time_relative -e ip.src -e ip.dst -e tcp.srcport -e http.content_type
else
    echo "tshark not found, falling back to tcpdump summaries (install tshark/wireshark for the detailed view)" >&2
    echo
    echo "=== Robocol (udp/20884) ==="
    tcpdump -r "$pcap" -nn 'udp port 20884'
    echo
    echo "=== Limelight (tcp/5800, tcp/5802) ==="
    tcpdump -r "$pcap" -nn -A 'tcp port 5800 or tcp port 5802'
fi
