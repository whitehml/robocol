//! The single shared surface of `ds_daemon` and its front ends (`ds_cli`,
//! `ds_agent`): what a request is and how each verb parses (`request`), how
//! it travels (`transport`), and how replies and events are filtered and
//! shown (`filter`, `render`). Holds no `robocol` types: only `ds_daemon`
//! talks to the robot.

pub mod filter;
pub mod render;
pub mod request;
/// Unix domain sockets: the only transport so far. Gated so the rest of the
/// crate still builds where a future front end (say, Deck Station on
/// Windows) would need another.
#[cfg(unix)]
pub mod transport;

pub use filter::WatchFilter;
pub use render::render;
pub use request::{GamepadInput, PARSE_TABLE, Pad, Request, Role, Verb, parse, split_line};
#[cfg(unix)]
pub use transport::{read_message, socket_path, write_message};
