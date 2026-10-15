//! The airplay receiver: a small RTSP server the phone connects back to.
//!
//! After CarPlayStartSession goes out over iAP2 the phone opens a *second*,
//! unrelated connection to the port we advertised and speaks a different
//! protocol entirely. Nothing in this module shares code with iAP2; the only
//! thing carried across is the port number.
//!
//! The ordering matters and is enforced by which fields each step requires:
//!
//! ```text
//! OPTIONS
//!   POST /pair-setup   (first pairing only)
//!   POST /pair-verify  -> shared secret -> every later key
//!   POST /auth-setup   -> MFiSAP: prove the licence, or the phone hangs up
//! SETUP                (session: announces timing + event ports)
//!   GET /info          -> who we are; incomplete here means no picture
//! SETUP                (streams: announces video/audio data ports)
//! RECORD               -> media starts flowing
//!   POST /feedback     (once a second, forever — see below)
//! ```
//!
//! `server` walks that conversation for one connection at a time and `event` is
//! the input channel going back the other way; the rest are the pieces those two
//! reach for, and `crypto` holds the frame format they both write.

pub mod audio;
pub mod bplist;
pub mod clock;
pub mod crypto;
pub mod event;
pub mod info;
pub mod mic;
pub mod pairing;
pub mod rtsp;
pub mod screen;
mod server;

pub use self::event::EventSink;
pub use self::mic::MicSink;
pub use self::server::{serve, AudioClock, ScreenParams, Shared, SharedScreen};
