//! The input channel (`CH_INPUT` = 8): **head unit → phone**.
//!
//! We are the phone, so unlike LIVI's head-unit code we *receive* these rather
//! than send them. Everything a driver touches — the screen, the knobs, the
//! steering-wheel keys — arrives here as an `InputReport`:
//!
//! ```text
//! InputReport {
//!     timestamp      = 1   uint64, microseconds
//!     touch_event    = 3   TouchEvent
//!     key_event      = 4   KeyEvent
//!     absolute_event = 5   (unused here)
//!     relative_event = 6   RelativeEvent — rotary deltas
//! }
//! ```
//!
//! # Why the raw numbers are kept
//!
//! Codes arrive as plain integers rather than a closed enum on purpose. Head
//! units disagree about the details, and a caller that wants to forward
//! input somewhere else needs the value even when this crate does not
//! recognise it. Match against the `ButtonCode` / `TouchAction` constants and
//! pass the rest through or drop it — your choice.
//!
//! # A trap worth knowing
//!
//! In Android Auto, `DPAD_UP`/`DOWN`/`LEFT`/`RIGHT` only move focus *between*
//! focusable regions; scrolling *inside* a list or picker needs a rotary delta
//! on `relative_event`. Head units get this wrong often enough that it is
//! worth stating here — see `sendRotary`'s comment in LIVI's `InputChannel.ts`.
//! CarPlay does not have this split (the phone decides from context), so a
//! caller bridging to CarPlay can simply map all four directions to axis
//! deflections and let the iPhone sort it out.

use super::proto::{read_varint, varint_field};

pub const CH_INPUT: u8 = 8;

/// Button codes a head unit may send: Android `KeyEvent.KEYCODE_*` plus a few
/// Android Auto extensions. Values follow LIVI's `BUTTON_KEY` table.
pub struct ButtonCode(pub u32);

#[allow(non_upper_case_globals)]
impl ButtonCode {
    pub const UNKNOWN: u32 = 0;
    pub const HOME: u32 = 3;
    pub const BACK: u32 = 4;
    pub const PHONE_ACCEPT: u32 = 5;
    pub const PHONE_DECLINE: u32 = 6;
    /// D-pad. These move focus between regions, not inside lists.
    pub const DPAD_UP: u32 = 19;
    pub const DPAD_DOWN: u32 = 20;
    pub const DPAD_LEFT: u32 = 21;
    pub const DPAD_RIGHT: u32 = 22;
    pub const DPAD_CENTER: u32 = 23;
    pub const ENTER: u32 = 66;
    pub const HEADSETHOOK: u32 = 79;
    pub const MENU: u32 = 82;
    pub const SEARCH: u32 = 84;
    pub const MEDIA_PLAY_PAUSE: u32 = 85;
    pub const MEDIA_STOP: u32 = 86;
    pub const MEDIA_NEXT: u32 = 87;
    pub const MEDIA_PREV: u32 = 88;
    pub const MUTE: u32 = 91;
    pub const MEDIA_PLAY: u32 = 126;
    pub const MEDIA_PAUSE: u32 = 127;
    pub const ASSIST: u32 = 219;
    pub const VOICE_ASSIST: u32 = 231;
    pub const NAVIGATE_PREVIOUS: u32 = 260;
    pub const NAVIGATE_NEXT: u32 = 261;
    pub const NAVIGATE_IN: u32 = 262;
    pub const NAVIGATE_OUT: u32 = 263;
    /// The AA extension for a rotary controller.
    pub const ROTARY_CONTROLLER: u32 = 65536;
    pub const MEDIA: u32 = 65537;
    pub const TERTIARY_BUTTON: u32 = 65543;
    pub const TURN_CARD: u32 = 65544;
}

/// `PointerAction` values.
pub struct TouchAction(pub u32);

#[allow(non_upper_case_globals)]
impl TouchAction {
    pub const DOWN: u32 = 0;
    pub const UP: u32 = 1;
    pub const MOVED: u32 = 2;
    pub const POINTER_DOWN: u32 = 5;
    pub const POINTER_UP: u32 = 6;
}

/// One finger, in the touchscreen coordinate space we advertised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pointer {
    pub x: u32,
    pub y: u32,
    pub id: u32,
}

/// An input event exactly as the head unit meant it. Nothing here is
/// translated towards any particular consumer — that is the caller's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// A hardware key. The head unit sends a press and a release; `metastate`
    /// is dropped because nothing upstream has ever needed it.
    Key {
        keycode: u32,
        down: bool,
        longpress: bool,
    },
    /// A rotary controller turned by `delta`. Negative is previous, positive
    /// next — the wire carries no axis, just direction.
    Rotary { keycode: u32, delta: i32 },
    Touch {
        action: u32,
        pointers: Vec<Pointer>,
        action_index: u32,
    },
}

/// Everything in one `InputReport`. Usually one event, but a report can carry
/// several keys at once (the head unit packs them into a single `KeyEvent`).
pub fn parse_input_report(body: &[u8]) -> Vec<InputEvent> {
    let mut out = Vec::new();
    if let Some(touch) = submessage(body, 3) {
        out.extend(parse_touch(touch));
    }
    if let Some(keys) = submessage(body, 4) {
        out.extend(parse_keys(keys));
    }
    if let Some(relative) = submessage(body, 6) {
        out.extend(parse_relative(relative));
    }
    out
}

/// The bytes of the first top-level length-delimited field with this number.
fn submessage(buf: &[u8], want: u32) -> Option<&[u8]> {
    let mut i = 0usize;
    while i < buf.len() {
        let tag = read_varint(buf, &mut i)?;
        let field = (tag >> 3) as u32;
        match tag & 7 {
            0 => {
                read_varint(buf, &mut i)?;
            }
            1 => i += 8,
            2 => {
                let len = read_varint(buf, &mut i)? as usize;
                let end = i.checked_add(len)?;
                if end > buf.len() {
                    return None;
                }
                if field == want {
                    return Some(&buf[i..end]);
                }
                i = end;
            }
            5 => i += 4,
            _ => return None,
        }
    }
    None
}

/// Every length-delimited field with this number — the repeated ones.
fn submessages(buf: &[u8], want: u32) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < buf.len() {
        let Some(tag) = read_varint(buf, &mut i) else {
            break;
        };
        let field = (tag >> 3) as u32;
        match tag & 7 {
            0 => {
                if read_varint(buf, &mut i).is_none() {
                    break;
                }
            }
            1 => i += 8,
            2 => {
                let Some(len) = read_varint(buf, &mut i) else {
                    break;
                };
                let Some(end) = i.checked_add(len as usize) else {
                    break;
                };
                if end > buf.len() {
                    break;
                }
                if field == want {
                    out.push(&buf[i..end]);
                }
                i = end;
            }
            5 => i += 4,
            _ => break,
        }
    }
    out
}

fn parse_touch(touch: &[u8]) -> Option<InputEvent> {
    let pointers = submessages(touch, 1)
        .iter()
        .map(|p| Pointer {
            x: varint_field(p, 1).unwrap_or(0) as u32,
            y: varint_field(p, 2).unwrap_or(0) as u32,
            id: varint_field(p, 3).unwrap_or(0) as u32,
        })
        .collect::<Vec<_>>();
    if pointers.is_empty() {
        return None;
    }
    Some(InputEvent::Touch {
        action: varint_field(touch, 3).unwrap_or(0) as u32,
        pointers,
        action_index: varint_field(touch, 2).unwrap_or(0) as u32,
    })
}

fn parse_keys(keys: &[u8]) -> Vec<InputEvent> {
    submessages(keys, 1)
        .iter()
        .map(|k| InputEvent::Key {
            keycode: varint_field(k, 1).unwrap_or(0) as u32,
            down: varint_field(k, 2).unwrap_or(0) != 0,
            longpress: varint_field(k, 4).unwrap_or(0) != 0,
        })
        .collect()
}

fn parse_relative(relative: &[u8]) -> Vec<InputEvent> {
    submessages(relative, 1)
        .iter()
        .map(|r| InputEvent::Rotary {
            keycode: varint_field(r, 1).unwrap_or(0) as u32,
            // int32 on the wire: a negative delta is sign-extended to ten
            // bytes, so take the low 32 bits back as signed.
            delta: varint_field(r, 2).unwrap_or(0) as i32,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aa_client::proto::pb_varint;

    fn key_entry(keycode: u32, down: bool) -> Vec<u8> {
        let mut key = pb_varint(1, keycode as u64);
        key.extend(pb_varint(2, down as u64));
        key.extend(pb_varint(3, 0));
        key.extend(pb_varint(4, 0));
        key
    }

    #[test]
    fn dpad_comes_through_as_a_key() {
        let mut key_event = Vec::new();
        key_event.extend({
            let e = key_entry(ButtonCode::DPAD_UP, true);
            let mut out = Vec::new();
            out.extend(crate::aa_client::proto::varint((1 << 3) | 2));
            out.extend(crate::aa_client::proto::varint(e.len() as u64));
            out.extend(e);
            out
        });
        let mut report = pb_varint(1, 1_000_000);
        report.extend({
            let mut out = Vec::new();
            out.extend(crate::aa_client::proto::varint((4 << 3) | 2));
            out.extend(crate::aa_client::proto::varint(key_event.len() as u64));
            out.extend(key_event);
            out
        });
        assert_eq!(
            parse_input_report(&report),
            vec![InputEvent::Key {
                keycode: ButtonCode::DPAD_UP,
                down: true,
                longpress: false
            }]
        );
    }

    #[test]
    fn negative_rotary_delta_stays_negative() {
        // -1 encoded the way protobuf encodes an int32: ten bytes, all ones.
        let mut rel = pb_varint(1, ButtonCode::ROTARY_CONTROLLER as u64);
        rel.extend(pb_varint(2, (-1i32) as u64));
        let mut relative_event = {
            let mut out = Vec::new();
            out.extend(crate::aa_client::proto::varint((1 << 3) | 2));
            out.extend(crate::aa_client::proto::varint(rel.len() as u64));
            out.extend(rel);
            out
        };
        let mut report = Vec::new();
        report.extend(crate::aa_client::proto::varint((6 << 3) | 2));
        report.extend(crate::aa_client::proto::varint(relative_event.len() as u64));
        report.append(&mut relative_event);
        assert_eq!(
            parse_input_report(&report),
            vec![InputEvent::Rotary {
                keycode: ButtonCode::ROTARY_CONTROLLER,
                delta: -1
            }]
        );
    }

    #[test]
    fn an_empty_report_yields_nothing() {
        assert!(parse_input_report(&[]).is_empty());
    }
}
