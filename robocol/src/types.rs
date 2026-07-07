//! Shared protocol enums.

/// Robot lifecycle state reported by the RC in Heartbeat and Telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobotState {
    Unknown,
    NotStarted,
    Init,
    Running,
    Stopped,
    EmergencyStop,
}

impl RobotState {
    /// Wire encoding is a signed byte: -1 Unknown, 0..=4 the rest.
    pub fn from_byte(b: u8) -> RobotState {
        match b as i8 {
            0 => RobotState::NotStarted,
            1 => RobotState::Init,
            2 => RobotState::Running,
            3 => RobotState::Stopped,
            4 => RobotState::EmergencyStop,
            _ => RobotState::Unknown,
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            RobotState::Unknown => (-1i8) as u8,
            RobotState::NotStarted => 0,
            RobotState::Init => 1,
            RobotState::Running => 2,
            RobotState::Stopped => 3,
            RobotState::EmergencyStop => 4,
        }
    }
}

/// Physical controller type advertised in Gamepad packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum GamepadType {
    #[default]
    Unknown = 0,
    LogitechF310 = 1,
    Xbox360 = 2,
    SonyPs4 = 3,
    SonyPs4Kernel = 4,
}

/// Sentinel gamepad IDs. The stock DS sends `Synthetic` for gamepads it
/// composes itself rather than raw HID devices.
pub const GAMEPAD_ID_SYNTHETIC: i32 = -2;
