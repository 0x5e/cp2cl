//! The car's half of the bridge: this device plays the phone a head unit
//! expects to be plugged into.
//!
//! The phone is not simulated, only its place on the wire is. A car asks the
//! device for a CarLife session and never the other way round, so everything
//! here reacts: `usb` composes the device this program presents and `gadget`
//! answers what then arrives on it, `session` walks the conversation that
//! follows, `frame` and `proto` are the two layers the messages are carried in,
//! and `desc` is the shape of the device as a host reads it.
//!
//! `desc`, `frame`, `proto` and `session` are plain code with no USB in them,
//! which is what lets them be tested against an in-memory pipe. `usb` and
//! `gadget` are the two that need the board, and the only two that are
//! Linux-only.

pub mod desc;
pub mod frame;
pub mod proto;
pub mod session;

#[cfg(target_os = "linux")]
pub mod gadget;
#[cfg(target_os = "linux")]
pub mod usb;

#[cfg(target_os = "linux")]
pub use gadget::run as serve;
