//! What the CarLife half needs from the program it runs in, and what it tells it.
//!
//! The host implements all three. Nothing here knows how the other half of the
//! bridge works.

use std::io;

use futures_util::future::BoxFuture;

use crate::panel::Panel;
use crate::press::Command;
use crate::settings::Screen;

/// Where the car's touch and panel keys go. The receiver is the CarPlay half,
/// so a key pressed in the car arrives at the phone by the road any other key
/// would.
pub trait Input: Send + Sync {
    fn press(&self, command: Command) -> BoxFuture<'_, io::Result<()>>;
}

/// What the host is told as the session runs.
pub trait Delegate: Send + Sync {
    /// `true` once the car has been given our device information, `false` when
    /// the session ends for any reason. This is what opens and closes the
    /// CarPlay half.
    fn session(&self, up: bool);

    /// The size the car asked to be rendered at.
    fn screen(&self, width: u32, height: u32, fps: u32);

    /// The channel number the car reported, which `/info` answers from: a head
    /// unit that names itself gets the icon and label of the make it belongs to.
    fn channel(&self, channel: Option<&str>);

    /// The screen the operator picked, read again for each session because the
    /// page can change it while the program runs.
    fn screen_override(&self) -> Screen;

    /// What the channel table says about the panel behind this channel number.
    fn panel(&self, channel: Option<&str>) -> Panel;
}

/// What the binary carries besides code: the guide's pictures and the cover a
/// song without artwork gets.
pub trait Assets: Send + Sync {
    fn get(&self, name: &str) -> Option<&'static [u8]>;

    /// Every name the binary carries. The guide ships one file per screen size,
    /// and the set is whatever was built rather than a list kept beside them.
    fn names(&self) -> Vec<&'static str>;
}
