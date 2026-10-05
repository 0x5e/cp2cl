//! Keyboard input, parsed from stdin lines.
//!
//! One command per line, no arguments beyond the few that need a value. Anything
//! unrecognised prints the available list rather than failing silently, because
//! this is the only control surface the demo has.

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
    // Navigation guidance announcement.
    NavGuidance,
    // Knob buttons.
    Home,
    Back,
    Select,
    // Knob movement: a momentary push, so each one sends and releases.
    Left,
    Right,
    Up,
    Down,
    Wheel(i8),
    /// A multitouch report: one entry per slot the /info descriptor declares,
    /// whether or not a finger is on it.
    Touch([TouchContact; TOUCH_SLOTS]),
    // Telephony.
    Answer,
    Hangup,
    Mute,
    Key(char),
    Star,
    Pound,
    Del,
    // Session level.
    Night(bool),
    Siri,
    KeyFrame,
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
pub const MEDIA_NAV_GUIDANCE: u8 = 6;

/// Telephony indices, mirroring LIVI's `TelephonyButton`.
pub const PHONE_HOOK_SWITCH: u8 = 1;
pub const PHONE_FLASH: u8 = 2;
pub const PHONE_DROP: u8 = 3;
pub const PHONE_MUTE: u8 = 4;
/// key0..key9 start here, then star and pound and del follow.
pub const PHONE_KEY0: u8 = 5;

pub fn parse(line: &str) -> Option<Command> {
    let mut parts = line.split_whitespace();
    let head = parts.next()?.to_ascii_lowercase();
    let arg = parts.next();
    Some(match head.as_str() {
        "play" => Command::Play,
        "pause" => Command::Pause,
        "playpause" | "pp" => Command::PlayPause,
        "next" => Command::Next,
        "prev" => Command::Prev,
        "nav" => Command::NavGuidance,
        "home" => Command::Home,
        "back" => Command::Back,
        "select" | "selectup" => Command::Select,
        "left" => Command::Left,
        "right" => Command::Right,
        "up" => Command::Up,
        "down" => Command::Down,
        "wheel" => Command::Wheel(arg?.parse().ok()?),
        "answer" => Command::Answer,
        "hangup" => Command::Hangup,
        "mute" => Command::Mute,
        "key" => Command::Key(arg?.chars().next()?),
        "star" => Command::Star,
        "pound" => Command::Pound,
        "del" => Command::Del,
        "night" => Command::Night(arg.map(|a| a != "off").unwrap_or(true)),
        "siri" => Command::Siri,
        "keyframe" => Command::KeyFrame,
        "quit" | "exit" => Command::Quit,
        _ => return None,
    })
}

pub const HELP: &str = "\
commands (one per line):
  media      play | pause | playpause | next | prev | nav
  knob       home | back | select | left | right | up | down | wheel <n>
  phone      answer | hangup | mute | key <0-9> | star | pound | del
  session    night [on|off] | siri | keyframe | quit
";
