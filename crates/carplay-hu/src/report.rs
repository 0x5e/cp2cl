//! What the CarPlay half knows, kept rather than printed, for the page to read.
//!
//! Nothing here observes anything for itself. Every part of this half already
//! says what it learned in its log lines, and this is that same knowledge kept:
//! each module that learns something writes it down here, and whoever draws the
//! page reads the lot.
//!
//! Two rules keep it from becoming a second source of truth:
//!
//! * **A write is a report, never a decision.** Nothing here is read back, so a
//!   field that is stale, missing or wrong costs a page and nothing else.
//! * **A lock is never held across a wait.** Every write is a few field
//!   assignments, so a plain `std::sync::Mutex` is the right one and an `async`
//!   one would only add a queue. Nothing here ever awaits while holding it.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::{json, Value};

/// The half's own state, as the page reads it.
pub struct Report {
    inner: Mutex<Snapshot>,
}

impl Report {
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
        // than thrown away: the page is not worth failing a session over.
        let mut snapshot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        report(&mut snapshot);
    }

    /// The whole thing, as the page is served it.
    pub fn json(&self) -> Value {
        let snapshot = {
            let held = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            held.clone()
        };
        snapshot.carplay.json(&snapshot.video)
    }

    /// The phone the current session belongs to, as the link that found it
    /// named it: a serial on the cable, an address over Bluetooth.
    ///
    /// Read by the tunnel, which has no identity of its own to print: it is the
    /// same phone, and the link that carried the first half of the session is
    /// the one that knows who it is.
    pub fn phone(&self) -> Option<String> {
        let snapshot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        snapshot.carplay.phone.clone()
    }

    /// Whether a phone currently has the receiver open. Set when one connects
    /// and cleared when the session that connection belongs to ends.
    pub fn airplay_up(&self) -> bool {
        let snapshot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        snapshot.carplay.airplay.peer.is_some()
    }

    /// Everything on the page that belongs to the phone on the link. An iAP2
    /// session that ended is not a phone that is still talking to us.
    pub fn phone_gone(&self) {
        self.with(|s| {
            s.carplay.phone = None;
            s.carplay.identified = false;
            s.carplay.authenticated = false;
            s.carplay.subscribed = false;
            s.carplay.advertised = None;
            s.carplay.av_iface = None;
        });
    }

    /// The coding of the video stream: the two fields the other half needs and
    /// nothing else.
    ///
    /// The car forwards the pictures unchanged, so the coding is the only part
    /// of the stream it is told about — its own report carries the same three
    /// fields beside the counters it has and the car does not.
    pub fn coding(&self) -> Value {
        let snapshot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        json!({
            "codec": crate::airplay::screen::CODEC,
            "profile": snapshot.video.profile,
            "level": snapshot.video.level,
        })
    }

    /// Reports that the phone's connection to the airplay receiver is gone.
    ///
    /// Everything about the connection goes with it. The fields are cleared
    /// rather than left standing because a page is read to find out what is
    /// happening now: the last client's address, its ports and its stream
    /// counters describe a connection that has ended, and leaving them there
    /// would be the page's most convincing lie.
    ///
    /// The two halves of the state that outlive a connection — the phone's
    /// serial and how far iAP2 got — are not touched: they belong to the cable,
    /// not to this socket.
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

impl Default for Report {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// The phone's half: an iPhone talking iAP2 and AirPlay to us.
    pub carplay: Carplay,
    /// The one video stream, and the coding it is in.
    ///
    /// One stream and not two: the phone encodes it and the car's video channel
    /// forwards it unchanged, so the profile and the level describe what the car
    /// decodes just as much as what the phone sends. Written from the codec
    /// description the phone sends — see `airplay::screen`.
    pub video: VideoFormat,
}

/// The coding of the video stream, as far as the stream says so itself.
///
/// Empty until the phone has described it: the coding's name is a property of
/// this receiver, but which profile and level a phone picked is its own decision
/// and is only known once it has said.
#[derive(Debug, Clone, Default)]
pub struct VideoFormat {
    /// `Baseline`, `Constrained Baseline`, `Main`, `High`, …
    pub profile: Option<String>,
    /// `3.1`, `4`, `1b`.
    pub level: Option<String>,
}

/// What the CarPlay side knows, which is everything the phone told us.
#[derive(Debug, Clone, Default)]
pub struct Carplay {
    /// The link the running session arrived on: `usb` or `wireless`. `None`
    /// while both links are waiting for a phone.
    pub transport: Option<&'static str>,
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
    pub screen: Option<Rendering>,
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
    /// How far our clock is from the phone's, in microseconds. It says whether
    /// the two agree, which is what audio timing rests on.
    pub clock_offset_us: Option<i64>,
    /// The round trips it was measured over, which say how much to trust that.
    pub rtt: Rtt,
}

/// How many round trips the page's three numbers are taken over. The phone
/// answers once a second, so this is five minutes of the link's behaviour: long
/// enough for a stalling access point to show up in it, short enough that a
/// link which has recovered stops being described by what it used to be.
const RTT_WINDOW: usize = 300;

/// The recent round trips, in microseconds.
///
/// A window rather than three running figures, because a minimum is the one
/// number that cannot be maintained over a stream: once the worst trip falls
/// out, nothing else says what the next worst was.
#[derive(Debug, Clone, Default)]
pub struct Rtt {
    samples: VecDeque<i64>,
}

impl Rtt {
    pub fn push(&mut self, us: i64) {
        if self.samples.len() == RTT_WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(us);
    }

    /// The three numbers the page shows, or nothing before the phone has
    /// answered once — three zeroes would say the trip is instant.
    fn json(&self) -> Option<Value> {
        let min = *self.samples.iter().min()?;
        let max = *self.samples.iter().max()?;
        let sum: i64 = self.samples.iter().sum();
        Some(json!({
            "min": min,
            "avg": sum / self.samples.len() as i64,
            "max": max,
        }))
    }
}

/// What the phone has been asked to render, and what that works out to.
///
/// The page's record rather than the request: [`Geometry`] is what a car named,
/// and this is that, the canvas it became and the size the phone was told.
#[derive(Debug, Clone, Default)]
pub struct Rendering {
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

impl Carplay {
    fn json(&self, video: &VideoFormat) -> Value {
        json!({
            "transport": self.transport,
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
                "clock_rtt_us": self.airplay.rtt.json(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The page asks three questions of the window rather than of a running
    /// total: the best the link gets, the worst, and where it usually sits.
    /// What has fallen out of the window has to stop answering, or the minimum
    /// would go on describing a link that no longer exists.
    #[test]
    fn the_round_trip_window_forgets_what_has_fallen_out_of_it() {
        let mut rtt = Rtt::default();
        assert_eq!(
            rtt.json(),
            None,
            "three zeroes would say the trip is instant"
        );

        // A window that has not filled is averaged over what it holds.
        for us in [3_000, 1_000, 2_000] {
            rtt.push(us);
        }
        assert_eq!(
            rtt.json(),
            Some(json!({ "min": 1_000, "avg": 2_000, "max": 3_000 }))
        );

        // Filled the rest of the way with a steady 2 ms, the one 3 ms trip is
        // still the maximum…
        for _ in 3..RTT_WINDOW {
            rtt.push(2_000);
        }
        assert_eq!(rtt.json().expect("a full window")["max"], json!(3_000));

        // …and once the window turns past it, it is not: the oldest sample is
        // the one that leaves, which is the whole reason this is a window.
        rtt.push(2_000);
        assert_eq!(rtt.json().expect("a turned window")["max"], json!(2_000));

        // The 1 ms trip was taken second, so it outlives the 3 ms one by a turn
        // — samples leave in the order they arrived, not by how they look.
        rtt.push(2_000);
        let after = rtt.json().expect("still a window");
        assert_eq!(after["min"], json!(2_000));
        assert_eq!(after["avg"], json!(2_000));
        assert_eq!(after["max"], json!(2_000));
    }
}
