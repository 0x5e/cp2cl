//! The CarLife half: this program plays the phone a head unit plugs into.
//!
//! The car asks for a session and never the other way round, so everything here
//! reacts. What the program hands over is `Ports` and what it runs is `Md`;
//! `press`, `now_playing` and `media` are the vocabulary the two ends share.
pub mod channels;
pub mod link;
pub mod machine;
pub mod media;
pub mod now_playing;
pub mod outlet;
pub mod press;
pub mod report;

mod carlife;
mod desc;
mod frame;
mod gps;
mod names;
mod ports;
mod proto;

pub use carlife::{Md, Signer};
pub use ports::Ports;
pub use proto::{CarGps, MAX_ARTWORK};
