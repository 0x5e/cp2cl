//! The vocabulary of input.
//!
//! Nothing here parses anything: whoever has an input event — the CarLife
//! session, once it carries touch and keys back up — turns it into one of these.
//! `event.rs` is the only place that knows how each one reaches the phone.
//!
//! Nothing produces one yet, which is what the allow below is about: the CarLife
//! side is the caller this is kept for. See `PLAN-CL.md` §4.3.
#![allow(dead_code)]

/// One finger in a touch report, in the display's own pixel space.
///
/// The phone tracks a finger by the slot it is reported in, not by an id, so a
/// contact that vanishes between two reports has to leave its slot empty rather
/// than be dropped from the list — `down: false` is what says "this slot is
/// free".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TouchContact {
    pub down: bool,
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    // Media keys, sent to the media HID device.
    Play,
    Pause,
    PlayPause,
    Next,
    Prev,
    // Knob buttons.
    Home,
    Back,
    Select,
    // Knob movement: a momentary push, so each one sends and releases.
    Left,
    Right,
    Up,
    Down,
    /// A wheel tick, read off a knob's axis. No reference implementation
    /// exposes a wheel *key*, so there is no named command for this — only the
    /// axis, which the HID descriptor keeps.
    Wheel(i8),
    /// A multitouch report: one entry per slot the /info descriptor declares,
    /// whether or not a finger is on it.
    Touch([TouchContact; TOUCH_SLOTS]),
    /// A raw telephony index. Its call buttons are indices into the telephony
    /// report rather than named keys, so passing one through unchanged is the
    /// whole of the mapping needed.
    Telephony(u8),
    KeyFrame,
    // Session level.
    Siri,
    /// Day or night rendering, from a setting of the operator's. A session
    /// command rather than a HID report: the phone is told to redraw, not to
    /// press anything.
    Night(bool),
}

/// How many simultaneous fingers the touchscreen descriptor declares — and so
/// how many slots a touch report carries. Two is enough for pinch and rotate.
pub const TOUCH_SLOTS: usize = 2;

/// Media key indices, mirroring LIVI's `MediaButton`.
pub const MEDIA_PLAY: u8 = 1;
pub const MEDIA_PAUSE: u8 = 2;
pub const MEDIA_PLAY_PAUSE: u8 = 3;
pub const MEDIA_NEXT: u8 = 4;
pub const MEDIA_PREV: u8 = 5;
