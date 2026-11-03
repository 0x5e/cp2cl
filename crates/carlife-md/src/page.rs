//! What the page shows about the car, and the one place the CarLife half
//! reports into.
//!
//! A write is a report, never a decision: nothing here is read back, so a field
//! that is stale costs a page and nothing else. The lock is never held across a
//! wait, which is what makes a plain `Mutex` the right one.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::proto::CarGps;

/// How many strings a head unit may name the device with. The protocol names
/// six; they are kept for the page and do not rename the device.
const ASKED: usize = 6;

pub struct Page {
    inner: Mutex<Snapshot>,
}

impl Page {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Snapshot::default()),
        }
    }

    /// The closure is handed the whole report so a report that touches two
    /// fields at once stays one write.
    pub fn with(&self, report: impl FnOnce(&mut Snapshot)) {
        // A poisoned lock means a report panicked half way through. What is
        // behind it is still the best account anyone has.
        let mut held = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        report(&mut held);
    }

    /// The car's half of the page. `video` is the stream's coding, which the
    /// other half reports and this one does not name: it is put in as it stands.
    pub fn carlife_json(&self, video: Value) -> Value {
        let held = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut body = held.carlife.json();
        body["video"] = video;
        body
    }

    pub fn now_playing_json(&self) -> Value {
        let held = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        held.now_playing.json()
    }

    /// The cover the car was last given, with the name the page asks for it by.
    pub fn cover(&self) -> Option<Cover> {
        let held = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        held.now_playing.cover.clone()
    }

    /// An empty one is the phone saying this song has none, and it puts the
    /// last one down.
    pub fn cover_changed(&self, bytes: Vec<u8>) {
        let cover = (!bytes.is_empty()).then(|| Cover {
            id: cover_id(&bytes),
            bytes: Arc::new(bytes),
        });
        self.with(|report| report.now_playing.cover = cover);
    }

    /// A new session starts with no song: what the last car was showing belongs
    /// to a car that is no longer there.
    pub fn now_playing_cleared(&self) {
        self.with(|report| report.now_playing = NowPlaying::default());
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// The car's half: a head unit talking CarLife to the device we present.
    pub carlife: Carlife,
    /// The song the car's panel is showing, which belongs to neither end.
    pub now_playing: NowPlaying,
}

/// What the CarLife side knows, which is everything the car told us.
#[derive(Debug, Clone, Default)]
pub struct Carlife {
    pub usb: Usb,
    /// Whether the head unit has asked us to come back as an accessory.
    pub accessory: bool,
    /// The AOA version answered when the head unit asked.
    pub aoa_version: Option<u16>,
    /// The six strings the head unit sent to name the device with, by index:
    /// manufacturer, model, description, version, URI, serial number.
    pub asked: [Option<String>; ASKED],
    /// The CarLife version the head unit opened with, major and minor.
    pub protocol: Option<(i32, i32)>,
    /// Whether the head unit has been given our device information.
    pub session_up: bool,
    /// What the head unit reports about itself.
    pub statistic: Option<Statistic>,
    /// Whether the head unit's channel number is one a phone serves. Our
    /// verdict, not the head unit's.
    pub channel_legal: Option<bool>,
    /// The challenge the head unit set, as it arrived.
    pub challenge: Option<String>,
    /// Whether that challenge was answered. `false` is a session with no key.
    pub challenge_answered: Option<bool>,
    /// The head unit's own verdict on us, from `HU_AUTHEN_RESULT`.
    pub accepted: Option<bool>,
    /// What the head unit asked to be rendered at, and what we agreed to.
    pub requested: Option<Size>,
    pub agreed: Option<Size>,
    /// What the agreed size works out to on the car's panel, in millimetres.
    pub physical: Option<(u32, u32)>,
    /// Whether the head unit has the encoder running.
    pub encoding: bool,
    /// The last frame rate the two ends agreed to.
    pub frame_rate: Option<i32>,
    /// The switches the head unit set, in the order it set them.
    pub features: Vec<(String, i32)>,
    /// The rate and channel count the media channel was announced in.
    pub media: Option<(u32, u16)>,
    /// How many bytes the head unit's last error message was.
    pub error_bytes: Option<usize>,
    /// The last position the head unit reported, in the units of
    /// `MSG_CMD_CAR_GPS`.
    pub gps: Option<CarGps>,
    /// The last speed the head unit reported, in kilometres an hour.
    pub speed: Option<i32>,
}

/// The USB device this program presents, as a host reads it.
#[derive(Debug, Clone, Default)]
pub struct Usb {
    /// `None` until the gadget is on the controller; then whether a host has
    /// enumerated it.
    pub enumerated: Option<bool>,
    /// The controller the gadget is bound to, as the kernel names it.
    pub controller: Option<String>,
    /// The numbers and strings in the device descriptor, which change when the
    /// head unit switches the device.
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
}

/// A screen, as either end of the bridge names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: i32,
    pub height: i32,
    pub fps: i32,
}

impl Size {
    fn json(&self) -> Value {
        json!({ "width": self.width, "height": self.height, "fps": self.fps })
    }
}

/// `MSG_CMD_HU_STATISTIC_INFO`: who the head unit thinks it is talking to.
#[derive(Debug, Clone, Default)]
pub struct Statistic {
    pub cuid: String,
    pub version_name: String,
    pub version_code: i32,
    pub channel: String,
    pub connect_count: i32,
    pub connect_success_count: i32,
    /// Reported as it arrives. Neither the guide nor the head unit's own library
    /// says in what unit, so no attempt is made to read it as a duration.
    pub connect_time: i32,
}

impl Statistic {
    fn json(&self) -> Value {
        json!({
            "cuid": self.cuid,
            "version_name": self.version_name,
            "version_code": self.version_code,
            "channel": self.channel,
            "connect_count": self.connect_count,
            "connect_success_count": self.connect_success_count,
            "connect_time": self.connect_time,
        })
    }
}

/// The song on the car's panel.
#[derive(Debug, Clone, Default)]
pub struct NowPlaying {
    /// The app it is playing from, which the car shows as the song's source.
    pub source: Option<String>,
    pub song: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Seconds, as `Track` has it.
    pub duration_s: Option<i32>,
    /// The position that last went to the car's bar, in milliseconds.
    pub progress_ms: Option<u32>,
    /// What the car was last told the song is doing. `None` until the phone has
    /// said, because the car hears about a change of state and nothing else.
    pub playing: Option<bool>,
    /// The cover exactly as it went to the car. Kept when a song changes without
    /// one, because that is what the car does with it too.
    pub cover: Option<Cover>,
}

/// A picture, and the name the page asks for it by.
#[derive(Debug, Clone)]
pub struct Cover {
    /// The bytes, exactly as the car was given them.
    pub bytes: Arc<Vec<u8>>,
    /// A hash of those bytes, which is what the page puts in its request.
    pub id: u64,
}

/// A hash rather than a counter: the page keeps what it has fetched and asks
/// again only when the name changes, and a name that came round again would show
/// a picture belonging to a song that has finished. FNV-1a over the bytes.
fn cover_id(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl Carlife {
    fn json(&self) -> Value {
        json!({
            "usb": {
                "enumerated": self.usb.enumerated,
                "controller": self.usb.controller,
                "vid": self.usb.vid,
                "pid": self.usb.pid,
                "manufacturer": self.usb.manufacturer,
                "product": self.usb.product,
                "serial": self.usb.serial,
            },
            "accessory": self.accessory,
            "aoa_version": self.aoa_version,
            "asked": self.asked,
            "protocol": self
                .protocol
                .map(|(major, minor)| json!({ "major": major, "minor": minor })),
            "session_up": self.session_up,
            "statistic": self.statistic.as_ref().map(Statistic::json),
            "channel_legal": self.channel_legal,
            "challenge": self.challenge,
            "challenge_answered": self.challenge_answered,
            "accepted": self.accepted,
            "requested": self.requested.as_ref().map(Size::json),
            "agreed": self.agreed.as_ref().map(Size::json),
            "physical": self.physical.map(|(width_mm, height_mm)| json!({
                "width_mm": width_mm,
                "height_mm": height_mm,
            })),
            "encoding": self.encoding,
            "frame_rate": self.frame_rate,
            "features": self
                .features
                .iter()
                .map(|(key, value)| json!({ "key": key, "value": value }))
                .collect::<Vec<_>>(),
            "media": self
                .media
                .map(|(rate, channels)| json!({ "rate": rate, "channels": channels, "bits": 16 })),
            // Filled in by `Page::carlife_json`: the coding is the phone's
            // stream, which the other half of the bridge reports.
            "video": Value::Null,
            "error_bytes": self.error_bytes,
            "gps": self.gps.as_ref().map(|gps| json!({
                "antenna_state": gps.antenna_state,
                "latitude": gps.latitude,
                "longitude": gps.longitude,
                "height": gps.height,
                "speed": gps.speed,
                "heading": gps.heading,
                "hor_pos_error": gps.hor_pos_error,
                "vert_pos_error": gps.vert_pos_error,
                "year": gps.year,
                "month": gps.month,
                "day": gps.day,
                "hour": gps.hour,
                "minute": gps.minute,
                "second": gps.second,
                "timestamp_ms": gps.timestamp_ms,
            })),
            "speed": self.speed,
        })
    }
}

impl NowPlaying {
    fn json(&self) -> Value {
        json!({
            "source": self.source,
            "song": self.song,
            "artist": self.artist,
            "album": self.album,
            "duration_s": self.duration_s,
            "progress_ms": self.progress_ms,
            "playing": self.playing,
            // The bytes are not in the answer: the page fetches the picture
            // from its own path, and this is what it asks for it by.
            "cover": self.cover.as_ref().map(|cover| json!({
                "id": format!("{:016x}", cover.id),
                "bytes": cover.bytes.len(),
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_is_one_write_and_the_page_reads_it_back() {
        let page = Page::new();
        page.with(|report| {
            report.carlife.session_up = true;
            report.carlife.agreed = Some(Size {
                width: 1280,
                height: 720,
                fps: 30,
            });
            report.now_playing.song = Some("a song".into());
        });

        assert_eq!(page.carlife_json(Value::Null)["session_up"], json!(true));
        assert_eq!(
            page.carlife_json(Value::Null)["agreed"],
            json!({ "width": 1280, "height": 720, "fps": 30 })
        );
        assert_eq!(page.now_playing_json()["song"], json!("a song"));
    }

    #[test]
    fn the_coding_is_whatever_the_other_half_reports() {
        let page = Page::new();
        let video = json!({ "codec": "H.264", "profile": "High", "level": "4" });
        assert_eq!(page.carlife_json(video.clone())["video"], video);
    }

    /// A cover is named by its bytes, and an empty one puts the last down.
    #[test]
    fn the_cover_is_named_by_what_it_is() {
        let page = Page::new();
        page.cover_changed(vec![1, 2, 3]);
        let first = page.cover().expect("a picture");
        assert_eq!(first.bytes.as_slice(), &[1, 2, 3]);

        page.cover_changed(vec![1, 2, 3]);
        assert_eq!(page.cover().expect("still there").id, first.id);

        page.cover_changed(Vec::new());
        assert!(page.cover().is_none(), "no picture is not a picture");
    }

    #[test]
    fn a_new_session_starts_with_no_song() {
        let page = Page::new();
        page.with(|report| report.now_playing.song = Some("a song".into()));
        page.now_playing_cleared();
        assert!(page.now_playing_json()["song"].is_null());
    }
}
