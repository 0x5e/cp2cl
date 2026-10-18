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

use std::sync::Mutex;

use serde_json::{json, Value};

/// How many strings a head unit may name the device with. The protocol names
/// six; see `carlife::desc::Asked`.
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
    /// two differ when the car names a rate outside the range we serve.
    pub requested: Option<Size>,
    pub agreed: Option<Size>,
    /// What `agreed` works out to on the car's panel, in millimetres. Derived
    /// from `CP2CL_PPI`, because no CarLife message carries a physical size: the
    /// density is a property of the screen this device is wired to, so the size
    /// is reported on the car's side of the page rather than the phone's.
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
    /// The density the physical size was derived from. See `CP2CL_PPI`.
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
            "carlife": self.carlife.json(),
            "carplay": self.carplay.json(),
        })
    }
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
            "error_bytes": self.error_bytes,
        })
    }
}

impl Carplay {
    fn json(&self) -> Value {
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
                // like any other field instead of knowing one of its own.
                "codec": crate::airplay::screen::CODEC,
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
