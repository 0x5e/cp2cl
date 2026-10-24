//! One CarLife session: the conversation the car starts, over whichever pipe the
//! USB side handed us.
//!
//! The car speaks first and every step is its idea, so this is a state machine
//! that reacts rather than one that drives. It is written against
//! `AsyncRead + AsyncWrite` rather than against the gadget so it can be run —
//! and tested — over an ordinary in-memory pipe; `gadget` supplies the other
//! end of that pipe on the board.
//!
//! The order of the messages, the ids and the field numbers all come from the
//! car's own library and the official guide; see the plan for the table and the
//! citations. One thing is deliberately incomplete:
//!
//! * **Touch and panel keys go one way.** They arrive on the sixth channel and
//!   are turned into the commands the CarPlay side already knows how to send;
//!   nothing comes back the other way.
//!
//! The car's authentication challenge is answered, and the car is judged in turn,
//! by `auth`; what that needs and what happens without it is documented there.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use livi_runtime::vehicle::Vehicle;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

use super::{auth, frame, gps, proto};
use crate::airplay::{info, EventSink, ScreenParams, SharedChannel};
use crate::commands::{Command, TouchContact, PHONE_DROP, PHONE_HOOK_SWITCH, PHONE_MUTE};
use crate::config::Config;
use crate::navigation::{turn_kind, Guidance};
use crate::playing::{Playing, Track};
use crate::sink::{ClOutlet, Media, Navi};
use crate::web::{Size, Statistic, Status};

// What the car sends, by name — the numbers are CarLifeVehicleLib's
// `EPackageHeadType`.
const HU_PROTOCOL_VERSION: u32 = 0x0001_8001;
const HU_INFO: u32 = 0x0001_8003;
const HU_VIDEO_ENCODER_INIT: u32 = 0x0001_8007;
const HU_VIDEO_ENCODER_START: u32 = 0x0001_8009;
const HU_VIDEO_ENCODER_PAUSE: u32 = 0x0001_800A;
const HU_VIDEO_ENCODER_RESET: u32 = 0x0001_800B;
const HU_VIDEO_ENCODER_FRAME_RATE_CHANGE: u32 = 0x0001_800C;
const HU_CAR_GPS: u32 = 0x0001_8010;
const HU_GO_TO_FOREGROUND: u32 = 0x0001_8025;
const HU_STATISTIC_INFO: u32 = 0x0001_8027;
const HU_CAR_DATA_SUBSCRIBE_DONE: u32 = 0x0001_8032;
const HU_DATA_SUBSCRIBE: u32 = 0x0001_8043;
const HU_DATA_SUBSCRIBE_START: u32 = 0x0001_8045;
const HU_DATA_SUBSCRIBE_STOP: u32 = 0x0001_8046;
const HU_AUTHEN_REQUEST: u32 = 0x0001_8048;
const HU_AUTHEN_RESULT: u32 = 0x0001_804A;
const HU_FEATURE_CONFIG_RESPONSE: u32 = 0x0001_8052;
const HU_ERROR_CODE: u32 = 0x0001_8055;

// What we send.
const GO_TO_DESKTOP: u32 = 0x0001_0021;
const MATCH_STATUS: u32 = 0x0001_0002;
const MD_INFO: u32 = 0x0001_0004;
const VIDEO_ENCODER_INIT_DONE: u32 = 0x0001_0008;
const VIDEO_ENCODER_FRAME_RATE_CHANGE_DONE: u32 = 0x0001_000D;
const MODULE_STATUS: u32 = 0x0001_0026;
const NAVI_NEXT_TURN_INFO: u32 = 0x0001_0030;
const CAR_DATA_SUBSCRIBE: u32 = 0x0001_0031;
const CAR_DATA_SUBSCRIBE_START: u32 = 0x0001_0033;
const MEDIA_INFO: u32 = 0x0001_0035;
const DATA_SUBSCRIBE_DONE: u32 = 0x0001_0044;
const MD_AUTHEN_RESPONSE: u32 = 0x0001_0049;
const MD_AUTHEN_RESULT: u32 = 0x0001_004B;
const GO_TO_FOREGROUND_RESPONSE: u32 = 0x0001_004C;
const FEATURE_CONFIG_REQUEST: u32 = 0x0001_0051;
const VIDEO_DATA: u32 = 0x0002_0001;
const VIDEO_HEARTBEAT: u32 = 0x0002_0002;
const MEDIA_INIT: u32 = 0x0003_0001;
const MEDIA_STOP: u32 = 0x0003_0002;
const MEDIA_DATA: u32 = 0x0003_0006;
// The navigation prompt has a channel of its own, with the same three messages
// in the same order. See the guide's §10.4 and CarLifeVehicleLib's
// `CommonParams.java`.
const NAVI_TTS_INIT: u32 = 0x0004_0001;
const NAVI_TTS_END: u32 = 0x0004_0002;
const NAVI_TTS_DATA: u32 = 0x0004_0003;

const TOUCH_ACTION: u32 = 0x0006_8001;
const TOUCH_HARD_KEY: u32 = 0x0006_8008;

/// What a touch action means: the values of Android's `MotionEvent`, which is
/// what the car speaks. Only the release is told apart from the rest — a press
/// and a drag are the same thing to the phone.
const TOUCH_UP: i32 = 1;

/// The car's panel keys, `KEYCODE_*` in CarLifeVehicleLib. The last three are not
/// in that list: they are the reference head unit's own, for the three controls
/// the protocol has no key of its own for.
const KEY_HOME: i32 = 0x0000_0001;
const KEY_PHONE_CALL: i32 = 0x0000_0002;
const KEY_PHONE_END: i32 = 0x0000_0003;
const KEY_SELECTOR_NEXT: i32 = 0x0000_0006;
const KEY_SELECTOR_PREVIOUS: i32 = 0x0000_0007;
const KEY_BACK: i32 = 0x0000_000E;
const KEY_SEEK_SUB: i32 = 0x0000_000F;
const KEY_SEEK_ADD: i32 = 0x0000_0010;
const KEY_MUTE: i32 = 0x0000_0013;
const KEY_OK: i32 = 0x0000_0014;
const KEY_MOVE_LEFT: i32 = 0x0000_0015;
const KEY_MOVE_RIGHT: i32 = 0x0000_0016;
const KEY_MOVE_UP: i32 = 0x0000_0017;
const KEY_MOVE_DOWN: i32 = 0x0000_0018;
const KEY_MAIN: i32 = 0x0000_001E;
const KEY_MEDIA_START: i32 = 0x0000_001F;
const KEY_MEDIA_STOP: i32 = 0x0000_0020;
const KEY_VR_START: i32 = 0x0000_0021;

/// How often the video channel is touched when there is no picture to send. The
/// reference does the same whenever its own interface is not in the foreground.
const HEARTBEAT: Duration = Duration::from_secs(1);

/// How long a car is given to say the first thing before the pipe is probed.
///
/// A head unit that speaks at all speaks within milliseconds of the pipe
/// opening — it is the car that opens the conversation — so this is only ever
/// reached by one that is not going to.
const PROBE_QUIET: Duration = Duration::from_secs(2);

/// How long the probe's write is waited on. A host that is reading the endpoint
/// takes it in one transfer; one that is not never does.
const PROBE_WAIT: Duration = Duration::from_secs(1);

/// How long silence on the navigation channel may last before the car is told
/// the prompt is over.
///
/// The phone does not always tear a prompt's stream down when it stops speaking,
/// and a head unit that took the channel up keeps the music down until it hears
/// `MSG_NAVI_TTS_END` — so silence is the only evidence there is that the end is
/// not coming. Long enough not to cut a sentence in two at the pause between two
/// of them, short enough that the music comes back before anyone wonders why it
/// stopped.
const TTS_IDLE: Duration = Duration::from_millis(700);

/// The frame rates the video channel agrees to.
///
/// The car names one and the phone is then asked to render at it, so the range is
/// not ours to narrow: there is nothing to scale and nothing to re-time, and a
/// car that asks for 60 and is answered with 30 gets half the picture rate its
/// own panel was built around. Sixty is the top of what a CarPlay encoder
/// produces and three is the bottom of what the reference counts as a rate.
const MIN_FPS: i32 = 3;
const MAX_FPS: i32 = 60;

/// Where the video channel's pictures come from. A file the operator names is
/// the whole source when there is one; otherwise the source is the CarPlay
/// session, and `None` is what a run with neither falls back to.
pub enum Frames {
    /// Nothing but heartbeats: the session is up and the car is kept waiting.
    None,
    /// The access units of a file, in the order the file holds them, at the
    /// agreed rate, started over at the end.
    ///
    /// A file with one access unit in it is the case the bench is run on when
    /// there is no phone: one picture, sent again and again, which is nothing a
    /// person would watch but is a real H.264 stream all the same, and proves the
    /// channel and the car's decoder end to end with no encoder in the picture. A
    /// file with a stream in it is what makes the picture move. `CP2CL_INPUT_FILE`
    /// names the file.
    Cycle(Vec<Vec<u8>>),
    /// The CarPlay side, as it arrives. Frames are written to the car as they
    /// come in rather than on the session's own clock: they are a coded stream,
    /// and it is the phone's encoder that decides the rate.
    Live(Arc<ClOutlet>),
}

impl Frames {
    /// The access units of `path`, in file order.
    ///
    /// A file that cannot be read is worth a warning and not a failure: the
    /// session is still useful without it, and saying so beats refusing to start.
    pub fn from_file(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) if !bytes.is_empty() => {
                let units = access_units(&bytes);
                if units.is_empty() {
                    tracing::warn!(
                        "cl.video_source_empty file={path:?} action=heartbeat_only"
                    );
                    return Self::None;
                }
                tracing::info!(
                    "cl.video_source file={path:?} bytes={} access_units={}",
                    bytes.len(),
                    units.len()
                );
                Self::Cycle(units)
            }
            Ok(_) => {
                tracing::warn!("cl.video_source_empty file={path:?} action=heartbeat_only");
                Self::None
            }
            Err(e) => {
                tracing::warn!(
                    "cl.video_source_unreadable file={path:?} err={e} action=heartbeat_only"
                );
                Self::None
            }
        }
    }
}

/// Splits an annex-B stream into access units, one per coded picture.
///
/// What a decoder is handed is a picture together with the parameter sets that
/// describe it, so the boundaries are where a slice arrives after another slice
/// and where a parameter set or an access unit delimiter arrives after a
/// picture. Splitting on the slices alone would leave each picture's parameter
/// sets at the end of the picture before it.
///
/// Anything ahead of the first start code is dropped, and a stream with no start
/// code in it has no access units.
fn access_units(bytes: &[u8]) -> Vec<Vec<u8>> {
    // Where each NAL begins and what kind it is; the type is the low five bits
    // of the byte after the start code, which is the whole of a NAL header.
    let mut nals: Vec<(usize, u8)> = Vec::new();
    let mut at = 0;
    while at + 3 <= bytes.len() {
        let header = if bytes[at..at + 3] == [0, 0, 1] {
            at + 3
        } else if at + 4 <= bytes.len() && bytes[at..at + 4] == [0, 0, 0, 1] {
            at + 4
        } else {
            at += 1;
            continue;
        };
        if header >= bytes.len() {
            break;
        }
        nals.push((at, bytes[header] & 0x1f));
        at = header;
    }

    // A stream with no start code in it has no access units to give.
    if nals.is_empty() {
        return Vec::new();
    }

    // Where the units begin, starting at the first NAL. A unit opens at every
    // slice that follows one, and at every parameter set or delimiter that
    // follows a picture.
    let mut starts = vec![0usize];
    let mut in_picture = false;
    for (index, &(_, kind)) in nals.iter().enumerate() {
        if let 1..=5 = kind {
            if in_picture {
                starts.push(index);
            }
            in_picture = true;
        } else if in_picture && matches!(kind, 7 | 8 | 9) {
            starts.push(index);
            in_picture = false;
        }
    }

    let mut bounds: Vec<usize> = starts.iter().map(|&index| nals[index].0).collect();
    bounds.push(bytes.len());
    bounds
        .windows(2)
        .map(|unit| bytes[unit[0]..unit[1]].to_vec())
        .collect()
}

/// Where the size a head unit asks to be rendered at is reported, once it has
/// named one: width, height and frame rate. It has to be `Send` because a session
/// runs in a task of its own.
pub type ScreenReport<'a> = &'a mut (dyn FnMut(u32, u32, u32) + Send);

/// Runs one session until the car goes away.
///
/// `carplay` is told `true` the first time the car has been given our device
/// information — from that point the session is up and the CarPlay side has
/// somewhere to hand its streams — and `false` once, when the session ends for
/// any reason. On the board that switch is what starts and stops the whole
/// CarPlay half; see `main`.
///
/// `auth` is the key the car's challenge is answered with. It comes from the
/// caller rather than from a file: the key is built into the binary, and a
/// session handed `None` skips the challenge and runs on rather than failing.
///
/// `screen` is told the size the car asked to be rendered at, as width, height
/// and frame rate. The car is the one that knows its own screen, so this is where
/// what the phone is asked to render comes from; `main` is what does something
/// with it.
///
/// `events` is where the car's touch and panel keys go. It is the CarPlay side's
/// own sink, so a key pressed on the car arrives at the phone by exactly the road
/// a key from anywhere else would; the phone cannot tell them apart.
///
/// `channel` is where the channel number the car reports is kept, for `/info` to
/// read: a head unit that names itself gets the icon and label that go with the
/// make it belongs to.
///
/// `desktop` fires when the phone's icon for us is tapped. On the phone that
/// icon stands for the car's own interface, so the car is asked to show it.
pub async fn run<S, F>(
    stream: S,
    frames: &Frames,
    cfg: &Config,
    auth: Option<auth::Signer>,
    screen: ScreenReport<'_>,
    channel: SharedChannel,
    events: Arc<EventSink>,
    carplay: &mut F,
    desktop: &broadcast::Sender<()>,
    status: Arc<Status>,
    vehicle: Arc<Vehicle>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    F: FnMut(bool),
{
    // Taken first, so a tap that lands while the session is still being built is
    // not missed.
    let mut desktop = desktop.subscribe();

    // A session is what drains the bridge, so it takes all five receiving halves
    // for as long as it runs: one channel carries pictures, two the phone's
    // sound — the music and the navigation prompt — and the last two what the
    // phone says about itself, the song it is playing and the route it is
    // guiding along. The car unplugged and plugged back in is a new session and
    // a new call to this function, which is why the halves are given back on the
    // way out rather than dropped with everything else.
    // A panel that has just been plugged in has no song on it: what the last
    // session was showing belongs to a car that is no longer here, and the page
    // would otherwise go on showing it.
    status.now_playing_cleared();
    let (mut video, mut media, mut navi, mut playing, mut guidance, source) = match frames {
        Frames::Live(outlet) => match outlet.take_video() {
            Some(rx) => (
                Some(rx),
                outlet.take_media(),
                outlet.take_navi(),
                outlet.take_playing(),
                outlet.take_guidance(),
                Source::Live,
            ),
            None => {
                tracing::warn!("cl.video_source_taken action=heartbeat_only");
                (None, None, None, None, None, Source::None)
            }
        },
        Frames::Cycle(units) => (
            None,
            None,
            None,
            None,
            None,
            Source::Cycle {
                units: units.clone(),
                next: 0,
            },
        ),
        Frames::None => (None, None, None, None, None, Source::None),
    };

    // Reads are their own task. A frame can be six hundred kilobytes and the
    // video tick fires while one is arriving, so the read cannot live inside a
    // `select!` that would drop it half way and lose the bytes it had taken.
    // The channel in between is what makes the tick safe to wait on.
    let (mut reader, writer) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<ChunkOrEnd>(4);
    let reading = tokio::spawn(async move {
        loop {
            let arriving = match frame::read_chunk(&mut reader).await {
                Ok(Some(chunk)) => ChunkOrEnd::Chunk(chunk),
                Ok(None) => ChunkOrEnd::End,
                Err(e) => ChunkOrEnd::Failed(e),
            };
            let last = !matches!(arriving, ChunkOrEnd::Chunk(_));
            if tx.send(arriving).await.is_err() || last {
                return;
            }
        }
    });

    let mut session = Session {
        writer,
        cfg,
        screen,
        channel,
        events,
        source,
        channel_touched: None,
        screen_in_force: None,
        panel: info::Panel::default(),
        started_at: None,
        media_format: None,
        navi_format: None,
        navi_open: false,
        nav_deadline: None,
        track: Track::default(),
        art: None,
        // A route is described by the phone in messages of its own, so the
        // session starts knowing nothing about one and hears about it as it
        // goes. A car that plugs in mid-route is a car the phone describes the
        // whole route to again, because a new CarLife session is a new CarPlay
        // one.
        guidance: Guidance::default(),
        subscribed: false,
        navi: None,
        shown: None,
        // Built in beside the icon and the page, so the same key answers every
        // session — a car plugged out and back in finds what the first one did.
        auth,
        next_tick: Instant::now(),
        position_seen: false,
        last_fix: None,
        delivered: false,
        handed: None,
        // Nothing is assumed about the datum before the car has named one, and a
        // fix that arrives before it would not be subscribed to anyway.
        gcj02: false,
        vehicle,
        up: false,
        frames_seen: 0,
        last_touch: None,
        status,
    };

    // A car that has configured the pipe and then said nothing is either one
    // that never opened it or one that is waiting for this side to speak first,
    // and from here the two look the same: a read that waits for ever. One
    // frame tells them apart — see `probe` — and only where the car has been
    // quiet long enough that there is nothing else it could be doing.
    let probe_at = Instant::now() + PROBE_QUIET;
    let mut probed = false;

    let outcome = loop {
        let deadline = session.next_tick;
        let prompt_idle = session.nav_deadline;
        tokio::select! {
            arriving = rx.recv() => match arriving {
                Some(ChunkOrEnd::Chunk(chunk)) => {
                    if let Err(e) = session.handle(chunk, carplay).await {
                        break Err(e);
                    }
                }
                Some(ChunkOrEnd::End) => break Ok(()),
                Some(ChunkOrEnd::Failed(e)) => break Err(e.into()),
                // The reader task is gone, and it only goes away when the
                // channel is closed, which only happens here.
                None => break Ok(()),
            },
            // A picture from the CarPlay side, written straight through. It is
            // not put on the tick: the phone's encoder decides how often frames
            // come, and holding them back to a schedule of our own would only
            // add delay.
            access_unit = recv_video(&mut video) => match access_unit {
                Some(access_unit) => {
                    if let Err(e) = session.send_video(access_unit).await {
                        break Err(e);
                    }
                }
                // The CarPlay side has gone, taking its frames with it. The
                // heartbeats are what is left to keep the channel alive.
                None => video = None,
            },
            // The phone's sound, on the media channel. Written as it arrives,
            // like the pictures and for the same reason: the phone is what
            // decides when it plays.
            item = recv_media(&mut media) => match item {
                Some(item) => {
                    if let Err(e) = session.send_media(item).await {
                        break Err(e);
                    }
                }
                // The CarPlay side has gone; there is nothing left to play.
                None => media = None,
            },
            // The prompt, on the channel the car keeps for speech. Written as it
            // arrives for the same reason as the music, but the end of it is
            // also decided here: see `TTS_IDLE`.
            item = recv_navi(&mut navi) => match item {
                Some(item) => {
                    if let Err(e) = session.send_navi(item).await {
                        break Err(e);
                    }
                }
                // The CarPlay side has gone; there is nothing left to say.
                None => navi = None,
            },
            // The prompt has been quiet long enough that it is over. This is the
            // branch that puts the music back on a head unit that paused it.
            _ = prompt_idle_after(prompt_idle) => {
                if let Err(e) = session.close_navi().await {
                    break Err(e);
                }
            },
            // What the phone is playing, on the command channel. Written as it
            // arrives, like the three above: the phone is what decides when the
            // song changes, and what the car is told is the whole of what is
            // known, so a message says only what the phone has said.
            said = recv_playing(&mut playing) => match said {
                Some(item) => {
                    if let Err(e) = session.send_playing(item).await {
                        break Err(e);
                    }
                }
                // The CarPlay side has gone; the panel keeps what it was last
                // told, which is the last song there was.
                None => playing = None,
            },
            // Where the phone is going, on the command channel. Written as it
            // arrives, like the three above: the phone is what decides when the
            // route changes, and what the strip is told is the whole of what is
            // known, so a message is only sent when it says something new.
            moving = recv_guidance(&mut guidance) => match moving {
                Some(update) => {
                    if let Err(e) = session.send_guidance(update).await {
                        break Err(e);
                    }
                }
                // The CarPlay side has gone; the strip keeps what it was last
                // told, which is the last turn there was.
                None => guidance = None,
            },
            // A tap on the phone's icon for us. The phone shows that icon in
            // place of the car's own interface, so the car is told to show it.
            _ = desktop.recv() => {
                if let Err(e) = session.send(GO_TO_DESKTOP, &[]).await {
                    break Err(e);
                }
            },
            // Only a tick moves the schedule forward, so a busy stream of
            // messages cannot starve the video channel.
            _ = tokio::time::sleep_until(deadline) => {
                if let Err(e) = session.tick().await {
                    break Err(e);
                }
                session.next_tick = Instant::now() + session.period();
            }
            // Nothing from the car since the pipe opened, and the handshake
            // has not got as far as `up`. See `probe`.
            _ = tokio::time::sleep_until(probe_at), if !probed && !session.up => {
                probed = true;
                session.probe().await;
            }
        }
    };

    reading.abort();
    if let Frames::Live(outlet) = frames {
        if let Some(rx) = video {
            outlet.return_video(rx);
        }
        if let Some(rx) = media {
            outlet.return_media(rx);
        }
        if let Some(rx) = navi {
            outlet.return_navi(rx);
        }
        if let Some(rx) = playing {
            outlet.return_playing(rx);
        }
        if let Some(rx) = guidance {
            outlet.return_guidance(rx);
        }
    }
    // The car has gone, and its position went with it: a phone still connected
    // to us would otherwise keep using the last place the car was.
    session.take_back_from_phone();
    if session.up {
        carplay(false);
        tracing::info!("cl.session_over action=close_carplay");
    }
    // A session that has ended is not one that is running, and the page has to
    // say so: an encoder left "running" after the car has gone is the one thing
    // on it that would be actively wrong rather than merely stale.
    session.status.with(|s| {
        s.carlife.session_up = false;
        s.carlife.encoding = false;
        // A fix belongs to the car it came from. Left behind, it would describe a
        // position this device no longer has any way of updating.
        s.carlife.gps = None;
    });
    outcome
}

/// What the reader task hands over.
enum ChunkOrEnd {
    Chunk(frame::Chunk),
    End,
    Failed(std::io::Error),
}

/// Where the pictures come from, holding everything the session needs to know
/// about it: the receiving half is not here, because it belongs to the `select!`.
enum Source {
    None,
    /// The file's access units, and which of them goes out next.
    Cycle { units: Vec<Vec<u8>>, next: usize },
    Live,
}

/// A point on a picture `(width, height)`, clamped: a point outside it is not on
/// it, and a phone given one outside the canvas has nowhere to put it.
fn clamped(x: i32, y: i32, (width, height): (u32, u32)) -> (u16, u16) {
    let width = width.max(1) as i32;
    let height = height.max(1) as i32;
    (x.clamp(0, width - 1) as u16, y.clamp(0, height - 1) as u16)
}

/// A panel key as one of the commands the CarPlay side sends, or nothing when the
/// key is the car's own business.
///
/// A head unit is full of keys a phone has no use for — its radio, its
/// navigation, its own volume — and those are not mistakes to report: they are the
/// car's, and they stop here. The two families of direction keys both land on the
/// four directions a phone has: `move` names the direction to go in, and
/// `selector` is what a knob does when it moves its focus, which is the same
/// thing to a phone.
fn command_for(key: i32) -> Option<Command> {
    Some(match key {
        KEY_HOME | KEY_MAIN => Command::Home,
        KEY_BACK => Command::Back,
        KEY_OK => Command::Select,
        KEY_MOVE_LEFT => Command::Left,
        KEY_MOVE_RIGHT => Command::Right,
        KEY_MOVE_UP | KEY_SELECTOR_PREVIOUS => Command::Up,
        KEY_MOVE_DOWN | KEY_SELECTOR_NEXT => Command::Down,
        KEY_SEEK_SUB => Command::Prev,
        KEY_SEEK_ADD => Command::Next,
        KEY_MEDIA_START => Command::Play,
        KEY_MEDIA_STOP => Command::Pause,
        KEY_VR_START => Command::Siri,
        KEY_PHONE_CALL => Command::Telephony(PHONE_HOOK_SWITCH),
        KEY_PHONE_END => Command::Telephony(PHONE_DROP),
        KEY_MUTE => Command::Telephony(PHONE_MUTE),
        _ => return None,
    })
}

/// The next access unit from the CarPlay side, or never if there is no CarPlay
/// side. Waiting on a future that never resolves is what keeps the branch out of
/// the way without a second code path in the loop.
async fn recv_video(video: &mut Option<mpsc::Receiver<Vec<u8>>>) -> Option<Vec<u8>> {
    match video {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The same, for the media channel.
async fn recv_media(media: &mut Option<mpsc::Receiver<Media>>) -> Option<Media> {
    match media {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The same, for the navigation channel.
async fn recv_navi(navi: &mut Option<mpsc::Receiver<Navi>>) -> Option<Navi> {
    match navi {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Waits for the moment a prompt that has gone quiet is taken as over, or never
/// when none is waiting — the same trick the three above use to keep a branch
/// in the loop without a second code path.
async fn prompt_idle_after(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The same, for what the phone is playing.
async fn recv_playing(playing: &mut Option<mpsc::Receiver<Playing>>) -> Option<Playing> {
    match playing {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The same, for where the phone is going.
async fn recv_guidance(guidance: &mut Option<mpsc::Receiver<Guidance>>) -> Option<Guidance> {
    match guidance {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

struct Session<'a, W> {
    writer: W,
    cfg: &'a Config,
    /// Told the size the car asked to be rendered at, whenever it changes.
    screen: ScreenReport<'a>,
    /// Where the channel number the car reports is written, for `/info` to read.
    channel: SharedChannel,
    /// Where the car's touch and panel keys go.
    events: Arc<EventSink>,
    source: Source,
    /// When the video channel last carried anything at all. With pictures
    /// arriving on their own clock, this is what decides whether the heartbeat
    /// still has a job to do.
    channel_touched: Option<Instant>,
    /// The screen the channel carries, once the car has named one: what it named
    /// with its rate clamped into range, or the table's size stored in its place
    /// where the table preconfigures this number. A preset is written here as it
    /// arrives rather than kept beside the car's, so everything that reads the
    /// screen reads one field and nothing downstream asks which it was.
    screen_in_force: Option<proto::VideoEncoderInfo>,
    /// What the channel table says about the panel behind this car's number, once
    /// that number has arrived: the size the phone is asked for, when the table
    /// preconfigures one, and the density the panel's millimetres are worked out
    /// at. Nothing has been said yet, so nothing is assumed but the default.
    panel: info::Panel,
    /// When the encoder started, which is what the video channel's timestamps
    /// are measured from.
    started_at: Option<Instant>,
    /// The format the car has been told the media channel's samples are in, once
    /// it has been told. A stream that opens with the same one is not worth a
    /// second message: nothing about it has changed.
    media_format: Option<(u32, u16)>,
    /// The same for the navigation channel, and kept past the end of a prompt:
    /// the next one announces its player again, and the format it announces is
    /// the one the phone last said, not one to be worked out twice.
    navi_format: Option<(u32, u16)>,
    /// Whether the car currently has a prompt player open, which is what
    /// `MSG_NAVI_TTS_END` closes. It goes out with the first frame of a prompt
    /// and with nothing else.
    navi_open: bool,
    /// When silence on the navigation channel is taken as the end of the prompt.
    /// `None` while no prompt player is open.
    nav_deadline: Option<Instant>,
    /// What the phone has said about the song that is playing, folded together.
    /// It describes one attribute per update and the car's message is the whole
    /// song, so this is what the two are reconciled through.
    track: Track,
    /// The cover art that belongs to `track`, once the phone has sent it. Held
    /// apart from the track because it arrives as a file of its own, after the
    /// update that named the song and names none itself.
    art: Option<Vec<u8>>,
    /// The route the phone is guiding along, folded together from the two
    /// messages it describes one with — see `crate::navigation`, which is also
    /// where the phone's turn types become the car's.
    guidance: Guidance,
    /// Whether the car has asked to be told about the route. It asks with
    /// `MSG_CMD_CARLIFE_DATA_SUBSCRIBE_START` and stops asking with the `_STOP`
    /// that follows it, and no turn is sent before it does: the strip is the
    /// car's, and a car that has not asked for one is a car with nowhere to put
    /// it. The navigation module's own state is not behind this — see
    /// `publish_hud`.
    subscribed: bool,
    /// The navigation state the car was last told, so it is told again only when
    /// it changes. `None` is a car that has not been told yet.
    navi: Option<i32>,
    /// The turn the car's strip was last told to show, and `None` when it was
    /// last told to take one down. The action of the next message follows from
    /// what this holds and what is known now.
    shown: Option<proto::NextTurn>,
    /// The key the car's challenge is answered with, when one was built in.
    /// `None` is a session that cannot answer it, which is said out loud when the
    /// car asks.
    auth: Option<auth::Signer>,
    /// Whether the car has sent a position yet. The first one is worth a line at
    /// `info`, because it is the evidence that the subscription worked; every
    /// one after it belongs in `debug`, and only when it says something new.
    position_seen: bool,
    /// The last fix the car reported, with the clock fields cleared — see
    /// `untimed`. A parked car sends the same position over and over with only
    /// the timestamp moving, and that report is not worth a line.
    last_fix: Option<proto::CarGps>,
    /// Whether a position has been handed to the phone at all, which is the
    /// other half of that evidence: the car spoke, and the phone was told.
    delivered: bool,
    /// The last fix the phone was given, kept because a position has to be taken
    /// back as well as given: the phone holds it until it is told otherwise, and
    /// the sentences that take it back are stamped with the clock it came with.
    handed: Option<proto::CarGps>,
    /// Whether the car reports its coordinates in GCJ-02 rather than WGS84. The
    /// car names one of the two when it is asked to start reporting, and the
    /// phone's maps are in the datum the satellites are in.
    gcj02: bool,
    /// The iAP2 side's sending half, shared with whichever session is up: a
    /// position only reaches the phone through it.
    vehicle: Arc<Vehicle>,
    next_tick: Instant,
    up: bool,
    /// How many frames the car has sent, up to the handful reported raw — see
    /// `note_frames`. Counted here rather than kept as a local because the
    /// reporting sits in `handle`, which is called once per frame.
    frames_seen: u32,
    /// Where the finger was the last time the car said. A release has to name it
    /// — see `touch`.
    last_touch: Option<(u16, u16)>,
    /// Where the status page reads this session from. Reported to and never
    /// read back: see `crate::web::status`.
    status: Arc<Status>,
}

impl<W: AsyncWrite + Unpin> Session<'_, W> {
    /// Reports the first few frames the car sends, whatever they are.
    ///
    /// A type this code has no arm for is named at `debug`, and so is a channel
    /// it does not read, but neither says what was in the body. The first few
    /// frames are therefore reported as bytes, and then the log goes quiet
    /// again: after a handful the shape is known, and a session that is running
    /// sends frames sixty times a second.
    fn note_frames(&mut self, chunk: &frame::Chunk) {
        const REPORTED: u32 = 8;
        if self.frames_seen >= REPORTED {
            return;
        }
        self.frames_seen += 1;
        // Enough of the message to see where its head ends and its body starts.
        // Eight bytes was enough to tell a frame from a frame and not enough to
        // tell a head from the bytes around it.
        let body = &chunk.body[..chunk.body.len().min(64)];
        tracing::debug!(
            "cl.frame channel={} bytes={} body={:02x?}",
            chunk.channel,
            chunk.body.len(),
            body
        );
    }

    /// One message from the car.
    async fn handle<F: FnMut(bool)>(&mut self, chunk: frame::Chunk, carplay: &mut F) -> Result<()> {
        self.note_frames(&chunk);
        if chunk.channel != frame::CMD {
            self.other_channel(chunk).await;
            return Ok(());
        }
        let Some(message) = frame::decode(frame::CMD, &chunk.body) else {
            tracing::warn!("cl.command_dropped reason=decode_failed");
            return Ok(());
        };
        if !frame::from_car(message.service_type) {
            tracing::debug!(
                "cl.message_ignored type={:#010x} reason=direction_bit_clear",
                message.service_type
            );
            return Ok(());
        }

        match message.service_type {
            HU_PROTOCOL_VERSION => {
                let (major, minor) = proto::decode_protocol_version(&message.body);
                tracing::info!("cl.version version={major}.{minor}");
                self.status
                    .with(|s| s.carlife.protocol = Some((major, minor)));
                let body = proto::protocol_version_match_status(proto::MATCH);
                self.send(MATCH_STATUS, &body).await?;
            }
            HU_INFO => {
                let body = proto::device_info(&proto::DeviceInfo {
                    os: "Android".into(),
                    brand: self.cfg.name.clone(),
                    model: self.cfg.name.clone(),
                    release: "10".into(),
                    sdk_int: 29,
                    bt_address: self.cfg.bt_mac.clone(),
                });
                self.send(MD_INFO, &body).await?;
                if !self.up {
                    self.up = true;
                    self.status.with(|s| s.carlife.session_up = true);
                    tracing::info!("cl.session_up");
                    carplay(true);
                }
            }
            HU_STATISTIC_INFO => {
                let info = proto::decode_statistic_info(&message.body);
                // `connect_time` is reported as-is: the field is an int32 and
                // neither the guide nor the head-unit library says in what unit.
                tracing::info!(
                    "cl.car_info cuid={:?} version={:?} build={} channel={:?} connections={} \
                     connected={} connect_time={}",
                    info.cuid,
                    info.version_name,
                    info.version_code,
                    info.channel,
                    info.connect_count,
                    info.connect_success_count,
                    info.connect_time
                );
                self.status.with(|s| {
                    s.carlife.statistic = Some(Statistic {
                        cuid: info.cuid.to_string(),
                        version_name: info.version_name.to_string(),
                        version_code: info.version_code,
                        channel: info.channel.to_string(),
                        connect_count: info.connect_count,
                        connect_success_count: info.connect_success_count,
                        connect_time: info.connect_time,
                    });
                });
                // `/info` is answered from this: the channel picks the icon and
                // the label the phone shows for us.
                if let Ok(mut reported) = self.channel.lock() {
                    *reported = Some(info.channel.to_string());
                }
                // What the table says about this car's panel. The number arrives
                // here and nowhere else, so it is read here: the size is what
                // stands in for the one the car will name, and the density is
                // what the millimetres are worked out at. Nothing is reported
                // from it — `INIT_DONE` is where the screen is settled, and it is
                // settled in one place.
                self.panel = info::panel(Some(info.channel));
                if let Some(preset) = self.panel.preset {
                    tracing::info!(
                        "cl.panel_preset width={} height={} fps={} ppi={} source=channel_map",
                        preset.width,
                        preset.height,
                        preset.fps,
                        self.panel.ppi
                    );
                }
                // The other half of the authentication: the car's channel number
                // is what the phone judges it by, and the judgement is what this
                // message answers with. A verdict either way is sent — the car is
                // told what a phone makes of it rather than left waiting for an
                // answer it may take as acceptance.
                self.judge(info.channel).await?;
                // The car will not configure anything until it is asked, and it
                // has a two-minute limit on sending the statistics above.
                self.send(FEATURE_CONFIG_REQUEST, &[]).await?;
                // And the same question a phone asks right after that one: what
                // vehicle data the car has. Nothing waits for a route to start —
                // the answer is a property of the car, and a fix is worth having
                // whether or not anything is being guided along it.
                self.send(CAR_DATA_SUBSCRIBE, &[]).await?;
            }
            HU_FEATURE_CONFIG_RESPONSE => {
                let configs = proto::decode_feature_configs(&message.body);
                for (key, value) in &configs {
                    tracing::info!("cl.setting key={key} value={value:?}");
                }
                self.status.with(|s| s.carlife.features = configs);
            }
            // The car's answer to the question asked with the statistics: the
            // modules it can report. GPS is the one this bridge consumes, and it
            // is asked to start — the other five are the phone's own navigation's
            // business and nothing here reads them.
            HU_CAR_DATA_SUBSCRIBE_DONE => {
                let offered = proto::decode_vehicle_infos(&message.body);
                let modules: Vec<i32> = offered.iter().map(|info| info.module).collect();
                let gps = offered.iter().find(|info| info.module == proto::CAR_DATA_GPS);
                match gps {
                    // The flag is the coordinate system, and a phone only takes a
                    // GPS module in one of the two it knows — so a car naming a
                    // third is not asked for the reports it would refuse anyway.
                    Some(info) if matches!(info.flag, 1 | 2) => {
                        tracing::info!(
                            "cl.car_data_offered modules={modules:?} action=subscribe \
                             module={} flag={}",
                            proto::CAR_DATA_GPS,
                            info.flag
                        );
                        // What every report after this one is in: 1 is WGS84, 2
                        // is the datum Chinese maps are drawn in.
                        self.gcj02 = info.flag == 2;
                        let body = proto::car_data_subscribe_start(
                            proto::CAR_DATA_GPS,
                            info.flag,
                            0,
                        );
                        self.send(CAR_DATA_SUBSCRIBE_START, &body).await?;
                    }
                    Some(info) => tracing::warn!(
                        "cl.car_data_offered modules={modules:?} action=none \
                         reason=coordinate_system flag={}",
                        info.flag
                    ),
                    None => {
                        tracing::info!("cl.car_data_offered modules={modules:?} action=none")
                    }
                }
            }
            // The car's position, at the rate it reports at. Two consumers: the
            // page, and the phone, which takes a fix only as NMEA sentences and
            // only while it has asked for them.
            HU_CAR_GPS => {
                let fix = proto::decode_car_gps(&message.body);
                let latitude = f64::from(fix.latitude) / 1e6;
                let longitude = f64::from(fix.longitude) / 1e6;
                if !self.position_seen {
                    self.position_seen = true;
                    tracing::info!(
                        "cl.car_gps_received antenna={} latitude={latitude:.6} \
                         longitude={longitude:.6}",
                        fix.antenna_state
                    );
                }
                let report = untimed(fix);
                if self.last_fix != Some(report) {
                    self.last_fix = Some(report);
                    tracing::debug!(
                        "cl.car_gps antenna={} latitude={latitude:.6} longitude={longitude:.6} \
                         height={} speed={} heading={} accuracy_m={}",
                        fix.antenna_state,
                        fix.height,
                        fix.speed,
                        fix.heading,
                        fix.hor_pos_error
                    );
                }
                self.hand_to_phone(&fix);
                self.status.with(|s| s.carlife.gps = Some(fix));
            }
            HU_AUTHEN_REQUEST => {
                let challenge = proto::decode_authen_request(&message.body).unwrap_or_default();
                self.answer(challenge).await?;
            }
            HU_AUTHEN_RESULT => {
                // The car's own verdict on us, which it sends whether or not it
                // liked the answer. Nothing is owed back: the phone's verdict is
                // about the car and was sent with the statistics above.
                let passed = proto::decode_authen_result(&message.body).unwrap_or(false);
                self.status.with(|s| s.carlife.accepted = Some(passed));
                if passed {
                    tracing::info!("cl.auth_result result=accepted");
                } else if self.auth.is_none() {
                    // The answer to a challenge that was skipped, which is
                    // nothing the operator was not already told at startup.
                    tracing::debug!("cl.auth_result result=rejected reason=no_key");
                } else {
                    tracing::warn!("cl.auth_result result=rejected consequence=disconnect");
                }
            }
            HU_VIDEO_ENCODER_INIT => {
                let asked = proto::decode_video_encoder_info(&message.body);
                // What the car names, clamped into what this side can produce.
                let fallback = ScreenParams::default();
                let named = proto::VideoEncoderInfo {
                    width: if asked.width > 0 {
                        asked.width
                    } else {
                        fallback.width as i32
                    },
                    height: if asked.height > 0 {
                        asked.height
                    } else {
                        fallback.height as i32
                    },
                    frame_rate: match asked.frame_rate {
                        MIN_FPS..=MAX_FPS => asked.frame_rate,
                        _ => fallback.fps as i32,
                    },
                };
                // And what the channel carries, which is `INIT_DONE`'s answer:
                // the table's size where it preconfigures this number, because a
                // make that got its own screen wrong asked for the wrong
                // numbers. That is what is stored, in place of what the car
                // said, so nothing past this point has to ask which it was.
                let in_force = self.preset_in_place_of(named);
                tracing::info!(
                    "cl.geometry_asked width={} height={} fps={} served_width={} \
                     served_height={} served_fps={} source={}",
                    asked.width,
                    asked.height,
                    asked.frame_rate,
                    in_force.width,
                    in_force.height,
                    in_force.frame_rate,
                    if self.panel.preset.is_some() {
                        "channel_map"
                    } else {
                        "car"
                    }
                );
                self.screen_in_force = Some(in_force);
                // The panel is the car's, so its physical size is worked out
                // here, from the size the phone is actually asked for and the
                // density the table gives this car — 120 dpi where it says
                // nothing. Nothing in the protocol carries a size.
                let ppi = self.panel.ppi;
                let physical = (
                    info::millimetres(in_force.width as u32, ppi),
                    info::millimetres(in_force.height as u32, ppi),
                );
                self.status.with(|s| {
                    s.carlife.requested = Some(Size {
                        width: asked.width,
                        height: asked.height,
                        fps: asked.frame_rate,
                    });
                    s.carlife.agreed = Some(Size {
                        width: in_force.width,
                        height: in_force.height,
                        fps: in_force.frame_rate,
                    });
                    s.carlife.physical = Some(physical);
                    s.carlife.frame_rate = Some(in_force.frame_rate);
                });
                let body = proto::video_encoder_info(in_force);
                self.send(VIDEO_ENCODER_INIT_DONE, &body).await?;
                // The channel is what the car decodes and what the phone draws,
                // so this is the size the other half is told: the two halves
                // meet here.
                (self.screen)(
                    in_force.width as u32,
                    in_force.height as u32,
                    in_force.frame_rate as u32,
                );
            }
            HU_VIDEO_ENCODER_START => {
                tracing::info!("cl.encoder state=started");
                let now = Instant::now();
                self.started_at = Some(now);
                self.status.with(|s| s.carlife.encoding = true);
                // The start itself is what the car hears next, so the first
                // heartbeat is only owed if no picture arrives within a period.
                self.channel_touched = Some(now);
                self.next_tick = now;
            }
            HU_VIDEO_ENCODER_PAUSE | HU_VIDEO_ENCODER_RESET => {
                tracing::info!("cl.encoder state=stopped");
                self.started_at = None;
                self.status.with(|s| s.carlife.encoding = false);
            }
            HU_VIDEO_ENCODER_FRAME_RATE_CHANGE => {
                let asked = proto::decode_frame_rate(&message.body).unwrap_or(0);
                // Only a car the table says nothing about changes its own rate. A
                // preset's rate came with its size, and the phone is rendering at
                // that one: agreeing to another would leave the car pacing a
                // picture the phone never sends.
                let current = self.screen_in_force.map_or(30, |screen| screen.frame_rate);
                let accepted = match asked {
                    MIN_FPS..=MAX_FPS if self.panel.preset.is_none() => asked,
                    _ => current,
                };
                tracing::info!("cl.frame_rate asked={asked} agreed={accepted}");
                if let Some(screen) = self.screen_in_force.as_mut() {
                    screen.frame_rate = accepted;
                }
                self.status.with(|s| {
                    if let Some(agreed) = s.carlife.agreed.as_mut() {
                        agreed.fps = accepted;
                    }
                    s.carlife.frame_rate = Some(accepted);
                });
                let body = proto::frame_rate(accepted);
                self.send(VIDEO_ENCODER_FRAME_RATE_CHANGE_DONE, &body)
                    .await?;
                // The size is unchanged, and the rate is the channel's pacing
                // rather than anything the phone produces, so the CarPlay side is
                // not told: it would cost a session for a number it ignores.
            }
            HU_GO_TO_FOREGROUND => {
                // There is no window here to bring forward, so the honest reply
                // is sent at once rather than after the five seconds the guide
                // gives a device that is actually switching.
                tracing::debug!("cl.foreground");
                self.send(GO_TO_FOREGROUND_RESPONSE, &[]).await?;
            }
            // The car asking whether this device can tell it about a route. The
            // answer is owed per module, and the car is the one that decides what
            // to do with it — so it is told exactly what this side has.
            HU_DATA_SUBSCRIBE => {
                let asked = proto::decode_data_subscribe(&message.body);
                tracing::info!("cl.car_data_query modules={asked:?}");
                let answer: Vec<(i32, bool)> = asked
                    .iter()
                    .map(|module| (*module, *module == proto::DATA_TURN_BY_TURN))
                    .collect();
                let body = proto::data_subscribe_done(&answer);
                self.send(DATA_SUBSCRIBE_DONE, &body).await?;
            }
            // And the car asking to be sent it, or to stop being sent it. Both
            // are answered by what the strip is told next rather than by a
            // message of their own.
            HU_DATA_SUBSCRIBE_START | HU_DATA_SUBSCRIBE_STOP => {
                let asked = proto::decode_data_subscribe(&message.body);
                let wanted = message.service_type == HU_DATA_SUBSCRIBE_START;
                tracing::info!(
                    "cl.car_data_subscription action={} modules={asked:?}",
                    if wanted { "send" } else { "stop" }
                );
                if asked.contains(&proto::DATA_TURN_BY_TURN) {
                    self.subscribed = wanted;
                }
                // Now rather than at the next change of route: a car that asks
                // mid-route is owed the turn it is already on, and one that has
                // stopped asking is owed the strip taken down.
                self.publish_hud().await?;
            }
            HU_ERROR_CODE => {
                self.status
                    .with(|s| s.carlife.error_bytes = Some(message.body.len()));
                tracing::warn!("cl.error_report bytes={}", message.body.len());
            }
            other => tracing::debug!("cl.message_ignored type={other:#010x}"),
        }
        Ok(())
    }

    /// One fix, handed to the phone's half of the bridge.
    ///
    /// What the phone does with the sentences is its own business — it asks for
    /// the ones it wants, and LIVI writes out only those. A car that reports no
    /// fix has nothing to hand over and is told apart from one whose clock is
    /// unset: the first is a position the phone may already hold and has to give
    /// up, the second leaves the phone to its own position rather than to a
    /// made-up one, and there is nothing to say.
    fn hand_to_phone(&mut self, fix: &proto::CarGps) {
        if fix.antenna_state != 1 {
            self.take_back_from_phone();
            return;
        }
        let Some(block) = gps::sentences(fix, self.gcj02) else {
            return;
        };
        self.push_location(&block);
        self.handed = Some(*fix);
        if !self.delivered {
            self.delivered = true;
            tracing::info!("cl.car_gps_handed sentences={}", block.lines().count());
        }
    }

    /// The car's fix, gone: the phone is told so, once.
    ///
    /// Only ever said after a fix went out — a phone given nothing has nothing
    /// to give up. Where the car has gone altogether, the caller is the end of
    /// the session, and the phone outlives it: CarPlay here is wireless, and
    /// what it keeps of a car that has driven off is the last place it was told.
    fn take_back_from_phone(&mut self) {
        let Some(at) = self.handed.take() else {
            return;
        };
        // What went out had a clock, so the sentences that take it back have one.
        if let Some(block) = gps::no_fix(&at) {
            self.push_location(&block);
            tracing::info!("cl.car_gps_withdrawn");
        }
    }

    /// The one way to the phone, which takes the block base64'd. A block encoded
    /// here cannot fail to decode again, so there is nothing to report.
    fn push_location(&self, block: &str) {
        let _ = self.vehicle.push_location(&BASE64.encode(block));
    }

    /// The car's authentication challenge, answered with the phone's key.
    ///
    /// A challenge that cannot be answered is not answered at all: the app sends
    /// nothing when it cannot put one through its key, and a car given an answer
    /// it cannot check is no better off than one given none — except that it was
    /// lied to. The car does not get to hear about the failure either way; it
    /// works it out for itself, and says so with `HU_AUTHEN_RESULT`.
    ///
    /// A session handed no key skips the exchange altogether and carries on. The
    /// head unit asks again every half minute and is skipped again, so the line
    /// is `debug`: the operator was told once, where the key was read.
    async fn answer(&mut self, challenge: &str) -> Result<()> {
        // Reported before it is attempted, so that a challenge nothing can
        // answer shows up as asked-and-unanswered rather than as never asked.
        self.status.with(|s| {
            s.carlife.challenge = Some(challenge.to_string());
            s.carlife.challenge_answered = Some(false);
        });
        let Some(key) = self.auth.as_ref() else {
            tracing::debug!(
                "cl.challenge result=skipped reason=no_key challenge={challenge:?}"
            );
            return Ok(());
        };
        let Some(value) = key.answer(challenge) else {
            return Ok(());
        };
        tracing::info!(
            "cl.challenge result=answered chars={} challenge={challenge:?}",
            value.len()
        );
        self.status
            .with(|s| s.carlife.challenge_answered = Some(true));
        let body = proto::authen_response(&value);
        self.send(MD_AUTHEN_RESPONSE, &body).await
    }

    /// The car's channel number, judged, and the car told what a phone makes of
    /// it. See `auth::channel_is_legal` for what the judgement is.
    ///
    /// An answer is sent either way. A phone that does not accept a car is
    /// expected to disconnect from it, and this device does not: the car is the
    /// only way the phone's screen reaches a panel, and a channel number that is
    /// not one the app serves is a reason to say so rather than to take the
    /// picture away. The car is free to act on the verdict, which is the same
    /// thing a real phone would be telling it.
    async fn judge(&mut self, channel: &str) -> Result<()> {
        let legal = auth::channel_is_legal(channel);
        self.status.with(|s| s.carlife.channel_legal = Some(legal));
        if legal {
            tracing::info!("cl.channel_check channel={channel:?} result=legal");
        } else {
            tracing::warn!("cl.channel_check channel={channel:?} result=illegal");
        }
        let body = proto::authen_result(legal);
        self.send(MD_AUTHEN_RESULT, &body).await
    }

    /// A chunk on any channel but the command channel.
    ///
    /// The sixth channel is the car's input, and it is the only one of the other
    /// five that carries anything: the four after it are the media and voice
    /// channels, which are the car's own. What arrives here is turned into the
    /// commands the CarPlay side sends, and a key or a touch the CarLife protocol
    /// has no meaning for is dropped with a line rather than guessed at.
    async fn other_channel(&mut self, chunk: frame::Chunk) {
        if chunk.channel != frame::TOUCH {
            tracing::debug!(
                "cl.chunk_ignored channel={} bytes={}",
                chunk.channel,
                chunk.body.len()
            );
            return;
        }
        let Some(message) = frame::decode(frame::TOUCH, &chunk.body) else {
            tracing::warn!("cl.touch_dropped reason=decode_failed");
            return;
        };
        let command = match message.service_type {
            TOUCH_ACTION => match proto::decode_touch_action(&message.body) {
                Some(touch) => Some(self.touch(touch)),
                None => {
                    tracing::warn!("cl.touch_dropped reason=missing_fields");
                    None
                }
            },
            TOUCH_HARD_KEY => match proto::decode_hard_key(&message.body) {
                Some(key) => {
                    let command = command_for(key);
                    if command.is_none() {
                        tracing::debug!("cl.key_ignored key={key:#04x}");
                    }
                    command
                }
                None => {
                    tracing::warn!("cl.key_dropped reason=missing_field");
                    None
                }
            },
            other => {
                tracing::debug!("cl.touch_ignored type={other:#010x}");
                None
            }
        };
        let Some(command) = command else {
            return;
        };
        // Failures are the phone's to have: a press that arrives before the event
        // channel is up is dropped by the sink with a line of its own.
        if let Err(e) = self.events.dispatch(command).await {
            tracing::warn!("cl.input_send_failed err={e}");
        }
    }

    /// One touch action, as the report the phone wants: one six-byte slot per
    /// finger the descriptor declares, and the car sends one finger, so the
    /// second slot is always empty.
    ///
    /// The release names where the finger was. The car repeats the last position
    /// for it, and that is what the phone needs: it completes a tap only when the
    /// finger lifts inside the control it went down on, so a release at the
    /// origin reads as the finger jumping to the top left corner and the tap is
    /// cancelled instead — a button that animates and does nothing.
    fn touch(&mut self, touch: proto::TouchAction) -> Command {
        if touch.action == TOUCH_UP {
            let (x, y) = self.last_touch.unwrap_or((0, 0));
            return Command::Touch([
                TouchContact { down: false, x, y },
                TouchContact::default(),
            ]);
        }
        let (x, y) = self.on_canvas(touch.x, touch.y);
        self.last_touch = Some((x, y));
        Command::Touch([
            TouchContact { down: true, x, y },
            TouchContact::default(),
        ])
    }

    /// What the video channel carries where the channel table preconfigures this
    /// car's number: the table's size stands in for the one the car named. This
    /// is the only place the table and the car are compared — the value it
    /// returns is stored, and everything else reads the store.
    fn preset_in_place_of(&self, named: proto::VideoEncoderInfo) -> proto::VideoEncoderInfo {
        match self.panel.preset {
            Some(preset) => proto::VideoEncoderInfo {
                width: preset.width as i32,
                height: preset.height as i32,
                frame_rate: preset.fps as i32,
            },
            None => named,
        }
    }

    /// The size the picture is drawn at, which is the size the car was answered
    /// with. A preset was stored in place of what the car named, so this is one
    /// field and there is nothing to choose between.
    fn canvas(&self) -> (u32, u32) {
        self.screen_in_force.map_or(
            (u32::from(u16::MAX), u32::from(u16::MAX)),
            |screen| (screen.width.max(1) as u32, screen.height.max(1) as u32),
        )
    }

    /// The car's own pixels are the phone's. `INIT_DONE` is what told the car
    /// which picture to measure its touches in, and a preset was stored there in
    /// place of what it named, so a touch arrives in those numbers already: it is
    /// clamped and nothing else, because a point outside the picture is not on
    /// it.
    fn on_canvas(&self, x: i32, y: i32) -> (u16, u16) {
        clamped(x, y, self.canvas())
    }

    /// Sends whatever the video channel owes right now.
    async fn tick(&mut self) -> Result<()> {
        let Some(started_at) = self.started_at else {
            // Nothing has asked for a picture, so nothing is sent. The tick is
            // still alive, because it is what notices when that changes.
            return Ok(());
        };
        // The file's next picture, taken before it is written: the write wants
        // the session mutably and the file is part of the session.
        if let Some(unit) = self.next_file_unit() {
            let elapsed = started_at.elapsed().as_millis() as u32;
            return self.send_frame(VIDEO_DATA, elapsed, &unit).await;
        }
        match &self.source {
            // Pictures arrive on the phone's own clock, so there is nothing for
            // the tick to send while they do. A heartbeat goes out once they
            // stop, which is what tells the car the channel is still ours.
            Source::Live => {
                let idle = self
                    .channel_touched
                    .map_or(true, |touched| touched.elapsed() >= HEARTBEAT);
                if idle {
                    self.channel_touched = Some(Instant::now());
                    self.send(VIDEO_HEARTBEAT, &[]).await
                } else {
                    Ok(())
                }
            }
            _ => self.send(VIDEO_HEARTBEAT, &[]).await,
        }
    }

    /// Writes one frame into a pipe the car has not used, and says what became
    /// of it.
    ///
    /// What a write costs is the only thing this side can learn about the other:
    /// it completes once the host has read the endpoint, and it does not while
    /// nobody is reading. So a car that configured the device and then went
    /// quiet is told apart by one frame — a host that takes it is a host that is
    /// listening and waiting to be spoken to, and one that leaves it queued
    /// never opened the pipe at all. The frame is a video heartbeat: the
    /// emptiest message the protocol has, and one a head unit that has not
    /// started the encoder drops.
    ///
    /// The wait is bounded, and a write still queued when it runs out is left
    /// where it is: it goes out if the host ever reads, and until then it costs
    /// a blocking thread that nothing waits on.
    async fn probe(&mut self) {
        match tokio::time::timeout(PROBE_WAIT, self.send(VIDEO_HEARTBEAT, &[])).await {
            Ok(Ok(())) => tracing::info!("usb.probe result=written"),
            Ok(Err(e)) => tracing::warn!("usb.probe result=failed err={e:#}"),
            Err(_) => tracing::warn!(
                "usb.probe result=not_taken waited_ms={}",
                PROBE_WAIT.as_millis()
            ),
        }
    }

    /// The file's next access unit, moving the file past it, or nothing at all
    /// when the pictures are coming from somewhere else.
    ///
    /// The end of the file is the start of it: a file that ran out would leave
    /// the channel with heartbeats and a picture that stopped.
    fn next_file_unit(&mut self) -> Option<Vec<u8>> {
        let Source::Cycle { units, next } = &mut self.source else {
            return None;
        };
        let unit = units[*next].clone();
        *next = (*next + 1) % units.len();
        Some(unit)
    }

    /// One access unit from the CarPlay side, on the video channel.
    ///
    /// A frame that arrives before the car has started the encoder, or while it
    /// has it paused, is dropped rather than queued: the car is not expecting
    /// pictures then, and one held back would arrive attached to a timestamp
    /// measured from a start that has since been reset.
    async fn send_video(&mut self, access_unit: Vec<u8>) -> Result<()> {
        let Some(started_at) = self.started_at else {
            return Ok(());
        };
        let timestamp_ms = started_at.elapsed().as_millis() as u32;
        self.channel_touched = Some(Instant::now());
        self.send_frame(VIDEO_DATA, timestamp_ms, &access_unit).await
    }

    /// One thing the phone's audio produced, on the media channel.
    ///
    /// The car is told the format before it is given any samples, and only when
    /// it changes: the message is what sets a player up, and the same one twice
    /// is a player set up for a stream that did not begin. A stream that opens
    /// after a silence is the case this is here for.
    async fn send_media(&mut self, item: Media) -> Result<()> {
        match item {
            Media::Format { rate, channels } => {
                if self.media_format == Some((rate, channels)) {
                    return Ok(());
                }
                tracing::info!("cl.audio_ready channel=media rate_hz={rate} channels={channels}");
                self.media_format = Some((rate, channels));
                self.status
                    .with(|s| s.carlife.media = Some((rate, channels)));
                let body = proto::media_init(rate, channels);
                self.send(MEDIA_INIT, &body).await
            }
            Media::Pcm {
                timestamp_ms,
                data,
            } => self.send_frame(MEDIA_DATA, timestamp_ms, &data).await,
            // The phone stopped playing. Saying so is what lets the car put down
            // what it took up for the stream rather than hold it over silence.
            Media::Ended => {
                self.media_format = None;
                self.send(MEDIA_STOP, &[]).await
            }
        }
    }

    /// One thing the phone's navigation prompt produced, on the TTS channel.
    ///
    /// The car is told the format with the first frame and not before: the INIT
    /// is what sets a player up, and a player set up over silence is one the car
    /// has an open channel for and nothing to put in. The frames themselves are
    /// 20 ms of 16 kHz mono, one message each — the channel's body cap is 50 KB
    /// and a frame is 640 bytes, so nothing here needs splitting.
    async fn send_navi(&mut self, item: Navi) -> Result<()> {
        match item {
            Navi::Format { rate, channels } => {
                // The same format as the last one is not a new player, and
                // restarting it mid-sentence would be heard as one.
                if self.navi_format == Some((rate, channels)) {
                    return Ok(());
                }
                // A player already open at another format is put down first: the
                // car builds one from the INIT it is sent, and two of them for
                // one voice is the kind of thing that is only heard, not seen.
                self.close_navi().await?;
                tracing::info!("cl.audio_ready channel=navi rate_hz={rate} channels={channels}");
                self.navi_format = Some((rate, channels));
                Ok(())
            }
            Navi::Pcm {
                timestamp_ms,
                data,
            } => {
                // Every frame is the prompt still going on, so every frame puts
                // the end of it further away.
                self.nav_deadline = Some(Instant::now() + TTS_IDLE);
                if !self.navi_open {
                    let (rate, channels) = self.navi_format.unwrap_or((16_000, 1));
                    self.navi_open = true;
                    self.send(NAVI_TTS_INIT, &proto::media_init(rate, channels))
                        .await?;
                }
                self.send_frame(NAVI_TTS_DATA, timestamp_ms, &data).await
            }
            Navi::Ended => self.close_navi().await,
        }
    }

    /// Tells the car the prompt is over, once.
    ///
    /// Called both when the phone says so and when it has gone quiet for longer
    /// than `TTS_IDLE`, which is why it has to be safe to call twice: the phone
    /// often leaves the stream open after the last word, and the second END
    /// would be closing a player that is not there.
    async fn close_navi(&mut self) -> Result<()> {
        self.nav_deadline = None;
        if !self.navi_open {
            return Ok(());
        }
        self.navi_open = false;
        tracing::debug!("cl.prompt state=over");
        self.send(NAVI_TTS_END, &[]).await
    }

    /// One thing the phone said it is playing, folded into what is known and
    /// then written out.
    ///
    /// The phone describes one attribute at a time and the car's message is the
    /// whole song, so every update sends the song as it now stands: the panel
    /// redraws once per attribute, which is what the phone's own panel does
    /// with the same messages. An update that says nothing new is not worth a
    /// message.
    ///
    /// The cover art is the one thing that is not a field of that message's
    /// subject: it is a file, it arrives after the song it belongs to, and a
    /// song the panel has not been told about is not one to attach it to yet.
    ///
    /// It is held until the phone sends another, or until a song that carries a
    /// length says a new one has started — see the arm below for why that is
    /// the only moment at which a stale cover can be told from an early one.
    async fn send_playing(&mut self, item: Playing) -> Result<()> {
        match item {
            Playing::Track(update) => {
                // A length is sent for a song and never for a change of play
                // state, and it arrives with the rest of the new song rather
                // than after it — so it is what says here that a song has
                // started. The cover that is held belongs to the one before
                // it: a file transfer names no song, so there is no later
                // moment at which the two could be told apart, and a new song
                // that arrives without a picture would otherwise wear the last
                // one's. It goes down here, and the song is sent without it.
                let started = update.duration_s.is_some();
                let before = self.track.clone();
                self.track.merge(update);
                if self.track == before && !started {
                    return Ok(());
                }
                if started {
                    self.art = None;
                    self.publish_cover();
                }
            }
            Playing::Artwork(image) => {
                // The car reads the song and its cover from one command-channel
                // message, and the reference keeps a cover under 30 KB for it.
                // A cover too large for that has already been re-encoded on the
                // CarPlay side — see `crate::artwork` — so what arrives here is
                // either small enough or not going to be. This is the backstop:
                // the song still goes, the cover does not, and the message stays
                // one the car can take.
                if image.len() > proto::MAX_ART {
                    tracing::warn!(
                        "cl.media_cover_dropped bytes={} cap_bytes={}",
                        image.len(),
                        proto::MAX_ART
                    );
                    return Ok(());
                }
                // A file transfer that carried nothing is the phone saying this
                // song has no picture. It is not ignored: it takes the place of
                // the cover that is held — which is what puts the last song's
                // down — and the message that follows carries no `albumArt` at
                // all rather than a zero-length one.
                if self.art.as_deref() == Some(image.as_slice()) {
                    return Ok(());
                }
                if image.is_empty() {
                    tracing::debug!("cl.media_cover_empty action=send_without_cover");
                }
                self.art = Some(image);
                // The page draws it as soon as it is here rather than when the
                // song it belongs to goes out: it is the same picture the car
                // will be given, and a cover often arrives after the title.
                self.publish_cover();
            }
        }

        // An app name on its own is not a song, and the panel is better left
        // with the last song it was told than emptied by the play state.
        if self.track.is_silent() {
            return Ok(());
        }
        self.publish_song();
        self.write_media().await
    }

    /// Puts the song on the status page, which is the one place anyone but the
    /// car can see it.
    ///
    /// Reported before the message is written rather than after: the page is a
    /// report of what is playing, not of what has been written to the socket.
    fn publish_song(&self) {
        let track = &self.track;
        self.status.with(|s| {
            s.now_playing.source = track.source.clone();
            s.now_playing.song = track.song.clone();
            s.now_playing.artist = track.artist.clone();
            s.now_playing.album = track.album.clone();
            s.now_playing.duration_s = track.duration_s;
        });
    }

    /// And the cover, which the page draws from a path of its own.
    ///
    /// A cover that has just been put down is reported as one: the page draws
    /// nothing rather than the picture of a song that has finished.
    fn publish_cover(&self) {
        self.status
            .cover_changed(self.art.clone().unwrap_or_default());
    }

    /// The whole of what is known about the song, as the car's one message.
    ///
    /// What `send_playing` calls once the phone's word is folded in.
    async fn write_media(&mut self) -> Result<()> {
        // A song that has been emptied since is not one to tell the panel
        // about: the last one it was told is better than a blank.
        if self.track.is_silent() {
            return Ok(());
        }
        let song = self.track.song.as_deref().unwrap_or_default();
        let artist = self.track.artist.as_deref().unwrap_or_default();
        let album = self.track.album.as_deref().unwrap_or_default();
        let source = self.track.source.as_deref().unwrap_or_default();
        // The size rather than a flag, so this line and the car's own can be put
        // side by side: the panel takes the picture from this message and
        // nothing else, so a size that does not match is the whole difference.
        //
        // `debug` rather than `info`: an app that puts its lyrics in the title
        // has the phone sending a new one per line, so this is per-event detail
        // however much it looks like one line per song.
        let cover = self
            .art
            .as_ref()
            .map_or(0.0, |art| art.len() as f64 / 1024.0);
        tracing::debug!(
            "cl.media_info song={song:?} artist={artist:?} album={album:?} duration_s={} \
             source={source:?} cover_kb={cover:.1}",
            self.track.duration_s.unwrap_or(0)
        );
        let body = proto::media_info(&self.track, self.art.as_deref().filter(|art| !art.is_empty()));
        self.send(MEDIA_INFO, &body).await
    }

    /// One thing the phone said about the route it is guiding along.
    ///
    /// The phone describes a route in messages of its own, each carrying one
    /// part of it, so what arrives is folded into what is known and the car is
    /// told the whole of it when that changes. See `crate::navigation` for the
    /// folding and for the one table that knows both vocabularies.
    async fn send_guidance(&mut self, update: Guidance) -> Result<()> {
        self.guidance.merge(update);
        self.publish_hud().await
    }

    /// Whatever the car's turn-by-turn strip is owed right now, and nothing when
    /// it is owed nothing.
    ///
    /// Two messages, neither sent twice with the same content.
    /// `MSG_CMD_MODULE_STATUS` is the state of this device's navigation module,
    /// which is the phone's own business rather than part of the data a car
    /// subscribes to, so it goes out whether or not a strip was asked for.
    /// `MSG_CMD_NAVI_NEXT_TURN_INFO` is the turn itself, and its action follows
    /// from what the strip was last told: the first turn put up is a `SHOW`, one
    /// that replaces a turn already there is an `UPDATE`, and a strip with
    /// nothing left to show is taken down with a `HIDE`.
    async fn publish_hud(&mut self) -> Result<()> {
        if let Some(running) = self.guidance.running() {
            let state = if running {
                proto::NAVI_START
            } else {
                proto::NAVI_IDLE
            };
            if self.navi != Some(state) {
                self.navi = Some(state);
                tracing::info!(
                    "cl.navi state={}",
                    if running { "running" } else { "idle" }
                );
                let body = proto::module_status(proto::NAVI_MODULE, state);
                self.send(MODULE_STATUS, &body).await?;
            }
        }

        let wanted = self.turn_to_show();
        let action = match (&self.shown, &wanted) {
            // Nothing was up and there is nothing to put up.
            (None, None) => return Ok(()),
            (None, Some(_)) => proto::ACTION_SHOW,
            // The route is over, or the car has stopped asking: either way there
            // is nothing left to show.
            (Some(_), None) => proto::ACTION_HIDE,
            // The same turn described again, with nothing about it changed.
            (Some(shown), Some(turn)) if shown == turn => return Ok(()),
            (Some(_), Some(_)) => proto::ACTION_UPDATE,
        };
        self.shown = match action {
            proto::ACTION_HIDE => None,
            _ => wanted,
        };

        let nothing = proto::NextTurn::default();
        let turn = self.shown.as_ref().unwrap_or(&nothing);
        let action_name = match action {
            proto::ACTION_SHOW => "show",
            proto::ACTION_UPDATE => "update",
            _ => "hide",
        };
        if action == proto::ACTION_HIDE {
            tracing::info!("cl.navi_turn action={action_name}");
        } else {
            tracing::info!(
                "cl.navi_turn action={action_name} kind={:?} road={:?} remain_m={:?}",
                turn.kind.map(|k| format!("0x{k:02X}")),
                turn.road,
                turn.remain_m
            );
        }
        let body = proto::navi_next_turn_info(action, turn);
        self.send(NAVI_NEXT_TURN_INFO, &body).await
    }

    /// The turn the strip is to show, or nothing when there is none to show.
    ///
    /// Three things have to hold. The car has to have asked for the data, the
    /// phone has to say a route is being guided, and the guidance has to point
    /// at a turn the phone has described. Anything less is a strip with nothing
    /// behind it, and the car's own field for each of them is written only when
    /// there is something to put in it.
    fn turn_to_show(&self) -> Option<proto::NextTurn> {
        if !self.subscribed || self.guidance.running() != Some(true) {
            return None;
        }
        let turn = self.guidance.current()?;
        Some(proto::NextTurn {
            kind: turn.kind.and_then(turn_kind),
            road: turn.onto.clone(),
            // Metres either way, so only the width of the two numbers differs.
            // The car's field is an `int32` and the phone's a `uint32`; a
            // distance that does not fit is a distance this side does not have
            // rather than one it rounds.
            remain_m: self
                .guidance
                .distance_m()
                .and_then(|metres| i32::try_from(metres).ok()),
        })
    }

    /// How long until the next tick: a frame's worth of time when there is a
    /// picture to send on a schedule and one has been agreed, otherwise the
    /// heartbeat's.
    fn period(&self) -> Duration {
        let rate = self.screen_in_force.map(|screen| screen.frame_rate);
        match (&self.source, self.started_at, rate) {
            (Source::Cycle { .. }, Some(_), Some(fps)) if fps > 0 => {
                Duration::from_millis(1000 / fps as u64)
            }
            _ => HEARTBEAT,
        }
    }

    /// One message out on the command channel.
    async fn send(&mut self, service_type: u32, body: &[u8]) -> Result<()> {
        self.send_frame(service_type, 0, body).await
    }

    /// One message out, on the channel its number names.
    ///
    /// The channel is the number's top half-word and is written from it rather
    /// than passed separately, so the two cannot disagree.
    ///
    /// A body larger than the channel allows is dropped here rather than
    /// written. The car's own reader refuses such a message, and refusing it
    /// means the length no longer matches what is on the wire: the whole channel
    /// goes with it. Dropping it costs one message; writing it costs the
    /// session.
    async fn send_frame(&mut self, service_type: u32, timestamp_ms: u32, body: &[u8]) -> Result<()> {
        let channel = service_type >> 16;
        let cap = frame::max_body(channel);
        if body.len() > cap as usize {
            tracing::warn!(
                "cl.message_dropped type={service_type:#010x} bytes={} cap_bytes={cap}",
                body.len()
            );
            return Ok(());
        }
        let message = frame::encode(channel, service_type, timestamp_ms, body);
        frame::write_chunk(&mut self.writer, channel, &message)
            .await
            .with_context(|| format!("write {service_type:#010x}"))
    }
}

/// The fix as it is compared between reports: the clock fields cleared, because
/// a car that has not moved reports the same position with a later timestamp,
/// and only a change in what it says about the position is worth a line.
fn untimed(fix: proto::CarGps) -> proto::CarGps {
    proto::CarGps {
        year: 0,
        month: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        timestamp_ms: 0,
        ..fix
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::{AudioCodec, MediaSink, StreamEvent, StreamId};

    /// One stream: a type the phone names it with, and the instance it opened.
    fn stream(stream_type: u8, connection_id: i64) -> StreamId {
        StreamId {
            stream_type,
            connection_id,
        }
    }
    use std::sync::{Arc, Mutex};
    use tokio::io::DuplexStream;

    /// The other end of the pipe: enough of a car to walk the conversation, and
    /// no more. Everything it sends is what the reference sends.
    struct Car {
        pipe: DuplexStream,
    }

    impl Car {
        async fn send(&mut self, service_type: u32, body: &[u8]) {
            let channel = service_type >> 16;
            let message = frame::encode(channel, service_type, 0, body);
            frame::write_chunk(&mut self.pipe, channel, &message)
                .await
                .expect("the car can write");
        }

        /// The next message, which is expected to be the one named.
        async fn expect(&mut self, service_type: u32) -> frame::Message {
            let deadline = Duration::from_secs(2);
            let chunk = tokio::time::timeout(deadline, frame::read_chunk(&mut self.pipe))
                .await
                .expect("something arrived before the deadline")
                .expect("the pipe is intact")
                .expect("the session is still there");
            let message = frame::decode(chunk.channel, &chunk.body).expect("it decodes");
            assert_eq!(
                message.service_type, service_type,
                "expected {service_type:#010x}"
            );
            message
        }

        /// Everything the car asks before it will start the encoder, answering
        /// on `channel`, with `served` expected back — which is what it asked
        /// for unless the channel table preconfigures the screen.
        async fn handshake_on(
            &mut self,
            channel: &str,
            asked: proto::VideoEncoderInfo,
            served: proto::VideoEncoderInfo,
        ) {
            let mut version = Vec::new();
            proto::put_int(&mut version, 1, 1);
            proto::put_int(&mut version, 2, 0);
            self.send(HU_PROTOCOL_VERSION, &version).await;
            self.expect(MATCH_STATUS).await;

            self.send(HU_INFO, &[]).await;
            self.expect(MD_INFO).await;

            self.send(HU_STATISTIC_INFO, &statistics(channel)).await;
            self.expect(MD_AUTHEN_RESULT).await;
            self.expect(FEATURE_CONFIG_REQUEST).await;
            self.expect(CAR_DATA_SUBSCRIBE).await;

            self.send(HU_VIDEO_ENCODER_INIT, &proto::video_encoder_info(asked))
                .await;
            let answer = self.expect(VIDEO_ENCODER_INIT_DONE).await;
            assert_eq!(proto::decode_video_encoder_info(&answer.body), served);
        }

        async fn handshake(&mut self, asked: proto::VideoEncoderInfo) {
            self.handshake_on("20062100", asked, asked).await;
        }

        async fn start_the_encoder(&mut self) {
            self.send(HU_VIDEO_ENCODER_START, &[]).await;
        }

        /// The car asking which data this device can provide, and the answer it
        /// gets back.
        async fn ask(&mut self, modules: &[i32]) -> frame::Message {
            self.send(HU_DATA_SUBSCRIBE, &module_list(modules)).await;
            self.expect(DATA_SUBSCRIBE_DONE).await
        }

        /// The car asking whether this device can tell it about a route, and
        /// then asking to be told.
        async fn subscribe(&mut self, modules: &[i32]) {
            self.ask(modules).await;
            self.send(HU_DATA_SUBSCRIBE_START, &module_list(modules)).await;
        }

        /// Nothing at all on the pipe for this long.
        async fn expect_silence(&mut self, quiet_for: Duration) {
            let heard = tokio::time::timeout(quiet_for, frame::read_chunk(&mut self.pipe)).await;
            assert!(heard.is_err(), "the session spoke when it should not have");
        }
    }

    /// What the car sends when it asks about a module, or asks to be sent it: a
    /// count and that many `{moduleID}` entries.
    fn module_list(modules: &[i32]) -> Vec<u8> {
        let mut out = Vec::new();
        proto::put_int(&mut out, 1, modules.len() as i32);
        for module in modules {
            let mut entry = Vec::new();
            proto::put_int(&mut entry, 1, *module);
            proto::put_bytes(&mut out, 2, &entry);
        }
        out
    }

    fn asked() -> proto::VideoEncoderInfo {
        proto::VideoEncoderInfo {
            width: 1280,
            height: 720,
            frame_rate: 30,
        }
    }

    /// The size the channel table preconfigures for `20062104`, which is what
    /// that number is served instead of what it asks for.
    fn preset() -> proto::VideoEncoderInfo {
        proto::VideoEncoderInfo {
            width: 1920,
            height: 720,
            frame_rate: 30,
        }
    }

    /// What the car answers when asked what vehicle data it has: a count and
    /// that many `{moduleID, flag, frequency}` entries.
    fn vehicle_list(modules: &[(i32, i32)]) -> Vec<u8> {
        let mut out = Vec::new();
        proto::put_int(&mut out, 1, modules.len() as i32);
        for (module, flag) in modules {
            let mut entry = Vec::new();
            proto::put_int(&mut entry, 1, *module);
            proto::put_int(&mut entry, 2, *flag);
            proto::put_int(&mut entry, 3, 0);
            proto::put_bytes(&mut out, 2, &entry);
        }
        out
    }

    /// A fix at 30.317880 N, 120.128094 E, 45.6 m up and moving at 12.34 km/h.
    fn position() -> Vec<u8> {
        position_with(1)
    }

    /// The same report with the antenna state given: `-1` is a receiver that has
    /// lost its satellites, and it keeps reporting.
    fn position_with(antenna_state: i32) -> Vec<u8> {
        let mut body = Vec::new();
        proto::put_int(&mut body, 1, antenna_state);
        proto::put_int(&mut body, 3, 30_317_880);
        proto::put_int(&mut body, 4, 120_128_094);
        proto::put_int(&mut body, 5, 456);
        proto::put_int(&mut body, 6, 1234);
        proto::put_int(&mut body, 7, 900);
        proto::put_int(&mut body, 8, 2026);
        proto::put_int(&mut body, 9, 10);
        proto::put_int(&mut body, 10, 20);
        proto::put_int(&mut body, 11, 14);
        proto::put_int(&mut body, 12, 33);
        proto::put_int(&mut body, 13, 57);
        proto::put_int(&mut body, 20, 35);
        proto::put_int(&mut body, 21, 20);
        body
    }

    /// Where `/info` reads the channel number from. A test that is about
    /// something else leaves it empty, which is what a car that has not reported
    /// one looks like.
    fn no_icon() -> SharedChannel {
        Arc::new(Mutex::new(None))
    }

    /// The signal a tap on the phone's icon arrives on, for a test that does not
    /// tap.
    fn no_tap() -> broadcast::Sender<()> {
        broadcast::channel(1).0
    }

    /// The car's statistics, which is where the channel number it is judged by
    /// arrives.
    fn statistics(channel: &str) -> Vec<u8> {
        let mut body = Vec::new();
        proto::put_str(&mut body, 2, "7.0.0");
        proto::put_str(&mut body, 4, channel);
        body
    }

    /// What a phone makes of a car that names this channel.
    async fn verdict(channel: String) -> bool {
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_STATISTIC_INFO, &statistics(&channel)).await;
            let answer = car.expect(MD_AUTHEN_RESULT).await;
            proto::decode_authen_result(&answer.body).expect("a boolean")
        });

        run(
            device,
            &Frames::None,
            &Config::from_env(),
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap()
    }

    /// A tap on the phone's icon reaches the car as `MSG_CMD_GO_TO_DESKTOP`, with
    /// no body: on the phone that icon stands for the car's own interface, so
    /// the car is asked to show it.
    #[tokio::test]
    async fn a_tap_on_our_icon_asks_the_car_for_its_desktop() {
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let (tap, _) = broadcast::channel(1);
        let tapping = tap.clone();
        let mut carplay = |_: bool| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(asked()).await;
            tapping.send(()).expect("the session is listening");
            car.expect(GO_TO_DESKTOP).await
        });

        run(
            device,
            &Frames::None,
            &Config::from_env(),
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &tap,
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");

        let message = car.await.expect("the car saw it");
        assert!(message.body.is_empty(), "the command carries no body");
    }

    #[tokio::test]
    async fn the_cars_channel_number_is_what_it_is_judged_by() {
        // A number out of the app's own table of 64.
        assert!(verdict("20062100".into()).await);
        // And one that is not in it at all, which the app serves all the same:
        // the rule is a shape, not a list.
        assert!(verdict("21350001".into()).await);
        // The two the app refuses outright.
        assert!(!verdict("20000000".into()).await);
        assert!(!verdict("20000001".into()).await);
        // Anything that is not eight digits starting with a two.
        assert!(!verdict(String::new()).await);
        assert!(!verdict("10011100".into()).await);
    }

    /// A key of this test's own, in the shape `run` is handed one. Not the real
    /// key: what is under test is the road the challenge travels.
    fn a_key_of_its_own() -> (String, rsa::RsaPrivateKey) {
        use rsa::pkcs8::EncodePrivateKey;
        let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 1024).expect("a key");
        let pem = key
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .expect("the key writes as PEM")
            .to_string();
        (pem, key)
    }

    /// The car's challenge is answered with the key the session was given, and
    /// the answer is one the car's own check takes.
    #[tokio::test]
    async fn the_cars_challenge_is_answered_with_the_key_that_was_put_in() {
        use base64::engine::general_purpose::STANDARD as BASE64;
        use base64::Engine as _;

        let (pem, key) = a_key_of_its_own();
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let challenge = "9f2c1a_4.6.0";

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            let mut body = Vec::new();
            proto::put_str(&mut body, 1, challenge);
            car.send(HU_AUTHEN_REQUEST, &body).await;

            let answer = car.expect(MD_AUTHEN_RESPONSE).await;
            // The car reads one string field out of it, which is where the
            // answer is.
            let Some(proto::Field::Bytes { number: 1, value }) =
                proto::Fields::new(&answer.body).next_field()
            else {
                panic!("the answer is not the string field it should be");
            };
            let sealed = BASE64.decode(value).expect("Base64");
            assert_eq!(sealed.len(), 128, "one block of the key the car was given");
            // What the car does with it: the public half, and a comparison with
            // the challenge it sent.
            key.to_public_key()
                .verify(
                    rsa::pkcs1v15::Pkcs1v15Sign::new_unprefixed(),
                    challenge.as_bytes(),
                    &sealed,
                )
                .expect("the car's check passes");
        });

        run(
            device,
            &Frames::None,
            &cfg,
            auth::Signer::from_pem(&pem),
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    #[tokio::test]
    async fn the_handshake_is_answered_and_the_session_is_reported_up_then_down() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let switches = Arc::new(Mutex::new(Vec::new()));
        let seen = switches.clone();
        let mut carplay = move |up: bool| seen.lock().unwrap().push(up);

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(asked()).await;
            car.start_the_encoder().await;
            car.expect(VIDEO_HEARTBEAT).await;
            // Dropping the pipe is the car going away.
        });

        run(
            device,
            &Frames::None,
            &cfg,
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();

        assert_eq!(*switches.lock().unwrap(), vec![true, false]);
    }

    /// The car's own screen is what the other half is told to render at: no one
    /// else knows how big it is, so it is the car that decides.
    #[tokio::test]
    async fn the_cars_screen_is_reported_to_the_other_half() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        // Deliberately unlike the fallback, so a report of the fallback would
        // not pass.
        let car_screen = proto::VideoEncoderInfo {
            width: 1920,
            height: 480,
            frame_rate: 24,
        };

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(car_screen).await;
        });

        let screens = Arc::new(Mutex::new(Vec::new()));
        let seen = screens.clone();
        let mut screen = move |width: u32, height: u32, fps: u32| {
            seen.lock().unwrap().push((width, height, fps));
        };
        run(
            device,
            &Frames::None,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();

        assert_eq!(*screens.lock().unwrap(), vec![(1920, 480, 24)]);
    }

    /// A number the channel table preconfigures is answered with the table's
    /// size, not with the one the car asked for: the car's own application is
    /// what got its screen wrong, so its idea of the screen is what loses. The
    /// table's numbers are stored in place of the car's, which is why the size the
    /// other half is told — and the size a touch is measured in — needs no rule of
    /// its own.
    #[tokio::test]
    async fn a_preconfigured_number_is_served_the_size_the_table_names() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake_on("20062104", asked(), preset()).await;
        });

        let screens = Arc::new(Mutex::new(Vec::new()));
        let seen = screens.clone();
        let mut screen = move |width: u32, height: u32, fps: u32| {
            seen.lock().unwrap().push((width, height, fps));
        };
        let status = Arc::new(Status::new());
        run(
            device,
            &Frames::None,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            status.clone(),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.expect("the car saw its answer");

        // The phone draws the table's size, so that is the size it is told — and
        // this callback is what `/info` reads, so it is also the size the phone
        // is asked for.
        assert_eq!(*screens.lock().unwrap(), vec![(1920, 720, 30)]);
        status.with(|s| {
            // One size, and it is the table's: the car's own numbers were
            // overwritten where they were stored, so this is all there is to read.
            let agreed = s.carlife.agreed.expect("the answer is on the page");
            assert_eq!((agreed.width, agreed.height, agreed.fps), (1920, 720, 30));
            let asked = s.carlife.requested.expect("what the car asked for is kept");
            assert_eq!((asked.width, asked.height), (1280, 720));
        });
    }

    /// A preset says what the screen is, and the car is answered with it, so its
    /// touches are in that size already: a point near the right edge stays there
    /// rather than being scaled up from the size the car claimed.
    #[test]
    fn a_touch_is_taken_as_it_arrives_on_the_size_in_force() {
        assert_eq!(clamped(1900, 700, (1920, 720)), (1900, 700));
        assert_eq!(clamped(1920, 720, (1920, 720)), (1919, 719));
        assert_eq!(clamped(-5, -5, (1920, 720)), (0, 0));
        assert_eq!(
            clamped(640, 360, (1280, 720)),
            (640, 360),
            "a car the table says nothing about is untouched"
        );
    }

    /// A rate above 30 is the car's to ask for. Nothing here scales or re-times
    /// anything, so agreeing to less than was asked hands the car a picture at
    /// half the rate its own panel was built around. `Car::handshake` is what
    /// holds this: it compares the answer with the request field by field.
    #[tokio::test]
    async fn a_frame_rate_above_thirty_is_agreed_to_rather_than_cut_down() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(proto::VideoEncoderInfo {
                width: 1280,
                height: 720,
                frame_rate: 60,
            })
            .await;
        });

        run(
            device,
            &Frames::None,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// The media channel is told what its samples are before it is given any,
    /// and then the sound follows on the channel the format named. A stream that
    /// opens with the format the car is already holding is not worth a second
    /// message, and the phone's own voice streams are not the car's music.
    #[tokio::test]
    async fn the_media_channel_is_told_the_format_before_it_is_given_any_sound() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            // The music: the stream opens, and then there is sound on it.
            sink.push(StreamEvent::AudioConfig {
                stream: stream(102, 1),
                codec: AudioCodec::AacLc {
                    rate: 44_100,
                    channels: 2,
                },
                name: "media".into(),
            });
            let init = car.expect(MEDIA_INIT).await;
            assert_eq!(init.body, proto::media_init(44_100, 2));

            sink.push(StreamEvent::AudioPacket {
                stream: stream(102, 1),
                timestamp_us: 1_234_000,
                data: vec![0x11, 0x22, 0x33, 0x44],
            });
            let sound = car.expect(MEDIA_DATA).await;
            assert_eq!(sound.body, vec![0x11, 0x22, 0x33, 0x44]);
            assert_eq!(sound.timestamp_ms, Some(1_234));

            // The next track opens a stream of its own, in the format the car is
            // already holding: nothing to say, and the sound goes on.
            sink.push(StreamEvent::AudioConfig {
                stream: stream(102, 2),
                codec: AudioCodec::AacLc {
                    rate: 44_100,
                    channels: 2,
                },
                name: "media".into(),
            });
            sink.push(StreamEvent::AudioPacket {
                stream: stream(102, 2),
                timestamp_us: 1_300_000,
                data: vec![0x55, 0x66],
            });
            assert_eq!(car.expect(MEDIA_DATA).await.body, vec![0x55, 0x66]);

            // A call is one of the car's own channels, not this one: what comes
            // out next is the music, with nothing between them.
            sink.push(StreamEvent::AudioConfig {
                stream: stream(100, 3),
                codec: AudioCodec::Opus {
                    rate: 48_000,
                    channels: 1,
                },
                name: "telephony".into(),
            });
            sink.push(StreamEvent::AudioPacket {
                stream: stream(100, 3),
                timestamp_us: 1_400_000,
                data: vec![0x77],
            });
            sink.push(StreamEvent::AudioPacket {
                stream: stream(102, 2),
                timestamp_us: 1_500_000,
                data: vec![0x88],
            });
            assert_eq!(car.expect(MEDIA_DATA).await.body, vec![0x88]);

            // The phone stopped playing, and the car is told rather than left
            // holding a player open over nothing.
            sink.push(StreamEvent::AudioStopped {
                stream: stream(102, 2),
            });
            car.expect(MEDIA_STOP).await;
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// The prompt goes out on the car's channel for speech, not through the
    /// music's: a player set up with the format its first frame arrives in, the
    /// frames themselves, and an end that leaves the music's channel alone.
    #[tokio::test]
    async fn the_prompt_is_announced_and_ended_on_its_own_channel() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            // The stream opens. A player set up over silence is one the car
            // holds open with nothing in it, so nothing is said yet.
            sink.push(StreamEvent::AudioConfig {
                stream: stream(100, 5),
                codec: AudioCodec::Pcm {
                    rate: 16_000,
                    channels: 1,
                },
                name: "default".into(),
            });
            car.expect_silence(Duration::from_millis(200)).await;

            // The first frame says both things: what the samples are, and then
            // the samples.
            sink.push(StreamEvent::AudioPacket {
                stream: stream(100, 5),
                timestamp_us: 2_000_000,
                data: vec![0xaa, 0xbb],
            });
            let init = car.expect(NAVI_TTS_INIT).await;
            assert_eq!(init.body, proto::media_init(16_000, 1));
            let said = car.expect(NAVI_TTS_DATA).await;
            assert_eq!(said.body, vec![0xaa, 0xbb]);
            assert_eq!(said.timestamp_ms, Some(2_000));

            // The next frame is more of the same player, and says nothing about
            // the format.
            sink.push(StreamEvent::AudioPacket {
                stream: stream(100, 5),
                timestamp_us: 2_020_000,
                data: vec![0xcc],
            });
            assert_eq!(car.expect(NAVI_TTS_DATA).await.body, vec![0xcc]);

            // The phone says the prompt is over.
            sink.push(StreamEvent::AudioStopped {
                stream: stream(100, 5),
            });
            car.expect(NAVI_TTS_END).await;
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// The phone often leaves a prompt's stream open after the last word, and a
    /// head unit that took the channel up keeps the music down until it is told
    /// the prompt is over. So silence ends it — once, and no more.
    #[tokio::test]
    async fn a_prompt_that_goes_quiet_is_ended_for_it() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            sink.push(StreamEvent::AudioConfig {
                stream: stream(100, 6),
                codec: AudioCodec::Pcm {
                    rate: 16_000,
                    channels: 1,
                },
                name: "alert".into(),
            });
            sink.push(StreamEvent::AudioPacket {
                stream: stream(100, 6),
                timestamp_us: 3_000_000,
                data: vec![0x01],
            });
            car.expect(NAVI_TTS_INIT).await;
            car.expect(NAVI_TTS_DATA).await;

            // Nothing more is said, and the phone does not close the stream. The
            // end arrives anyway, inside the deadline the test allows for it.
            car.expect(NAVI_TTS_END).await;
            // And it arrives once: a second one would be closing a player the
            // car no longer has.
            car.expect_silence(TTS_IDLE * 2).await;

            // The same stream speaking again is a new player, announced with the
            // format it was last given.
            sink.push(StreamEvent::AudioPacket {
                stream: stream(100, 6),
                timestamp_us: 4_000_000,
                data: vec![0x02],
            });
            assert_eq!(
                car.expect(NAVI_TTS_INIT).await.body,
                proto::media_init(16_000, 1)
            );
            assert_eq!(car.expect(NAVI_TTS_DATA).await.body, vec![0x02]);
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// The panel is told the song, not the phone's way of describing it: what
    /// arrives as one attribute per update goes out as one message per change,
    /// with the cover art joined onto the song it belongs to when it lands.
    #[tokio::test]
    async fn the_cars_panel_is_told_the_song_that_is_playing() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            // The phone names the song, and nothing else about it yet.
            sink.playing(Playing::Track(Track {
                song: Some("Reeperbahn".into()),
                ..Track::default()
            }));
            let named = car.expect(MEDIA_INFO).await;
            assert_eq!(
                named.body,
                proto::media_info(
                    &Track {
                        song: Some("Reeperbahn".into()),
                        ..Track::default()
                    },
                    None
                )
            );

            // Then the artist, which is the same song described further.
            sink.playing(Playing::Track(Track {
                artist: Some("Hamburg".into()),
                ..Track::default()
            }));
            let described = car.expect(MEDIA_INFO).await;
            assert_eq!(
                described.body,
                proto::media_info(
                    &Track {
                        song: Some("Reeperbahn".into()),
                        artist: Some("Hamburg".into()),
                        ..Track::default()
                    },
                    None
                )
            );

            // The cover arrives as a file of its own, after the song it is of.
            sink.playing(Playing::Artwork(vec![0xff, 0xd8, 0xff]));
            let covered = car.expect(MEDIA_INFO).await;
            assert_eq!(
                covered.body,
                proto::media_info(
                    &Track {
                        song: Some("Reeperbahn".into()),
                        artist: Some("Hamburg".into()),
                        ..Track::default()
                    },
                    Some(&[0xff, 0xd8, 0xff])
                )
            );

            // The same song again is not worth a fourth message.
            for _ in 0..2 {
                sink.playing(Playing::Track(Track {
                    song: Some("Reeperbahn".into()),
                    ..Track::default()
                }));
            }
            car.expect_silence(Duration::from_millis(300)).await;
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// A cover that arrives before the song it belongs to is kept for it, not
    /// thrown away when the song turns up.
    #[tokio::test]
    async fn a_cover_that_arrives_before_the_song_is_kept_for_it() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            // The cover arrives first, with no song to attach it to yet.
            sink.playing(Playing::Artwork(vec![0xff, 0xd8, 0xff]));
            car.expect_silence(Duration::from_millis(300)).await;

            // Then the song: the cover is still there, and the message carries
            // both.
            sink.playing(Playing::Track(Track {
                song: Some("Reeperbahn".into()),
                ..Track::default()
            }));
            let covered = car.expect(MEDIA_INFO).await;
            assert_eq!(
                covered.body,
                proto::media_info(
                    &Track {
                        song: Some("Reeperbahn".into()),
                        ..Track::default()
                    },
                    Some(&[0xff, 0xd8, 0xff])
                )
            );
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// A title that changes without a length is not another song but the same
    /// one, renamed: an app that puts its lyrics in the title sends one per
    /// line, and none of them is a song of its own. The cover stays, because
    /// the song it belongs to has not gone anywhere.
    #[tokio::test]
    async fn a_line_of_lyrics_keeps_the_cover() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            let reeperbahn = Track {
                song: Some("Reeperbahn".into()),
                artist: Some("Hamburg".into()),
                album: Some("Beatles".into()),
                duration_s: Some(180),
                ..Track::default()
            };

            sink.playing(Playing::Track(reeperbahn.clone()));
            car.expect(MEDIA_INFO).await;
            sink.playing(Playing::Artwork(vec![0xff, 0xd8]));
            let covered = car.expect(MEDIA_INFO).await;
            assert_eq!(
                covered.body,
                proto::media_info(&reeperbahn, Some(&[0xff, 0xd8]))
            );

            // The next line of the lyrics: a title, and nothing else said.
            sink.playing(Playing::Track(Track {
                song: Some("Reeperbahn — line two".into()),
                ..Track::default()
            }));
            let told = car.expect(MEDIA_INFO).await;
            let renamed = Track {
                song: Some("Reeperbahn — line two".into()),
                ..reeperbahn.clone()
            };
            assert_eq!(
                told.body,
                proto::media_info(&renamed, Some(&[0xff, 0xd8]))
            );
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// A file transfer that carried nothing is the phone saying the song has no
    /// picture. It is kept as the cover that is held, so the one before it goes
    /// down with it — and the message that follows carries no `albumArt` at all
    /// rather than a field with nothing in it to draw.
    #[tokio::test]
    async fn an_empty_cover_puts_the_last_one_down() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let song = Track {
            song: Some("Reeperbahn".into()),
            ..Track::default()
        };

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            sink.playing(Playing::Track(song.clone()));
            car.expect(MEDIA_INFO).await;

            // A picture, which the car is given.
            let art = vec![0xff, 0xd8, 0xff];
            sink.playing(Playing::Artwork(art.clone()));
            let told = car.expect(MEDIA_INFO).await;
            assert_eq!(told.body, proto::media_info(&song, Some(&art)));

            // And the phone saying there is none takes it back.
            sink.playing(Playing::Artwork(Vec::new()));
            let told = car.expect(MEDIA_INFO).await;
            assert_eq!(told.body, proto::media_info(&song, None));
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// A length is what says a song has started, and the cover that is held
    /// belongs to the one before it. A file transfer names no song, so the new
    /// one is sent without a cover rather than wearing the last one's — and the
    /// page puts its picture down with it.
    #[tokio::test]
    async fn a_song_with_a_length_puts_the_last_cover_down() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let first = Track {
            song: Some("Reeperbahn".into()),
            ..Track::default()
        };
        let next = Track {
            song: Some("So What".into()),
            duration_s: Some(545),
            ..Track::default()
        };

        let status = Arc::new(Status::new());
        let page = status.clone();
        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            sink.playing(Playing::Track(first.clone()));
            car.expect(MEDIA_INFO).await;

            let art = vec![0xff, 0xd8, 0xff];
            sink.playing(Playing::Artwork(art.clone()));
            let told = car.expect(MEDIA_INFO).await;
            assert_eq!(told.body, proto::media_info(&first, Some(&art)));
            assert!(page.cover().is_some(), "the page has the picture");

            // The next song announces itself with its length.
            sink.playing(Playing::Track(next.clone()));
            let told = car.expect(MEDIA_INFO).await;
            assert_eq!(told.body, proto::media_info(&next, None));
            assert!(page.cover().is_none(), "and the page puts it down");

            // Its own picture then goes out with it, as any cover does.
            sink.playing(Playing::Artwork(vec![0x89, 0x50]));
            let told = car.expect(MEDIA_INFO).await;
            assert_eq!(told.body, proto::media_info(&next, Some(&[0x89, 0x50])));
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            status,
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// The app the phone is playing from is not a song. It is sent with every
    /// change of play state, and a panel emptied by one of those is worse than
    /// one the car never updated.
    #[tokio::test]
    async fn the_play_state_on_its_own_is_not_a_song() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            sink.playing(Playing::Track(Track {
                source: Some("Music".into()),
                ..Track::default()
            }));
            car.expect_silence(Duration::from_millis(300)).await;

            // And the song that follows carries the app with it.
            sink.playing(Playing::Track(Track {
                song: Some("Reeperbahn".into()),
                ..Track::default()
            }));
            let told = car.expect(MEDIA_INFO).await;
            assert_eq!(
                told.body,
                proto::media_info(
                    &Track {
                        source: Some("Music".into()),
                        song: Some("Reeperbahn".into()),
                        ..Track::default()
                    },
                    None
                )
            );
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// The phone describes a song one attribute at a time, and each of those is
    /// sent as it arrives: the car's message is the whole song, so the panel is
    /// told the song as it stands rather than waiting for the phone to finish
    /// describing it. It redraws once per attribute, which is what the phone's
    /// own panel does with the same messages.
    #[tokio::test]
    async fn every_word_from_the_phone_goes_out_at_once() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            // The title, and nothing else yet.
            sink.playing(Playing::Track(Track {
                song: Some("Reeperbahn".into()),
                ..Track::default()
            }));
            let told = car.expect(MEDIA_INFO).await;
            assert_eq!(
                told.body,
                proto::media_info(
                    &Track {
                        song: Some("Reeperbahn".into()),
                        ..Track::default()
                    },
                    None
                )
            );

            // The artist, which goes out with the title already known.
            sink.playing(Playing::Track(Track {
                artist: Some("Hamburg".into()),
                ..Track::default()
            }));
            let told = car.expect(MEDIA_INFO).await;
            assert_eq!(
                told.body,
                proto::media_info(
                    &Track {
                        song: Some("Reeperbahn".into()),
                        artist: Some("Hamburg".into()),
                        ..Track::default()
                    },
                    None
                )
            );

            // And the album, which is the last of it.
            sink.playing(Playing::Track(Track {
                album: Some("Stadt".into()),
                ..Track::default()
            }));
            let told = car.expect(MEDIA_INFO).await;
            assert_eq!(
                told.body,
                proto::media_info(
                    &Track {
                        song: Some("Reeperbahn".into()),
                        artist: Some("Hamburg".into()),
                        album: Some("Stadt".into()),
                        ..Track::default()
                    },
                    None
                )
            );
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// A cover the car's message cannot carry is left out, and the song is not:
    /// the panel loses the picture, not the song.
    #[tokio::test]
    async fn a_cover_too_large_for_the_cars_message_is_left_out() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            let track = Track {
                song: Some("Reeperbahn".into()),
                ..Track::default()
            };

            // One byte past what the message may carry.
            sink.playing(Playing::Artwork(vec![0xff; proto::MAX_ART + 1]));
            sink.playing(Playing::Track(track.clone()));
            let told = car.expect(MEDIA_INFO).await;
            assert_eq!(told.body, proto::media_info(&track, None));

            // And one that fits exactly is the car's to draw.
            let fits = vec![0x89; proto::MAX_ART];
            sink.playing(Playing::Artwork(fits.clone()));
            let covered = car.expect(MEDIA_INFO).await;
            assert_eq!(covered.body, proto::media_info(&track, Some(&fits)));
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// A body past what its channel allows is dropped rather than written. The
    /// car refuses such a message, and a refused length leaves the channel
    /// waiting for bytes that will never come: one lost message is the cheaper
    /// of the two.
    #[tokio::test]
    async fn a_message_larger_than_its_channel_allows_is_dropped() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            sink.push(StreamEvent::AudioConfig {
                stream: stream(102, 1),
                codec: AudioCodec::AacLc {
                    rate: 48_000,
                    channels: 2,
                },
                name: "media".into(),
            });
            car.expect(MEDIA_INIT).await;

            // One byte past the media channel's body cap.
            sink.push(StreamEvent::AudioPacket {
                stream: stream(102, 1),
                timestamp_us: 1_000_000,
                data: vec![0x11; 100 * 1024 + 1],
            });
            car.expect_silence(Duration::from_millis(300)).await;

            // And the channel is still there, with nothing left over on it.
            sink.push(StreamEvent::AudioPacket {
                stream: stream(102, 1),
                timestamp_us: 1_100_000,
                data: vec![0x22],
            });
            let sound = car.expect(MEDIA_DATA).await;
            assert_eq!(sound.body, vec![0x22]);
            assert_eq!(sound.timestamp_ms, Some(1_100));
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// The strip is the whole point of the subscription. The phone's two
    /// route-guidance messages say nothing about a strip and the car's two say
    /// nothing about a route, so this is the join between them — and what the
    /// car is told changes exactly when the strip it would draw does.
    #[tokio::test]
    async fn the_cars_strip_is_told_the_turn_the_phone_is_guiding_to() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;
            car.subscribe(&[proto::DATA_TURN_BY_TURN]).await;

            // The phone describes a turn before it says which one is current,
            // and until it says, there is no strip to draw.
            sink.guidance(Guidance::about_a_turn(1, Some(1), Some("Elbchaussee")));
            car.expect_silence(Duration::from_millis(300)).await;

            // Now the route: a left turn, 350 m away.
            sink.guidance(Guidance::about_the_route(Some(1), Some(350), Some(vec![1])));
            let state = car.expect(MODULE_STATUS).await;
            assert_eq!(
                state.body,
                proto::module_status(proto::NAVI_MODULE, proto::NAVI_START)
            );
            let shown = car.expect(NAVI_NEXT_TURN_INFO).await;
            assert_eq!(
                shown.body,
                proto::navi_next_turn_info(
                    proto::ACTION_SHOW,
                    &proto::NextTurn {
                        kind: Some(0x07),
                        road: Some("Elbchaussee".into()),
                        remain_m: Some(350),
                    }
                )
            );

            // The same turn, closer: an update and not a second show.
            sink.guidance(Guidance::about_the_route(Some(1), Some(120), None));
            let updated = car.expect(NAVI_NEXT_TURN_INFO).await;
            assert_eq!(
                updated.body,
                proto::navi_next_turn_info(
                    proto::ACTION_UPDATE,
                    &proto::NextTurn {
                        kind: Some(0x07),
                        road: Some("Elbchaussee".into()),
                        remain_m: Some(120),
                    }
                )
            );

            // And the same turn again is not worth a third message.
            sink.guidance(Guidance::about_the_route(None, Some(120), None));
            car.expect_silence(Duration::from_millis(300)).await;

            // The route is over: the module is idle and the strip comes down.
            sink.guidance(Guidance::about_the_route(Some(0), None, None));
            let idle = car.expect(MODULE_STATUS).await;
            assert_eq!(
                idle.body,
                proto::module_status(proto::NAVI_MODULE, proto::NAVI_IDLE)
            );
            let hidden = car.expect(NAVI_NEXT_TURN_INFO).await;
            assert_eq!(
                hidden.body,
                proto::navi_next_turn_info(proto::ACTION_HIDE, &proto::NextTurn::default())
            );
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// The strip is behind the car's subscription; the navigation module's own
    /// state is not. The first is data the car asked to be told, and the second
    /// is the phone saying something about itself, which the car hears either
    /// way — and a car with no strip to fill is a car with nowhere to put one.
    #[tokio::test]
    async fn the_strip_is_not_sent_until_the_car_asks_for_it() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            // A whole route, described and being guided, with nobody having
            // asked: a right turn onto the B27, 90 m away.
            sink.guidance(Guidance::about_a_turn(1, Some(2), Some("B27")));
            sink.guidance(Guidance::about_the_route(Some(1), Some(90), Some(vec![1])));

            let state = car.expect(MODULE_STATUS).await;
            assert_eq!(
                state.body,
                proto::module_status(proto::NAVI_MODULE, proto::NAVI_START)
            );
            car.expect_silence(Duration::from_millis(300)).await;

            // And the moment the car asks, the turn it is already on.
            car.subscribe(&[proto::DATA_TURN_BY_TURN]).await;
            let shown = car.expect(NAVI_NEXT_TURN_INFO).await;
            assert_eq!(
                shown.body,
                proto::navi_next_turn_info(
                    proto::ACTION_SHOW,
                    &proto::NextTurn {
                        kind: Some(0x03),
                        road: Some("B27".into()),
                        remain_m: Some(90),
                    }
                )
            );
            // The state it was already told is not said twice.
            car.expect_silence(Duration::from_millis(300)).await;
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// The car's question is about what this device can do, so it is answered
    /// per module — including the one there is nothing behind, which is not a
    /// thing to claim.
    #[tokio::test]
    async fn the_cars_question_about_what_this_device_can_provide_is_answered() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            // The two the guide names, in the order it names them: the simple
            // guidance is what this device produces, the assistant guide is not.
            let answer = car.ask(&[proto::DATA_TURN_BY_TURN, 1]).await;
            assert_eq!(
                answer.body,
                proto::data_subscribe_done(&[(proto::DATA_TURN_BY_TURN, true), (1, false)])
            );

            // And a module neither side has a name for is not one to claim
            // either.
            let answer = car.ask(&[7]).await;
            assert_eq!(answer.body, proto::data_subscribe_done(&[(7, false)]));
        });

        run(
            device,
            &Frames::None,
            &cfg,
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// The car's position is asked for as soon as the car has said what it is,
    /// and not when something starts being guided: the answer is a property of
    /// the car, and the fix is worth having either way.
    #[tokio::test]
    async fn the_cars_position_is_asked_for_and_reported() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let status = Arc::new(Status::new());
        let reported = status.clone();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;

            // The question goes out next to the feature-config request, which is
            // where a phone sends it.
            car.send(HU_STATISTIC_INFO, &statistics("20062100")).await;
            car.expect(MD_AUTHEN_RESULT).await;
            car.expect(FEATURE_CONFIG_REQUEST).await;
            car.expect(CAR_DATA_SUBSCRIBE).await;

            // A car with a GPS in WGS84, and one module it cannot report.
            car.send(HU_CAR_DATA_SUBSCRIBE_DONE, &vehicle_list(&[(0, 1), (1, 0)]))
                .await;
            let start = car.expect(CAR_DATA_SUBSCRIBE_START).await;
            assert_eq!(
                start.body,
                proto::car_data_subscribe_start(proto::CAR_DATA_GPS, 1, 0)
            );

            car.send(HU_CAR_GPS, &position()).await;
            // A message the session answers, so that the position is known to
            // have been read by the time the page is looked at.
            car.send(HU_GO_TO_FOREGROUND, &[]).await;
            car.expect(GO_TO_FOREGROUND_RESPONSE).await;

            let gps = reported.json()["carlife"]["gps"].clone();
            assert_eq!(gps["antenna_state"], 1);
            assert_eq!(gps["latitude"], 30_317_880);
            assert_eq!(gps["longitude"], 120_128_094);
            assert_eq!(gps["height"], 456);
            assert_eq!(gps["speed"], 1234);
            assert_eq!(gps["heading"], 900);
            assert_eq!(gps["hor_pos_error"], 35);
            assert_eq!(gps["vert_pos_error"], 20);
            assert_eq!(gps["year"], 2026);
            assert_eq!(gps["second"], 57);
        });

        run(
            device,
            &Frames::None,
            &cfg,
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            status.clone(),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();

        // A fix belongs to the car that reported it, so the page is not left
        // showing where the last one was.
        assert!(
            status.json()["carlife"]["gps"].is_null(),
            "the position goes when the car does"
        );
    }

    /// The page is not the fix's only reader: the phone takes the same position,
    /// as the two sentences it reads a map from, through the iAP2 half's own
    /// sending half — which is the one handed in here.
    #[tokio::test]
    async fn the_cars_position_is_handed_to_the_phone() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let vehicle = Arc::new(Vehicle::default());
        let feed = vehicle.feed();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;
            car.send(HU_STATISTIC_INFO, &statistics("20062100")).await;
            car.expect(MD_AUTHEN_RESULT).await;
            car.expect(FEATURE_CONFIG_REQUEST).await;
            car.expect(CAR_DATA_SUBSCRIBE).await;
            // A car whose GPS is in WGS84, so nothing about the fix is moved.
            car.send(HU_CAR_DATA_SUBSCRIBE_DONE, &vehicle_list(&[(0, 1)]))
                .await;
            car.expect(CAR_DATA_SUBSCRIBE_START).await;

            car.send(HU_CAR_GPS, &position()).await;
            // A message the session answers, so the fix is known to have been
            // read by the time the phone's half is looked at.
            car.send(HU_GO_TO_FOREGROUND, &[]).await;
            car.expect(GO_TO_FOREGROUND_RESPONSE).await;
        });

        run(
            device,
            &Frames::None,
            &cfg,
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            vehicle,
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();

        let said = feed.location.borrow().1.clone();
        assert!(said.starts_with("$GPGGA,"), "{said}");
        assert!(said.contains("\r\n$GPRMC,"), "{said}");
    }

    /// A car that loses its fix has not gone quiet, and the phone is not left to
    /// work that out: without being told, it keeps using the last position it
    /// was given for as long as it stays connected.
    #[tokio::test]
    async fn a_lost_fix_takes_the_position_back() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let vehicle = Arc::new(Vehicle::default());
        let feed = vehicle.feed();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;
            car.send(HU_STATISTIC_INFO, &statistics("20062100")).await;
            car.expect(MD_AUTHEN_RESULT).await;
            car.expect(FEATURE_CONFIG_REQUEST).await;
            car.expect(CAR_DATA_SUBSCRIBE).await;
            car.send(HU_CAR_DATA_SUBSCRIBE_DONE, &vehicle_list(&[(0, 1)]))
                .await;
            car.expect(CAR_DATA_SUBSCRIBE_START).await;

            car.send(HU_CAR_GPS, &position()).await;
            // The tunnel, or a garage: the receiver says it has nothing.
            car.send(HU_CAR_GPS, &position_with(-1)).await;
            car.send(HU_GO_TO_FOREGROUND, &[]).await;
            car.expect(GO_TO_FOREGROUND_RESPONSE).await;
        });

        run(
            device,
            &Frames::None,
            &cfg,
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            vehicle,
        )
        .await
        .expect("the session ends cleanly");
        car.await.unwrap();

        let said = feed.location.borrow().1.clone();
        assert!(said.contains(",0,00,"), "the fix quality is none: {said}");
        assert!(said.contains(",V,"), "and RMC says the same: {said}");
        assert!(!said.contains("3019.0728"), "and no position is left: {said}");
    }

    /// The car drives off and the position goes with it: the phone is still
    /// there, on a wireless link that outlives the car's.
    #[tokio::test]
    async fn the_position_goes_when_the_car_does() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let vehicle = Arc::new(Vehicle::default());
        let feed = vehicle.feed();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;
            car.send(HU_STATISTIC_INFO, &statistics("20062100")).await;
            car.expect(MD_AUTHEN_RESULT).await;
            car.expect(FEATURE_CONFIG_REQUEST).await;
            car.expect(CAR_DATA_SUBSCRIBE).await;
            car.send(HU_CAR_DATA_SUBSCRIBE_DONE, &vehicle_list(&[(0, 1)]))
                .await;
            car.expect(CAR_DATA_SUBSCRIBE_START).await;

            car.send(HU_CAR_GPS, &position()).await;
            // And then the cable: the pipe closes behind the fix.
        });

        run(
            device,
            &Frames::None,
            &cfg,
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            vehicle,
        )
        .await
        .expect("the session ends cleanly");
        car.await.unwrap();

        let said = feed.location.borrow().1.clone();
        assert!(said.contains(",0,00,"), "the position was taken back: {said}");
        assert!(!said.contains("3019.0728"), "and none is left: {said}");
    }

    /// A turn the two vocabularies have no word in common for goes out without
    /// one. The road and the distance are still true, and an arrow would not be.
    #[tokio::test]
    async fn a_turn_the_car_has_no_word_for_is_sent_without_one() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;
            car.subscribe(&[proto::DATA_TURN_BY_TURN]).await;

            // Leaving a motorway: iAP2's own type 8, which the car's list of
            // turns has no arrow for.
            sink.guidance(Guidance::about_a_turn(1, Some(8), Some("B27")));
            sink.guidance(Guidance::about_the_route(Some(1), Some(400), Some(vec![1])));
            car.expect(MODULE_STATUS).await;
            let shown = car.expect(NAVI_NEXT_TURN_INFO).await;
            assert_eq!(
                shown.body,
                proto::navi_next_turn_info(
                    proto::ACTION_SHOW,
                    &proto::NextTurn {
                        kind: None,
                        road: Some("B27".into()),
                        remain_m: Some(400),
                    }
                )
            );
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// A car that stops asking has the strip taken down — and is told nothing
    /// else, because the navigation it stopped being told about is still
    /// running.
    #[tokio::test]
    async fn the_strip_is_taken_down_when_the_car_stops_asking_for_it() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let mut screen = |_width: u32, _height: u32, _fps: u32| {};

        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());
        let sink = outlet.sink();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;
            car.subscribe(&[proto::DATA_TURN_BY_TURN]).await;

            sink.guidance(Guidance::about_a_turn(1, Some(2), Some("B27")));
            sink.guidance(Guidance::about_the_route(Some(1), Some(90), Some(vec![1])));
            car.expect(MODULE_STATUS).await;
            car.expect(NAVI_NEXT_TURN_INFO).await;

            car.send(
                HU_DATA_SUBSCRIBE_STOP,
                &module_list(&[proto::DATA_TURN_BY_TURN]),
            )
            .await;
            let hidden = car.expect(NAVI_NEXT_TURN_INFO).await;
            assert_eq!(
                hidden.body,
                proto::navi_next_turn_info(proto::ACTION_HIDE, &proto::NextTurn::default())
            );
            car.expect_silence(Duration::from_millis(300)).await;
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut screen,
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    #[tokio::test]
    async fn the_files_access_units_go_out_in_order_and_start_over_at_the_end() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(asked()).await;
            car.start_the_encoder().await;
            // A file two units long: the third picture out is the first again.
            for expected in [
                [0x00, 0x00, 0x00, 0x01, 0x65],
                [0x00, 0x00, 0x00, 0x01, 0x41],
                [0x00, 0x00, 0x00, 0x01, 0x65],
            ] {
                let picture = car.expect(VIDEO_DATA).await;
                assert_eq!(picture.body, expected);
                assert!(picture.timestamp_ms.is_some());
            }
        });

        let frames = Frames::Cycle(vec![
            vec![0x00, 0x00, 0x00, 0x01, 0x65],
            vec![0x00, 0x00, 0x00, 0x01, 0x41],
        ]);
        run(
            device,
            &frames,
            &cfg,
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    #[test]
    fn a_stream_is_split_into_one_access_unit_per_picture() {
        // Parameter sets, an IDR, a picture that follows it, and then a picture
        // with its parameter sets written out again, the way an encoder does at
        // a keyframe.
        let stream: Vec<u8> = [
            &[0u8, 0, 0, 1, 0x67][..],
            &[0, 0, 0, 1, 0x68][..],
            &[0, 0, 0, 1, 0x65][..],
            &[0, 0, 1, 0x41][..],
            &[0, 0, 0, 1, 0x67][..],
            &[0, 0, 0, 1, 0x68][..],
            &[0, 0, 0, 1, 0x41][..],
        ]
        .concat();

        let units = access_units(&stream);
        assert_eq!(units.len(), 3, "one unit per picture");
        assert_eq!(
            units[0],
            [0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65],
            "the first picture keeps the parameter sets in front of it"
        );
        assert_eq!(units[1], [0, 0, 1, 0x41], "a picture on its own");
        assert_eq!(
            units[2],
            [0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x41],
            "a repeat of the parameter sets opens the unit, not closes it"
        );

        // One picture in a file is one access unit; nothing in a file is none.
        assert_eq!(access_units(&[0, 0, 0, 1, 0x65, 0xaa]).len(), 1);
        assert!(access_units(&[1, 2, 3, 4]).is_empty());
    }

    #[tokio::test]
    async fn nothing_is_sent_on_the_video_channel_until_the_car_starts_it() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(asked()).await;
            // Long enough for a heartbeat to have arrived had one been coming.
            car.expect_silence(Duration::from_millis(500)).await;
        });

        let frames = Frames::Cycle(vec![vec![0x00, 0x00, 0x00, 0x01, 0x65]]);
        run(
            device,
            &frames,
            &cfg,
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// A challenge with no key behind it is left alone rather than answered with
    /// something the car would only reject, and the session carries on: the head
    /// unit asks again every half minute and is skipped again. The absence is
    /// passed in rather than looked for, so that the test says the same thing
    /// whether or not the machine it runs on was built with a key.
    #[tokio::test]
    async fn a_challenge_with_no_key_behind_it_is_left_alone() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let status = Arc::new(Status::new());
        let reported = status.clone();

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            let mut challenge = Vec::new();
            proto::put_str(&mut challenge, 1, "A_B");
            car.send(HU_AUTHEN_REQUEST, &challenge).await;
            car.expect_silence(Duration::from_millis(500)).await;

            // Still there: the next question is answered as it would have been.
            car.send(HU_INFO, &[]).await;
            car.expect(MD_INFO).await;
        });

        run(
            device,
            &Frames::None,
            &cfg,
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            reported,
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();

        assert_eq!(
            status.json()["carlife"]["challenge_answered"],
            false,
            "the challenge is reported as asked and unanswered"
        );
    }

    #[tokio::test]
    async fn a_frame_from_the_carplay_side_goes_out_as_it_arrives() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(asked()).await;
            car.start_the_encoder().await;
            let _ = started_tx.send(());
            let first = car.expect(VIDEO_DATA).await;
            assert_eq!(first.body, vec![0x00, 0x00, 0x00, 0x01, 0x41]);
            assert!(first.timestamp_ms.is_some());
        });

        // What the CarPlay side would push, once the car is ready for it. The
        // pause is not slack: the session has to have read START and marked the
        // encoder running, or the frame is dropped on arrival, the same way it
        // would be from a phone that sent one too early.
        let pushing = tokio::spawn(async move {
            let _ = started_rx.await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            outlet.sink().push(StreamEvent::VideoFrame {
                keyframe: false,
                data: vec![0x00, 0x00, 0x00, 0x01, 0x41],
            });
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
        pushing.await.unwrap();
    }

    #[tokio::test]
    async fn every_channel_is_given_back_when_the_session_ends() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(asked()).await;
            // Dropping the pipe is the car going away.
        });

        run(
            device,
            &frames,
            &cfg,
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();

        // The next session after the car is plugged back in gets receivers that
        // still work, which means the first ones were handed back rather than
        // dropped with the session.
        let rx = outlet
            .take_video()
            .expect("the video channel was handed back");
        outlet.return_video(rx);
        let rx = outlet
            .take_media()
            .expect("the media channel was handed back");
        outlet.return_media(rx);
        let rx = outlet
            .take_playing()
            .expect("what the phone is playing was handed back");
        outlet.return_playing(rx);
        let rx = outlet
            .take_guidance()
            .expect("where the phone is going was handed back");
        outlet.return_guidance(rx);
    }

    #[tokio::test]
    async fn a_panel_key_is_read_rather_than_dropped_on_the_floor() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            let mut key = Vec::new();
            proto::put_int(&mut key, 1, 0x14);
            car.send(TOUCH_HARD_KEY, &key).await;
            // Nothing answers it, so the session must simply carry on.
            car.expect_silence(Duration::from_millis(300)).await;
        });

        run(
            device,
            &Frames::None,
            &cfg,
            None,
            &mut |_, _, _| {},
            no_icon(),
            EventSink::new(),
            &mut carplay,
            &no_tap(),
            Arc::new(Status::new()),
            Arc::new(Vehicle::default()),
        )
        .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// Which of the car's keys a phone has a use for, and what each one means to
    /// it. Both families of direction key land on the same four directions.
    #[test]
    fn the_panel_keys_a_phone_has_a_use_for_are_the_ones_that_reach_it() {
        assert_eq!(command_for(KEY_MAIN), Some(Command::Home));
        assert_eq!(command_for(KEY_HOME), Some(Command::Home));
        assert_eq!(command_for(KEY_BACK), Some(Command::Back));
        assert_eq!(command_for(KEY_OK), Some(Command::Select));
        assert_eq!(command_for(KEY_MOVE_LEFT), Some(Command::Left));
        assert_eq!(command_for(KEY_MOVE_RIGHT), Some(Command::Right));
        assert_eq!(command_for(KEY_MOVE_UP), Some(Command::Up));
        assert_eq!(command_for(KEY_MOVE_DOWN), Some(Command::Down));
        assert_eq!(command_for(KEY_SELECTOR_NEXT), Some(Command::Down));
        assert_eq!(command_for(KEY_SELECTOR_PREVIOUS), Some(Command::Up));
        assert_eq!(command_for(KEY_SEEK_SUB), Some(Command::Prev));
        assert_eq!(command_for(KEY_SEEK_ADD), Some(Command::Next));
        assert_eq!(command_for(KEY_MEDIA_START), Some(Command::Play));
        assert_eq!(command_for(KEY_MEDIA_STOP), Some(Command::Pause));
        assert_eq!(command_for(KEY_VR_START), Some(Command::Siri));

        assert_eq!(
            command_for(KEY_PHONE_CALL),
            Some(Command::Telephony(PHONE_HOOK_SWITCH))
        );
        assert_eq!(
            command_for(KEY_PHONE_END),
            Some(Command::Telephony(PHONE_DROP))
        );
        assert_eq!(command_for(KEY_MUTE), Some(Command::Telephony(PHONE_MUTE)));
    }

    /// The rest of the car's keys: its radio, its navigation, its own volume and
    /// settings. They are the head unit's business, and a phone gets nothing.
    #[test]
    fn the_keys_that_are_the_cars_own_go_no_further() {
        for key in [
            0x04, 0x05, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x11, 0x12, 0x1D,
        ] {
            assert_eq!(command_for(key), None, "key {key:#04x}");
        }
    }
}
