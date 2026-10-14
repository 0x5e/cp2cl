//! The car's half of the bridge: this device plays the phone a head unit
//! expects to be plugged into.
//!
//! The phone is not simulated, only its place on the wire is. A car asks the
//! device for a CarLife session and never the other way round, so everything
//! here reacts: `gadget` answers the control requests the car sends over the
//! interface it is handed, `session` walks the conversation that follows, `frame`
//! and `proto` are the two layers the messages are carried in.
//!
//! `frame`, `proto` and `session` are plain code with no USB in them, which is
//! what lets them be tested against an in-memory pipe. `gadget` is the only part
//! that needs the board, and the only part that is Linux-only.

pub mod frame;
pub mod proto;
pub mod session;

#[cfg(target_os = "linux")]
pub mod gadget;

#[cfg(target_os = "linux")]
pub use gadget::run as serve;
