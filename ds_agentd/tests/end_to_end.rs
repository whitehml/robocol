//! Drives the full `ds_agent` -> `ds_agentd` -> `fake_rc` stack the way a
//! human would drive `ds_cli` -> a real Control Hub, but scripted.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// `cargo test -p ds_agentd` only builds this crate's own binaries, so build
/// the two siblings the test drives before touching them.
fn build_sibling_bins() {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "ds_agent", "-p", "fake_rc"])
        .current_dir(workspace_root())
        .status()
        .expect("run cargo build for ds_agent/fake_rc");
    assert!(status.success(), "building ds_agent/fake_rc failed");
}

fn bin_path(name: &str) -> PathBuf {
    // CARGO_BIN_EXE_ds_agentd is set for this crate's own binary; sibling
    // binaries built by build_sibling_bins() share the same target/debug
    // dir, one level up from ds_agentd's own exe path.
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_ds_agentd"));
    path.pop();
    path.push(name);
    path
}

fn run_agent(socket_dir: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let output = Command::new(bin_path("ds_agent"))
        .args(args)
        .env("XDG_RUNTIME_DIR", socket_dir)
        .output()
        .expect("run ds_agent");
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "ds_agent {args:?} did not print JSON: {e}\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn drives_an_opmode_and_watches_telemetry_against_fake_rc() {
    build_sibling_bins();

    let tmp = tempdir();
    let fake_rc_port: u16 = 20950;

    let mut fake_rc = ChildGuard(
        Command::new(bin_path("fake_rc"))
            .arg(fake_rc_port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start fake_rc"),
    );

    let mut daemon = ChildGuard(
        Command::new(bin_path("ds_agentd"))
            .args([
                "--peer",
                "127.0.0.1",
                "--peer-port",
                &fake_rc_port.to_string(),
                "--bind-port",
                "0",
            ])
            .env("XDG_RUNTIME_DIR", &tmp)
            .env("DS_AGENT_TIMEOUT_MS", "2000")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start ds_agentd"),
    );

    // Wait for the daemon's socket to exist before sending anything.
    let socket_path = tmp.join("ds_agentd.sock");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !socket_path.exists() {
        assert!(Instant::now() < deadline, "ds_agentd socket never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Wait for the daemon to actually connect to fake_rc (discovery + a
    // couple of heartbeats) before issuing commands that depend on it.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let status = run_agent(&tmp, &["status"]);
        if status["connected"] == true {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ds_agentd never connected to fake_rc"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let list = run_agent(&tmp, &["list"]);
    assert_eq!(list["ok"], true);
    let opmodes = list["opmodes"].as_array().expect("opmodes array");
    assert!(opmodes.iter().any(|m| m["name"] == "Duo (TeleOp)"));

    let init = run_agent(&tmp, &["init", "Duo (TeleOp)"]);
    assert_eq!(init["ok"], true);
    assert_eq!(init["name"], "Duo (TeleOp)");

    let run = run_agent(&tmp, &["run"]); // no name: falls back to last_inited
    assert_eq!(run["ok"], true);
    assert_eq!(run["name"], "Duo (TeleOp)");

    let stop = run_agent(&tmp, &["stop"]);
    assert_eq!(stop["ok"], true);

    // Watch: fake_rc streams telemetry at 10 Hz, so a short-lived watch
    // process should see at least one line before it's killed.
    let mut watch = Command::new(bin_path("ds_agent"))
        .args(["watch", "--types", "telemetry"])
        .env("XDG_RUNTIME_DIR", &tmp)
        .stdout(Stdio::piped())
        .spawn()
        .expect("start ds_agent watch");
    let stdout = watch.stdout.take().expect("watch stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read a telemetry line");
    let event: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("watch line is JSON");
    assert_eq!(event["event"], "telemetry");
    let _ = watch.kill();
    let _ = watch.wait();

    // Explicit teardown via `daemon stop`, ahead of ChildGuard's Drop kill,
    // proves the shutdown command itself works.
    let stopped = run_agent(&tmp, &["daemon", "stop"]);
    assert_eq!(stopped["ok"], true);
    std::thread::sleep(Duration::from_millis(200));
    let _ = daemon.0.try_wait();
    let _ = fake_rc.0.kill();

    std::fs::remove_dir_all(&tmp).expect("clean up runtime dir");
}

fn tempdir() -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/ds_agent_e2e_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp runtime dir");
    dir
}
