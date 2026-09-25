//! Drives the full `ds_agent` -> `ds_daemon` -> `fake_rc` stack the way a
//! human would drive `ds_cli` -> a real Control Hub, but scripted.
//!
//! The tests here use different runtime dirs and UDP ports, so cargo may run
//! them in parallel. Waits for a process to start are generous: on macOS the
//! first launch of a freshly built binary can take seconds while the system
//! assesses it, and a tight bound turns that into a false failure.

use std::io::{BufRead, BufReader, Write};
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

/// Longest a single `ds_agent` call may take in these tests.
const AGENT_LIMIT: Duration = Duration::from_secs(30);
/// Longest a process may take to start up (see the module doc).
const LAUNCH_LIMIT: Duration = Duration::from_secs(20);

/// Waits for `child` to exit and collects its output, killing it if that
/// takes longer than `limit`; `None` on timeout. A child that never finishes
/// must fail the test rather than hang the run: on macOS, a freshly built
/// binary can stall at launch while the system assesses it.
fn wait_within(child: Child, limit: Duration) -> Option<std::process::Output> {
    let pid = child.id();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(child.wait_with_output());
    });
    match done_rx.recv_timeout(limit) {
        Ok(output) => Some(output.expect("wait for a child process")),
        Err(_) => {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            None
        }
    }
}

/// Runs `command` with its output captured, failing the test if it takes
/// longer than `limit`.
fn output_within(mut command: Command, limit: Duration) -> std::process::Output {
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("start {command:?}: {e}"));
    wait_within(child, limit)
        .unwrap_or_else(|| panic!("{command:?} did not finish within {limit:?}"))
}

fn run_agent_with_env(socket_dir: &Path, args: &[&str], env: &[(&str, &str)]) -> serde_json::Value {
    let mut command = Command::new(bin_path("ds_agent"));
    command
        .args(args)
        .env("XDG_RUNTIME_DIR", socket_dir)
        .envs(env.iter().copied());
    let output = output_within(command, AGENT_LIMIT);
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

/// A test's private runtime dir (socket and log), under `/tmp` because
/// macOS caps a socket path near 104 bytes. Removed on drop, so a failing
/// test leaves nothing behind.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for TempDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
}

fn tempdir(tag: &str) -> TempDir {
    let dir = PathBuf::from(format!("/tmp/ds_agent_e2e_{tag}{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp runtime dir");
    TempDir(dir)
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

/// Starts `fake_rc` on `port`, with extra environment such as `FAKE_RC_BUSY`.
fn start_fake_rc(port: u16, env: &[(&str, &str)]) -> ChildGuard {
    ChildGuard(
        Command::new(bin_path("fake_rc"))
            .arg(port.to_string())
            .envs(env.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start fake_rc"),
    )
}

/// Starts a daemon pointed at a `fake_rc` on `rc_port`, serving `socket_dir`,
/// and waits for its socket. Binds UDP port 0, never the default 20884,
/// which a running Deck Station may hold.
fn start_daemon(socket_dir: &Path, rc_port: u16) -> ChildGuard {
    let daemon = ChildGuard(
        Command::new(bin_path("ds_daemon"))
            .args([
                "--peer",
                "127.0.0.1",
                "--peer-port",
                &rc_port.to_string(),
                "--bind-port",
                "0",
            ])
            .env("XDG_RUNTIME_DIR", socket_dir)
            .env("DS_DAEMON_TIMEOUT_MS", "2000")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start ds_daemon"),
    );
    let socket = socket_dir.join("ds_daemon.sock");
    assert!(
        wait_until(LAUNCH_LIMIT, || socket.exists()),
        "ds_daemon socket never appeared"
    );
    daemon
}

/// Waits for the daemon to finish discovery with its RC.
fn wait_connected(socket_dir: &Path) {
    assert!(
        wait_until(
            LAUNCH_LIMIT,
            || run_agent(socket_dir, &["status"])["connected"] == true
        ),
        "ds_daemon never connected to fake_rc"
    );
}

/// `shutdown`, and proof that the daemon process actually exits on it.
fn shut_down(socket_dir: &Path, daemon: &mut ChildGuard) {
    let stopped = run_agent(socket_dir, &["shutdown"]);
    assert_eq!(stopped["ok"], true, "shutdown reply: {stopped}");
    assert!(
        wait_until(Duration::from_secs(2), || matches!(
            daemon.0.try_wait(),
            Ok(Some(_))
        )),
        "daemon did not exit after `shutdown`"
    );
}

/// Runs `ds_agent watch <args>` and streams its JSON lines into a channel,
/// so a test can wait for particular events with a timeout. The guard kills
/// the watch, which ends the reader thread.
fn watch_events(
    socket_dir: &Path,
    args: &[&str],
) -> (ChildGuard, mpsc::Receiver<serde_json::Value>) {
    let mut watch = ChildGuard(
        Command::new(bin_path("ds_agent"))
            .arg("watch")
            .args(args)
            .env("XDG_RUNTIME_DIR", socket_dir)
            .stdout(Stdio::piped())
            .spawn()
            .expect("start ds_agent watch"),
    );
    let stdout = watch.0.stdout.take().expect("watch stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(event) = serde_json::from_str(&line)
                && tx.send(event).is_err()
            {
                break;
            }
        }
    });
    (watch, rx)
}

/// The first event within `limit` that `wanted` accepts, skipping others.
fn next_event(
    events: &mpsc::Receiver<serde_json::Value>,
    limit: Duration,
    wanted: impl Fn(&serde_json::Value) -> bool,
) -> Option<serde_json::Value> {
    let deadline = Instant::now() + limit;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match events.recv_timeout(left) {
            Ok(event) if wanted(&event) => return Some(event),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Starts `ds_agent <args>` in the background with its output captured,
/// for requests the test acts on while they run.
fn spawn_agent(socket_dir: &Path, args: &[&str]) -> Child {
    Command::new(bin_path("ds_agent"))
        .args(args)
        .env("XDG_RUNTIME_DIR", socket_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start ds_agent")
}

/// A raw operator session on the daemon's socket, standing in for `ds_cli`
/// (which has its own test).
struct Operator {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Operator {
    fn connect(socket_dir: &Path) -> Operator {
        let stream =
            UnixStream::connect(socket_dir.join("ds_daemon.sock")).expect("connect as operator");
        stream
            .set_read_timeout(Some(AGENT_LIMIT))
            .expect("bound the operator's reads");
        let mut operator = Operator {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        };
        let hello = operator.send(serde_json::json!({"cmd": "hello", "role": "operator"}));
        assert_eq!(hello["ok"], true, "hello: {hello}");
        operator
    }

    fn send(&mut self, request: serde_json::Value) -> serde_json::Value {
        writeln!(self.writer, "{request}").expect("write to the daemon");
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read the reply");
        serde_json::from_str(&line).expect("reply is JSON")
    }
}

#[test]
fn drives_an_opmode_and_watches_telemetry_against_fake_rc() {
    build_bins(&["ds_agent", "fake_rc"]);

    let tmp = tempdir("");
    let fake_rc_port: u16 = 20950;
    let mut fake_rc = start_fake_rc(fake_rc_port, &[]);
    let mut daemon = start_daemon(&tmp, fake_rc_port);
    wait_connected(&tmp);

    let list = run_agent(&tmp, &["list"]);
    assert_eq!(list["ok"], true);
    let opmodes = list["opmodes"].as_array().expect("opmodes array");
    assert!(opmodes.iter().any(|m| m["name"] == "Duo (TeleOp)"));

    let init = run_agent(&tmp, &["init", "Duo (TeleOp)"]);
    assert_eq!(init["ok"], true);
    assert_eq!(init["name"], "Duo (TeleOp)");

    // Each ds_agent call is its own connection, so a bare `run` has no
    // `init` to fall back on and must name the opmode.
    let bare = run_agent(&tmp, &["run"]);
    assert_eq!(bare["error"], "bad_args", "bare run: {bare}");
    let run = run_agent(&tmp, &["run", "Duo (TeleOp)"]);
    assert_eq!(run["ok"], true);
    assert_eq!(run["name"], "Duo (TeleOp)");

    let stop = run_agent(&tmp, &["stop"]);
    assert_eq!(stop["ok"], true);

    let mut pretty = Command::new(bin_path("ds_agent"));
    pretty
        .args(["--pretty", "status"])
        .env("XDG_RUNTIME_DIR", &tmp);
    let pretty = output_within(pretty, AGENT_LIMIT);
    let text = String::from_utf8_lossy(&pretty.stdout);
    assert!(text.contains("connected: true"), "--pretty status: {text}");

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
            .recv_timeout(LAUNCH_LIMIT)
            .expect("no telemetry line within 5s")
            .expect("read a telemetry line");
        let event: serde_json::Value =
            serde_json::from_str(line.trim_end()).expect("watch line is JSON");
        assert_eq!(event["event"], "telemetry");
    }

    // Explicit teardown via `shutdown`, ahead of ChildGuard's Drop kill,
    // proves the shutdown command itself works.
    shut_down(&tmp, &mut daemon);
    let _ = fake_rc.0.kill();
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
        let shutdown = Command::new(bin_path("ds_agent"))
            .arg("shutdown")
            .env("XDG_RUNTIME_DIR", &self.dir)
            .envs(self.env.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(child) = shutdown {
            let _ = wait_within(child, AGENT_LIMIT);
        }
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
        dir: tmp.to_path_buf(),
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

#[test]
fn a_daemon_that_dies_on_startup_is_explained() {
    build_bins(&["ds_agent"]);

    let tmp = tempdir("dead_");
    // Hold a UDP port so the daemon ds_agent starts cannot bind it: the most
    // common real failure (Deck Station already running on 20884).
    let held = std::net::UdpSocket::bind(("0.0.0.0", 0)).expect("hold a UDP port");
    let port = held.local_addr().unwrap().port();
    let args = format!("--peer 127.0.0.1 --peer-port 1 --bind-port {port}");

    let mut status = Command::new(bin_path("ds_agent"));
    status
        .arg("status")
        .env("XDG_RUNTIME_DIR", &tmp)
        .env("DS_DAEMON_ARGS", &args);
    let output = output_within(status, AGENT_LIMIT);
    assert_eq!(output.status.code(), Some(1));
    let reply: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ds_agent printed JSON");
    assert_eq!(reply["error"], "daemon_unreachable");
    let message = reply["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("cannot bind UDP port {port}")),
        "the reason should reach ds_agent's own output: {message}"
    );
    assert!(
        !tmp.join("ds_daemon.sock").exists(),
        "a daemon that failed to start left its socket behind"
    );
}

#[test]
fn a_busy_rc_is_reported_until_connect() {
    build_bins(&["ds_agent", "fake_rc"]);

    let tmp = tempdir("busy_");
    let fake_rc_port: u16 = 20951;
    // The daemon first, so its discovery is already running when fake_rc
    // starts; fake_rc then acts owned by another driver station for its
    // first 3 s, counted from when it actually runs.
    let mut daemon = start_daemon(&tmp, fake_rc_port);
    let mut fake_rc = start_fake_rc(fake_rc_port, &[("FAKE_RC_BUSY", "3")]);

    assert!(
        wait_until(LAUNCH_LIMIT, || {
            !run_agent(&tmp, &["status"])["peer_busy"].is_null()
        }),
        "the daemon never reported the busy RC"
    );
    let list = run_agent(&tmp, &["list"]);
    assert_eq!(list["error"], "peer_busy", "list: {list}");

    // Discovery stays paused after the other driver station goes away: the
    // daemon waits to be told before trying again. The window began before
    // the busy report above, so it is over 3.5 s after it.
    std::thread::sleep(Duration::from_millis(3500));
    assert_eq!(run_agent(&tmp, &["status"])["connected"], false);

    let connect = run_agent(&tmp, &["connect"]);
    assert_eq!(connect["ok"], true, "connect: {connect}");
    wait_connected(&tmp);
    assert!(run_agent(&tmp, &["status"])["peer_busy"].is_null());

    shut_down(&tmp, &mut daemon);
    let _ = fake_rc.0.kill();
}

#[test]
fn gamepad_ownership_and_pulses_against_fake_rc() {
    build_bins(&["ds_agent", "fake_rc"]);

    let tmp = tempdir("pads_");
    let fake_rc_port: u16 = 20952;
    let mut fake_rc = start_fake_rc(fake_rc_port, &[]);
    let mut daemon = start_daemon(&tmp, fake_rc_port);
    wait_connected(&tmp);

    // fake_rc echoes what it last received on each pad in its telemetry.
    let (watch, events) = watch_events(
        &tmp,
        &[
            "--types",
            "telemetry,gamepad_owner",
            "--tag",
            "TELEMETRY_DATA",
            "--key",
            "gamepad2_left_stick_y",
        ],
    );
    let stick2 =
        |value: f64| move |e: &serde_json::Value| e["numbers"]["gamepad2_left_stick_y"] == value;
    let owner_event = |owner: &'static str, previous: serde_json::Value| {
        move |e: &serde_json::Value| {
            e["event"] == "gamepad_owner"
                && e["user"] == 2
                && e["owner"] == owner
                && e["previous"] == previous
        }
    };

    // Telemetry arriving proves the watch has subscribed, so no event
    // below can slip past it.
    assert!(
        next_event(&events, LAUNCH_LIMIT, |e| e["event"] == "telemetry").is_some(),
        "the watch never started"
    );

    let claimed = run_agent(&tmp, &["claim", "gamepad2"]);
    assert_eq!(
        claimed,
        serde_json::json!({"ok": true, "held": ["gamepad2"]})
    );
    assert!(
        next_event(
            &events,
            Duration::from_secs(3),
            owner_event("agent", serde_json::Value::Null)
        )
        .is_some()
    );

    let pulse = run_agent(
        &tmp,
        &[
            "gamepad",
            "--user",
            "2",
            "--left-stick-y",
            "-0.5",
            "--duration",
            "600ms",
        ],
    );
    assert_eq!(
        pulse,
        serde_json::json!({"ok": true, "user": 2, "packets": 30})
    );
    assert!(
        next_event(&events, Duration::from_secs(3), stick2(-0.5)).is_some(),
        "fake_rc never saw the pulse"
    );
    assert!(
        next_event(&events, Duration::from_secs(3), stick2(0.0)).is_some(),
        "the stick never came back to rest after the pulse"
    );

    // The operator takes the pad in the middle of a long agent pulse: the
    // pulse ends early with `preempted`, and the pad goes back to rest.
    let long_pulse = Command::new(bin_path("ds_agent"))
        .args([
            "gamepad",
            "--user",
            "2",
            "--left-stick-y",
            "0.75",
            "--duration",
            "5s",
        ])
        .env("XDG_RUNTIME_DIR", &tmp)
        .stdout(Stdio::piped())
        .spawn()
        .expect("start a long pulse");
    assert!(
        next_event(&events, Duration::from_secs(3), stick2(0.75)).is_some(),
        "fake_rc never saw the long pulse"
    );
    let mut operator = Operator::connect(&tmp);
    let took = operator.send(serde_json::json!({"cmd": "claim", "pads": ["gamepad1", "gamepad2"]}));
    assert_eq!(
        took["held"],
        serde_json::json!(["gamepad1", "gamepad2"]),
        "claim: {took}"
    );
    let output = wait_within(long_pulse, Duration::from_secs(2))
        .expect("the agent's pulse kept running after the operator took the pad");
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON reply");
    assert_eq!(reply["error"], "preempted", "long pulse: {reply}");
    assert!(
        next_event(
            &events,
            Duration::from_secs(3),
            owner_event("operator", "agent".into())
        )
        .is_some()
    );
    assert!(
        next_event(&events, Duration::from_secs(3), stick2(0.0)).is_some(),
        "the operator inherited a pushed stick"
    );

    // While the operator holds the pads the agent is refused, and says so.
    let refused = run_agent(&tmp, &["gamepad", "--user", "2", "--a", "true"]);
    assert_eq!(refused["error"], "preempted", "{refused}");
    assert_eq!(
        run_agent(&tmp, &["claim", "gamepad2"])["error"],
        "preempted"
    );

    let released = operator.send(serde_json::json!({"cmd": "release"}));
    assert_eq!(
        released["released"],
        serde_json::json!(["gamepad1", "gamepad2"])
    );
    assert_eq!(run_agent(&tmp, &["claim", "gamepad2"])["ok"], true);

    drop(operator);
    drop(watch);
    shut_down(&tmp, &mut daemon);
    let _ = fake_rc.0.kill();
}

#[test]
fn ds_cli_is_an_operator_on_the_same_daemon() {
    build_bins(&["ds_agent", "ds_cli"]);

    let tmp = tempdir("cli_");
    // Port 1: nothing answers, so no fake_rc is needed.
    let env: Vec<(&str, &str)> = vec![(
        "DS_DAEMON_ARGS",
        "--peer 127.0.0.1 --peer-port 1 --bind-port 0",
    )];
    let _daemon = SpawnedDaemonGuard {
        dir: tmp.to_path_buf(),
        env: env.clone(),
    };

    // Piped input: ds_cli reads lines without a terminal.
    let mut cli = Command::new(bin_path("ds_cli"))
        .arg("--json")
        .env("XDG_RUNTIME_DIR", &tmp)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start ds_cli");
    {
        let mut stdin = cli.stdin.take().expect("ds_cli stdin");
        for line in [
            "status",
            "claim gamepad1",
            "status",
            "init",
            "watch --types nope",
            "quit",
        ] {
            writeln!(stdin, "{line}").expect("type into ds_cli");
        }
    }
    let output = wait_within(cli, AGENT_LIMIT).expect("ds_cli did not quit");
    assert!(
        output.status.success(),
        "ds_cli exited with {}",
        output.status
    );

    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert!(
        lines
            .iter()
            .any(|v| v["ok"] == true && v["connected"] == false),
        "no status reply in {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|v| v["gamepads"]["gamepad1"] == "operator"),
        "the claim was not the operator's while ds_cli ran: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|v| v["held"] == serde_json::json!(["gamepad1"])),
        "no claim reply in {lines:?}"
    );
    assert_eq!(
        lines.iter().filter(|v| v["error"] == "bad_args").count(),
        2,
        "`init` without a name and a bad watch filter: {lines:?}"
    );

    // Quitting ds_cli, the last operator, hands its pads back, so the agent
    // is not locked out by a claim nobody is left to release.
    let status = run_agent_with_env(&tmp, &["status"], &env);
    assert_eq!(
        status["gamepads"]["gamepad1"],
        serde_json::Value::Null,
        "{status}"
    );
}

#[test]
fn gamepad_handovers_survive_packet_loss() {
    build_bins(&["ds_agent", "fake_rc"]);

    let tmp = tempdir("lossy_");
    let fake_rc_port: u16 = 20953;
    // fake_rc drops a fifth of the gamepad packets it receives: every way a
    // pad returns to rest must survive losing some of them.
    let mut fake_rc = start_fake_rc(fake_rc_port, &[("FAKE_RC_GAMEPAD_LOSS", "0.2")]);
    let mut daemon = start_daemon(&tmp, fake_rc_port);
    wait_connected(&tmp);

    let (watch, events) = watch_events(
        &tmp,
        &[
            "--types",
            "telemetry",
            "--tag",
            "TELEMETRY_DATA",
            "--key",
            "gamepad2_left_stick_y",
        ],
    );
    assert!(
        next_event(&events, LAUNCH_LIMIT, |e| e["event"] == "telemetry").is_some(),
        "the watch never started"
    );
    let stick2 =
        |value: f64| move |e: &serde_json::Value| e["numbers"]["gamepad2_left_stick_y"] == value;
    let pushed = || {
        assert!(
            next_event(&events, Duration::from_secs(3), stick2(0.75)).is_some(),
            "fake_rc never saw the pulse"
        )
    };
    let at_rest = |after: &str| {
        assert!(
            next_event(&events, Duration::from_secs(3), stick2(0.0)).is_some(),
            "the pad stayed pushed after {after}"
        )
    };
    let long_pulse = [
        "gamepad",
        "--user",
        "2",
        "--left-stick-y",
        "0.75",
        "--duration",
        "5s",
    ];

    assert_eq!(run_agent(&tmp, &["claim", "gamepad2"])["ok"], true);

    // A pulse that runs its course ends with its neutral tail.
    let pulse = run_agent(
        &tmp,
        &[
            "gamepad",
            "--user",
            "2",
            "--left-stick-y",
            "0.75",
            "--duration",
            "300ms",
        ],
    );
    assert_eq!(pulse["ok"], true, "{pulse}");
    pushed();
    at_rest("a completed pulse");

    // The operator takes the pad mid-pulse.
    let taken = spawn_agent(&tmp, &long_pulse);
    pushed();
    let mut operator = Operator::connect(&tmp);
    let took = operator.send(serde_json::json!({"cmd": "claim", "pads": ["gamepad2"]}));
    assert_eq!(took["ok"], true, "{took}");
    at_rest("the operator's takeover");
    wait_within(taken, AGENT_LIMIT).expect("the preempted pulse ended");

    // The operator leaving hands the pad back to whoever claims it next.
    drop(operator);
    assert!(
        wait_until(Duration::from_secs(3), || run_agent(
            &tmp,
            &["claim", "gamepad2"]
        )["ok"]
            == true),
        "the pad stayed with an operator that had left"
    );

    // A client that dies mid-pulse does not leave the pad driving.
    let mut dying = spawn_agent(&tmp, &long_pulse);
    pushed();
    let _ = dying.kill();
    let _ = dying.wait();
    at_rest("its client died mid-pulse");

    // Releasing mid-pulse returns the pad to rest too.
    let released = spawn_agent(&tmp, &long_pulse);
    pushed();
    let reply = run_agent(&tmp, &["release", "gamepad2"]);
    assert_eq!(
        reply["released"],
        serde_json::json!(["gamepad2"]),
        "{reply}"
    );
    at_rest("a release mid-pulse");
    wait_within(released, AGENT_LIMIT).expect("the released pulse ended");

    drop(watch);
    shut_down(&tmp, &mut daemon);
    let _ = fake_rc.0.kill();
}
