//! Builds synthetic captures and runs the real binary over them. Covers the
//! two things that are easy to get silently wrong: IPv4 reassembly (large
//! OpMode lists always fragment) and the report's new-field/new-flavor alarms.

use std::process::Command as Proc;

use robocol::packets::{Command, PeerDiscovery, Telemetry};

const PORT: u16 = 20884;
const DS: [u8; 4] = [192, 168, 43, 100];
const RC: [u8; 4] = [192, 168, 43, 1];

fn ethernet(payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0u8; 12];
    frame.extend_from_slice(&0x0800u16.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn udp(src: [u8; 4], dst: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut datagram = Vec::new();
    datagram.extend_from_slice(&PORT.to_be_bytes());
    datagram.extend_from_slice(&PORT.to_be_bytes());
    datagram.extend_from_slice(&((payload.len() + 8) as u16).to_be_bytes());
    datagram.extend_from_slice(&0u16.to_be_bytes());
    datagram.extend_from_slice(payload);
    let mut fragments = ip_fragments(src, dst, &datagram, u16::MAX as usize);
    assert_eq!(
        fragments.len(),
        1,
        "unfragmented helper must emit one packet"
    );
    fragments.remove(0)
}

/// Splits `datagram` into IPv4 fragments of at most `mtu_payload` bytes each
/// (rounded down to an 8-byte boundary, as the fragment-offset field requires).
fn ip_fragments(src: [u8; 4], dst: [u8; 4], datagram: &[u8], mtu_payload: usize) -> Vec<Vec<u8>> {
    let step = (mtu_payload / 8).max(1) * 8;
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < datagram.len() {
        let end = (offset + step).min(datagram.len());
        let more = end < datagram.len();
        let chunk = &datagram[offset..end];

        let mut header = vec![0u8; 20];
        header[0] = 0x45;
        let total = (20 + chunk.len()) as u16;
        header[2..4].copy_from_slice(&total.to_be_bytes());
        header[4..6].copy_from_slice(&0x1234u16.to_be_bytes());
        let flags = if more { 0x2000 } else { 0 } | ((offset / 8) as u16);
        header[6..8].copy_from_slice(&flags.to_be_bytes());
        header[8] = 64;
        header[9] = 17;
        header[12..16].copy_from_slice(&src);
        header[16..20].copy_from_slice(&dst);
        header.extend_from_slice(chunk);
        out.push(header);
        offset = end;
    }
    out
}

fn fragmented_udp(src: [u8; 4], dst: [u8; 4], payload: &[u8], mtu: usize) -> Vec<Vec<u8>> {
    let mut datagram = Vec::new();
    datagram.extend_from_slice(&PORT.to_be_bytes());
    datagram.extend_from_slice(&PORT.to_be_bytes());
    datagram.extend_from_slice(&((payload.len() + 8) as u16).to_be_bytes());
    datagram.extend_from_slice(&0u16.to_be_bytes());
    datagram.extend_from_slice(payload);
    ip_fragments(src, dst, &datagram, mtu)
}

fn write_pcap(path: &std::path::Path, frames: &[Vec<u8>]) {
    let mut out = Vec::new();
    out.extend_from_slice(&0xa1b2c3d4u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&65535u32.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    for (i, frame) in frames.iter().enumerate() {
        out.extend_from_slice(&(1_700_000_000 + i as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        out.extend_from_slice(frame);
    }
    std::fs::write(path, out).unwrap();
}

fn run(frames: &[Vec<u8>], extra_args: &[&str]) -> String {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!("pcap_decode_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = dir.join(format!("{n}.pcap"));
    write_pcap(&path, frames);

    let out = Proc::new(env!("CARGO_BIN_EXE_pcap_decode"))
        .arg(&path)
        .args(extra_args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr)
}

/// An OpMode list as a hypothetical newer SDK might send it: a third flavor
/// and a field `OpModeMeta` has no home for.
const NEW_SDK_LIST: &str = r#"[
    {"name":"Blue Auto","flavor":"AUTONOMOUS","group":"comp"},
    {"name":"Main TeleOp","flavor":"TELEOP","group":"comp"},
    {"name":"Motor Tuner","flavor":"TUNING","group":"tuning","autoTransition":"Main TeleOp"}
]"#;

#[test]
fn reassembles_a_fragmented_opmode_list() {
    let big: Vec<String> = (0..80)
        .map(|i| format!(r#"{{"name":"OpMode{i}","flavor":"TELEOP","group":"g"}}"#))
        .collect();
    let extra = format!("[{}]", big.join(","));
    let packet = Command::new("CMD_NOTIFY_OP_MODE_LIST", &extra, 1).serialize();
    assert!(packet.len() > 1400, "test payload must exceed one MTU");

    let mut frames: Vec<Vec<u8>> = fragmented_udp(RC, DS, &packet, 1480)
        .into_iter()
        .map(|ip| ethernet(&ip))
        .collect();
    assert!(frames.len() > 1, "payload should have fragmented");
    frames.push(ethernet(&udp(RC, DS, &Telemetry::default().serialize())));

    let out = run(&frames, &[]);
    assert!(out.contains("reassembled"), "{out}");
    assert!(out.contains("80 entries in JSON, 80 survived"), "{out}");
}

#[test]
fn flags_new_flavor_and_dropped_field() {
    let frames = vec![
        ethernet(&udp(RC, DS, &Telemetry::default().serialize())),
        ethernet(&udp(
            RC,
            DS,
            &Command::new("CMD_NOTIFY_OP_MODE_LIST", NEW_SDK_LIST, 1).serialize(),
        )),
    ];
    let out = run(&frames, &[]);
    assert!(out.contains("\"TUNING\""), "{out}");
    assert!(out.contains("autoTransition"), "{out}");
    assert!(out.contains("3 entries in JSON, 3 survived"), "{out}");
}

#[test]
fn flags_unknown_command_and_version_skew() {
    let newer = PeerDiscovery {
        robocol_version: 125,
        sdk_minor_version: 2,
        ..PeerDiscovery::default()
    };

    let frames = vec![
        ethernet(&udp(RC, DS, &Telemetry::default().serialize())),
        ethernet(&udp(RC, DS, &newer.serialize())),
        ethernet(&udp(
            RC,
            DS,
            &Command::new("CMD_NOTIFY_SOMETHING_NEW", "{}", 2).serialize(),
        )),
    ];
    let out = run(&frames, &[]);
    assert!(out.contains("CMD_NOTIFY_SOMETHING_NEW"), "{out}");
    assert!(out.contains("not declared in robocol::cmd"), "{out}");
    assert!(out.contains("consider bumping"), "{out}");
}

#[test]
fn reports_when_the_list_shape_breaks_parsing() {
    let wrapped = r#"{"opModes":[{"name":"A","flavor":"TELEOP"}]}"#;
    let frames = vec![
        ethernet(&udp(RC, DS, &Telemetry::default().serialize())),
        ethernet(&udp(
            RC,
            DS,
            &Command::new("CMD_NOTIFY_OP_MODE_LIST", wrapped, 1).serialize(),
        )),
    ];
    let out = run(&frames, &[]);
    assert!(out.contains("not a JSON array"), "{out}");
}
