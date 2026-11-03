//! The CarLife half: this program plays the phone a head unit plugs into.
//!
//! The car asks for a session and never the other way round, so everything here
//! reacts. Protocol and transport live in `carlife::`; `press`, `playing` and
//! `media` are the vocabulary it hands the host.
pub mod carlife;
pub mod desc;
pub mod frame;
pub mod gps;
pub mod link;
pub mod media;
pub mod names;
pub mod outlet;
pub mod page;
pub mod panel;
pub mod playing;
pub mod press;
pub mod proto;
pub mod settings;
