//! What the CarLife half needs from the program it runs in, and what it tells it.
//!
//! The host implements all of it. Nothing here knows how the other half of the
//! bridge works.

use std::io;

use futures_util::future::BoxFuture;

use crate::channels::ChannelEntry;
use crate::machine::ScreenOverride;
use crate::press::Command;

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
    fn screen_named(&self, width: u32, height: u32, fps: u32);

    /// The channel number the car reported, which `/info` answers from: a head
    /// unit that names itself gets the icon and label of the make it belongs to.
    fn channel_id(&self, channel: Option<&str>);

    /// The screen the operator picked, read again for each session because the
    /// page can change it while the program runs.
    fn screen_chosen(&self) -> ScreenOverride;

    /// What the channel table says about the panel behind this channel number.
    fn channel_entry(&self, channel: Option<&str>) -> ChannelEntry;
}

/// Where the car's position goes, as a block of NMEA sentences: either a fix or
/// the sentences that withdraw one. The phone reads it as its own location.
pub trait Location: Send + Sync {
    fn fix(&self, nmea: &str);
}

/// The car's microphone, on its way to the phone.
///
/// One direction only, and synchronous: the samples arrive on the session's own
/// reader, which must not be held up by an encoder or by a socket. What the
/// phone asked for — a coding, a rate, a packet size — is not this side's to
/// know; it was told at the stream's SETUP.
///
/// Whether the phone is listening is not asked here. The car records whenever
/// the voice channel is open, which is what the phone whose messages it copies
/// does: the microphone is the other half of that channel, not a separate
/// question.
pub trait Voice: Send + Sync {
    /// One block of the car's microphone, 16 kHz mono signed 16-bit
    /// little-endian, exactly as the VR channel carried it.
    fn mic(&self, pcm: &[u8]);
}
