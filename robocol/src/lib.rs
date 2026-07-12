//! FTC Robocol — the UDP protocol spoken between the Driver Station and the
//! Robot Controller (Control Hub).
//!
//! Wire formats follow the TypeScript reference implementation
//! [Epiteugma/librobocol](https://github.com/Epiteugma/librobocol) and its consumer
//! [Epiteugma/FtcDriverStation](https://github.com/Epiteugma/FtcDriverStation),
//! which were validated against real Control Hubs. Places where this crate
//! knowingly deviates from librobocol are called out with `LIBROBOCOL
//! DEVIATION` comments.
//!
//! Both peers bind UDP port [`ROBOCOL_PORT`]. The DS broadcasts
//! [`packets::PeerDiscovery`] until the RC answers, then keeps the link alive
//! with [`packets::Heartbeat`] every ~200 ms. Everything else rides on
//! [`packets::Command`], [`packets::Gamepad`], and [`packets::Telemetry`].

#![cfg_attr(not(test), warn(clippy::pedantic))]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::dbg_macro
    )
)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::similar_names,
    clippy::must_use_candidate,
    clippy::struct_excessive_bools,
    clippy::large_stack_arrays,
    clippy::needless_pass_by_value,
    clippy::duration_suboptimal_units
)]

mod wire;

pub mod base64;
pub mod client;
pub mod cmd;
pub mod packets;
pub mod types;
pub mod video;

pub use client::{ClientConfig, Event, RobocolClient};
pub use packets::Packet;
pub use types::RobotState;

pub const ROBOCOL_PORT: u16 = 20884;

/// Default addresses a Control Hub (.43.1) or RC phone (.49.1) answers on.
pub const DEFAULT_PEER_ADDRS: [&str; 2] = ["192.168.43.1", "192.168.49.1"];

pub mod tracked_rc {
    /// Upstream `FtcRobotController` release tag the code is validated against.
    pub const RELEASE_TAG: Option<&str> = Some("v11.1");

    /// Upstream repository the release tag refers to.
    pub const UPSTREAM_REPO: &str = "FIRST-Tech-Challenge/FtcRobotController";
}
