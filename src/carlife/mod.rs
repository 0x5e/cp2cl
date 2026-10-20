//! The car's half of the bridge: this device plays the phone a head unit
//! expects to be plugged into.
//!
//! The phone is not simulated, only its place on the wire is. A car asks the
//! device for a CarLife session and never the other way round, so everything
//! here reacts: `usb` composes the device this program presents and `gadget`
//! answers what then arrives on it, `session` walks the conversation that
//! follows, `auth` answers the car's challenge and judges it in turn, `frame` and
//! `proto` are the two layers the messages are carried in, `gps` turns the car's
//! position into the sentences the phone reads one from, and `desc` is the shape
//! of the device as a host reads it.
//!
//! `auth`, `desc`, `frame`, `gps`, `proto` and `session` are plain code with no
//! USB in them, which is what lets them be tested against an in-memory pipe.
//! `usb` and `gadget` are the two that need the board.

pub mod auth;
pub mod desc;
pub mod frame;
pub mod gps;
pub mod proto;
pub mod session;

pub mod gadget;
pub mod usb;

pub use gadget::run as serve;
