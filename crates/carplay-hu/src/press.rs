//! The vocabulary of input, as the head unit side produces it.
//!
//! `KeyFrame` has no control behind it — it is a session asking for a whole
//! picture. `Night` is the operator's. Both are kept so a command has one
//! spelling on both sides of the bridge.
#![allow(dead_code)]

/// One finger in a touch report, in the display's pixel space. A slot with
/// `down: false` is empty rather than absent.
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

/// Telephony key indices in the report the `/info` descriptor declares, mirroring
/// LIVI's `TelephonyButton`. The report is a numbered list of call buttons, so a
/// head unit's call button is an index rather than a named key.
pub const PHONE_HOOK_SWITCH: u8 = 1;
pub const PHONE_DROP: u8 = 3;
pub const PHONE_MUTE: u8 = 4;
