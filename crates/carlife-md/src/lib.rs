//! The CarLife half: this program plays the phone a head unit plugs into.
//!
//! The car asks for a session and never the other way round, so everything here
//! reacts. What the program hands over is `Ports` and what it runs is `Carlife`;
//! `press`, `playing` and `media` are the vocabulary the two ends share.
pub mod link;
pub mod media;
pub mod outlet;
pub mod page;
pub mod panel;
pub mod playing;
pub mod press;
pub mod settings;

mod carlife;
mod desc;
mod frame;
mod gps;
mod names;
mod ports;
mod proto;

pub use carlife::Carlife;
pub use ports::Ports;
pub use proto::{CarGps, MAX_ART};
