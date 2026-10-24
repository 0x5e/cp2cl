//! The `GET /info` answer: who we are and what we can take.
//!
//! This is the single most important reply in the whole session. The phone reads
//! it right after the session-level SETUP and only asks for the screen stream if
//! the answer is complete. A missing `displays` entry or mismatched HID uuids
//! and the phone quietly stops talking — no error, just no picture.
//!
//! Values here are taken from LIVI's `getInfo.ts`, which is a known-good answer.

use std::collections::HashMap;
use std::sync::OnceLock;

use plist::{dictionary::Dictionary, Value};

use super::bplist::to_binary;

pub const MAIN_UUID: &str = "1f5a3f1e-7a04-4c0b-9a52-6b4d3c8e2a10";
pub const TOUCH_HID_UID: u32 = 0x2a2a2a2a;
pub const KNOB_HID_UID: u32 = 0x2a2a2a2b;
pub const MEDIA_HID_UID: u32 = 0x2a2a2a2c;
pub const TELEPHONY_HID_UID: u32 = 0x2a2a2a2d;

const SOURCE_VERSION: &str = "320.17";
/// Every feature bit LIVI advertises. Copying this wholesale is safer than
/// computing just the few we implement: the phone key negotiation off it.
const CARPLAY_FEATURES: i64 = 0x6156_53ae_e2;

const RESOURCE_SCREEN: i64 = 1;
const RESOURCE_AUDIO: i64 = 2;
const TRANSFER_TAKE: i64 = 1;
const PRIORITY_NICE_TO_HAVE: i64 = 100;
const CONSTRAINT_ANYTIME: i64 = 100;

// Bit values are LIVI's `getInfo.ts`, verbatim.
const DISPLAY_KNOBS: i64 = 0x02;
const DISPLAY_HIGH_FIDELITY_TOUCH: i64 = 0x08;
/// LIVI declares a knob device but still reports 1 (touch) here, and the phone
/// behaves differently on 3 — with 3 it never opens a screen stream.
const PRIMARY_INPUT_TOUCH: i64 = 1;
const STREAM_TYPE_MAIN_SCREEN: i64 = 110;

pub struct Static {
    pub bt_mac: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The display's density, in pixels per inch. The physical size the phone is
    /// told follows from it and the canvas above.
    pub ppi: u32,
    /// The channel number the head unit reported, when it has reported one. It
    /// picks the label and the icon the phone shows for us on its CarPlay
    /// screen.
    pub channel: Option<String>,
}

pub fn build(static_: &Static) -> Result<Vec<u8>, plist::Error> {
    let mut d = Dictionary::new();
    d.insert("sourceVersion".into(), Value::String(SOURCE_VERSION.into()));
    d.insert("features".into(), Value::Integer(CARPLAY_FEATURES.into()));
    d.insert("statusFlags".into(), Value::Integer(4.into()));
    // What the accessory is, under the name the machine goes by. Nothing here
    // invents a second name for it: the phone shows this one in its CarPlay
    // list, and the car is told the same in the CarLife handshake.
    d.insert("model".into(), Value::String(static_.name.clone()));
    d.insert("manufacturer".into(), Value::String(static_.name.clone()));
    // One address under two keys: this machine's own Bluetooth address is the
    // only identifier the accessory has.
    d.insert("deviceID".into(), Value::String(static_.bt_mac.clone()));
    d.insert(
        "bluetoothIDs".into(),
        Value::Array(vec![Value::String(static_.bt_mac.clone())]),
    );
    d.insert("name".into(), Value::String(static_.name.clone()));
    d.insert("rightHandDrive".into(), Value::Boolean(false));
    d.insert("keepAliveLowPower".into(), Value::Boolean(false));
    d.insert("keepAliveSendStatsAsBody".into(), Value::Boolean(false));
    d.insert("modes".into(), modes());
    d.insert("audioLatencies".into(), audio_latencies());
    d.insert("audioFormats".into(), audio_formats(48_000));
    d.insert(
        "extendedFeatures".into(),
        Value::Array(vec![
            Value::String("vocoderInfo".into()),
            Value::String("enhancedRequestCarUI".into()),
        ]),
    );
    d.insert(
        "displays".into(),
        Value::Array(vec![display_entry(static_)]),
    );
    d.insert("hidDevices".into(), hid_devices(static_));
    // The icon the phone draws for us on its CarPlay screen, and the label under
    // it. The head unit's channel number decides both: a make we ship a picture
    // for gets its own, everything else gets the default.
    let label = oem_label(static_.channel.as_deref());
    d.insert("oemIconVisible".into(), Value::Boolean(true));
    d.insert("oemIconLabel".into(), Value::String(label.into()));
    d.insert("oemIcons".into(), Value::Array(oem_icons(label)));
    to_binary(&Value::Dictionary(d))
}

/// The label under our icon when no channel has named one.
const DEFAULT_LABEL: &str = "App";

/// The channel table as it is written, parsed.
///
/// `assets/carlife_channel_map.json`: one entry per make, holding the channel
/// numbers that make answers on. A make with several numbers is one entry with
/// several `channels`, which is why the table is not keyed by the number — each
/// reader below keys what it takes by the number itself.
///
/// Read twice, once for the labels and once for the sizes to serve, and each
/// reader keeps only its own field: an entry may name a label, a size, or both.
fn map_entries() -> Vec<serde_json::Value> {
    let table = crate::assets::text("carlife_channel_map.json")
        .expect("the channel map is built into the binary");
    serde_json::from_str(table).expect("the channel map is valid JSON")
}

/// The channel number a head unit reports, and the label worth showing for it.
///
/// For the makes we ship a picture for, the entry also carries the `label` the
/// phone draws under the icon. Only entries that name one are kept: `code` names
/// the head unit on the page and is not part of this answer.
static CHANNEL_LABELS: OnceLock<HashMap<String, String>> = OnceLock::new();

fn channel_labels() -> &'static HashMap<String, String> {
    CHANNEL_LABELS.get_or_init(|| {
        let mut labels = HashMap::new();
        for entry in map_entries() {
            // No label, or a blank one, is a make with no picture of its own.
            let label = entry["label"].as_str().unwrap_or("").trim();
            if label.is_empty() {
                continue;
            }
            let channels = entry["channels"].as_array().into_iter().flatten();
            for id in channels.filter_map(|channel| channel["id"].as_str()) {
                labels.insert(id.trim().to_string(), label.to_string());
            }
        }
        labels
    })
}

/// The label the phone shows under our icon for `channel`.
pub fn oem_label(channel: Option<&str>) -> &'static str {
    channel
        .and_then(|c| channel_labels().get(c.trim()))
        .map_or(DEFAULT_LABEL, String::as_str)
}

/// The density a panel is taken to have when the channel table says nothing about
/// it: 120 pixels per inch, which is what the 8-inch panels these head units
/// carry work out to.
///
/// The density is what decides how finely the phone draws: the physical size it is
/// told is turned back into a dpi, and inventing too large a size drops it to 1×
/// and the picture goes blurry. The size in millimetres follows from this and the
/// canvas, because no CarLife message carries one. A make that knows its own says
/// so in its entry.
pub const DEFAULT_PPI: u32 = 120;

/// A size and rate a head unit's make is served at, from the `video` field of its
/// entry in the channel table: `"1920x720@30"`.
///
/// The field is for a head unit whose own CarLife application asks for a smaller
/// screen than the panel it drives. The phone is then asked for these numbers
/// instead of the ones the car named, and the car scales what it receives to the
/// panel — which is what it does with a size it did not ask for anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// What the channel table says about the panel behind a head unit's number.
///
/// Both fields are optional and neither implies the other: a make may name the
/// size to serve, the density of its panel, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panel {
    /// The size and rate the phone is asked for, when the entry names one.
    pub preset: Option<Preset>,
    /// The density the panel's millimetres are worked out at: the entry's own
    /// `ppi` field, or [`DEFAULT_PPI`] where it names none.
    pub ppi: u32,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            preset: None,
            ppi: DEFAULT_PPI,
        }
    }
}

/// The same table, keyed by the channel number, holding only the numbers that say
/// something about their panel. The fields belong to the number rather than to
/// the make: one make answers on several numbers, and only some of them are wrong
/// about their screen.
static CHANNEL_PANELS: OnceLock<HashMap<String, Panel>> = OnceLock::new();

fn channel_panels() -> &'static HashMap<String, Panel> {
    CHANNEL_PANELS.get_or_init(|| {
        let mut panels = HashMap::new();
        for entry in map_entries() {
            let channels = entry["channels"].as_array().into_iter().flatten();
            for channel in channels {
                let Some(id) = channel["id"].as_str() else {
                    continue;
                };
                let preset = channel["video"].as_str().and_then(parse_preset);
                let ppi = channel["ppi"]
                    .as_u64()
                    .and_then(|density| u32::try_from(density).ok())
                    .filter(|density| *density > 0);
                if preset.is_none() && ppi.is_none() {
                    continue;
                }
                let panel = Panel {
                    preset,
                    ppi: ppi.unwrap_or(DEFAULT_PPI),
                };
                panels.insert(id.trim().to_string(), panel);
            }
        }
        panels
    })
}

/// What the table says about the panel behind `channel`. A number it does not
/// hold, or no number at all, is the default: 120 pixels per inch, and the size
/// the car itself asked for.
pub fn panel(channel: Option<&str>) -> Panel {
    channel
        .and_then(|number| channel_panels().get(number.trim()))
        .copied()
        .unwrap_or_default()
}

/// A `video` field as the table writes it: `"1920x720@30"`.
///
/// One that does not parse is a preset that is not there rather than a failure:
/// the table is written by hand, and a line that is wrong should cost the one
/// make it is wrong for and nothing else. `every_video_field_parses` is what says
/// so before it ships, because a typo here looks like nothing happening.
fn parse_preset(text: &str) -> Option<Preset> {
    let (size, fps) = text.trim().split_once('@')?;
    let (width, height) = size.split_once(['x', 'X'])?;
    let preset = Preset {
        width: width.trim().parse().ok()?,
        height: height.trim().parse().ok()?,
        fps: fps.trim().parse().ok()?,
    };
    (preset.width > 0 && preset.height > 0 && preset.fps > 0).then_some(preset)
}

/// The PNG of our icon for `label`, at `size` pixels square.
///
/// A make we ship a picture for is `icons/<label>-<size>.png` in `assets/`;
/// anything else is `icons/Default-<size>.png`. Both are in the binary, so this
/// is a lookup rather than a file read — and adding a make is a matter of
/// dropping two pictures in, not of adding a branch here.
fn icon(label: &str, size: u32) -> &'static [u8] {
    let named = format!("icons/{label}-{size}.png");
    let fallback = format!("icons/Default-{size}.png");
    crate::assets::get(&named)
        .or_else(|| crate::assets::get(&fallback))
        .unwrap_or_else(|| panic!("neither {named} nor {fallback} is in the binary"))
}

/// Our icon in the two sizes the phone may ask for, as `/info` carries it.
///
/// `prerendered` says each picture is already at the size it claims, so the
/// phone draws it as it is rather than scaling it first.
fn oem_icons(label: &str) -> Vec<Value> {
    [256, 512]
        .into_iter()
        .map(|size| {
            let mut entry = Dictionary::new();
            let pixels = Value::Integer(i64::from(size).into());
            entry.insert("imageData".into(), Value::Data(icon(label, size).to_vec()));
            entry.insert("widthPixels".into(), pixels.clone());
            entry.insert("heightPixels".into(), pixels);
            entry.insert("prerendered".into(), Value::Boolean(true));
            Value::Dictionary(entry)
        })
        .collect()
}

fn resource(id: i64) -> Value {
    let mut d = Dictionary::new();
    d.insert("resourceID".into(), Value::Integer(id.into()));
    d.insert("transferType".into(), Value::Integer(TRANSFER_TAKE.into()));
    d.insert(
        "transferPriority".into(),
        Value::Integer(PRIORITY_NICE_TO_HAVE.into()),
    );
    d.insert(
        "takeConstraint".into(),
        Value::Integer(CONSTRAINT_ANYTIME.into()),
    );
    d.insert(
        "borrowConstraint".into(),
        Value::Integer(CONSTRAINT_ANYTIME.into()),
    );
    d.insert(
        "unborrowConstraint".into(),
        Value::Integer(CONSTRAINT_ANYTIME.into()),
    );
    Value::Dictionary(d)
}

fn modes() -> Value {
    let mut d = Dictionary::new();
    d.insert(
        "resources".into(),
        Value::Array(vec![resource(RESOURCE_SCREEN), resource(RESOURCE_AUDIO)]),
    );
    let mut speech = Dictionary::new();
    speech.insert("appStateID".into(), Value::Integer(1.into()));
    speech.insert("speechMode".into(), Value::Integer((-1).into()));
    let mut call = Dictionary::new();
    call.insert("appStateID".into(), Value::Integer(2.into()));
    call.insert("state".into(), Value::Boolean(false));
    let mut nav = Dictionary::new();
    nav.insert("appStateID".into(), Value::Integer(3.into()));
    nav.insert("state".into(), Value::Boolean(false));
    d.insert(
        "appStates".into(),
        Value::Array(vec![
            Value::Dictionary(call),
            Value::Dictionary(speech),
            Value::Dictionary(nav),
        ]),
    );
    Value::Dictionary(d)
}

fn latency_base(type_: i64, audio_type: Option<&str>) -> Value {
    let mut d = Dictionary::new();
    d.insert("type".into(), Value::Integer(type_.into()));
    d.insert("inputLatencyMicros".into(), Value::Integer(0.into()));
    d.insert("outputLatencyMicros".into(), Value::Integer(0.into()));
    if let Some(a) = audio_type {
        d.insert("audioType".into(), Value::String(a.into()));
    }
    Value::Dictionary(d)
}

fn audio_latencies() -> Value {
    Value::Array(vec![
        latency_base(100, None),
        latency_base(100, Some("default")),
        latency_base(100, Some("media")),
        latency_base(100, Some("telephony")),
        latency_base(100, Some("speechRecognition")),
        latency_base(100, Some("alert")),
        latency_base(101, None),
        latency_base(101, Some("default")),
        latency_base(102, Some("default")),
    ])
}

fn format_entry(type_: i64, audio_type: &str, output: i64, input: Option<i64>) -> Value {
    let mut d = Dictionary::new();
    d.insert("type".into(), Value::Integer(type_.into()));
    d.insert("audioType".into(), Value::String(audio_type.into()));
    d.insert("audioOutputFormats".into(), Value::Integer(output.into()));
    if let Some(i) = input {
        d.insert("audioInputFormats".into(), Value::Integer(i.into()));
    }
    Value::Dictionary(d)
}

/// What the phone is offered, per stream type.
///
/// PCM is offered throughout, but a phone on wireless CarPlay answers this by
/// naming AAC-LC for the entertainment stream and does not take any of the PCM
/// entries: measured on 2026-10-13. The low-latency streams still come back as
/// OPUS. Whatever the phone names is what this list has to be read alongside.
fn audio_formats(rate: u32) -> Value {
    let pcm_voice: i64 = 0x3fc;
    let is48 = rate == 48_000;
    let pcm = pcm_voice | if is48 { 0xc000 } else { 0xc00 };
    let pcm_mono: i64 = 0x154 | if is48 { 0x4000 } else { 0x400 };
    let pcm_media: i64 = if is48 { 0x8000 } else { 0x800 };
    let opus: i64 = 0x7000_0000;
    let aac_lc: i64 = if is48 { 0x800000 } else { 0x400000 };

    let entries: [(i64, &str, i64, Option<i64>); 9] = [
        (100, "compatibility", pcm, Some(pcm_mono)),
        (101, "compatibility", pcm, None),
        (100, "default", pcm | opus, Some(pcm_mono | opus)),
        (100, "alert", pcm | opus, None),
        (100, "media", pcm_media, None),
        (100, "telephony", pcm_mono | opus, Some(pcm_mono | opus)),
        (100, "speechRecognition", pcm_mono | opus, Some(pcm_mono | opus)),
        (101, "default", pcm | opus, None),
        (102, "media", aac_lc, None),
    ];
    Value::Array(
        entries
            .into_iter()
            .map(|(type_, name, output, input)| format_entry(type_, name, output, input))
            .collect(),
    )
}

/// Pixels to millimetres at a density, in integer arithmetic: 25.4 millimetres to
/// the inch. A density of zero would be a division by zero, so it is read as one
/// pixel per inch — a physical size that large is what tells the phone to draw
/// at 1×, which is what asking for it means.
pub(crate) fn millimetres(pixels: u32, ppi: u32) -> u32 {
    (pixels as u64 * 254 / (ppi.max(1) as u64 * 10)) as u32
}

fn display_entry(s: &Static) -> Value {
    // How finely the phone draws is a dpi, and a dpi is a pixel count over a
    // physical size. The pixels are the car's, and the density is the operator's:
    // the size that goes to the phone is what the two of them make.
    let width_mm = millimetres(s.width, s.ppi);
    let height_mm = millimetres(s.height, s.ppi);

    let mut d = Dictionary::new();
    d.insert("uuid".into(), Value::String(MAIN_UUID.into()));
    d.insert(
        "type".into(),
        Value::Integer(STREAM_TYPE_MAIN_SCREEN.into()),
    );
    d.insert("maxFPS".into(), Value::Integer(s.fps.into()));
    d.insert("widthPixels".into(), Value::Integer(s.width.into()));
    d.insert("heightPixels".into(), Value::Integer(s.height.into()));
    d.insert("widthPhysical".into(), Value::Integer(width_mm.into()));
    d.insert("heightPhysical".into(), Value::Integer(height_mm.into()));
    d.insert(
        "features".into(),
        Value::Integer((DISPLAY_HIGH_FIDELITY_TOUCH | DISPLAY_KNOBS).into()),
    );
    d.insert(
        "primaryInputDevice".into(),
        Value::Integer(PRIMARY_INPUT_TOUCH.into()),
    );
    // SETUP advertises `viewAreas`, so the display has to carry one or the phone
    // sees a contradiction and tears the session down instead of opening streams.
    d.insert("viewAreas".into(), Value::Array(vec![view_area(s)]));
    d.insert("initialViewArea".into(), Value::Integer(0.into()));
    Value::Dictionary(d)
}

/// One view area covering the whole panel, with a safe area of the same size.
/// LIVI always sends this (from its configured insets, which default to zero).
fn view_area(s: &Static) -> Value {
    let mut safe = Dictionary::new();
    safe.insert("widthPixels".into(), Value::Integer(s.width.into()));
    safe.insert("heightPixels".into(), Value::Integer(s.height.into()));
    safe.insert("originXPixels".into(), Value::Integer(0.into()));
    safe.insert("originYPixels".into(), Value::Integer(0.into()));
    // MacPlay sets this, and it is the one field our /info was missing.
    safe.insert("drawUIOutsideSafeArea".into(), Value::Boolean(true));

    let mut view = Dictionary::new();
    view.insert("widthPixels".into(), Value::Integer(s.width.into()));
    view.insert("heightPixels".into(), Value::Integer(s.height.into()));
    view.insert("originXPixels".into(), Value::Integer(0.into()));
    view.insert("originYPixels".into(), Value::Integer(0.into()));
    view.insert("safeArea".into(), Value::Dictionary(safe));
    Value::Dictionary(view)
}

fn hid_device(uid: u32, name: &str, descriptor: Vec<u8>) -> Value {
    let mut d = Dictionary::new();
    d.insert("hidProductID".into(), Value::Integer(1.into()));
    d.insert("hidVendorID".into(), Value::Integer(2.into()));
    d.insert("hidCountryCode".into(), Value::Integer(0.into()));
    d.insert("uuid".into(), Value::String(format!("{uid:x}")));
    d.insert("name".into(), Value::String(name.into()));
    d.insert("displayUUID".into(), Value::String(MAIN_UUID.into()));
    d.insert("hidDescriptor".into(), Value::Data(descriptor));
    Value::Dictionary(d)
}

fn hid_devices(s: &Static) -> Value {
    Value::Array(vec![
        hid_device(
            TOUCH_HID_UID,
            "cp2cl Touchscreen",
            multitouch_descriptor(s.width, s.height),
        ),
        hid_device(KNOB_HID_UID, "cp2cl Knob", KNOB_DESCRIPTOR.to_vec()),
        hid_device(MEDIA_HID_UID, "cp2cl Media", MEDIA_DESCRIPTOR.to_vec()),
        hid_device(
            TELEPHONY_HID_UID,
            "cp2cl Telephony",
            TELEPHONY_DESCRIPTOR.to_vec(),
        ),
    ])
}

/// Two absolute-position finger collections, then the application collection.
/// Declared even though we never send a touch report, because the phone wants a
/// pointing device for the main display.
fn multitouch_descriptor(x_max: u32, y_max: u32) -> Vec<u8> {
    let mut out = vec![0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01];
    #[rustfmt::skip]
    let finger = |x_max: u32, y_max: u32| -> Vec<u8> {
        vec![
            0x05, 0x0d,                      // Usage Page (Digitizers)
            0x09, 0x22,                      // Usage (Finger)
            0xa1, 0x02,                      // Collection (Logical)
            0x09, 0x38,                      //   Usage (Transducer Index)
            0x75, 0x08, 0x95, 0x01, 0x81, 0x02,
            0x15, 0x00,                      //   Logical Minimum (0)
            0x25, 0x01,                      //   Logical Maximum (1)
            0x09, 0x33,                      //   Usage (Touch)
            0x75, 0x01, 0x95, 0x01, 0x81, 0x02,
            0x95, 0x07, 0x81, 0x03,          //   padding
            0x05, 0x01,                      //   Usage Page (Generic Desktop)
            0x26, (x_max & 0xff) as u8, ((x_max >> 8) & 0xff) as u8,
            0x09, 0x30, 0x75, 0x10, 0x95, 0x01, 0x81, 0x02,
            0x26, (y_max & 0xff) as u8, ((y_max >> 8) & 0xff) as u8,
            0x09, 0x31, 0x81, 0x02,
            0xc0,                            // End Collection
        ]
    };
    out.extend(finger(x_max, y_max));
    out.extend(finger(x_max, y_max));
    out.push(0xc0);
    out
}

/// From LIVI's `hid.ts`, verbatim.
#[rustfmt::skip]
const KNOB_DESCRIPTOR: &[u8] = &[
    0x05,0x01,0x09,0x08,0xa1,0x01,0x05,0x09,0x09,0x01,0x15,0x00,0x25,0x01,0x75,0x01,
    0x95,0x01,0x81,0x02,0x05,0x0c,0x0a,0x23,0x02,0x0a,0x24,0x02,0x95,0x02,0x81,0x02,
    0x95,0x05,0x81,0x01,0x05,0x01,0x09,0x01,0xa1,0x00,0x09,0x30,0x09,0x31,0x15,0x81,
    0x25,0x7f,0x75,0x08,0x95,0x02,0x81,0x02,0xc0,0x09,0x38,0x15,0x81,0x25,0x7f,0x75,
    0x08,0x95,0x01,0x81,0x06,0xc0,
];

#[rustfmt::skip]
const MEDIA_DESCRIPTOR: &[u8] = &[
    0x05,0x0c,0x09,0x01,0xa1,0x01,0x15,0x00,0x25,0x06,0x05,0x0c,0x0a,0x00,0x00,0x0a,
    0xb0,0x00,0x0a,0xb1,0x00,0x0a,0xcd,0x00,0x0a,0xb5,0x00,0x0a,0xb6,0x00,0x0a,0x9e,
    0x02,0x75,0x08,0x95,0x01,0x81,0x00,0xc0,
];

#[rustfmt::skip]
const TELEPHONY_DESCRIPTOR: &[u8] = &[
    0x05,0x0b,0x09,0x07,0xa1,0x01,0x15,0x00,0x25,0x11,0x05,0x0b,0x09,0x00,0x09,0x20,
    0x09,0x21,0x09,0x26,0x09,0x2f,0x09,0xb0,0x09,0xb1,0x09,0xb2,0x09,0xb3,0x09,0xb4,
    0x09,0xb5,0x09,0xb6,0x09,0xb7,0x09,0xb8,0x09,0xb9,0x09,0xba,0x09,0xbb,0x05,0x07,
    0x09,0x2a,0x75,0x08,0x95,0x01,0x81,0x00,0xc0,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// One entry's output formats, or `None` when that entry is not offered.
    fn offered(value: &Value, type_: i64, audio_type: &str) -> Option<i64> {
        let Value::Array(items) = value else {
            panic!("audioFormats is an array");
        };
        items
            .iter()
            .filter_map(|item| item.as_dictionary())
            .find(|d| {
                d.get("type").and_then(|v| v.as_signed_integer()) == Some(type_)
                    && d.get("audioType").and_then(|v| v.as_string()) == Some(audio_type)
            })
            .and_then(|d| d.get("audioOutputFormats"))
            .and_then(|v| v.as_signed_integer())
    }

    /// The bitmaps a 48 kHz offer names: the full-band PCM family, and the
    /// mono one the low-latency streams are offered.
    const PCM_48K: i64 = 0x3fc | 0xc000;
    const PCM_MONO_48K: i64 = 0x154 | 0x4000;

    fn count(value: &Value) -> usize {
        let Value::Array(items) = value else {
            panic!("audioFormats is an array");
        };
        items.len()
    }

    /// The offer as it stands: every stream type, with the codings the phone
    /// picks from. A missing entry is a stream the phone will not open at all,
    /// so the shape of this table is part of the protocol rather than a detail.
    #[test]
    fn every_stream_type_is_offered_its_codings() {
        let table = audio_formats(48_000);
        assert_eq!(count(&table), 9);
        assert_eq!(offered_entry(&table, 102, "media"), 0x80_0000);
        assert_eq!(offered_entry(&table, 100, "media"), 0x8000);
        assert_eq!(offered_entry(&table, 100, "alert"), PCM_48K | 0x7000_0000);
        assert_eq!(
            offered_entry(&table, 100, "telephony"),
            PCM_MONO_48K | 0x7000_0000
        );
    }

    fn offered_entry(value: &Value, type_: i64, audio_type: &str) -> i64 {
        offered(value, type_, audio_type).expect("that entry is offered")
    }

    /// A channel with a label of its own gets that label and the matching
    /// picture; every other channel, and no channel at all, gets the default.
    /// The label belongs to the make, so every number the make answers on gets
    /// it — not just the first.
    #[test]
    fn a_channel_names_its_own_icon() {
        for channel in ["20062100", "20062101", "20062104"] {
            assert_eq!(oem_label(Some(channel)), "Mercedes", "{channel} is a Mercedes");
        }
        assert_eq!(oem_label(Some(" 20062100 ")), "Mercedes");
        assert_eq!(oem_label(Some("20062106")), DEFAULT_LABEL, "not a number it answers on");
        assert_eq!(oem_label(Some("20012100")), DEFAULT_LABEL, "a make with no label");
        assert_eq!(oem_label(None), DEFAULT_LABEL);
        assert_ne!(icon("Mercedes", 256), icon(DEFAULT_LABEL, 256));

        let icons = oem_icons("Mercedes");
        assert_eq!(icons.len(), 2);
        let first = icons[0].as_dictionary().expect("an icon is a dictionary");
        assert_eq!(
            first.get("widthPixels").and_then(Value::as_signed_integer),
            Some(256)
        );
        assert_eq!(
            first.get("prerendered").and_then(Value::as_boolean),
            Some(true)
        );
        let png = first
            .get("imageData")
            .and_then(Value::as_data)
            .expect("the picture is carried");
        assert_eq!(&png[..4], b"\x89PNG");
    }

    /// A number the table gives a size or a density to is served at those, and one
    /// it says nothing about gets the default size — whatever the car asked for —
    /// and 120 dpi. The fields belong to the number, so another number of the same
    /// make is not covered by them.
    #[test]
    fn a_number_names_the_panel_behind_it() {
        let mercedes = Preset {
            width: 1920,
            height: 720,
            fps: 30,
        };
        assert_eq!(
            panel(Some("20062104")),
            Panel {
                preset: Some(mercedes),
                ppi: 120
            }
        );
        assert_eq!(
            panel(Some(" 20062104 ")),
            panel(Some("20062104")),
            "the number is trimmed as it is everywhere else"
        );
        assert_eq!(
            panel(Some("20062100")),
            Panel::default(),
            "the same make, a number with nothing of its own"
        );
        assert_eq!(panel(Some("20062106")), Panel::default(), "not a number it answers on");
        assert_eq!(panel(None), Panel::default());
        assert_eq!(
            Panel::default(),
            Panel {
                preset: None,
                ppi: DEFAULT_PPI
            }
        );

        assert_eq!(parse_preset("1920x720@30"), Some(mercedes));
        assert_eq!(parse_preset(" 1920 X 720 @ 30 "), Some(mercedes));
        for bad in ["1920x720", "1920x720@", "x720@30", "1920x@30", "1920x720@0", ""] {
            assert_eq!(parse_preset(bad), None, "{bad:?} is not a size and a rate");
        }
    }

    /// Every field the table carries has to be the thing it claims to be. These
    /// are the mistakes in a hand-written table that look like nothing happening:
    /// the field is there, it is not read, and the head unit is served what it
    /// asked for at the default density.
    #[test]
    fn every_field_the_table_carries_parses() {
        let mut fields = 0;
        for entry in map_entries() {
            let channels = entry["channels"].as_array().into_iter().flatten();
            for channel in channels {
                let id = channel["id"].as_str().unwrap_or_default();
                if let Some(field) = channel["video"].as_str() {
                    fields += 1;
                    assert!(
                        parse_preset(field).is_some(),
                        "{id} names a size that is not one: {field:?}"
                    );
                }
                if let Some(density) = channel.get("ppi") {
                    fields += 1;
                    let usable = density
                        .as_u64()
                        .and_then(|density| u32::try_from(density).ok())
                        .is_some_and(|density| density > 0);
                    assert!(usable, "{id} names a density that is not one: {density}");
                }
            }
        }
        assert!(fields > 0, "at least one number names something");
    }

    /// The table is edited by hand, and the two mistakes that cost something are
    /// silent ones: a make with no number answers nothing, and a number listed
    /// twice is answered by whichever entry came last. Both fail here instead.
    #[test]
    fn a_channel_number_belongs_to_one_make() {
        let table = crate::assets::text("carlife_channel_map.json")
            .expect("the channel map is built into the binary");
        let entries: Vec<serde_json::Value> =
            serde_json::from_str(table).expect("the channel map is valid JSON");

        let mut seen = std::collections::HashSet::new();
        for entry in &entries {
            let code = entry["code"].as_str().unwrap_or_default().trim();
            assert!(!code.is_empty(), "every entry names a code");
            let channels = entry["channels"]
                .as_array()
                .unwrap_or_else(|| panic!("{code} lists its channels"));
            assert!(!channels.is_empty(), "{code} answers on a number");
            for channel in channels {
                let id = channel["id"].as_str().unwrap_or_default().trim();
                assert!(!id.is_empty(), "{code} has a number that is not blank");
                assert!(seen.insert(id.to_string()), "{id} is listed under two makes");
            }
        }
    }
}
