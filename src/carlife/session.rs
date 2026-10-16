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
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{auth, frame, proto};
use crate::airplay::{EventSink, ScreenParams};
use crate::commands::{Command, TouchContact, PHONE_DROP, PHONE_HOOK_SWITCH, PHONE_MUTE};
use crate::config::Config;
use crate::navigation::{turn_kind, Guidance};
use crate::playing::{Playing, Track};
use crate::sink::{ClOutlet, Media};

// What the car sends, by name — the numbers are CarLifeVehicleLib's
// `EPackageHeadType`.
const HU_PROTOCOL_VERSION: u32 = 0x0001_8001;
const HU_INFO: u32 = 0x0001_8003;
const HU_VIDEO_ENCODER_INIT: u32 = 0x0001_8007;
const HU_VIDEO_ENCODER_START: u32 = 0x0001_8009;
const HU_VIDEO_ENCODER_PAUSE: u32 = 0x0001_800A;
const HU_VIDEO_ENCODER_RESET: u32 = 0x0001_800B;
const HU_VIDEO_ENCODER_FRAME_RATE_CHANGE: u32 = 0x0001_800C;
const HU_GO_TO_FOREGROUND: u32 = 0x0001_8025;
const HU_STATISTIC_INFO: u32 = 0x0001_8027;
const HU_DATA_SUBSCRIBE: u32 = 0x0001_8043;
const HU_DATA_SUBSCRIBE_START: u32 = 0x0001_8045;
const HU_DATA_SUBSCRIBE_STOP: u32 = 0x0001_8046;
const HU_AUTHEN_REQUEST: u32 = 0x0001_8048;
const HU_AUTHEN_RESULT: u32 = 0x0001_804A;
const HU_FEATURE_CONFIG_RESPONSE: u32 = 0x0001_8052;
const HU_ERROR_CODE: u32 = 0x0001_8055;

// What we send.
const MATCH_STATUS: u32 = 0x0001_0002;
const MD_INFO: u32 = 0x0001_0004;
const VIDEO_ENCODER_INIT_DONE: u32 = 0x0001_0008;
const VIDEO_ENCODER_FRAME_RATE_CHANGE_DONE: u32 = 0x0001_000D;
const MODULE_STATUS: u32 = 0x0001_0026;
const NAVI_NEXT_TURN_INFO: u32 = 0x0001_0030;
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
                        "[cl] {} holds no access unit; sending heartbeats only",
                        path.display()
                    );
                    return Self::None;
                }
                tracing::info!(
                    "[cl] the video channel will play {} \
                     ({} bytes in {} access units)",
                    path.display(),
                    bytes.len(),
                    units.len()
                );
                Self::Cycle(units)
            }
            Ok(_) => {
                tracing::warn!(
                    "[cl] {} is empty; sending heartbeats only",
                    path.display()
                );
                Self::None
            }
            Err(e) => {
                tracing::warn!(
                    "[cl] {} could not be read ({e}); sending heartbeats only",
                    path.display()
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
/// `screen` is told the size the car asked to be rendered at, as width, height
/// and frame rate. The car is the one that knows its own screen, so this is where
/// what the phone is asked to render comes from; `main` is what does something
/// with it.
///
/// `events` is where the car's touch and panel keys go. It is the CarPlay side's
/// own sink, so a key pressed on the car arrives at the phone by exactly the road
/// a key from anywhere else would; the phone cannot tell them apart.
pub async fn run<S, F>(
    stream: S,
    frames: &Frames,
    cfg: &Config,
    screen: ScreenReport<'_>,
    events: Arc<EventSink>,
    carplay: &mut F,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    F: FnMut(bool),
{
    // A session is what drains the bridge, so it takes all four receiving halves
    // for as long as it runs: one channel carries pictures, one the phone's
    // sound, and the last two what the phone says about itself — the song it is
    // playing and the route it is guiding along. The car unplugged and plugged
    // back in is a new session and a new call to this function, which is why the
    // halves are given back on the way out rather than dropped with everything
    // else.
    let (mut video, mut media, mut playing, mut guidance, source) = match frames {
        Frames::Live(outlet) => match outlet.take_video() {
            Some(rx) => (
                Some(rx),
                outlet.take_media(),
                outlet.take_playing(),
                outlet.take_guidance(),
                Source::Live,
            ),
            None => {
                tracing::warn!(
                    "[cl] the video channel is already being read; sending heartbeats only"
                );
                (None, None, None, None, Source::None)
            }
        },
        Frames::Cycle(units) => (
            None,
            None,
            None,
            None,
            Source::Cycle {
                units: units.clone(),
                next: 0,
            },
        ),
        Frames::None => (None, None, None, None, Source::None),
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
        events,
        source,
        channel_touched: None,
        agreed: None,
        started_at: None,
        media_format: None,
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
        // Read here rather than once for the program: a car plugged out and back
        // in is a new session, and a key that has gone missing since is worth
        // saying so each time the car asks for an answer.
        auth: auth::Signer::load(cfg.auth_dir.as_deref()),
        next_tick: Instant::now(),
        up: false,
        last_touch: None,
    };

    let outcome = loop {
        let deadline = session.next_tick;
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
            // What the phone is playing, on the command channel. Written as it
            // arrives, like the two above and for the same reason: the phone is
            // what decides when the song changes.
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
            // Only a tick moves the schedule forward, so a busy stream of
            // messages cannot starve the video channel.
            _ = tokio::time::sleep_until(deadline) => {
                if let Err(e) = session.tick().await {
                    break Err(e);
                }
                session.next_tick = Instant::now() + session.period();
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
        if let Some(rx) = playing {
            outlet.return_playing(rx);
        }
        if let Some(rx) = guidance {
            outlet.return_guidance(rx);
        }
    }
    if session.up {
        carplay(false);
        tracing::info!("[cl] the session is over; the CarPlay side goes with it");
    }
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
    /// Where the car's touch and panel keys go.
    events: Arc<EventSink>,
    source: Source,
    /// When the video channel last carried anything at all. With pictures
    /// arriving on their own clock, this is what decides whether the heartbeat
    /// still has a job to do.
    channel_touched: Option<Instant>,
    /// What the car asked for and what we agreed to, once it has asked.
    agreed: Option<proto::VideoEncoderInfo>,
    /// When the encoder started, which is what the video channel's timestamps
    /// are measured from.
    started_at: Option<Instant>,
    /// The format the car has been told the media channel's samples are in, once
    /// it has been told. A stream that opens with the same one is not worth a
    /// second message: nothing about it has changed.
    media_format: Option<(u32, u16)>,
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
    /// The key the car's challenge is answered with, when the operator has put
    /// one where credentials are looked for. `None` is a session that cannot
    /// answer it, which is said out loud when the car asks.
    auth: Option<auth::Signer>,
    next_tick: Instant,
    up: bool,
    /// Where the finger was the last time the car said. A release has to name it
    /// — see `touch`.
    last_touch: Option<(u16, u16)>,
}

impl<W: AsyncWrite + Unpin> Session<'_, W> {
    /// One message from the car.
    async fn handle<F: FnMut(bool)>(&mut self, chunk: frame::Chunk, carplay: &mut F) -> Result<()> {
        if chunk.channel != frame::CMD {
            self.other_channel(chunk).await;
            return Ok(());
        }
        let Some(message) = frame::decode(frame::CMD, &chunk.body) else {
            tracing::warn!("[cl] a command chunk that does not decode was dropped");
            return Ok(());
        };
        if !frame::from_car(message.service_type) {
            tracing::debug!(
                "[cl] ignoring {:#010x} — the car's direction bit is clear",
                message.service_type
            );
            return Ok(());
        }

        match message.service_type {
            HU_PROTOCOL_VERSION => {
                let (major, minor) = proto::decode_protocol_version(&message.body);
                tracing::info!("[cl] the car speaks CarLife {major}.{minor}");
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
                    tracing::info!("[cl] the car has our device information; the session is up");
                    carplay(true);
                }
            }
            HU_STATISTIC_INFO => {
                let info = proto::decode_statistic_info(&message.body);
                // `connect_time` is reported as-is: the field is an int32 and
                // neither the guide nor the head-unit library says in what unit.
                tracing::info!(
                    "[cl] the car reports cuid {:?} version {:?} (build {}), channel {:?}; \
                     {} connections, {} of them successful, connect time {}",
                    info.cuid,
                    info.version_name,
                    info.version_code,
                    info.channel,
                    info.connect_count,
                    info.connect_success_count,
                    info.connect_time
                );
                // The other half of the authentication: the car's channel number
                // is what the phone judges it by, and the judgement is what this
                // message answers with. A verdict either way is sent — the car is
                // told what a phone makes of it rather than left waiting for an
                // answer it may take as acceptance.
                self.judge(info.channel).await?;
                // The car will not configure anything until it is asked, and it
                // has a two-minute limit on sending the statistics above.
                self.send(FEATURE_CONFIG_REQUEST, &[]).await?;
            }
            HU_FEATURE_CONFIG_RESPONSE => {
                for (key, value) in proto::decode_feature_configs(&message.body) {
                    tracing::info!("[cl] the car set {key} = {value}");
                }
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
                if passed {
                    tracing::info!("[cl] the car accepted us");
                } else {
                    tracing::warn!(
                        "[cl] the car did not accept us; the guide has it disconnecting here"
                    );
                }
            }
            HU_VIDEO_ENCODER_INIT => {
                let asked = proto::decode_video_encoder_info(&message.body);
                // What we can actually produce, which is what `INIT_DONE` is
                // for: the reference says both ends work from the device's
                // answer, and the car scales if the two disagree.
                let fallback = ScreenParams::default();
                let agreed = proto::VideoEncoderInfo {
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
                tracing::info!(
                    "[cl] the car asked for {}x{} at {} fps; agreeing to {}x{} at {}",
                    asked.width,
                    asked.height,
                    asked.frame_rate,
                    agreed.width,
                    agreed.height,
                    agreed.frame_rate
                );
                self.agreed = Some(agreed);
                let body = proto::video_encoder_info(agreed);
                self.send(VIDEO_ENCODER_INIT_DONE, &body).await?;
                // The car has just said what its screen is, and that is what the
                // phone is asked to render at: the two halves meet here.
                (self.screen)(
                    agreed.width as u32,
                    agreed.height as u32,
                    agreed.frame_rate as u32,
                );
            }
            HU_VIDEO_ENCODER_START => {
                tracing::info!("[cl] the car started the encoder; video begins");
                let now = Instant::now();
                self.started_at = Some(now);
                // The start itself is what the car hears next, so the first
                // heartbeat is only owed if no picture arrives within a period.
                self.channel_touched = Some(now);
                self.next_tick = now;
            }
            HU_VIDEO_ENCODER_PAUSE | HU_VIDEO_ENCODER_RESET => {
                tracing::info!("[cl] the car stopped the encoder");
                self.started_at = None;
            }
            HU_VIDEO_ENCODER_FRAME_RATE_CHANGE => {
                let asked = proto::decode_frame_rate(&message.body).unwrap_or(0);
                let accepted = match asked {
                    MIN_FPS..=MAX_FPS => asked,
                    _ => self.agreed.map(|agreed| agreed.frame_rate).unwrap_or(30),
                };
                tracing::info!("[cl] the car asked for {asked} fps; agreeing to {accepted}");
                if let Some(agreed) = self.agreed.as_mut() {
                    agreed.frame_rate = accepted;
                }
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
                tracing::debug!("[cl] the car asked us to come to the foreground");
                self.send(GO_TO_FOREGROUND_RESPONSE, &[]).await?;
            }
            // The car asking whether this device can tell it about a route. The
            // answer is owed per module, and the car is the one that decides what
            // to do with it — so it is told exactly what this side has.
            HU_DATA_SUBSCRIBE => {
                let asked = proto::decode_data_subscribe(&message.body);
                tracing::info!("[cl] the car asks which data we can provide: {asked:?}");
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
                    "[cl] the car asks us to {} {asked:?}",
                    if wanted { "send" } else { "stop sending" }
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
                tracing::warn!(
                    "[cl] the car reported an error, {} bytes of it",
                    message.body.len()
                );
            }
            other => tracing::debug!("[cl] {other:#010x} arrived and is not acted on"),
        }
        Ok(())
    }

    /// The car's authentication challenge, answered with the phone's key.
    ///
    /// A challenge that cannot be answered is not answered at all: the app sends
    /// nothing when it cannot put one through its key, and a car given an answer
    /// it cannot check is no better off than one given none — except that it was
    /// lied to. The car does not get to hear about the failure either way; it
    /// works it out for itself, and says so with `HU_AUTHEN_RESULT`.
    async fn answer(&mut self, challenge: &str) -> Result<()> {
        let Some(key) = self.auth.as_ref() else {
            tracing::warn!(
                "[cl] the car asked us to authenticate and there is no key to answer it with; \
                 put the phone's key in ./authentication or set CP2CL_AUTH_DIR"
            );
            return Ok(());
        };
        let Some(value) = key.answer(challenge) else {
            return Ok(());
        };
        tracing::info!(
            "[cl] the car's challenge {challenge:?} is answered; {} characters went out",
            value.len()
        );
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
        if legal {
            tracing::info!("[cl] the car's channel {channel:?} is one a phone serves");
        } else {
            tracing::warn!("[cl] the car's channel {channel:?} is not one a phone serves");
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
            tracing::debug!("[cl] channel {} sent {} bytes", chunk.channel, chunk.body.len());
            return;
        }
        let Some(message) = frame::decode(frame::TOUCH, &chunk.body) else {
            tracing::warn!("[cl] a touch chunk that does not decode was dropped");
            return;
        };
        let command = match message.service_type {
            TOUCH_ACTION => match proto::decode_touch_action(&message.body) {
                Some(touch) => Some(self.touch(touch)),
                None => {
                    tracing::warn!("[cl] a touch action with fields missing was dropped");
                    None
                }
            },
            TOUCH_HARD_KEY => match proto::decode_hard_key(&message.body) {
                Some(key) => {
                    let command = command_for(key);
                    if command.is_none() {
                        tracing::debug!("[cl] panel key {key:#04x} means nothing to a phone");
                    }
                    command
                }
                None => {
                    tracing::warn!("[cl] a panel key with its field missing was dropped");
                    None
                }
            },
            other => {
                tracing::debug!("[cl] {other:#010x} arrived on the touch channel");
                None
            }
        };
        let Some(command) = command else {
            return;
        };
        // Failures are the phone's to have: a press that arrives before the event
        // channel is up is dropped by the sink with a line of its own.
        if let Err(e) = self.events.dispatch(command).await {
            tracing::warn!("[cl] the phone did not take an input event: {e}");
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

    /// The car's own pixels as the phone's. They are the same numbers: the canvas
    /// the car asked to be rendered at is what the phone was asked for, so a
    /// touch is only clamped, because a point outside the picture is not on it.
    fn on_canvas(&self, x: i32, y: i32) -> (u16, u16) {
        let (width, height) = match self.agreed {
            Some(agreed) => (agreed.width, agreed.height),
            None => (i32::from(u16::MAX), i32::from(u16::MAX)),
        };
        (
            x.clamp(0, width.max(1) - 1) as u16,
            y.clamp(0, height.max(1) - 1) as u16,
        )
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
                tracing::info!("[cl] the media channel carries PCM at {rate} Hz, {channels}ch");
                self.media_format = Some((rate, channels));
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

    /// One thing the phone said it is playing, on the command channel.
    ///
    /// The phone describes one attribute at a time and the car's message is the
    /// whole song, so what arrives is folded into what is known and the message
    /// goes out when that changes. An update that says nothing new — the play
    /// state alone, which the phone sends on its own — is not worth a second
    /// message: the panel is being told the song, and the song is the same one.
    ///
    /// The cover art is the one thing that is not a field of that message's
    /// subject: it is a file, it arrives after the song it belongs to, and a
    /// song the panel has not been told about is not one to attach it to yet.
    async fn send_playing(&mut self, item: Playing) -> Result<()> {
        match item {
            Playing::Track(update) => {
                let before = self.track.clone();
                // A title that has changed is another song, and the cover that
                // is held belongs to the one before it. Only the title: the
                // phone also corrects the artist and the album of a song that is
                // still the same one, and its cover is not sent again.
                // A cover that arrived before the first song is kept for it:
                // "another" is only a song that replaces a song.
                let another = update.song.is_some()
                    && self.track.song.is_some()
                    && update.song != self.track.song;
                self.track.merge(update);
                if self.track == before {
                    return Ok(());
                }
                if another {
                    self.art = None;
                }
            }
            Playing::Artwork(image) => self.art = Some(image),
        }

        // An app name on its own is not a song, and the panel is better left
        // with the last song it was told than emptied by the play state.
        if self.track.is_silent() {
            return Ok(());
        }
        let song = self.track.song.as_deref().unwrap_or_default();
        let artist = self.track.artist.as_deref().unwrap_or_default();
        let album = self.track.album.as_deref().unwrap_or_default();
        let source = self.track.source.as_deref().unwrap_or_default();
        tracing::info!(
            "[cl] media info: song={song:?} artist={artist:?} album={album:?} \
             duration={} source={source:?} cover={}",
            self.track.duration_s.unwrap_or(0),
            self.art.is_some()
        );
        let body = proto::media_info(&self.track, self.art.as_deref());
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
                tracing::info!("[cl] navi state: running={running}");
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
            tracing::info!("[cl] navi turn: action={action_name}");
        } else {
            tracing::info!(
                "[cl] navi turn: action={action_name} kind={:?} road={:?} remain_m={:?}",
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
        match (&self.source, self.started_at, self.agreed) {
            (Source::Cycle { .. }, Some(_), Some(agreed)) if agreed.frame_rate > 0 => {
                Duration::from_millis(1000 / agreed.frame_rate as u64)
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
    async fn send_frame(&mut self, service_type: u32, timestamp_ms: u32, body: &[u8]) -> Result<()> {
        let channel = service_type >> 16;
        let message = frame::encode(channel, service_type, timestamp_ms, body);
        frame::write_chunk(&mut self.writer, channel, &message)
            .await
            .with_context(|| format!("write {service_type:#010x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::{AudioCodec, MediaSink, StreamEvent};
    use std::path::PathBuf;
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

        /// Everything the car asks before it will start the encoder.
        async fn handshake(&mut self, asked: proto::VideoEncoderInfo) {
            let mut version = Vec::new();
            proto::put_int(&mut version, 1, 1);
            proto::put_int(&mut version, 2, 0);
            self.send(HU_PROTOCOL_VERSION, &version).await;
            self.expect(MATCH_STATUS).await;

            self.send(HU_INFO, &[]).await;
            self.expect(MD_INFO).await;

            self.send(HU_STATISTIC_INFO, &statistics("20062100")).await;
            self.expect(MD_AUTHEN_RESULT).await;
            self.expect(FEATURE_CONFIG_REQUEST).await;

            self.send(HU_VIDEO_ENCODER_INIT, &proto::video_encoder_info(asked))
                .await;
            let agreed = self.expect(VIDEO_ENCODER_INIT_DONE).await;
            assert_eq!(
                proto::decode_video_encoder_info(&agreed.body),
                proto::decode_video_encoder_info(&proto::video_encoder_info(asked))
            );
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
            &mut |_, _, _| {},
            EventSink::new(),
            &mut carplay,
        )
        .await
        .expect("the session ends cleanly");
        car.await.unwrap()
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

    /// A key of this test's own on disk, which is all `run` asks for. Not the
    /// real one: what is under test is the road the challenge travels.
    fn a_key_on_disk() -> (PathBuf, rsa::RsaPrivateKey) {
        use rsa::pkcs8::EncodePrivateKey;
        let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 1024).expect("a key");
        let pem = key
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .expect("the key writes as PEM");
        let dir = std::env::temp_dir().join("cp2cl-session-auth-test");
        std::fs::create_dir_all(&dir).expect("a directory to hold it");
        std::fs::write(dir.join(auth::KEY_FILE), pem.as_bytes()).expect("the key is written");
        (dir, key)
    }

    /// The car's challenge is answered with the key the operator put in, and the
    /// answer is one the car's own check takes.
    #[tokio::test]
    async fn the_cars_challenge_is_answered_with_the_key_that_was_put_in() {
        use base64::engine::general_purpose::STANDARD as BASE64;
        use base64::Engine as _;

        let (dir, key) = a_key_on_disk();
        let cfg = Config {
            auth_dir: Some(dir),
            ..Config::from_env()
        };
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

        run(device, &Frames::None, &cfg, &mut |_, _, _| {}, EventSink::new(), &mut carplay)
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

        run(device, &Frames::None, &cfg, &mut |_, _, _| {}, EventSink::new(), &mut carplay)
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
        run(device, &Frames::None, &cfg, &mut screen, EventSink::new(), &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();

        assert_eq!(*screens.lock().unwrap(), vec![(1920, 480, 24)]);
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

        run(device, &Frames::None, &cfg, &mut screen, EventSink::new(), &mut carplay)
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
                stream_type: 102,
                codec: AudioCodec::AacLc {
                    rate: 44_100,
                    channels: 2,
                },
                name: "media".into(),
            });
            let init = car.expect(MEDIA_INIT).await;
            assert_eq!(init.body, proto::media_init(44_100, 2));

            sink.push(StreamEvent::AudioPacket {
                stream_type: 102,
                timestamp_us: 1_234_000,
                data: vec![0x11, 0x22, 0x33, 0x44],
            });
            let sound = car.expect(MEDIA_DATA).await;
            assert_eq!(sound.body, vec![0x11, 0x22, 0x33, 0x44]);
            assert_eq!(sound.timestamp_ms, Some(1_234));

            // The same format again says nothing the car is not already holding.
            sink.push(StreamEvent::AudioConfig {
                stream_type: 102,
                codec: AudioCodec::AacLc {
                    rate: 44_100,
                    channels: 2,
                },
                name: "media".into(),
            });
            sink.push(StreamEvent::AudioPacket {
                stream_type: 102,
                timestamp_us: 1_300_000,
                data: vec![0x55, 0x66],
            });
            assert_eq!(car.expect(MEDIA_DATA).await.body, vec![0x55, 0x66]);

            // A call is one of the car's own channels, not this one: what comes
            // out next is the music, with nothing between them.
            sink.push(StreamEvent::AudioConfig {
                stream_type: 100,
                codec: AudioCodec::Opus {
                    rate: 48_000,
                    channels: 1,
                },
                name: "telephony".into(),
            });
            sink.push(StreamEvent::AudioPacket {
                stream_type: 100,
                timestamp_us: 1_400_000,
                data: vec![0x77],
            });
            sink.push(StreamEvent::AudioPacket {
                stream_type: 102,
                timestamp_us: 1_500_000,
                data: vec![0x88],
            });
            assert_eq!(car.expect(MEDIA_DATA).await.body, vec![0x88]);

            // The phone stopped playing, and the car is told rather than left
            // holding a player open over nothing.
            sink.push(StreamEvent::AudioStopped { stream_type: 102 });
            car.expect(MEDIA_STOP).await;
        });

        run(device, &frames, &cfg, &mut screen, EventSink::new(), &mut carplay)
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

        run(device, &frames, &cfg, &mut screen, EventSink::new(), &mut carplay)
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

        run(device, &frames, &cfg, &mut screen, EventSink::new(), &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// The cover that is held belongs to the song it came with, so a new title
    /// puts it down: the phone does not send the old one's cover for the new
    /// song, and the panel would otherwise show it under the new name.
    #[tokio::test]
    async fn another_song_does_not_keep_the_cover_of_the_one_before_it() {
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

            let song = |name: &str| Playing::Track(Track {
                song: Some(name.into()),
                ..Track::default()
            });

            sink.playing(song("Reeperbahn"));
            car.expect(MEDIA_INFO).await;
            sink.playing(Playing::Artwork(vec![0xff, 0xd8]));
            let covered = car.expect(MEDIA_INFO).await;
            assert_eq!(
                covered.body,
                proto::media_info(
                    &Track {
                        song: Some("Reeperbahn".into()),
                        ..Track::default()
                    },
                    Some(&[0xff, 0xd8])
                )
            );

            // Another song, and the message that says so carries no cover.
            sink.playing(song("Stern"));
            let renamed = car.expect(MEDIA_INFO).await;
            assert_eq!(
                renamed.body,
                proto::media_info(
                    &Track {
                        song: Some("Stern".into()),
                        ..Track::default()
                    },
                    None
                )
            );

            // Until its own arrives.
            sink.playing(Playing::Artwork(vec![0x89, 0x50]));
            let its_own = car.expect(MEDIA_INFO).await;
            assert_eq!(
                its_own.body,
                proto::media_info(
                    &Track {
                        song: Some("Stern".into()),
                        ..Track::default()
                    },
                    Some(&[0x89, 0x50])
                )
            );
        });

        run(device, &frames, &cfg, &mut screen, EventSink::new(), &mut carplay)
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

        run(device, &frames, &cfg, &mut screen, EventSink::new(), &mut carplay)
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

        run(device, &frames, &cfg, &mut screen, EventSink::new(), &mut carplay)
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

        run(device, &frames, &cfg, &mut screen, EventSink::new(), &mut carplay)
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

        run(device, &Frames::None, &cfg, &mut |_, _, _| {}, EventSink::new(), &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();
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

        run(device, &frames, &cfg, &mut screen, EventSink::new(), &mut carplay)
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

        run(device, &frames, &cfg, &mut screen, EventSink::new(), &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    #[tokio::test]
    async fn the_files_access_units_go_out_in_order_and_start_over_at_the_end() {        let cfg = Config::from_env();
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
        run(device, &frames, &cfg, &mut |_, _, _| {}, EventSink::new(), &mut carplay)
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
        run(device, &frames, &cfg, &mut |_, _, _| {}, EventSink::new(), &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    /// A challenge with no key behind it is left alone rather than answered with
    /// something the car would only reject. The key is named here rather than
    /// looked for, so that the test says the same thing whether or not the
    /// machine it runs on has one.
    #[tokio::test]
    async fn a_challenge_with_no_key_behind_it_is_left_alone() {
        let cfg = Config {
            auth_dir: None,
            ..Config::from_env()
        };
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            let mut challenge = Vec::new();
            proto::put_str(&mut challenge, 1, "A_B");
            car.send(HU_AUTHEN_REQUEST, &challenge).await;
            car.expect_silence(Duration::from_millis(500)).await;
        });

        run(device, &Frames::None, &cfg, &mut |_, _, _| {}, EventSink::new(), &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();
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

        run(device, &frames, &cfg, &mut |_, _, _| {}, EventSink::new(), &mut carplay)
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

        run(device, &frames, &cfg, &mut |_, _, _| {}, EventSink::new(), &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();

        // The next session after the car is plugged back in gets receivers that
        // still work, which means the first ones were handed back rather than
        // dropped with the session.
        let rx = outlet.take_video().expect("the video channel was handed back");
        outlet.return_video(rx);
        let rx = outlet.take_media().expect("the media channel was handed back");
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

        run(device, &Frames::None, &cfg, &mut |_, _, _| {}, EventSink::new(), &mut carplay)
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
        assert_eq!(command_for(KEY_PHONE_END), Some(Command::Telephony(PHONE_DROP)));
        assert_eq!(command_for(KEY_MUTE), Some(Command::Telephony(PHONE_MUTE)));
    }

    /// The rest of the car's keys: its radio, its navigation, its own volume and
    /// settings. They are the head unit's business, and a phone gets nothing.
    #[test]
    fn the_keys_that_are_the_cars_own_go_no_further() {
        for key in [0x04, 0x05, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x11, 0x12, 0x1D] {
            assert_eq!(command_for(key), None, "key {key:#04x}");
        }
    }
}
