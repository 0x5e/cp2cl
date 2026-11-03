//! Property list helpers.
//!
//! `plist` 1.x has no `to_value_*`: the only way to get bytes out of a `Value`
//! is to hand the serialiser a writer, so that is what this wraps. Both the
//! `GET /info` body and every event-channel record go through it.
//!
//! The other direction — a request body as one line of JSON — is here for the
//! logs, which read a body far more often than they build one.

use plist::{Dictionary, Value};
use serde_json::{Map, Number, Value as Json};

/// Serialises `value` as a binary property list — the only encoding CarPlay
/// accepts on the control and event channels.
pub fn to_binary(value: &Value) -> Result<Vec<u8>, plist::Error> {
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, value)?;
    Ok(out)
}

/// A request body as one line of JSON.
///
/// `plist::Value` would serialise itself, but its `Data` comes out as an array
/// of byte numbers and its `Date` as an opaque newtype, and neither is a thing
/// to read in a log line. Here a blob is hex and a date is the ISO 8601 stamp
/// the XML format writes. Keys come out sorted, which is serde_json's doing and
/// no loss: a body is compared between runs, not read in order.
pub fn to_json(dict: &Dictionary) -> String {
    serde_json::to_string(&json_dict(dict)).unwrap_or_else(|_| String::from("{}"))
}

fn json_dict(dict: &Dictionary) -> Json {
    let mut out = Map::with_capacity(dict.len());
    for (key, value) in dict.iter() {
        out.insert(key.clone(), json_value(value));
    }
    Json::Object(out)
}

fn json_value(value: &Value) -> Json {
    match value {
        Value::Array(items) => Json::Array(items.iter().map(json_value).collect()),
        Value::Dictionary(dict) => json_dict(dict),
        Value::Boolean(flag) => Json::Bool(*flag),
        Value::Data(bytes) => Json::String(hex_preview(bytes)),
        Value::Date(date) => Json::String(date.to_xml_format()),
        // `f64` has no NaN or infinity in JSON, and a body carrying one is not
        // worth failing the whole line over.
        Value::Real(n) => Number::from_f64(*n).map_or(Json::Null, Json::Number),
        Value::Integer(n) => n
            .as_signed()
            .map(Json::from)
            .or_else(|| n.as_unsigned().map(Json::from))
            .unwrap_or(Json::Null),
        Value::String(text) => Json::String(text.clone()),
        Value::Uid(uid) => Json::from(uid.get()),
        // `Value` is `non_exhaustive`: a variant plist adds later is one this
        // build has never seen, and null is a fair thing to say about it.
        _ => Json::Null,
    }
}

/// The start of a blob as hex, with the length when there is more of it.
///
/// A body can carry a public key or a certificate, and all of one would bury
/// the rest of the line. What matters at this level is that the field was there
/// and how big it was.
fn hex_preview(bytes: &[u8]) -> String {
    const SHOWN: usize = 32;
    match bytes.len() {
        0 => String::new(),
        n if n <= SHOWN => hex::encode(bytes),
        n => format!("{}… ({n}B)", hex::encode(&bytes[..SHOWN])),
    }
}
