//! Fake Robot Controller for end-to-end testing without hardware.
//!
//! Answers discovery, serves an OpMode list, echoes INIT/RUN notifies,
//! streams telemetry at 10 Hz, and implements an interactive INIT-phase
//! pattern: pressing gamepad X toggles alliance, Y toggles start position,
//! echoed back through telemetry.
//!
//! It also runs an always-on Limelight MJPEG-over-HTTP stand-in on :5800, and
//! emulates the RC webcam's Robocol-native `CameraStreamServer` protocol
//! whenever an OpMode is init'd/running.
//!
//! ```sh
//! cargo run -p fake_rc              # binds 127.0.0.1:20884
//! cargo run -p fake_rc 20900        # custom port
//! ```
//! Point the Godot client at it with:
//! `DECK_DS_PEER=127.0.0.1 DECK_DS_BIND_PORT=0 godot4 --path godot`
//! (plus `DECK_DS_PEER_PORT=<port>` if not 20884).

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use jpeg_encoder::{ColorType, Encoder};
use robocol::ROBOCOL_PORT;
use robocol::cmd::{self, ConfigMeta};
use robocol::packets::{
    BATTERY_LEVEL_KEY, Command, Packet, PeerDiscovery, RC_BATTERY_STATUS_KEY, SYSTEM_KEY_PREFIX,
    Telemetry,
};
use robocol::types::RobotState;

const LIMELIGHT_PORT: u16 = 5800;
const CAM_WIDTH: u16 = 320;
const CAM_HEIGHT: u16 = 240;
const CAM_FRAME_INTERVAL: Duration = Duration::from_millis(83);

const OPMODES: &str = r#"[
    {"name":"$Stop$Robot$","flavor":"SYSTEM","group":"$$$$$$$","source":"BUILTIN","description":"","systemOpModeBaseDisplayName":"Stop Robot"},
    {"name":"$Configure$Robot$","flavor":"SYSTEM","group":"$$$$$$$","source":"BUILTIN","description":"","systemOpModeBaseDisplayName":"Configure Robot"},
    {"name":"Test Hardware (Built-in)","flavor":"UTILITY","group":"$$$$$$$","source":"BUILTIN","description":"Exercise each configured motor and servo","systemOpModeBaseDisplayName":"Test Hardware"},
    {"name":"Test Gamepad (Built-in)","flavor":"UTILITY","group":"$$$$$$$","source":"BUILTIN","description":"Show live gamepad input","systemOpModeBaseDisplayName":"Test Gamepad"},
    {"name":"Duo (TeleOp)","flavor":"TELEOP","group":"drive","source":"ANDROID_STUDIO","description":""},
    {"name":"Solo (TeleOp)","flavor":"TELEOP","group":"drive","source":"ANDROID_STUDIO","description":""},
    {"name":"Auto: Sand Run","flavor":"AUTONOMOUS","group":"auto","source":"ANDROID_STUDIO","description":""},
    {"name":"Crash Test","flavor":"TELEOP","group":"$$$$$$$","source":"ANDROID_STUDIO","description":""}
]"#;

/// Throws CRASH_DELAY after START, the way a real OpMode dies mid-loop.
const CRASH_OPMODE: &str = "Crash Test";
const CRASH_DELAY: Duration = Duration::from_secs(2);
const CRASH_STACKTRACE: &str = "java.lang.NullPointerException: Attempt to invoke virtual method \
'void com.qualcomm.robotcore.hardware.DcMotor.setPower(double)' on a null object reference\n\
\tat org.firstinspires.ftc.teamcode.CrashTest.loop(CrashTest.java:42)\n\
\tat com.qualcomm.robotcore.eventloop.opmode.OpModeManagerImpl.runActiveOpMode\
(OpModeManagerImpl.java:475)\n\
\tat com.qualcomm.robotcore.eventloop.opmode.FtcEventLoop.loop(FtcEventLoop.java:180)";

/// Canned device XML served for CMD_REQUEST_PARTICULAR_CONFIGURATION unless a
/// config has been saved (see `saved_xml`). The RC sends raw device XML here,
/// not JSON (see `cmd::ConfigMeta` docs). A realistic multi-hub tree so the
/// config editor has motors/servos/I2C/ethernet/webcam to parse.
const FAKE_CONFIG_XML: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>
<Robot type="FirstInspires-FTC">
    <LynxUsbDevice name="Control Hub Portal" serialNumber="(embedded)" parentModuleAddress="173">
        <LynxModule name="Control Hub" port="173">
            <goBILDA5202SeriesMotor name="front_right_motor" port="0" />
            <goBILDA5202SeriesMotor name="front_left_motor" port="1" />
            <goBILDA5202SeriesMotor name="back_right_motor" port="2" />
            <goBILDA5202SeriesMotor name="back_left_motor" port="3" />
            <Servo name="sortServo" port="5" />
            <ControlHubImuBHI260AP name="imu" port="0" bus="0" />
            <REV_VL53L0X_RANGE_SENSOR name="intakeDistance" port="0" bus="2" />
        </LynxModule>
        <LynxModule name="Expansion Hub 2" port="2">
            <goBILDA5202SeriesMotor name="intake" port="0" />
            <Servo name="kickoutServo" port="2" />
            <SparkFunOTOS name="otos" port="0" bus="0" />
        </LynxModule>
    </LynxUsbDevice>
    <Webcam name="Webcam 1" serialNumber="REPLACE_WITH_USB_SERIAL" />
</Robot>"#;

/// Canned CMD_SCAN_RESP: a scanned hardware tree that includes an
/// EthernetDevice.
/// Canned CMD_SCAN_RESP. A scan reports attached *USB* devices only — serial
/// number and coarse type — never config XML. Shape is
/// `ScannedDevices.toSerializationString()`: a GSON dump of
/// `Map<SerialNumber, DeviceManager.UsbDeviceType>` in the complex-key entry
/// form, with `errorMessage` omitted while null. Verified by running RobotCore
/// 11.2.1 against the gson 2.8.0 it bundles.
const SCAN_RESP: &str = concat!(
    r#"{"map":[{"key":"(embedded)","value":"LYNX_USB_DEVICE"},"#,
    r#"{"key":"3E425A6F","value":"WEBCAM"},"#,
    r#"{"key":"EthernetOverUsb:eth0:172.29.0.24","value":"ETHERNET_DEVICE"}]}"#
);

/// Canned CMD_NOTIFY_USER_DEVICE_LIST. Shape matches the RC's own
/// `ConfigurationTypeManager.serializeUserDeviceTypes()`: a flat GSON array
/// whose `flavor` doubles as the RuntimeTypeAdapterFactory type label, so
/// annotation-registered types carry their real DeviceFlavor while
/// BuiltInConfigurationType members collapse to {xmlTag, name, "BUILT_IN"}.
const USER_DEVICE_TYPES: &str = r#"[
    {"name":"goBILDA 5202/3/4 series","flavor":"MOTOR","xmlTag":"goBILDA5202SeriesMotor","builtIn":true,"isDeprecated":false,"classSource":"APK"},
    {"name":"REV Core Hex Motor","flavor":"MOTOR","xmlTag":"RevRoboticsCoreHexMotor","builtIn":true,"isDeprecated":false,"classSource":"APK"},
    {"name":"REV HD Hex Motor 20:1","flavor":"MOTOR","xmlTag":"RevRobotics20HDHexMotor","builtIn":true,"isDeprecated":false,"classSource":"APK"},
    {"name":"Servo","flavor":"SERVO","xmlTag":"Servo","builtIn":true,"isDeprecated":false,"classSource":"APK"},
    {"name":"Continuous Rotation Servo","flavor":"SERVO","xmlTag":"ContinuousRotationServo","builtIn":true,"isDeprecated":false,"classSource":"APK"},
    {"name":"Control Hub IMU (BHI260AP)","flavor":"I2C","xmlTag":"ControlHubImuBHI260AP","builtIn":true,"isDeprecated":false,"classSource":"APK"},
    {"name":"REV 2M Distance Sensor","flavor":"I2C","xmlTag":"REV_VL53L0X_RANGE_SENSOR","builtIn":true,"isDeprecated":false,"classSource":"APK"},
    {"name":"SparkFun OTOS","flavor":"I2C","xmlTag":"SparkFunOTOS","builtIn":false,"isDeprecated":false,"classSource":"APK"},
    {"xmlTag":"LynxColorSensor","name":"REV Color/Range Sensor","flavor":"BUILT_IN"},
    {"xmlTag":"Webcam","name":"Webcam","flavor":"BUILT_IN"}
]"#;

struct FakeRc {
    socket: UdpSocket,
    ds: Option<SocketAddr>,
    state: RobotState,
    opmode: String,
    alliance: &'static str,
    start_position: &'static str,
    prev_x: bool,
    prev_y: bool,
    left_stick_y: f32,
    seq: u16,
    started: Instant,
    run_started: Option<Instant>,
    configs: Vec<ConfigMeta>,
    active_config: ConfigMeta,
    /// Device XML saved per config name, served back on a particular-config
    /// request so editor round-trips work; unsaved configs fall back to
    /// FAKE_CONFIG_XML.
    saved_xml: HashMap<String, String>,
    next_resource_id: i64,
    /// Set by CMD_ACTIVATE_CONFIGURATION, applied on the restart that
    /// follows — matches real hardware, where activation only takes
    /// effect after a restart.
    pending_activation: Option<ConfigMeta>,
    webcam_available: bool,
    webcam_frame_num: i32,
}

impl FakeRc {
    fn next_seq(&mut self) -> u16 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    fn send(&mut self, bytes: &[u8]) {
        if let Some(ds) = self.ds {
            let _ = self.socket.send_to(bytes, ds);
        }
    }

    fn send_command(&mut self, name: &str, extra: &str) {
        let mut command = Command::new(name, extra, self.started.elapsed().as_nanos() as i64);
        command.seq = self.next_seq();
        self.send(&command.serialize());
        println!(">> {name} {extra}");
    }

    fn handle_command(&mut self, command: Command) {
        if command.acknowledged {
            return;
        }
        let ack = Command::ack_of(&command);
        self.send(&ack.serialize());
        println!("<< {} {}", command.name, command.extra);
        match command.name.as_str() {
            cmd::REQUEST_OP_MODE_LIST => self.send_command(cmd::NOTIFY_OP_MODE_LIST, OPMODES),
            cmd::INIT_OP_MODE => {
                self.run_started = None;
                if command.extra == cmd::DEFAULT_OP_MODE {
                    self.state = RobotState::Stopped;
                    self.opmode.clear();
                    self.set_webcam_available(false);
                } else {
                    self.state = RobotState::Init;
                    self.opmode = command.extra.clone();
                    self.set_webcam_available(true);
                }
                self.send_command(cmd::NOTIFY_INIT_OP_MODE, &command.extra);
            }
            cmd::RUN_OP_MODE => {
                self.state = RobotState::Running;
                self.opmode = command.extra.clone();
                self.run_started = Some(Instant::now());
                self.send_command(cmd::NOTIFY_RUN_OP_MODE, &command.extra);
            }
            cmd::RESTART_ROBOT => {
                self.state = RobotState::NotStarted;
                self.opmode.clear();
                self.set_webcam_available(false);
                if let Some(meta) = self.pending_activation.take() {
                    self.active_config = meta.clone();
                    self.send_command(
                        cmd::NOTIFY_ACTIVE_CONFIGURATION,
                        &serde_json::to_string(&meta).unwrap(),
                    );
                }
            }
            cmd::REQUEST_FRAME => {
                if self.webcam_available {
                    self.send_webcam_frame();
                }
            }
            cmd::REQUEST_ACTIVE_CONFIG => {
                let extra = serde_json::to_string(&self.active_config).unwrap();
                self.send_command(cmd::NOTIFY_ACTIVE_CONFIGURATION, &extra);
            }
            cmd::REQUEST_CONFIGURATIONS => {
                let extra = serde_json::to_string(&self.configs).unwrap();
                self.send_command(cmd::REQUEST_CONFIGURATIONS_RESP, &extra);
            }
            cmd::REQUEST_PARTICULAR_CONFIGURATION => {
                let xml = serde_json::from_str::<ConfigMeta>(&command.extra)
                    .ok()
                    .and_then(|meta| self.saved_xml.get(&meta.name).cloned())
                    .unwrap_or_else(|| FAKE_CONFIG_XML.to_string());
                self.send_command(cmd::REQUEST_PARTICULAR_CONFIGURATION_RESP, &xml);
            }
            cmd::REQUEST_USER_DEVICE_TYPES => {
                self.send_command(cmd::NOTIFY_USER_DEVICE_LIST, USER_DEVICE_TYPES);
            }
            cmd::SCAN => {
                self.send_command(cmd::SCAN_RESP, SCAN_RESP);
            }
            cmd::ACTIVATE_CONFIGURATION => {
                if let Ok(meta) = serde_json::from_str::<ConfigMeta>(&command.extra) {
                    self.pending_activation = Some(meta);
                }
            }
            cmd::SAVE_CONFIGURATION => {
                // Wire format: `{meta JSON};{device XML}`.
                let Some((meta_json, xml)) = command.extra.split_once(';') else {
                    return;
                };
                let Ok(mut meta) = serde_json::from_str::<ConfigMeta>(meta_json) else {
                    return;
                };
                if meta.resource_id == 0 {
                    self.next_resource_id += 1;
                    meta.resource_id = self.next_resource_id;
                }
                self.saved_xml.insert(meta.name.clone(), xml.to_string());
                match self.configs.iter_mut().find(|c| c.name == meta.name) {
                    Some(existing) => *existing = meta.clone(),
                    None => self.configs.push(meta.clone()),
                }
                // The RC's handleCommandSaveConfiguration calls
                // setActiveConfigAndUpdateUI, so a save moves the active
                // pointer without restarting the robot.
                self.active_config = meta.clone();
                self.send_command(
                    cmd::NOTIFY_ACTIVE_CONFIGURATION,
                    &serde_json::to_string(&meta).unwrap(),
                );
            }
            cmd::DELETE_CONFIGURATION => {
                if let Ok(meta) = serde_json::from_str::<ConfigMeta>(&command.extra) {
                    self.configs.retain(|c| c.name != meta.name);
                }
            }
            _ => {}
        }
    }

    fn set_webcam_available(&mut self, available: bool) {
        if self.webcam_available == available {
            return;
        }
        self.webcam_available = available;
        self.send_command(cmd::STREAM_CHANGE, if available { "true" } else { "false" });
    }

    fn send_webcam_frame(&mut self) {
        let frame_num = self.webcam_frame_num;
        self.webcam_frame_num = self.webcam_frame_num.wrapping_add(1);
        let jpeg = render_frame("webcam", frame_num as u64);

        let begin = cmd::FrameBegin {
            frame_num,
            length: jpeg.len() as i32,
        };
        self.send_command(
            cmd::RECEIVE_FRAME_BEGIN,
            &serde_json::to_string(&begin).unwrap(),
        );

        for (chunk_num, chunk) in jpeg.chunks(cmd::FRAME_CHUNK_SIZE).enumerate() {
            let payload = cmd::FrameChunk {
                frame_num,
                chunk_num: chunk_num as i32,
                encoded_data: robocol::base64::encode(chunk),
            };
            self.send_command(
                cmd::RECEIVE_FRAME_CHUNK,
                &serde_json::to_string(&payload).unwrap(),
            );
        }
    }

    fn telemetry_tick(&mut self) {
        let mut t = Telemetry {
            seq: self.next_seq(),
            timestamp: self.started.elapsed().as_nanos() as i64,
            is_sorted: true,
            robot_state: self.state,
            tag: "TELEMETRY_DATA".to_string(),
            strings: Vec::new(),
            numbers: BTreeMap::new(),
        };
        let elapsed = self.started.elapsed().as_secs_f32();
        // Battery voltage rides every telemetry packet regardless of
        // OpMode state, same as a real Control Hub.
        t.strings.push((
            BATTERY_LEVEL_KEY.into(),
            format!("{:.2}", 12.8 - elapsed * 0.0005),
        ));
        // RC-originated system telemetry: present whenever connected, no
        // matter the OpMode state, like a real hub's own status lines.
        t.strings.push((
            format!("{SYSTEM_KEY_PREFIX}i2c"),
            "1 device on the I2C bus".into(),
        ));
        match self.state {
            RobotState::Init => {
                t.push_line(format!("00 OpMode : {}", self.opmode));
                t.push_line(format!("01 Alliance : {}", self.alliance));
                t.push_line(format!("02 Start Position : {}", self.start_position));
                t.push_line("03 Hint : X toggles alliance, Y toggles start");
            }
            RobotState::Running => {
                t.push_line("State : DRIVING");
                t.numbers.insert(
                    "flywheel_rpm".into(),
                    2800.0 + 300.0 * (elapsed * 2.0).sin(),
                );
                t.numbers.insert("target_rpm".into(), 3000.0);
                t.numbers
                    .insert("heading_deg".into(), (elapsed * 25.0) % 360.0 - 180.0);
                t.numbers
                    .insert("drive_power".into(), 0.5 + 0.5 * (elapsed * 0.7).sin());
                t.numbers.insert("left_stick_y".into(), self.left_stick_y);
                t.push_line(format!(
                    "Servo Bus Current : {:.2} A",
                    1.6 + 0.8 * (elapsed * 1.3).sin()
                ));
                push_field_demo(&mut t, elapsed);
            }
            // Other states still report battery voltage, like a real hub's
            // idle telemetry — just no OpMode-specific keys.
            _ => {}
        }
        let bytes = t.serialize();
        self.send(&bytes);
    }

    /// The RC's own battery status: its own packet, tagged with the reserved
    /// key it carries, `"<percent>|<isCharging>"` (real-capture shape).
    fn rc_battery_tick(&mut self) {
        let t = Telemetry {
            seq: self.next_seq(),
            timestamp: self.started.elapsed().as_nanos() as i64,
            is_sorted: true,
            robot_state: self.state,
            tag: RC_BATTERY_STATUS_KEY.to_string(),
            strings: vec![(RC_BATTERY_STATUS_KEY.into(), "100.0|true".into())],
            numbers: BTreeMap::new(),
        };
        let bytes = t.serialize();
        self.send(&bytes);
    }

    /// A crashing OpMode reports the exception and is torn down by the RC in
    /// the same breath — the DS gets no chance to ask why afterwards.
    fn crash_tick(&mut self) {
        if self.opmode != CRASH_OPMODE || self.state != RobotState::Running {
            return;
        }
        if self.run_started.is_none_or(|t| t.elapsed() < CRASH_DELAY) {
            return;
        }
        self.run_started = None;
        self.send_command(cmd::SHOW_STACKTRACE, CRASH_STACKTRACE);
        self.state = RobotState::Stopped;
        self.opmode.clear();
        self.set_webcam_available(false);
        self.send_command(cmd::NOTIFY_INIT_OP_MODE, cmd::DEFAULT_OP_MODE);
    }
}

/// Field-overlay demo lines, in the `#f` telemetry contract the Deck Driver
/// Station's Field page parses. Coordinates are the default Pedro frame:
/// 0,0 bottom-left to 144,144 top-right, in inches.
fn push_field_demo(t: &mut Telemetry, elapsed: f32) {
    let angle = elapsed * 0.4;
    let (x, y) = (72.0 + 40.0 * angle.cos(), 72.0 + 40.0 * angle.sin());
    let heading = angle.to_degrees() % 360.0;
    let (dx, dy) = (-18.0 * angle.sin(), 18.0 * angle.cos());
    t.push_line(format!("#f robot Robot x={x:.1} y={y:.1} h={heading:.1}"));
    t.push_line("#f zone Launch pts=0,0;48,0;48,24;0,24");
    t.push_line("#f zone Depot pts=96,120;144,120;144,144;96,144");
    t.push_line(format!(
        "#f vec Velocity x={x:.1} y={y:.1} dx={dx:.1} dy={dy:.1} unit=in/s"
    ));
    for i in 0..4 {
        let phase = elapsed * 0.3 + i as f32 * 1.6;
        t.push_line(format!(
            "#f point \"Game Pieces\" x={:.1} y={:.1} h={:.1}",
            72.0 + 55.0 * phase.cos(),
            72.0 + 55.0 * (phase * 0.7).sin(),
            phase.to_degrees() % 360.0
        ));
    }
}

/// One always-on MJPEG endpoint. Each accepted connection gets its own
/// thread streaming `multipart/x-mixed-replace` JPEG frames until the client
/// hangs up.
fn spawn_mjpeg_server(port: u16, source: &'static str) {
    thread::spawn(move || {
        let listener = match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("fake_rc: {source} camera port {port} unavailable: {e}");
                return;
            }
        };
        println!("fake_rc {source} MJPEG on 127.0.0.1:{port}");
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            thread::spawn(move || serve_stream(stream, source));
        }
    });
}

fn serve_stream(mut stream: TcpStream, source: &str) {
    // Drain the request head so the client's GET completes before we reply.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.read(&mut [0u8; 1024]);

    let boundary = "frame";
    let head = format!(
        "HTTP/1.0 200 OK\r\nConnection: close\r\n\
         Content-Type: multipart/x-mixed-replace; boundary={boundary}\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let mut n: u64 = 0;
    loop {
        let jpeg = render_frame(source, n);
        let part = format!(
            "--{boundary}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            jpeg.len()
        );
        if stream.write_all(part.as_bytes()).is_err()
            || stream.write_all(&jpeg).is_err()
            || stream.write_all(b"\r\n").is_err()
        {
            return;
        }
        n += 1;
        thread::sleep(CAM_FRAME_INTERVAL);
    }
}

fn render_frame(source: &str, n: u64) -> Vec<u8> {
    let rgb = test_pattern(source, n);
    let mut out = Vec::new();
    Encoder::new(&mut out, 70)
        .encode(&rgb, CAM_WIDTH, CAM_HEIGHT, ColorType::Rgb)
        .expect("jpeg encode");
    out
}

/// Scrolling color bars in the source's signature hue, plus a motion cue:
/// a bouncing white square for the webcam, a roving crosshair box for the
/// Limelight so the two panes are unmistakable and obviously live.
fn test_pattern(source: &str, n: u64) -> Vec<u8> {
    let w = CAM_WIDTH as i32;
    let h = CAM_HEIGHT as i32;
    let base_hue = if source == "limelight" { 0.30 } else { 0.58 };
    let bar_w = 32i32;
    let scroll = ((n as i32) * 4).rem_euclid(w);
    let mut img = vec![0u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let band = ((x + scroll) / bar_w) as f32;
            let hue = (base_hue + 0.07 * band).rem_euclid(1.0);
            let (r, g, b) = hsv_to_rgb(hue, 0.65, 0.55);
            let i = ((y * w + x) * 3) as usize;
            img[i] = r;
            img[i + 1] = g;
            img[i + 2] = b;
        }
    }
    let t = n as f32;
    if source == "limelight" {
        let cx = ((0.5 + 0.35 * (t * 0.05).sin()) * (w - 48) as f32) as i32;
        let cy = ((0.5 + 0.35 * (t * 0.037).cos()) * (h - 48) as f32) as i32;
        draw_rect_outline(&mut img, w, h, cx, cy, 48, 48, (255, 40, 40));
        fill_rect(&mut img, w, h, cx + 22, cy + 22, 4, 4, (255, 40, 40));
    } else {
        let cx = ((0.5 + 0.4 * (t * 0.11).sin()) * (w - 24) as f32) as i32;
        let cy = ((0.5 + 0.4 * (t * 0.07).cos()) * (h - 24) as f32) as i32;
        fill_rect(&mut img, w, h, cx, cy, 24, 24, (255, 255, 255));
    }
    img
}

#[allow(clippy::too_many_arguments)]
fn fill_rect(img: &mut [u8], w: i32, h: i32, x: i32, y: i32, rw: i32, rh: i32, rgb: (u8, u8, u8)) {
    for yy in y..(y + rh) {
        for xx in x..(x + rw) {
            put_pixel(img, w, h, xx, yy, rgb);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_rect_outline(
    img: &mut [u8],
    w: i32,
    h: i32,
    x: i32,
    y: i32,
    rw: i32,
    rh: i32,
    rgb: (u8, u8, u8),
) {
    for xx in x..(x + rw) {
        put_pixel(img, w, h, xx, y, rgb);
        put_pixel(img, w, h, xx, y + rh - 1, rgb);
    }
    for yy in y..(y + rh) {
        put_pixel(img, w, h, x, yy, rgb);
        put_pixel(img, w, h, x + rw - 1, yy, rgb);
    }
}

fn put_pixel(img: &mut [u8], w: i32, h: i32, x: i32, y: i32, rgb: (u8, u8, u8)) {
    if x < 0 || y < 0 || x >= w || y >= h {
        return;
    }
    let i = ((y * w + x) * 3) as usize;
    img[i] = rgb.0;
    img[i + 1] = rgb.1;
    img[i + 2] = rgb.2;
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .map(|p| p.parse().expect("invalid port"))
        .unwrap_or(ROBOCOL_PORT);
    let socket = UdpSocket::bind(("127.0.0.1", port)).expect("bind fake RC socket");
    socket
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    println!("fake_rc listening on 127.0.0.1:{port}");

    spawn_mjpeg_server(LIMELIGHT_PORT, "limelight");

    let mut rc = FakeRc {
        socket,
        ds: None,
        state: RobotState::NotStarted,
        opmode: String::new(),
        alliance: "RED",
        start_position: "LEFT",
        prev_x: false,
        prev_y: false,
        left_stick_y: 0.0,
        seq: 0,
        started: Instant::now(),
        run_started: None,
        configs: vec![
            ConfigMeta {
                is_dirty: false,
                location: "LOCAL_STORAGE".to_string(),
                name: "practice_bot".to_string(),
                resource_id: 1,
            },
            // An immutable built-in, to exercise the copy-only (Save As) path.
            ConfigMeta {
                is_dirty: false,
                location: "RESOURCE".to_string(),
                name: "goBILDA Starter Bot".to_string(),
                resource_id: 2132017159,
            },
        ],
        active_config: ConfigMeta {
            is_dirty: false,
            location: "NONE".to_string(),
            name: "<No Config Set>".to_string(),
            resource_id: 0,
        },
        saved_xml: HashMap::new(),
        next_resource_id: 1,
        pending_activation: None,
        webcam_available: false,
        webcam_frame_num: 0,
    };

    let mut buf = [0u8; 4096];
    let mut last_telemetry = Instant::now();
    let mut last_rc_battery = Instant::now();
    loop {
        match rc.socket.recv_from(&mut buf) {
            Ok((n, from)) => match Packet::parse(&buf[..n]) {
                Ok(Packet::PeerDiscovery(_)) => {
                    if rc.ds != Some(from) {
                        println!("DS connected: {from}");
                        rc.ds = Some(from);
                    }
                    let reply = PeerDiscovery::default().serialize();
                    let _ = rc.socket.send_to(&reply, from);
                }
                Ok(Packet::Heartbeat(mut hb)) if rc.ds == Some(from) => {
                    // Echo with our state, like the real RC.
                    hb.robot_state = rc.state;
                    hb.seq = rc.next_seq();
                    let _ = rc.socket.send_to(&hb.serialize(), from);
                }
                Ok(Packet::Command(c)) if rc.ds == Some(from) => rc.handle_command(c),
                Ok(Packet::Heartbeat(_) | Packet::Command(_)) => {
                    println!("!! ignoring {from}: not the connected DS ({:?})", rc.ds);
                }
                Ok(Packet::Gamepad(gp)) if gp.user == 1 => {
                    if gp.left_stick_y != rc.left_stick_y {
                        println!("gamepad{} left_stick_y={}", gp.user, gp.left_stick_y);
                    }
                    rc.left_stick_y = gp.left_stick_y;
                    if rc.state == RobotState::Init {
                        if gp.x && !rc.prev_x {
                            rc.alliance = if rc.alliance == "RED" { "BLUE" } else { "RED" };
                            println!("gamepad{} X: alliance -> {}", gp.user, rc.alliance);
                        }
                        if gp.y && !rc.prev_y {
                            rc.start_position = if rc.start_position == "LEFT" {
                                "RIGHT"
                            } else {
                                "LEFT"
                            };
                            println!("gamepad{} Y: start -> {}", gp.user, rc.start_position);
                        }
                    }
                    rc.prev_x = gp.x;
                    rc.prev_y = gp.y;
                }
                Ok(Packet::Gamepad(_)) => {}
                Ok(_) => {}
                Err(e) => println!("!! parse error: {e}"),
            },
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => println!("!! socket error: {e}"),
        }

        rc.crash_tick();

        if last_telemetry.elapsed() >= Duration::from_millis(100) {
            last_telemetry = Instant::now();
            rc.telemetry_tick();
        }

        if last_rc_battery.elapsed() >= Duration::from_secs(1) {
            last_rc_battery = Instant::now();
            rc.rc_battery_tick();
        }
    }
}
