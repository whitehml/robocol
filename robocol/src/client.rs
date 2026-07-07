//! Threaded Robocol client: owns the UDP socket, runs discovery/heartbeat,
//! acks and retransmits commands, and surfaces everything as [`Event`]s on
//! an mpsc channel. Not an async runtime, one background thread.

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::cmd::{self, OpModeMeta};
use crate::packets::{Command, Gamepad, Heartbeat, Packet, PeerDiscovery};
use crate::types::RobotState;
use crate::{DEFAULT_PEER_ADDRS, ROBOCOL_PORT};

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub bind_port: u16,
    pub peer_addrs: Vec<IpAddr>,
    pub peer_port: u16,
    pub heartbeat_interval: Duration,
    pub disconnect_timeout: Duration,
    pub command_retry_interval: Duration,
    pub command_max_attempts: u32,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            bind_port: 0,
            peer_addrs: DEFAULT_PEER_ADDRS
                .iter()
                .filter_map(|a| a.parse().ok())
                .collect(),
            peer_port: ROBOCOL_PORT,
            heartbeat_interval: Duration::from_millis(200),
            disconnect_timeout: Duration::from_secs(3),
            command_retry_interval: Duration::from_millis(500),
            command_max_attempts: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Connected { peer: SocketAddr },
    Disconnected,
    RobotState(RobotState),
    Telemetry(crate::packets::Telemetry),
    OpModeList(Vec<OpModeMeta>),
    OpModeInited(String),
    OpModeRunning(String),
    ActiveConfiguration(String),
    ConfigurationList(String),
    Configuration(String),
    UserDeviceList(String),
    ScanResult(String),
    LynxModules(String),
    Stacktrace(String),
    Command { name: String, extra: String },
    CommandDropped { name: String },
    ProtocolError(String),
}

enum Control {
    SendCommand(Command),
    SendGamepad(Box<Gamepad>),
    Shutdown,
}

pub struct RobocolClient {
    ctrl: Sender<Control>,
    worker: Option<JoinHandle<()>>,
}

impl RobocolClient {
    pub fn start(config: ClientConfig) -> io::Result<(RobocolClient, Receiver<Event>)> {
        let socket = UdpSocket::bind(("0.0.0.0", config.bind_port))?;
        socket.set_read_timeout(Some(Duration::from_millis(25)))?;
        let (ctrl_tx, ctrl_rx) = channel();
        let (event_tx, event_rx) = channel();
        let worker = std::thread::Builder::new()
            .name("robocol".into())
            .spawn(move || {
                Worker {
                    cfg: config,
                    socket,
                    ctrl: ctrl_rx,
                    events: event_tx,
                    peer: None,
                    robot_state: None,
                    last_rx: Instant::now(),
                    last_beat: None,
                    seq: 0,
                    pending: Vec::new(),
                    seen: VecDeque::new(),
                }
                .run();
            })?;
        Ok((
            RobocolClient {
                ctrl: ctrl_tx,
                worker: Some(worker),
            },
            event_rx,
        ))
    }

    pub fn send_command(&self, name: &str, extra: &str) {
        let _ = self
            .ctrl
            .send(Control::SendCommand(Command::new(name, extra, 0)));
    }

    pub fn send_gamepad(&self, gamepad: Gamepad) {
        let _ = self.ctrl.send(Control::SendGamepad(Box::new(gamepad)));
    }

    pub fn request_opmode_list(&self) {
        self.send_command(cmd::REQUEST_OP_MODE_LIST, "");
    }

    pub fn init_opmode(&self, name: &str) {
        self.send_command(cmd::INIT_OP_MODE, name);
    }

    pub fn run_opmode(&self, name: &str) {
        self.send_command(cmd::RUN_OP_MODE, name);
    }

    /// The stock DS stops a running OpMode by initing the SDK's built-in
    /// idle OpMode.
    pub fn stop_opmode(&self) {
        self.init_opmode(cmd::DEFAULT_OP_MODE);
    }

    pub fn restart_robot(&self) {
        self.send_command(cmd::RESTART_ROBOT, "");
    }

    pub fn request_active_config(&self) {
        self.send_command(cmd::REQUEST_ACTIVE_CONFIG, "");
    }

    pub fn request_configurations(&self) {
        self.send_command(cmd::REQUEST_CONFIGURATIONS, "");
    }

    pub fn request_particular_configuration(&self, config: &cmd::ConfigMeta) {
        self.send_config_command(cmd::REQUEST_PARTICULAR_CONFIGURATION, config);
    }

    /// Writes the configuration to the RC's storage. Unlike ACTIVATE, this
    /// does not restart the robot — saved edits land on the hub but the
    /// running config is untouched until an explicit `activate_configuration`.
    pub fn save_configuration(&self, config_json: &str) {
        self.send_command(cmd::SAVE_CONFIGURATION, config_json);
    }

    /// Config activation only takes effect once the RC restarts; this
    /// client auto-sends CMD_RESTART_ROBOT once the RC acks the activate.
    pub fn activate_configuration(&self, config: &cmd::ConfigMeta) {
        self.send_config_command(cmd::ACTIVATE_CONFIGURATION, config);
    }

    pub fn delete_configuration(&self, config: &cmd::ConfigMeta) {
        self.send_config_command(cmd::DELETE_CONFIGURATION, config);
    }

    fn send_config_command(&self, command: &str, config: &cmd::ConfigMeta) {
        if let Ok(json) = serde_json::to_string(config) {
            self.send_command(command, &json);
        }
    }

    pub fn request_user_device_types(&self) {
        self.send_command(cmd::REQUEST_USER_DEVICE_TYPES, "");
    }

    pub fn scan(&self) {
        self.send_command(cmd::SCAN, "");
    }

    pub fn discover_lynx_modules(&self, serial: &str) {
        self.send_command(cmd::DISCOVER_LYNX_MODULES, serial);
    }
}

impl Drop for RobocolClient {
    fn drop(&mut self) {
        let _ = self.ctrl.send(Control::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct PendingCommand {
    command: Command,
    last_sent: Option<Instant>,
    attempts: u32,
}

struct Worker {
    cfg: ClientConfig,
    socket: UdpSocket,
    ctrl: Receiver<Control>,
    events: Sender<Event>,
    peer: Option<SocketAddr>,
    robot_state: Option<RobotState>,
    last_rx: Instant,
    last_beat: Option<Instant>,
    seq: u16,
    pending: Vec<PendingCommand>,
    seen: VecDeque<(String, i64)>,
}

impl Worker {
    fn run(mut self) {
        let mut buf = [0u8; 65536];
        loop {
            loop {
                match self.ctrl.try_recv() {
                    Ok(Control::Shutdown) | Err(TryRecvError::Disconnected) => return,
                    Ok(Control::SendCommand(command)) => self.queue_command(command),
                    Ok(Control::SendGamepad(gamepad)) => self.transmit_gamepad(*gamepad),
                    Err(TryRecvError::Empty) => break,
                }
            }

            match self.socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if !self.handle_datagram(&buf[..n], from) {
                        return; // event receiver dropped
                    }
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => {
                    if self
                        .events
                        .send(Event::ProtocolError(format!("socket recv failed: {e}")))
                        .is_err()
                    {
                        return; // event receiver dropped
                    }
                }
            }

            if self.peer.is_some() && self.last_rx.elapsed() > self.cfg.disconnect_timeout {
                self.peer = None;
                self.robot_state = None;
                if self.events.send(Event::Disconnected).is_err() {
                    return;
                }
            }

            if self
                .last_beat
                .is_none_or(|t| t.elapsed() >= self.cfg.heartbeat_interval)
            {
                self.last_beat = Some(Instant::now());
                self.tick();
            }
            if !self.retransmit_pending() {
                return;
            }
        }
    }

    fn tick(&mut self) {
        match self.peer {
            None => {
                let discovery = PeerDiscovery::default().serialize();
                for addr in &self.cfg.peer_addrs {
                    let _ = self.socket.send_to(&discovery, (*addr, self.cfg.peer_port));
                }
            }
            Some(peer) => {
                let heartbeat = Heartbeat {
                    seq: self.next_seq(),
                    timestamp: now_nanos(),
                    robot_state: RobotState::NotStarted,
                    t0: now_millis(),
                    ..Default::default()
                };
                let _ = self.socket.send_to(&heartbeat.serialize(), peer);
            }
        }
    }

    fn queue_command(&mut self, mut command: Command) {
        if command.timestamp == 0 {
            command.timestamp = now_nanos();
        }
        self.pending.push(PendingCommand {
            command,
            last_sent: None,
            attempts: 0,
        });
    }

    fn retransmit_pending(&mut self) -> bool {
        let Some(peer) = self.peer else { return true };
        let retry = self.cfg.command_retry_interval;
        let max_attempts = self.cfg.command_max_attempts;
        let mut dropped = Vec::new();
        let mut to_send = Vec::new();

        self.pending.retain_mut(|p| {
            if p.last_sent.is_some_and(|t| t.elapsed() < retry) {
                return true;
            }
            if p.attempts >= max_attempts {
                dropped.push(p.command.name.clone());
                return false;
            }
            p.attempts += 1;
            p.last_sent = Some(Instant::now());
            to_send.push(p.command.clone());
            true
        });

        for mut command in to_send {
            command.seq = self.next_seq();
            let _ = self.socket.send_to(&command.serialize(), peer);
        }
        for name in dropped {
            if self.events.send(Event::CommandDropped { name }).is_err() {
                return false;
            }
        }
        true
    }

    fn transmit_gamepad(&mut self, mut gamepad: Gamepad) {
        let Some(peer) = self.peer else { return };
        gamepad.seq = self.next_seq();
        if gamepad.timestamp == 0 {
            gamepad.timestamp = now_millis();
        }
        let _ = self.socket.send_to(&gamepad.serialize(), peer);
    }

    fn handle_datagram(&mut self, data: &[u8], from: SocketAddr) -> bool {
        if self.peer.is_some_and(|peer| peer != from) {
            return true;
        }
        self.last_rx = Instant::now();

        let packet = match Packet::parse(data) {
            Ok(p) => p,
            Err(e) => {
                return self
                    .events
                    .send(Event::ProtocolError(e.to_string()))
                    .is_ok()
            }
        };

        if self.peer.is_none() {
            self.peer = Some(from);
            if self.events.send(Event::Connected { peer: from }).is_err() {
                return false;
            }
            self.queue_command(Command::new(cmd::REQUEST_ACTIVE_CONFIG, "", 0));
            self.queue_command(Command::new(cmd::REQUEST_CONFIGURATIONS, "", 0));
            self.queue_command(Command::new(cmd::REQUEST_OP_MODE_LIST, "", 0));
        }

        match packet {
            Packet::PeerDiscovery(_) | Packet::Gamepad(_) | Packet::KeepAlive(_) => true,
            Packet::Heartbeat(hb) => self.update_robot_state(hb.robot_state),
            Packet::Telemetry(t) => {
                if !self.update_robot_state(t.robot_state) {
                    return false;
                }
                self.events.send(Event::Telemetry(t)).is_ok()
            }
            Packet::Command(c) => self.handle_command(c, from),
        }
    }

    fn update_robot_state(&mut self, state: RobotState) -> bool {
        if self.robot_state == Some(state) {
            return true;
        }
        self.robot_state = Some(state);
        self.events.send(Event::RobotState(state)).is_ok()
    }

    fn handle_command(&mut self, command: Command, from: SocketAddr) -> bool {
        if command.acknowledged {
            self.pending.retain(|p| {
                p.command.name != command.name || p.command.timestamp != command.timestamp
            });
            // A config change only takes effect on the RC after a restart.
            // LIBROBOCOL DEVIATION: the reference DS also restarts after a
            // SAVE; we restart only on an explicit ACTIVATE, so edits can be
            // saved to the hub without interrupting a running robot.
            if command.name == cmd::ACTIVATE_CONFIGURATION {
                self.queue_command(Command::new(cmd::RESTART_ROBOT, "", 0));
            }
            return true;
        }

        let ack = Command::ack_of(&command);
        let _ = self.socket.send_to(&ack.serialize(), from);

        let key = (command.name.clone(), command.timestamp);
        if self.seen.contains(&key) {
            return true;
        }
        self.seen.push_back(key);
        if self.seen.len() > 32 {
            self.seen.pop_front();
        }

        let event = match command.name.as_str() {
            cmd::NOTIFY_OP_MODE_LIST => Event::OpModeList(cmd::parse_opmode_list(&command.extra)),
            cmd::NOTIFY_INIT_OP_MODE => Event::OpModeInited(command.extra),
            cmd::NOTIFY_RUN_OP_MODE => Event::OpModeRunning(command.extra),
            cmd::NOTIFY_ACTIVE_CONFIGURATION => Event::ActiveConfiguration(command.extra),
            cmd::REQUEST_CONFIGURATIONS_RESP => Event::ConfigurationList(command.extra),
            cmd::REQUEST_PARTICULAR_CONFIGURATION_RESP => Event::Configuration(command.extra),
            cmd::NOTIFY_USER_DEVICE_LIST => Event::UserDeviceList(command.extra),
            cmd::SCAN_RESP => Event::ScanResult(command.extra),
            cmd::DISCOVER_LYNX_MODULES_RESP => Event::LynxModules(command.extra),
            cmd::SHOW_STACKTRACE => Event::Stacktrace(command.extra),
            _ => Event::Command {
                name: command.name,
                extra: command.extra,
            },
        };
        self.events.send(event).is_ok()
    }

    fn next_seq(&mut self) -> u16 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }
}

fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
