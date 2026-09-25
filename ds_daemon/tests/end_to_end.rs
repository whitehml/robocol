//! Drives the full `ds_agent` -> `ds_daemon` -> `fake_rc` stack the way a
//! human would drive `ds_cli` -> a real Control Hub, but scripted.
//!
//! The two tests here use different runtime dirs and UDP ports, so cargo may
//! run them in parallel.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
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

/// `cargo test -p ds_daemon` only builds this crate's own binaries, so build
/// the sibling crates a test drives before touching them.
fn build_bins(packages: &[&str]) {
    let mut cmd = Command::new(env!("CARGO"));
    cmd.arg("build");
    for p in packages {
        cmd.args(["-p", p]);
    }
    let status = cmd
        .current_dir(workspace_root())
        .status()
        .unwrap_or_else(|e| panic!("run cargo build for {packages:?}: {e}"));
    assert!(status.success(), "building {packages:?} failed");
}

fn bin_path(name: &str) -> PathBuf {
    // CARGO_BIN_EXE_ds_daemon is set for this crate's own binary; sibling
    // binaries built by build_bins() share the same target/debug dir, one
    // level up from ds_daemon's own exe path.
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_ds_daemon"));
    path.pop();
    path.push(name);
    path
}

fn run_agent_with_env(socket_dir: &Path, args: &[&str], env: &[(&str, &str)]) -> serde_json::Value {
    let output = Command::new(bin_path("ds_agent"))
        .args(args)
        .env("XDG_RUNTIME_DIR", socket_dir)
        .envs(env.iter().copied())
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

fn run_agent(socket_dir: &Path, args: &[&str]) -> serde_json::Value {
    run_agent_with_env(socket_dir, args, &[])
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/ds_agent_e2e_{tag}{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp runtime dir");
    dir
}

/// Polls `cond` every 50 ms until it holds or `limit` elapses.
fn wait_until(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn drives_an_opmode_and_watches_telemetry_against_fake_rc() {
    build_bins(&["ds_agent", "fake_rc"]);

    let tmp = tempdir("");
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
        Command::new(bin_path("ds_daemon"))
            .args([
                "--peer",
                "127.0.0.1",
                "--peer-port",
                &fake_rc_port.to_string(),
                "--bind-port",
                "0",
            ])
            .env("XDG_RUNTIME_DIR", &tmp)
            .env("DS_DAEMON_TIMEOUT_MS", "2000")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start ds_daemon"),
    );

    // Wait for the daemon's socket to exist before sending anything.
    let socket_path = tmp.join("ds_daemon.sock");
    assert!(
        wait_until(Duration::from_secs(3), || socket_path.exists()),
        "ds_daemon socket never appeared"
    );

    // Wait for the daemon to actually connect to fake_rc (discovery + a
    // couple of heartbeats) before issuing commands that depend on it.
    assert!(
        wait_until(
            Duration::from_secs(3),
            || run_agent(&tmp, &["status"])["connected"] == true
        ),
        "ds_daemon never connected to fake_rc"
    );

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
    // process should see at least one line before it's killed. Scoped in a
    // block so the ChildGuard drops (killing the watch) before `daemon
    // stop` below, and so a panic on any of the asserts still kills it
    // rather than leaking an orphan `ds_agent watch` process. The blocking
    // read lives on a helper thread so a silent daemon fails the test
    // within 5 s instead of hanging it; the kill-on-drop EOFs that thread.
    {
        let mut watch = ChildGuard(
            Command::new(bin_path("ds_agent"))
                .args(["watch", "--types", "telemetry"])
                .env("XDG_RUNTIME_DIR", &tmp)
                .stdout(Stdio::piped())
                .spawn()
                .expect("start ds_agent watch"),
        );
        let stdout = watch.0.stdout.take().expect("watch stdout");
        let (tx, rx) = mpsc::channel::<std::io::Result<String>>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let _ = tx.send(reader.read_line(&mut line).map(|_| line));
        });
        let line = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no telemetry line within 5s")
            .expect("read a telemetry line");
        let event: serde_json::Value =
            serde_json::from_str(line.trim_end()).expect("watch line is JSON");
        assert_eq!(event["event"], "telemetry");
    }

    // Explicit teardown via `shutdown`, ahead of ChildGuard's Drop kill,
    // proves the shutdown command itself works — and that the daemon
    // actually exits on it, not just acknowledges.
    let stopped = run_agent(&tmp, &["shutdown"]);
    assert_eq!(stopped["ok"], true);
    assert!(
        wait_until(Duration::from_secs(2), || matches!(
            daemon.0.try_wait(),
            Ok(Some(_))
        )),
        "daemon did not exit after `shutdown`"
    );
    let _ = fake_rc.0.kill();

    std::fs::remove_dir_all(&tmp).expect("clean up runtime dir");
}

/// Tears down whatever `ds_agent` auto-spawned even if the test panics
/// midway: the daemon is detached in its own process group, so nothing else
/// would reap it.
struct SpawnedDaemonGuard {
    dir: PathBuf,
    env: Vec<(&'static str, &'static str)>,
}

impl Drop for SpawnedDaemonGuard {
    fn drop(&mut self) {
        let _ = Command::new(bin_path("ds_agent"))
            .arg("shutdown")
            .env("XDG_RUNTIME_DIR", &self.dir)
            .envs(self.env.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn auto_spawns_daemon_logs_it_and_stops_it() {
    build_bins(&["ds_agent"]);

    let tmp = tempdir("spawn_");
    // Port 1: nothing answers, so no fake_rc is needed and nothing clashes
    // with the other test (or a running Deck Station on 20884).
    let env: Vec<(&str, &str)> = vec![(
        "DS_DAEMON_ARGS",
        "--peer 127.0.0.1 --peer-port 1 --bind-port 0",
    )];
    let guard = SpawnedDaemonGuard {
        dir: tmp.clone(),
        env: env.clone(),
    };

    // No daemon is running for this runtime dir, so this spawns one.
    let status = run_agent_with_env(&tmp, &["status"], &env);
    assert_eq!(status["ok"], true, "status reply: {status}");
    assert_eq!(status["connected"], false);

    let log = tmp.join("ds_daemon.log");
    assert!(
        log.exists(),
        "spawned daemon's log {} missing",
        log.display()
    );

    let ping = run_agent_with_env(&tmp, &["ping"], &env);
    assert_eq!(ping["running"], true);

    let stopped = run_agent_with_env(&tmp, &["shutdown"], &env);
    assert_eq!(stopped["ok"], true, "shutdown reply: {stopped}");

    let socket = tmp.join("ds_daemon.sock");
    assert!(
        wait_until(Duration::from_secs(2), || UnixStream::connect(&socket)
            .is_err()),
        "ds_daemon still accepting connections after `shutdown`"
    );

    drop(guard); // best-effort second stop is a no-op; removes the dir
    assert!(!tmp.exists(), "runtime dir not removed");
}
