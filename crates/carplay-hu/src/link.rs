//! What this half needs from the program it runs in, and what it tells it.
//!
//! The program implements both. Nothing here knows how the other half of the
//! bridge works, or that there is one.

use crate::now_playing::NowPlaying;

/// Where what the phone is playing goes: the car's panel, by whatever road the
/// program has for it.
///
/// Never blocks, for the same reason a stream sink does not: the phone decides
/// when it says something, and a panel that is behind is a panel whose song is
/// stale rather than a session that should wait.
pub trait Panel: Send + Sync {
    fn playing(&self, item: NowPlaying);
}

/// How far a phone has got, which is the last of the three facts a status light
/// is drawn from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Link {
    /// No phone, or none that has reached the receiver.
    #[default]
    Idle,
    /// A phone has opened the receiver. Whether it will get anywhere is not
    /// known yet.
    Connecting,
    /// The phone's screen is arriving.
    Linked,
}

/// What the program is told as the receiver runs.
///
/// The two light facts are here rather than in a light: what a light shows is
/// the program's business, and this half is only the one source of "how far the
/// phone got".
pub trait Delegate: Send + Sync {
    /// Whether both radios this half needs are up. Said once per car session.
    fn radios(&self, up: bool);

    /// How far the phone has got.
    fn carplay(&self, link: Link);

    /// How far it had got, read back by the one caller that has to tell a phone
    /// that has gone from one that never arrived.
    fn link(&self) -> Link;

    /// The phone's icon for us was tapped on the head unit's screen, which is
    /// the car's own desktop being asked for. Nothing here can act on it: the
    /// icon is drawn by the car, so only the car can answer.
    fn tapped(&self);
}
