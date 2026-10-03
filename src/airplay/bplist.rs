//! Binary property list helper.
//!
//! `plist` 1.x has no `to_value_*`: the only way to get bytes out of a `Value`
//! is to hand the serialiser a writer, so that is what this wraps. Both the
//! `GET /info` body and every event-channel record go through it.

use plist::Value;

/// Serialises `value` as a binary property list — the only encoding CarPlay
/// accepts on the control and event channels.
pub fn to_binary(value: &Value) -> Result<Vec<u8>, plist::Error> {
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, value)?;
    Ok(out)
}
