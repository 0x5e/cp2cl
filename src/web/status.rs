//! What the page shows, and the one place every part of the bridge reports into.
//!
//! Nothing here observes anything for itself. The bridge already knows
//! everything the page displays — it says so in its log lines — and this is that
//! same knowledge kept rather than printed: each module that learns something
//! writes it down here, and the page reads the lot.
//!
//! Two rules keep it from becoming a second source of truth:
//!
//! * **A write is a report, never a decision.** Nothing in the bridge reads this
//!   back, so a field that is stale, missing or wrong costs a page and nothing
//!   else. That is what makes it safe to report from anywhere.
//! * **A lock is never held across a wait.** Every write is a few field
//!   assignments, so a plain `std::sync::Mutex` is the right one and an `async`
//!   one would only add a queue. Nothing here ever awaits while holding it.
//!
//! The shape of the snapshot is the page's contract, and it is written down once
//! — in `Snapshot::json` — rather than in three places that have to agree.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::device::Device;
use crate::carlife::proto::CarGps;

/// How many strings a head unit may name the device with. The protocol names
/// six; they are kept for the page and do not rename the device.
const ASKED: usize = 6;

/// The bridge's own state, as the page reads it.
///
/// Held behind one lock rather than one per field: a page wants a coherent
/// picture, and the cost of the coarser lock is a handful of assignments that
/// nobody is waiting on.
pub struct Status {
    inner: Mutex<Snapshot>,
}

impl Status {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Snapshot::default()),
        }
    }

    /// Reports something. The closure is handed the whole snapshot so a report
    /// that touches two fields at once stays one write — a page that caught the
    /// two halves apart would show a state that never existed.
    pub fn with(&self, report: impl FnOnce(&mut Snapshot)) {
        // A poisoned lock means a report panicked half way through. The data
        // behind it is still the best account anyone has, so it is taken rather
        // than thrown away: the page is not worth failing a bridge over.
        let mut snapshot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        report(&mut snapshot);
    }

    /// The whole thing, as the page is served it.
    pub fn json(&self) -> Value {
        let snapshot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        snapshot.json()
    }

    /// The cover the car was last given, for the page to draw.
    ///
    /// Handed over whole, with the name it is served under, because the two
    /// belong together: the page asks for a cover by that name and the answer
    /// has to be the same picture. The bytes are an `Arc`, so the caller copies
    /// them outside the lock — it is the one thing here that is not a few
    /// assignments.
    pub fn cover(&self) -> Option<Cover> {
        let snapshot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        snapshot.now_playing.cover.clone()
    }

    /// Reports the cover that has just gone to the car.
    ///
    /// An empty one is not a picture but the phone saying this song has none,
    /// and it puts the last one down: the page shows nothing rather than the
    /// cover of a song that has finished.
    pub fn cover_changed(&self, bytes: Vec<u8>) {
        let cover = (!bytes.is_empty()).then(|| Cover {
            id: cover_id(&bytes),
            bytes: Arc::new(bytes),
        });
        self.with(|s| s.now_playing.cover = cover);
    }

    /// Reports that there is no song any more, which is what a new session
    /// starts with.
    pub fn now_playing_cleared(&self) {
        self.with(|s| s.now_playing = NowPlaying::default());
    }

    /// Reports that the phone's connection to the airplay receiver is gone.
    ///
    /// Everything about the connection goes with it. The fields are cleared
    /// rather than left standing because a page is read to find out what is
    /// happening now: the last client's address, its ports and its stream
    /// counters describe a connection that has ended, and leaving them there
    /// would be the page's most convincing lie.
    ///
    /// The two halves of the CarPlay state that outlive a connection — the
    /// phone's serial and how far iAP2 got — are not touched: they belong to
    /// the cable, not to this socket.
    pub fn airplay_closed(&self) {
        self.with(|s| {
            s.carplay.airplay = Airplay::default();
            s.carplay.screen = None;
            s.carplay.video = Video::default();
            s.carplay.audio.clear();
            s.carplay.mic = None;
            s.video = VideoFormat::default();
        });
    }
}

impl Default for Status {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// The car's half: a head unit talking CarLife to the device we present.
    pub carlife: Carlife,
    /// The phone's half: an iPhone talking iAP2 and AirPlay to us.
    pub carplay: Carplay,
    /// The one video stream, and the coding it is in.
    ///
    /// One stream and not two: the phone encodes it and the CarLife video
    /// channel forwards it unchanged, so the profile and the level describe
    /// what the car decodes just as much as what the phone sends. Kept here
    /// rather than in either half for that reason, and written from the codec
    /// description the phone sends — see `airplay::screen`.
    pub video: VideoFormat,
    /// What the two halves agree on: the song the phone is playing, which is
    /// the one thing here that belongs to neither end. It is what the car's
    /// panel is showing, so it is also the only part of this a person sitting
    /// in the car can see for themselves.
    pub now_playing: NowPlaying,
    /// The machine itself, which is reported by the one module that reads
    /// `/proc` rather than by a part of the bridge — see `web::device`.
    pub device: Device,
}

/// The coding of the video stream, as far as the stream says so itself.
///
/// Empty until the phone has described it: the coding's name is a property of
/// this receiver, but which profile and level a phone picked is its own
/// decision and is only known once it has said.
#[derive(Debug, Clone, Default)]
pub struct VideoFormat {
    /// `Baseline`, `Constrained Baseline`, `Main`, `High`, …
    pub profile: Option<String>,
    /// `3.1`, `4`, `1b`.
    pub level: Option<String>,
}

/// The song on the car's panel.
#[derive(Debug, Clone, Default)]
pub struct NowPlaying {
    /// The app it is playing from, which the car shows as the song's source.
    pub source: Option<String>,
    pub song: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Seconds, as `Track` has it. The page writes it as minutes and seconds.
    pub duration_s: Option<i32>,
    /// The position that last went to the car's bar, in milliseconds — see
    /// `MSG_CMD_MEDIA_PROGRESS_BAR`. `None` until one has gone out, and kept
    /// after the bar stops moving: the car is still drawing that one.
    pub progress_ms: Option<u32>,
    /// What the car was last told the song is doing. `None` until the phone has
    /// said, because the car hears about a change of state and nothing else.
    pub playing: Option<bool>,
    /// The cover exactly as it went to the car, which is the only picture the
    /// panel can be showing. Kept when a song changes without one, because
    /// that is what the car does with it too.
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

/// A name for a picture, from the picture.
///
/// A hash rather than a counter: the page keeps what it has fetched and asks
/// again only when the name changes, so the same cover under two songs should
/// cost one request — and a name that came round again would show a picture
/// belonging to a song that has finished. FNV-1a over the bytes, which is small
/// enough to carry in this file and good enough to name a JPEG.
fn cover_id(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ---------------------------------------------------------------- carlife ---

/// What the CarLife side knows, which is everything the car told us.
#[derive(Debug, Clone, Default)]
pub struct Carlife {
    pub usb: Usb,
    /// Whether the head unit has asked us to come back as an accessory.
    pub accessory: bool,
    /// The AOA version answered when the head unit asked. Not a setting: the
    /// reply is a constant, and this is that constant reported.
    pub aoa_version: Option<u16>,
    /// The six strings the head unit sent to name the device with, by index:
    /// manufacturer, model, description, version, URI, serial number. The car's
    /// own CarLife application, in its own words — which is why it is worth
    /// showing and not just acting on.
    pub asked: [Option<String>; ASKED],
    /// The CarLife version the head unit opened with, major and minor.
    pub protocol: Option<(i32, i32)>,
    /// Whether the head unit has been given our device information, which is
    /// the point at which the session is up.
    pub session_up: bool,
    /// What the head unit reports about itself.
    pub statistic: Option<Statistic>,
    /// Whether the head unit's channel number is one a phone serves. Our
    /// verdict, not the head unit's.
    pub channel_legal: Option<bool>,
    /// The challenge the head unit set, as it arrived.
    pub challenge: Option<String>,
    /// Whether that challenge was answered. `false` is a session with no key to
    /// answer it with, which is a missing file rather than a protocol problem.
    pub challenge_answered: Option<bool>,
    /// The head unit's own verdict on us, from `HU_AUTHEN_RESULT`.
    pub accepted: Option<bool>,
    /// What the head unit asked to be rendered at, and what we agreed to. The
    /// two differ when the car names a rate outside the range we serve, and when
    /// the channel table preconfigures a size for its number: `agreed` is the
    /// size in force either way, because a preset is stored where the car's
    /// numbers would be.
    pub requested: Option<Size>,
    pub agreed: Option<Size>,
    /// What the agreed size works out to on the car's panel, in millimetres.
    /// Derived from the density the channel table gives this car, because no
    /// CarLife message carries a physical size: the density belongs to the screen
    /// this device is wired to, so the size is reported on the car's side of the
    /// page rather than the phone's.
    pub physical: Option<(u32, u32)>,
    /// Whether the head unit has the encoder running.
    pub encoding: bool,
    /// The last frame rate the two ends agreed to, which is not always the rate
    /// asked for: it moves on its own after the session is up.
    pub frame_rate: Option<i32>,
    /// The switches the head unit set, in the order it set them.
    pub features: Vec<(String, i32)>,
    /// The rate, channel count and sample width the media channel was announced
    /// in. The width is always 16 — see `carlife::proto::media_init`.
    pub media: Option<(u32, u16)>,
    /// How many bytes the head unit's last error message was. The protocol's
    /// error codes are not in any documentation we have, so the size is the only
    /// thing about it that can honestly be reported.
    pub error_bytes: Option<usize>,
    /// The last position the head unit reported, in the units of
    /// `MSG_CMD_CAR_GPS` — the page turns them into degrees and metres. `None`
    /// until the first one arrives, and again once the car has gone: it is the
    /// one report here that describes a place rather than a connection.
    pub gps: Option<CarGps>,
    /// The last speed the head unit reported, in kilometres an hour — see
    /// `MSG_CMD_CAR_VELOCITY`. `None` until the first one arrives, and again once
    /// the car has gone: like the position, it is something only the car can say
    /// and nothing here can keep saying for it.
    pub speed: Option<i32>,
}

/// The USB device this program presents, as a host reads it.
#[derive(Debug, Clone, Default)]
pub struct Usb {
    /// `None` until the gadget is on the controller; then whether a host has
    /// enumerated it. A device that is up and not enumerated is a cable, a car
    /// or a port, and the difference matters when nothing else is happening.
    pub enumerated: Option<bool>,
    /// The controller the gadget is bound to, as the kernel names it.
    pub controller: Option<String>,
    /// The numbers and strings in the device descriptor. These change when the
    /// head unit switches the device, so they are read back rather than assumed.
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

// ---------------------------------------------------------------- carplay ---

/// What the CarPlay side knows, which is everything the phone told us.
#[derive(Debug, Clone, Default)]
pub struct Carplay {
    /// Wireless rather than wired. A setting, not an observation, but it decides
    /// which of the fields below mean anything.
    pub wireless: bool,
    /// The phone's own serial, once one has been found on the cable. Over
    /// Bluetooth there is no equivalent: the peer is an address and nothing more.
    pub phone: Option<String>,
    /// What the accessory is called, which is the machine's hostname.
    pub name: String,
    /// This machine's own Bluetooth address.
    pub bt_mac: String,
    /// How far the iAP2 conversation got. The three are cumulative, and a
    /// failure leaves whichever ones had already happened standing.
    pub identified: bool,
    pub authenticated: bool,
    pub subscribed: bool,
    /// Why the last iAP2 session ended, when it ended badly.
    pub failed: Option<String>,
    /// The address the phone was told to come back to, which is ours on the
    /// interface the media arrives over.
    pub advertised: Option<String>,
    /// The port the airplay receiver listens on. Fixed for the life of a run.
    pub airplay_port: Option<u32>,
    /// The interface CarPlay's own media arrives on.
    pub av_iface: Option<String>,
    /// The access point the phone was told to join, when it is a wireless one.
    /// The passphrase is deliberately not here: it is a secret, and a page is
    /// not a place to keep one.
    pub access_point: Option<AccessPoint>,
    /// The connection the phone has open to the airplay receiver.
    pub airplay: Airplay,
    /// What the phone is being asked to render.
    pub screen: Option<Screen>,
    /// The screen stream.
    pub video: Video,
    /// Every audio stream the phone has open, keyed by nothing: the type and the
    /// name are both in the entry.
    pub audio: Vec<Audio>,
    /// The one stream that runs the other way.
    pub mic: Option<Mic>,
}

/// The network the phone is told to join. Everything here is in the access
/// point's own configuration, so it is what the interface is on air with rather
/// than a second opinion about it.
#[derive(Debug, Clone, Default)]
pub struct AccessPoint {
    pub ssid: String,
    pub channel: u8,
    pub iface: String,
}

/// The airplay session: how the phone connected and how far the handshake got.
#[derive(Debug, Clone, Default)]
pub struct Airplay {
    /// The client the phone connected from, address and port.
    pub peer: Option<String>,
    /// Whether pair-verify completed. Until it has, nothing is sealed and no
    /// stream can be set up.
    pub verified: bool,
    /// Whether the control channel is encrypted, which follows pair-verify but
    /// is a separate step.
    pub encrypted: bool,
    /// The port the phone posts its input requests to.
    pub event_port: Option<u16>,
    /// The port the phone answers our clock requests on.
    pub timing_port: Option<u16>,
    /// How far our clock is from the phone's, in microseconds, and the round
    /// trip it was measured over. The first says whether the two agree; the
    /// second says how much to trust that.
    pub clock_offset_us: Option<i64>,
    pub clock_rtt_us: Option<i64>,
}

/// What the phone has been asked to render, and what that works out to.
#[derive(Debug, Clone, Default)]
pub struct Screen {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The density the physical size was derived from: the channel table's `ppi`
    /// for this car, or its 120 dpi default.
    pub ppi: u32,
    /// The canvas the phone actually draws on, which is larger than the panel
    /// when a UI scale is set. Equal to the panel at the default scale.
    pub canvas_width: u32,
    pub canvas_height: u32,
    /// The physical size the phone is told, in millimetres. Derived rather than
    /// measured: no CarLife message carries one.
    pub width_mm: u32,
    pub height_mm: u32,
}

/// The screen stream, which arrives over TCP as length-framed messages.
#[derive(Debug, Clone, Default)]
pub struct Video {
    /// Whether the stream is open right now.
    pub active: bool,
    /// Messages read off the socket, and how many of them were pictures. They
    /// differ: the codec description and the keepalives are messages too.
    pub messages: u64,
    pub frames: u64,
}

/// One audio stream, as the phone set it up and as it turned out.
#[derive(Debug, Clone, Default)]
pub struct Audio {
    /// The stream type the phone numbers it with: 100, 101 or 102.
    pub stream_type: u8,
    /// The phone's own name for it: `media`, `telephony`, `speechrecognition`.
    pub name: String,
    /// The coding the phone named for it, which is not always the one offered.
    pub codec: String,
    pub rate: u32,
    pub channels: u16,
    /// How many frames of the codec the phone packs into each packet.
    pub frames_per_packet: u32,
    /// The phone's jitter buffer, in milliseconds.
    pub latency_ms: u64,
    /// What arrived. `missing` counts sequence numbers that never turned up;
    /// the other two are packets that did but out of turn.
    pub packets: u64,
    pub missing: u64,
    pub out_of_order: u64,
    pub duplicates: u64,
    /// The longest the phone went quiet, in milliseconds.
    pub worst_gap_ms: u64,
}

/// The microphone uplink, opened for whichever stream the phone said it wants
/// to hear on.
#[derive(Debug, Clone, Default)]
pub struct Mic {
    pub peer: Option<String>,
    pub codec: String,
    pub rate: u32,
    pub channels: u16,
}

// -------------------------------------------------------------------- json ---

impl Snapshot {
    fn json(&self) -> Value {
        json!({
            "carlife": self.carlife.json(&self.video),
            "carplay": self.carplay.json(&self.video),
            "now_playing": self.now_playing.json(),
            "device": self.device.json(),
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

impl Carlife {
    fn json(&self, video: &VideoFormat) -> Value {
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
            // What the video channel carries is the phone's stream, untouched,
            // so the coding and its profile are the same ones the phone named
            // rather than anything the car asked for — it asks for a size and a
            // rate and nothing else.
            "video": {
                "codec": crate::airplay::screen::CODEC,
                "profile": video.profile,
                "level": video.level,
            },
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

impl Carplay {
    fn json(&self, video: &VideoFormat) -> Value {
        json!({
            "wireless": self.wireless,
            "phone": self.phone,
            "name": self.name,
            "bt_mac": self.bt_mac,
            "identified": self.identified,
            "authenticated": self.authenticated,
            "subscribed": self.subscribed,
            "failed": self.failed,
            "advertised": self.advertised,
            "airplay_port": self.airplay_port,
            "av_iface": self.av_iface,
            "access_point": self.access_point.as_ref().map(|ap| json!({
                "ssid": ap.ssid,
                "channel": ap.channel,
                "iface": ap.iface,
            })),
            "airplay": {
                "peer": self.airplay.peer,
                "verified": self.airplay.verified,
                "encrypted": self.airplay.encrypted,
                "event_port": self.airplay.event_port,
                "timing_port": self.airplay.timing_port,
                "clock_offset_us": self.airplay.clock_offset_us,
                "clock_rtt_us": self.airplay.clock_rtt_us,
            },
            "screen": self.screen.as_ref().map(|s| json!({
                "width": s.width,
                "height": s.height,
                "fps": s.fps,
                "ppi": s.ppi,
                "canvas_width": s.canvas_width,
                "canvas_height": s.canvas_height,
                "width_mm": s.width_mm,
                "height_mm": s.height_mm,
            })),
            "video": {
                "active": self.video.active,
                // A coding is a property of this receiver rather than of the
                // session it happens to be carrying, so it is named where the
                // config parser is and reported from there: the page reads it
                // like any other field instead of knowing one of its own. The
                // profile and the level are not like that — the phone picks
                // them, and they arrive with the codec description.
                "codec": crate::airplay::screen::CODEC,
                "profile": video.profile,
                "level": video.level,
                "messages": self.video.messages,
                "frames": self.video.frames,
            },
            "audio": self
                .audio
                .iter()
                .map(|a| json!({
                    "stream_type": a.stream_type,
                    "name": a.name,
                    "codec": a.codec,
                    "rate": a.rate,
                    "channels": a.channels,
                    "frames_per_packet": a.frames_per_packet,
                    "latency_ms": a.latency_ms,
                    "packets": a.packets,
                    "missing": a.missing,
                    "out_of_order": a.out_of_order,
                    "duplicates": a.duplicates,
                    "worst_gap_ms": a.worst_gap_ms,
                }))
                .collect::<Vec<_>>(),
            "mic": self.mic.as_ref().map(|m| json!({
                "peer": m.peer,
                "codec": m.codec,
                "rate": m.rate,
                "channels": m.channels,
            })),
        })
    }
}
