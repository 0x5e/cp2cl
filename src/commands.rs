//! Keyboard input, parsed from stdin lines.
//!
//! One command per line, no arguments. Anything unrecognised prints the
//! available list rather than failing silently, because this is the only
//! control surface the demo has.

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
    /// A wheel tick, produced by the AA bridge when its rotary knob turns.
    /// No reference implementation exposes a wheel *key*, so there is no stdin
    /// command for this — only the axis itself, which the HID descriptor keeps.
    Wheel(i8),
    /// A multitouch report: one entry per slot the /info descriptor declares,
    /// whether or not a finger is on it. The AA bridge is the only producer.
    Touch([TouchContact; TOUCH_SLOTS]),
    // Telephony, plus the keyframe request below. None of these has a stdin
    // command either: a front end exposes the call keys as buttons rather than
    // as key bindings, and here it is the AA bridge that drives them, off the
    // head unit's own call and media keys.
    Answer,
    Hangup,
    Mute,
    KeyFrame,
    // Session level.
    Siri,
    Quit,
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

/// Telephony indices, mirroring LIVI's `TelephonyButton`.
pub const PHONE_HOOK_SWITCH: u8 = 1;
pub const PHONE_FLASH: u8 = 2;
pub const PHONE_DROP: u8 = 3;
pub const PHONE_MUTE: u8 = 4;

pub fn parse(line: &str) -> Option<Command> {
    let head = line.split_whitespace().next()?.to_ascii_lowercase();
    Some(match head.as_str() {
        "play" => Command::Play,
        "pause" => Command::Pause,
        "playpause" | "pp" => Command::PlayPause,
        "next" => Command::Next,
        "prev" => Command::Prev,
        "home" => Command::Home,
        "back" => Command::Back,
        "select" | "selectup" => Command::Select,
        "left" => Command::Left,
        "right" => Command::Right,
        "up" => Command::Up,
        "down" => Command::Down,
        "siri" => Command::Siri,
        "quit" | "exit" => Command::Quit,
        _ => return None,
    })
}

pub const HELP: &str = "\
commands (one per line):
  media      play | pause | playpause | next | prev
  knob       home | back | select | left | right | up | down
  session    siri
  local      quit
";
