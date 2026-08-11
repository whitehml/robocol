#!/usr/bin/env bash
# Start this BEFORE connecting a Driver Station to the Robot Controller, then
# drive a session in another terminal and Ctrl-C here. Writes a pcap and a
# decoded transcript, and prints the report of everything the robocol crate
# does not already understand.
#
#   ./capture/session.sh                 # capture, then decode on Ctrl-C
#   ./capture/session.sh --cli           # also run ds_cli here, logged
#   ./capture/session.sh --rc 192.168.49.1 --label new-sdk-11.2
#
# Uses dumpcap, which ships with wireshark and carries cap_net_raw, so this
# needs no sudo as long as you are in the `wireshark` group. Check with:
#   getcap "$(command -v dumpcap)" && id -nG | tr ' ' '\n' | grep -x wireshark

set -euo pipefail

rc_ip="192.168.43.1"
iface=""
label="session"
run_cli=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --rc) rc_ip="$2"; shift 2 ;;
        --iface) iface="$2"; shift 2 ;;
        --label) label="$2"; shift 2 ;;
        --cli) run_cli=1; shift ;;
        -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

if ! command -v dumpcap >/dev/null 2>&1; then
    echo "error: dumpcap not found. Install wireshark (or use tcpdump under sudo)." >&2
    exit 1
fi

# The interface that actually routes to the RC, so we never capture the wrong one.
if [[ -z "$iface" ]]; then
    iface="$(ip route get "$rc_ip" 2>/dev/null | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -1)"
fi
if [[ -z "$iface" ]]; then
    echo "error: no route to $rc_ip. Join the Control Hub's Wi-Fi first, or pass --iface." >&2
    exit 1
fi

stamp="$(date +%Y%m%d-%H%M%S)"
outdir="capture/captures/${stamp}-${label}"
mkdir -p "$outdir"
pcap="$outdir/session.pcap"

echo "building the decoder..."
cargo build -q -p pcap_decode

# Both peers use 20884; capturing the RC host as well picks up the Limelight's
# HTTP stream and anything else the RC opens, which is what you want when the
# question is "did something new appear".
filter="udp port 20884 or host $rc_ip"

echo
echo "interface : $iface"
echo "RC        : $rc_ip"
echo "output    : $outdir"
echo

dumpcap -i "$iface" -f "$filter" -w "$pcap" -q &
dumpcap_pid=$!
sleep 1

if ! kill -0 "$dumpcap_pid" 2>/dev/null; then
    echo "error: dumpcap exited immediately — check permissions on $iface" >&2
    exit 1
fi

finish() {
    trap - INT TERM EXIT
    echo
    echo "stopping capture..."
    kill -INT "$dumpcap_pid" 2>/dev/null || true
    wait "$dumpcap_pid" 2>/dev/null || true

    if [[ ! -s "$pcap" ]]; then
        echo "warning: $pcap is empty — nothing was captured." >&2
        exit 1
    fi

    ./target/debug/pcap_decode "$pcap" >"$outdir/decoded.txt" 2>&1 || true
    ./target/debug/pcap_decode "$pcap" --quiet >"$outdir/report.txt" 2>&1 || true
    ./target/debug/pcap_decode "$pcap" --all >"$outdir/decoded-full.txt" 2>&1 || true

    echo
    cat "$outdir/report.txt"
    echo
    echo "saved:"
    echo "  $pcap"
    echo "  $outdir/decoded.txt        transcript (heartbeats/gamepads/telemetry hidden)"
    echo "  $outdir/decoded-full.txt   transcript with everything"
    echo "  $outdir/report.txt         the summary above"
    [[ -f "$outdir/ds_cli.log" ]] && echo "  $outdir/ds_cli.log         ds_cli session log"
    exit 0
}
trap finish INT TERM EXIT

if [[ "$run_cli" == "1" ]]; then
    echo "running ds_cli (quit or Ctrl-D to end the session)"
    echo "suggested: opmodes / configs / init <name> / run <name> / stop"
    echo
    cargo run -q -p ds_cli -- "$rc_ip" 2>&1 | tee "$outdir/ds_cli.log" || true
else
    cat <<EOF
capturing. Now, in another terminal:

  1. connect the Driver Station (deck-station, ds_cli, or the stock DS app)
  2. let it reach the OpMode list
  3. INIT / START / STOP one OpMode of EACH category, the new one included
  4. open the config screen; activate a config
  5. open a camera view if you use one

then come back here and press Ctrl-C.

  note: only one client can bind UDP/20884 at a time, so do not run
        deck-station and ds_cli together.
EOF
    while true; do sleep 1; done
fi
