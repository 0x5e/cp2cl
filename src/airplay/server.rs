//! One connection to the phone, from the first OPTIONS to the teardown.
//!
//! `serve` accepts connections and hands each one to a `Session`, which walks
//! the handshake the `airplay` module header describes and then keeps answering
//! for the life of the session. Everything a session needs is owned once in
//! `Shared` and cloned per connection.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use plist::{dictionary::Dictionary, from_bytes, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, watch, Mutex};

use livi_runtime::AsyncAuth;

use super::audio::{self, AudioReceiver};
use super::bplist::{to_binary, to_json};
use super::clock::{ntp_diff, SyncedClock};
use super::crypto::{self, aes_ctr128, open_frames, seal_frames, sha1_all, sha256_all, SymKey};
use super::event::{pump_events, EventSink};
use super::info;
use super::mic::{MicConfig, MicSink};
use super::pairing::{PairSetup, PairVerify, Pairings, VerifyKeys};
use super::rtsp::{self, Request, Response, OCTET_CONTENT_TYPE, PAIRING_CONTENT_TYPE};
use super::screen::{self, VideoReceiver};
use super::tunnel;
use crate::config::{bt_address, dt_vendor, hostname, Config};
use crate::led::{Carplay, Signals};
use crate::sink::{AudioCodec, MediaSink, StreamEvent, StreamId};
use crate::web::{Audio, Mic, Screen, Status};

/// What a coding is called on the page. `AudioCodec`'s own `Debug` is a struct
/// dump, which is not a thing to put in front of someone.
fn codec_name(codec: AudioCodec) -> &'static str {
    match codec {
        AudioCodec::Pcm { .. } => "PCM",
        AudioCodec::AacLc { .. } => "AAC-LC",
        AudioCodec::Opus { .. } => "Opus",
    }
}

/// Every /auth-setup request starts with this byte.
const MFISAP_VERSION: u8 = 0x01;
/// Timing-port packet types (RTCP-shaped, not NTP): request and reply.
const TIMING_REQUEST: u8 = 210;
const TIMING_RESPONSE: u8 = 211;

/// The buffered media stream. It is the one stream the phone hands over in
/// lumps — ~384 ms at a time — rather than at its own playback rate, so it is
/// the one stream that has to be re-timed on the way out.
const BUFFERED_AUDIO: u8 = 102;

/// How long the phone may go unheard before its session is taken as over.
///
/// A phone that leaves the network does not close anything: it stops talking,
/// and no part of the socket says so — `read` keeps waiting and no FIN arrives.
/// What is left is the clock exchange, which runs once a second and which the
/// phone answers, plus the per-second requests it makes of us. Silence past this
/// is the only evidence there is that it is gone. The same number LIVI's own
/// stack ends a silent session on.
const PHONE_SILENT: Duration = Duration::from_secs(5);

/// How often that silence is looked at. It reads one timestamp, so it is cheap;
/// a second is short enough that the answer arrives while it is still worth
/// having.
const SILENCE_CHECK: Duration = Duration::from_secs(1);

/// How late a frame may be before the pacer stops resuming the schedule it was
/// following and takes that frame as the new zero.
///
/// Small on purpose: a frame that is merely a little late keeps its place, so
/// ordinary scheduling jitter does not accumulate into drift, while one held up
/// past this cannot be caught up on without firing a burst — and a burst is
/// exactly what makes the consumer reschedule.
const PACE_SLACK: Duration = Duration::from_millis(8);

/// A timestamp this far from the one before it is not the next frame: it is a
/// counter wraparound, or a stream that started again after a pause. The pacer
/// anchors on it instead of waiting out the difference.
const PACE_RESET: Duration = Duration::from_secs(5);

/// Every audio stream the phone has open, as far as `/feedback` is concerned.
///
/// The phone asks once a second and expects the playback position of every
/// active stream. A stream counts from the moment it is set up, not from the
/// moment its first packet lands: an empty reply while audio is running reads
/// as "that stream died", and the phone then tears it down every few seconds
/// until sound never comes back.
#[derive(Default)]
pub struct AudioClock {
    /// Handed out per registration so a stream that dies cannot remove the
    /// entry that replaced it.
    next_id: u64,
    streams: Vec<AudioStream>,
}

struct AudioStream {
    id: u64,
    stream_type: i64,
    connection_id: i64,
    sample_rate: u32,
    /// The phone's own number for how far the sound lags the samples it has
    /// already sent — its jitter buffer, in milliseconds. Subtracted from the
    /// reported position so `sampleTime` describes what the listener is hearing
    /// now rather than what we last received.
    playout_latency_ms: u64,
    /// RTP timestamp of the first packet, and when it arrived.
    origin: Option<(u32, std::time::Instant)>,
}

impl AudioClock {
    /// Registers a stream and hands back a handle to it. Opening the *same*
    /// stream again — the type and the connection it was opened as — replaces
    /// the entry, because there is nothing left of the one before it to report.
    /// A different connection is a different stream, even under the same type:
    /// the phone opens a fresh one for every track change, and both are live
    /// until it tears the older of the two down.
    pub fn open(
        &mut self,
        stream_type: i64,
        connection_id: i64,
        sample_rate: u32,
        playout_latency_ms: u64,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.streams
            .retain(|s| s.stream_type != stream_type || s.connection_id != connection_id);
        self.streams.push(AudioStream {
            id,
            stream_type,
            connection_id,
            sample_rate,
            playout_latency_ms,
            origin: None,
        });
        id
    }

    pub fn forget_streams(&mut self) {
        self.streams.clear();
    }

    /// Forgets one stream, by the handle its setup returned.
    pub fn close(&mut self, id: u64) {
        self.streams.retain(|s| s.id != id);
    }

    /// Forgets one instance of a stream, which is how a teardown names it: by
    /// the type the phone calls it, with the connection it was opened as.
    pub fn close_type(&mut self, stream_type: i64, connection_id: i64) {
        self.streams
            .retain(|s| s.stream_type != stream_type || s.connection_id != connection_id);
    }

    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    /// Anchors a stream's playout position on its first packet. Every later
    /// packet carries a counter that keeps advancing on its own, so the first
    /// one is all that has to be remembered.
    pub fn note(&mut self, id: u64, rtp_timestamp: u32) {
        let Some(s) = self.streams.iter_mut().find(|s| s.id == id) else {
            return;
        };
        if s.sample_rate > 0 && s.origin.is_none() {
            s.origin = Some((rtp_timestamp, std::time::Instant::now()));
        }
    }

    /// One entry per open stream, in the shape `/feedback` wants. Everything
    /// past the sample rate only means something once the first packet has
    /// placed the stream in time; announcing the stream without them is the
    /// whole point, because an empty reply reads as "gone".
    pub fn report(&self, timestamp: u64) -> Vec<Dictionary> {
        self.streams
            .iter()
            .map(|s| {
                let mut entry = Dictionary::new();
                entry.insert("type".into(), Value::Integer(s.stream_type.into()));
                entry.insert(
                    "sampleRate".into(),
                    Value::Integer((s.sample_rate as i64).into()),
                );
                if let Some(at) = s.sample_time() {
                    entry.insert(
                        "streamConnectionID".into(),
                        Value::Integer(s.connection_id.into()),
                    );
                    // The one field that has to come from the steered clock:
                    // the phone places `sampleTime` against it.
                    entry.insert("timestamp".into(), Value::Integer(timestamp.into()));
                    entry.insert("sampleTime".into(), Value::Integer(at.into()));
                }
                entry
            })
            .collect()
    }

    /// What is registered, for the once-only log line.
    pub fn summary(&self) -> String {
        let types: Vec<i64> = self.streams.iter().map(|s| s.stream_type).collect();
        let anchored = self.streams.iter().filter(|s| s.origin.is_some()).count();
        format!("{types:?} ({anchored} anchored)")
    }
}

impl AudioStream {
    /// First sample plus real time elapsed since, less the playout buffer.
    fn sample_time(&self) -> Option<u64> {
        let (first, started) = self.origin?;
        let elapsed = started.elapsed().as_secs_f64() - self.playout_latency_ms as f64 / 1000.0;
        Some(first as u64 + (elapsed.max(0.0) * self.sample_rate as f64) as u64)
    }
}

/// What the phone should render at.
///
/// A head unit names it over the CarLife session — `VIDEO_ENCODER_INIT` carries
/// the width, the height and the frame rate it wants — and `/info` is answered
/// from here. It only lands if it is written before the session starts, which is
/// why changing it costs a session: the iPhone reads `/info` once and does not
/// ask again. See `main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Canvas scale in percent, the way PlayPort's "UI size" control means it:
    /// the phone is asked for a canvas of `100/percent` the size, and the
    /// display side fits the result to its own surface, so a larger canvas draws
    /// proportionally *smaller* CarPlay controls. 100 leaves it alone.
    pub ui_scale: u32,
}

impl Default for ScreenParams {
    /// Until a head unit names a screen of its own: 720p at 30 fps, which is the
    /// shape a car's decoder is surest of, and the same numbers the CarLife side
    /// falls back on when a car names no size.
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            ui_scale: 100,
        }
    }
}

/// The canvas scales that are offered. Anything else is the default, which is
/// what PlayPort's own panel does with a typed-in value.
pub const UI_SCALES: [u32; 4] = [75, 85, 100, 115];

impl ScreenParams {
    /// The scale actually in force: one of the offered ones, or the default.
    pub fn ui_scale(&self) -> u32 {
        if UI_SCALES.contains(&self.ui_scale) {
            self.ui_scale
        } else {
            100
        }
    }

    /// The canvas the phone is actually asked for: the size above with the UI
    /// scale applied. Mirrors PlayPort's `CarPlayUiScale.apply`, including its
    /// refusal to ask for an enlarged canvas past 4K.
    ///
    /// Everything that has to line up with the stream uses this rather than
    /// `width`/`height`: the decoder's coded size, and the pixel space a touch
    /// is measured in.
    pub fn canvas(&self) -> (u32, u32) {
        let scale = self.ui_scale();
        if scale == 100 {
            return (self.width, self.height);
        }
        // Rounded up to even pixels, which is what CarPlay insists on.
        let pixels = |value: u32| ((value as u64 * 100 / scale as u64 + 1) / 2 * 2) as u32;
        let (width, height) = (pixels(self.width), pixels(self.height));
        if scale < 100 && (width.max(height) > 3840 || width.min(height) > 2160) {
            return (self.width, self.height);
        }
        (width, height)
    }
}

/// What the phone should render at, as the CarLife session writes it. `None`
/// means no head unit has named a screen yet, and the default stands.
pub type SharedScreen = Arc<std::sync::Mutex<Option<ScreenParams>>>;

/// The channel number the head unit reported, as the CarLife session writes it
/// out of `MSG_CMD_HU_STATISTIC_INFO`. `None` means no head unit has reported
/// one, and the default icon stands.
pub type SharedChannel = Arc<std::sync::Mutex<Option<String>>>;

/// Everything the airplay side needs, owned once and shared per connection.
pub struct Shared {
    pub cfg: Arc<Config>,
    pub pairings: Arc<Mutex<Pairings>>,
    pub video_sink: Arc<dyn MediaSink>,
    pub audio_sink: Arc<dyn MediaSink>,
    /// Set by the CarLife session, once a head unit has named its own screen.
    /// Read by `GET /info`.
    pub screen: SharedScreen,
    /// Set by the CarLife session, once a head unit has said which channel it
    /// is. Read by `GET /info` to pick the icon and the label under it.
    pub channel: SharedChannel,
    /// Fired when the phone's icon for us is tapped: that icon means the head
    /// unit's own desktop, so the car is asked for it.
    pub desktop: tokio::sync::broadcast::Sender<()>,
    /// Fired when what the phone should be rendering has changed. `/info` is
    /// answered once per session, so dropping the session is the only way a
    /// change lands: the phone reconnects, asks again, and gets the new answer.
    pub restart: tokio::sync::broadcast::Sender<()>,
    /// Fired when the CarLife session this half of the bridge exists for has
    /// ended. Different from `restart` on purpose: a restart means "start over",
    /// and this means "there is nothing to start over for", so it closes a
    /// running session without expecting another.
    pub shutdown: tokio::sync::broadcast::Sender<()>,
    /// The same MFi signer the iAP2 side uses, for `/auth-setup`.
    pub mfi: livi_runtime::mfi_async::SharedCoprocessor,
    /// Where the audio playout position is tracked, for `/feedback`.
    pub audio_clock: Arc<Mutex<AudioClock>>,
    /// Our clock, steered onto the phone's by the timing port. Shared rather
    /// than per session on purpose: `/feedback` and the timing task both stamp
    /// with it, and the phone only ever has one clock domain to be in.
    pub timing: Arc<Mutex<SyncedClock>>,
    /// Announced to the phone so it knows what audio to send.
    pub audio_codec: Arc<Mutex<AudioCodec>>,
    /// Where input lands. Nothing dispatches into it yet; the CarLife side will,
    /// once it carries touch and keys back up.
    pub event_sink: Arc<EventSink>,
    /// The one stream that runs both ways. Opened when the phone asks for
    /// bidirectional audio, and where audio captured at this end goes.
    pub mic: Arc<MicSink>,
    /// Where the status page reads this half from. Reported to and never read
    /// back: see `crate::web::status`.
    pub status: Arc<Status>,
    /// What the board's status light shows. How far a phone got is the one fact
    /// in it that this side of the bridge is the only source of, and it is
    /// reported rather than acted on: the light reads the whole answer.
    pub led: Arc<Signals>,
    /// What a type 130 stream needs to become an iAP2 session. `None` on a wired
    /// run: there is no second transport to move the control channel to, so the
    /// stream is answered with a port nobody listens on, exactly as before.
    pub tunnel: Option<Arc<tunnel::Setup>>,
}

/// Accepts connections and runs each one to completion.
///
/// `gate` is the CarLife session, and what it controls is when this half is open
/// for business at all. While it is shut nothing is accepted: an AirPlay
/// connection taken before the car is talking has nowhere for its streams to go
/// and would sit in the handshake until the phone gave up. When it shuts again,
/// what is already running is dropped as well — that is what makes the phone's
/// CarPlay session end when the car's CarLife session does, which is the
/// behaviour the bridge is supposed to have.
pub async fn serve(listener: TcpListener, shared: Shared, mut gate: watch::Receiver<bool>) {
    // Which connection is the current one, as a number that only goes up. A
    // phone that drops off the network and comes straight back leaves its old
    // connection behind — half open and silent — and that session must not go on
    // to declare the phone gone once a newer one is talking to it. See
    // `Wakeup::Silent`.
    let newest = Arc::new(AtomicU64::new(0));
    loop {
        if !*gate.borrow() {
            tracing::info!("carplay.airplay.held reason=car_session_not_up");
            if gate.changed().await.is_err() {
                return;
            }
            continue;
        }
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("carplay.airplay.accept_failed err={e}");
                        return;
                    }
                };
                let status = shared.status.clone();
                tracing::info!("carplay.airplay.connected peer={peer}");
                status.with(|s| s.carplay.airplay.peer = Some(peer.to_string()));
                shared.led.carplay(Carplay::Connecting);
                let serial = newest.fetch_add(1, Ordering::SeqCst) + 1;
                let current = newest.clone();
                *shared.timing.lock().await = SyncedClock::new();
                shared.audio_clock.lock().await.forget_streams();
                let cfg = shared.cfg.clone();
                let screen = shared.screen.clone();
                let channel = shared.channel.clone();
                let desktop = shared.desktop.clone();
                let restart = shared.restart.subscribe();
                let restart_tx = shared.restart.clone();
                let shutdown = shared.shutdown.subscribe();
                let pairings = shared.pairings.clone();
                let video_sink = shared.video_sink.clone();
                let audio_sink = shared.audio_sink.clone();
                let codec = shared.audio_codec.clone();
                let event_sink = shared.event_sink.clone();
                let mic = shared.mic.clone();
                let mfi = shared.mfi.clone();
                let audio_clock = shared.audio_clock.clone();
                let timing = shared.timing.clone();
                let tunnel_setup = shared.tunnel.clone();
                let led = shared.led.clone();

                tokio::spawn(async move {
                    let mut session = Session {
                        cfg,
                        screen,
                        channel,
                        desktop,
                        restart,
                        restart_tx,
                        shutdown,
                        pairings,
                        video_sink,
                        audio_sink,
                        codec,
                        event_sink,
                        mic,
                        mfi,
                        audio_clock,
                        timing,
                        tunnel_setup,
                        tunnel: None,
                        tunnel_start_pending: false,
                        peer,
                        setup: None,
                        verify: None,
                        keys: None,
                        cipher_read: None,
                        cipher_write: None,
                        _timing: Vec::new(),
                        streams: HashMap::new(),
                        pumps: HashMap::new(),
                        in_buf: Vec::new(),
                        frame_buf: Vec::new(),
                        plain_buf: Vec::new(),
                        read_ctr: 0,
                        write_ctr: 0,
                        cseq: 0,
                        feedback_log: String::new(),
                        heard: Arc::new(std::sync::Mutex::new(None)),
                        serial,
                        newest: current,
                        status,
                        led,
                    };
                    if let Err(e) = session.run(socket).await {
                        tracing::info!("carplay.airplay.ended peer={peer} err={e}");
                    }
                    // The connection is gone, and so is everything about it: a
                    // page still naming the last client's address, ports and
                    // stream counters would be describing a connection that no
                    // longer exists.
                    session.status.airplay_closed();
                    // The phone has gone. If the car went first, the light is
                    // already down to waiting and has to stay there; otherwise
                    // it is back to the car on its own.
                    if matches!(session.led.phone(), Carplay::Connecting | Carplay::Linked) {
                        session.led.carplay(Carplay::Idle);
                    }
                });
            }
            changed = gate.changed() => {
                if changed.is_err() {
                    return;
                }
                if !*gate.borrow() {
                    tracing::info!("carplay.airplay.closed reason=car_session_ended");
                    let _ = shared.shutdown.send(());
                }
            }
        }
    }
}

/// What is told to the phone for a stream we do not actually open.
///
/// The number is never bound, so the phone never reaches anything on it; it
/// exists because the answer's shape has a `dataPort` field. It used to come
/// from the same range as the real ports, back when those were handed out from
/// a counter — see `setup_session` for why that had to go.
const UNUSED_PORT: u16 = 45000;

/// Aborts a task when the session that started it goes away.
///
/// The timing socket loops on `recv_from` until something tells it to stop, and
/// nothing in the protocol ever does: each session binds its own, and the task
/// that owns it outlives the session that started it unless it is killed. The
/// descriptor would then live as long as the process, one per session — a
/// debugging run that changes resolution a dozen times leaves a dozen behind.
struct StopOnDrop(tokio::task::JoinHandle<()>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One audio frame waiting for the moment the phone's own timestamp gives it.
///
/// It is held sealed. Opening and decoding a frame is the most expensive thing
/// this loop does, and doing it to the eighteen that arrive in one lump is what
/// put the pacing behind its own schedule: the loop would spend a hundred
/// milliseconds on the lump, come back to find four frames overdue, and fire
/// them off together — which is the burst the re-timing exists to avoid. What is
/// decoded now is one frame at a time, as each one comes due.
struct Pending {
    timestamp_us: u64,
    sealed: Vec<u8>,
}

/// Puts a frame where its moment says, among the ones already waiting.
///
/// The queue is in the order the phone stamped the frames rather than the order
/// they arrived in. A packet that overtook another arrives after it, and handing
/// the decoder the two out of turn is a frame it cannot place: it says so, and
/// the twenty milliseconds come out as a click. Waiting a turn longer costs
/// nothing — the queue is a lump deep at the least.
///
/// Returns whether it was taken. A frame the queue already holds is not worth a
/// second decode, and one stamped at or before `sent` is a frame whose moment
/// has passed: the car has been given the sound up to there, and this one now
/// would land after its own neighbours.
fn hold(queue: &mut VecDeque<Pending>, frame: Pending, sent: Option<u64>) -> bool {
    if sent.is_some_and(|ts| frame.timestamp_us <= ts) {
        return false;
    }
    let at = queue
        .iter()
        .position(|f| f.timestamp_us > frame.timestamp_us)
        .unwrap_or(queue.len());
    if at > 0 && queue[at - 1].timestamp_us == frame.timestamp_us {
        return false;
    }
    queue.insert(at, frame);
    true
}

/// Waits until the frame stamped `ts` is due, `last` being the deadline the one
/// before it went out on.
///
/// Every frame is measured against the phone's timestamp rather than against
/// when it happened to arrive, which is what keeps the pacing from drifting: a
/// frame that is late delays only itself. Past `PACE_SLACK` even that stops
/// being true — the frames behind it would have to be fired off together to
/// catch up — so that frame becomes the new anchor instead.
async fn wait_due(last: &mut (u64, tokio::time::Instant), ts: u64) {
    let now = tokio::time::Instant::now();
    let ahead = ts.saturating_sub(last.0);
    if ahead > PACE_RESET.as_micros() as u64 {
        *last = (ts, now);
        return;
    }
    let mut due = last.1 + Duration::from_micros(ahead);
    if now > due + PACE_SLACK {
        due = now;
    }
    if due > now {
        tokio::time::sleep_until(due).await;
    }
    *last = (ts, due);
}

/// One open stream: the task reading it, and a socket held on the phone's
/// behalf. Aborting the task is the only way a UDP stream ever ends — nothing
/// in the protocol tells the socket the phone has stopped sending, so without
/// it the task would sit in `recv` for the life of the process holding a port.
struct Pump {
    task: tokio::task::JoinHandle<()>,
    /// Which instance of the stream this is. The phone labels a teardown with
    /// the type alone, so this is the only thing that says which of the type's
    /// instances the teardown is about.
    connection_id: i64,
    /// The stream's control port, bound but never read: leaving it closed makes
    /// the phone's RTCP bounce back as ICMPv6 port-unreachable, and an open
    /// socket nobody listens on is the cheapest way to answer "nothing to say".
    _control: Option<UdpSocket>,
}

struct Session {
    cfg: Arc<Config>,
    /// What the car asked for, if it has asked. Read when answering `/info`.
    screen: SharedScreen,
    /// Which channel the car's head unit is, if it has said. Read when
    /// answering `/info`.
    channel: SharedChannel,
    /// Fired when the phone's icon for us is tapped.
    desktop: tokio::sync::broadcast::Sender<()>,
    /// Fires when the session should end so the phone re-reads `/info`.
    restart: tokio::sync::broadcast::Receiver<()>,
    /// The sending end of that same signal, kept because the silence check is
    /// one of the things that can want a session over: a phone that has gone
    /// has to be asked to come back, and only the iAP2 half can ask it. See
    /// `Wakeup::Silent`.
    restart_tx: tokio::sync::broadcast::Sender<()>,
    /// Fires when the CarLife session that this one feeds has ended, which is
    /// also the end of this one.
    shutdown: tokio::sync::broadcast::Receiver<()>,
    pairings: Arc<Mutex<Pairings>>,
    video_sink: Arc<dyn MediaSink>,
    audio_sink: Arc<dyn MediaSink>,
    codec: Arc<Mutex<AudioCodec>>,
    event_sink: Arc<EventSink>,
    /// The microphone uplink, opened for whichever audio stream the phone said
    /// it wants to hear on.
    mic: Arc<MicSink>,
    mfi: livi_runtime::mfi_async::SharedCoprocessor,
    audio_clock: Arc<Mutex<AudioClock>>,
    /// The clock every timestamp we send is stamped with. See `clock::SyncedClock`.
    timing: Arc<Mutex<SyncedClock>>,
    /// What a type 130 stream is built from, when this run has anywhere to move
    /// iAP2 to.
    tunnel_setup: Option<Arc<tunnel::Setup>>,
    /// The phone's iAP2 session, once it asked to move it here. Owned by the
    /// session because the phone ends it by ending this: see `tunnel`.
    tunnel: Option<tunnel::Tunnel>,
    /// Whether a SETUP reply naming the tunnel is still on its way out. The
    /// session on the tunnel starts once it has gone: the port it is reached on
    /// is inside that reply. See `run`.
    tunnel_start_pending: bool,
    peer: SocketAddr,
    setup: Option<PairSetup>,
    verify: Option<PairVerify>,
    keys: Option<VerifyKeys>,
    cipher_read: Option<SymKey>,
    cipher_write: Option<SymKey>,
    /// stream id -> port, kept only for logging.
    streams: HashMap<u64, u16>,
    /// Tasks this session started that nothing else will stop. Held rather than
    /// awaited: dropping them is the whole point, see `StopOnDrop`.
    _timing: Vec<StopOnDrop>,
    /// Open streams by the type the phone names them with, oldest first.
    /// Teardown looks them up here to close what it says it is closing, and what
    /// it says is a type — so the oldest instance of that type is the one it
    /// means. A type holds more than one because the phone opens a stream
    /// without waiting for the last one to be torn down, and the teardown that
    /// arrives after that is about the one that came before.
    pumps: HashMap<i64, Vec<Pump>>,
    in_buf: Vec<u8>,
    /// Sealed frames waiting to be complete, once the channel is encrypted.
    frame_buf: Vec<u8>,
    /// What those frames decrypt to; a request can straddle frames.
    plain_buf: Vec<u8>,
    /// Nonce counters, one per direction. Both start at zero and advance by one
    /// per frame — read and write are independent.
    read_ctr: u64,
    write_ctr: u64,
    cseq: u32,
    /// The last `/feedback` summary printed, so the line only appears when the
    /// set of streams changes rather than once a second.
    feedback_log: String,
    /// When the phone was last heard from, by either of the two channels that
    /// hear it: a request on this connection, or an answer on the timing port.
    /// Written by both, read by the silence check in `run`.
    heard: Arc<std::sync::Mutex<Option<Instant>>>,
    /// Which connection this is, and the counter that says which one is current.
    /// Only the current one may decide the phone has gone.
    serial: u64,
    newest: Arc<AtomicU64>,
    /// Where the status page reads this connection from. Reported to and never
    /// read back: see `crate::web::status`.
    status: Arc<Status>,
    /// What the board's status light is told, so that the connection this
    /// session is can be seen from across a room.
    led: Arc<Signals>,
}

/// Why a session stopped waiting for the phone to say something.
///
/// Two of the three are signals from outside that mean the same thing to the
/// phone — the connection goes away — but they are not the same thing to us, and
/// keeping them apart is what lets each say so in the log.
enum Wakeup {
    /// The phone sent bytes, or nothing at all because it hung up.
    Read(usize),
    /// What to render changed, so this session is replaced by another.
    Restart,
    /// The CarLife session ended and this one has nothing left to feed.
    Shutdown,
    /// The phone stopped answering. Over Wi-Fi this is what a phone that left
    /// the network looks like: no bytes, no error, no end of file.
    Silent,
}

/// Says how a media stream ended, and which of the two ways it was.
///
/// A phone tearing a session down closes cleanly, and `read_exact` reports that
/// as `UnexpectedEof` — the same error it reports for a frame that stopped
/// halfway. Everything else is the connection failing underneath us. The counts
/// in the closing line cannot tell the two apart, and which one it was is the
/// first question anyone asks of a dropped stream, so the log says it here.
pub(super) fn ended(what: &str, part: &str, e: &std::io::Error) {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        tracing::info!("{}.stream_eof part={}", what, part);
    } else {
        tracing::warn!("{}.stream_read_failed part={} err={}", what, part, e);
    }
}

impl Session {
    async fn run(
        &mut self,
        socket: TcpStream,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (mut reader, mut writer) = socket.into_split();
        let mut buf = [0u8; 8192];
        // A phone that leaves the network leaves the connection open behind it,
        // so whether the session is still worth having is a question about the
        // phone rather than about the socket. See `PHONE_SILENT`.
        let mut silence = tokio::time::interval(SILENCE_CHECK);
        silence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let wakeup = tokio::select! {
                result = reader.read(&mut buf) => Wakeup::Read(result?),
                signal = self.restart.recv() => match signal {
                    // `Lagged` means a second change arrived before this one was
                    // read; either way the answer is the same session restart.
                    Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => Wakeup::Restart,
                    // Unreachable while any sender lives, and dropping a healthy
                    // session over it would be the wrong answer.
                    Err(broadcast::error::RecvError::Closed) => continue,
                },
                signal = self.shutdown.recv() => match signal {
                    Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => Wakeup::Shutdown,
                    Err(broadcast::error::RecvError::Closed) => continue,
                },
                _ = silence.tick() => Wakeup::Silent,
            };
            let n = match wakeup {
                Wakeup::Read(n) => {
                    if n > 0 {
                        self.heard_now();
                    }
                    n
                }
                Wakeup::Restart => {
                    tracing::info!(
                        "carplay.airplay.dropped peer={} reason=render_changed",
                        self.peer
                    );
                    self.close_streams().await;
                    return Ok(());
                }
                // There is nothing left for the phone to send us: the session it
                // was feeding is over on the other side of the bridge.
                Wakeup::Shutdown => {
                    tracing::info!("carplay.airplay.closed peer={} reason=car_session_over", self.peer);
                    self.close_streams().await;
                    return Ok(());
                }
                // Nothing has arrived for long enough that the phone is not
                // coming back on this connection. Its streams are dead already
                // and this session has nothing left to feed, but ending it here
                // is only half the job: a phone that dropped off the network
                // will not knock again until it is asked, and the asking is the
                // iAP2 half's — a fresh session is what carries the start
                // message. So the restart is asked for rather than waited out.
                Wakeup::Silent => {
                    // Most ticks find the phone where it should be, and the
                    // session goes on being waited on.
                    if !self.silent() {
                        continue;
                    }
                    // Unless it came back on a new connection while this one was
                    // going quiet. Then it is not gone, and this session is the
                    // stale one: its streams died with the old connection, but
                    // the ones that replaced them are live, so it lets go of its
                    // own without telling anyone what the screen is doing.
                    if self.newest.load(Ordering::SeqCst) != self.serial {
                        tracing::debug!(
                            "carplay.airplay.retired peer={} reason=superseded",
                            self.peer
                        );
                        self.abandon_streams();
                        return Ok(());
                    }
                    tracing::info!(
                        "carplay.airplay.silent peer={} idle_s={} action=new_session",
                        self.peer,
                        PHONE_SILENT.as_secs()
                    );
                    self.close_streams().await;
                    let _ = self.restart_tx.send(());
                    return Ok(());
                }
            };
            if n == 0 {
                tracing::info!("carplay.airplay.disconnected peer={}", self.peer);
                // A phone that hung up has not necessarily left: the car's
                // session is still up and this half is still open, but nothing
                // will knock again on its own. So this ends like the silence
                // above rather than like the socket — the same nudge, for the
                // same reason, because a fresh iAP2 session is what carries the
                // start message. Without it the phone sits there paired and
                // associated with a session that no longer exists, and nothing
                // asks it back. A session a newer connection has replaced stays
                // quiet: that one is the phone's.
                self.close_streams().await;
                if self.newest.load(Ordering::SeqCst) == self.serial {
                    let _ = self.restart_tx.send(());
                }
                return Ok(());
            }
            self.in_buf.extend_from_slice(&buf[..n]);

            // Pull whole frames out. Before pair-verify completes they are plain
            // RTSP; afterwards each frame is sealed and length framed.
            while let Some(request) = self.next_request()? {
                self.cseq = self.cseq.max(request.cseq());
                let response = self.handle(&request).await;
                let plaintext = response.encode(self.cseq);

                // Replies are framed exactly like the requests once encryption
                // is on: [2B LE ciphertext length][ciphertext][16B tag], with
                // the length doubling as the AAD. The nonce is our own counter,
                // one per direction — it is not the CSeq, which does not
                // increase once per frame.
                let out = match self.cipher_write.clone() {
                    Some(key) => seal_frames(&key, &mut self.write_ctr, &plaintext)?,
                    None => plaintext,
                };

                // The switch to encryption happens *after* the pair-verify
                // reply goes out in the clear, exactly as the phone expects.
                if self.cipher_write.is_none() {
                    if let Some(keys) = self.keys {
                        self.cipher_read = SymKey::from_bytes(&keys.control_read()).ok();
                        self.cipher_write = SymKey::from_bytes(&keys.control_write()).ok();
                        self.status.with(|s| s.carplay.airplay.encrypted = true);
                        tracing::info!("carplay.airplay.encrypted channel=control");
                    }
                }

                writer.write_all(&out).await?;

                // The reply that named the iAP channel has just gone out, which
                // is the first moment the phone can reach the port in it. The
                // session starts here rather than at RECORD because the phone
                // chooses the order of the two, and a session waiting for the
                // step it did not send never begins at all.
                if std::mem::take(&mut self.tunnel_start_pending) {
                    writer.flush().await?;
                    if let Some(tunnel) = self.tunnel.as_mut() {
                        tunnel.start_iap2();
                    }
                }
            }
        }
    }

    /// Notes that the phone has just said something, on whichever of the two
    /// channels heard it.
    fn heard_now(&self) {
        if let Ok(mut at) = self.heard.lock() {
            *at = Some(Instant::now());
        }
    }

    /// Whether the phone has been quiet for longer than `PHONE_SILENT`.
    ///
    /// Nothing is judged before it has been heard at least once. A session that
    /// has not got as far as the clock exchange has no heartbeat to miss, and
    /// ending it would be a guess rather than an observation.
    fn silent(&self) -> bool {
        self.heard
            .lock()
            .ok()
            .and_then(|at| *at)
            .is_some_and(|at| at.elapsed() > PHONE_SILENT)
    }

    fn next_request(
        &mut self,
    ) -> Result<Option<Request>, Box<dyn std::error::Error + Send + Sync>> {
        match self.cipher_read.clone() {
            None => {
                let Some((request, used)) = rtsp::parse(&self.in_buf)? else {
                    return Ok(None);
                };
                self.in_buf.drain(..used);
                Ok(Some(request))
            }
            Some(key) => loop {
                if let Err(e) = open_frames(
                    &key,
                    &mut self.read_ctr,
                    &mut self.frame_buf,
                    &mut self.plain_buf,
                ) {
                    tracing::warn!("carplay.airplay.frame_open_failed ctr={} err={e}", self.read_ctr);
                    return Err(e.into());
                }
                if let Some((request, used)) = rtsp::parse(&self.plain_buf)? {
                    self.plain_buf.drain(..used);
                    return Ok(Some(request));
                }
                if self.in_buf.is_empty() {
                    return Ok(None);
                }
                self.frame_buf
                    .extend_from_slice(&std::mem::take(&mut self.in_buf));
            },
        }
    }

    async fn handle(&mut self, req: &Request) -> Response {
        // One line per request: when the session stalls the last line printed
        // is the whole answer, and silence means the phone stopped talking.
        // `/feedback` and OPTIONS arrive once a second, so they are chatter —
        // logging them buries everything else.
        let chatty = req.method == "OPTIONS" || req.path.to_lowercase().ends_with("/feedback");
        if !chatty {
            tracing::info!(
                "carplay.airplay.request_in method={} path={} bytes={}",
                req.method,
                req.path,
                req.body.len()
            );
        }
        let result = self.route(req).await;
        match result {
            Ok(response) => response,
            Err(e) => {
                tracing::warn!(
                    "carplay.airplay.request_failed method={} path={} err={e}",
                    req.method,
                    req.path
                );
                Response::new(500)
            }
        }
    }

    async fn route(&mut self, req: &Request) -> Result<Response, String> {
        let path = req.path.as_str();
        match req.method.as_str() {
            "OPTIONS" => Ok(Response::new(200).header(
                "Public",
                "ANNOUNCE,SETUP,RECORD,PAUSE,FLUSH,TEARDOWN,OPTIONS,GET_PARAMETER,SET_PARAMETER,POST,GET",
            )),
            "GET" if path == "/info" => self.info(),
            "SETUP" => self.setup(req).await,
            "RECORD" => {
                tracing::info!("carplay.airplay.media_live");
                Ok(Response::new(200))
            }
            // Worth reading: the phone usually names the streams it is dropping,
            // and a session-level teardown right after RECORD means it gave up.
            "TEARDOWN" => {
                self.teardown(req).await;
                Ok(Response::new(200))
            }
            "FLUSH" | "PAUSE" => Ok(Response::new(200)),
            "POST" if path.ends_with("/pair-setup") => self.pair_setup(req).await,
            "POST" if path.ends_with("/pair-verify") => self.pair_verify(req).await,
            // MFiSAP: even wired CarPlay asks, and answering empty ends the
            // session right here.
            "POST" if path.ends_with("/auth-setup") => self.auth_setup(req).await,
            // Answered once a second. With audio running an empty body reads as
            // "the stream died" and the phone stops sending video.
            "POST" if path.ends_with("/feedback") => self.feedback().await,
            "POST" if path.ends_with("/command") => {
                self.command(req);
                Ok(Response::new(200))
            }
            _ => Ok(Response::new(200)),
        }
    }

    /// What the phone has been asked to render, if anything has asked.
    fn negotiated(&self) -> Option<ScreenParams> {
        self.screen.lock().ok().and_then(|g| *g)
    }

    fn info(&self) -> Result<Response, String> {
        // Whatever the screen in force is, or the default until there is one. A
        // number the channel table preconfigures is already in that value: the
        // CarLife session stores it where the car's own numbers would be, so
        // there is one answer here and nothing to choose between.
        let screen = self.negotiated().unwrap_or_default();
        let (width, height) = screen.canvas();
        let fps = screen.fps;
        // The physical size is the other half of what the phone draws at: it
        // turns millimetres back into a dpi, and a wrong one is a picture at the
        // wrong scale. No CarLife message carries a size, so the table says how
        // dense the panel is — 120 dpi where it says nothing — and the size
        // follows from that and the canvas above.
        let channel = self.channel.lock().ok().and_then(|c| c.clone());
        let ppi = info::panel(channel.as_deref()).ppi;
        // Reported before the answer goes out: this is the only place the two
        // halves meet — what the car said its screen is, and what the phone is
        // therefore asked to draw — and the page is where that is worth seeing.
        self.status.with(|s| {
            s.carplay.screen = Some(Screen {
                width: screen.width,
                height: screen.height,
                fps,
                ppi,
                canvas_width: width,
                canvas_height: height,
                width_mm: info::millimetres(width, ppi),
                height_mm: info::millimetres(height, ppi),
            });
        });
        let plist = info::build(&info::Static {
            bt_mac: bt_address(&self.cfg.bt_adapter),
            name: hostname(),
            manufacturer: dt_vendor(),
            width,
            height,
            fps,
            ppi,
            channel,
        })
        .map_err(|e| e.to_string())?;
        // H.264 is the only family on offer: CarLife carries an elementary
        // stream and its head units decode `video/avc`.
        tracing::info!(
            "carplay.airplay.info_answered width={width} height={height} fps={fps} ppi={ppi} codec={}",
            screen::CODEC
        );
        Ok(Response::new(200).plist(plist))
    }

    /// What the phone asks for on `/command`, which is UI behaviour — and the
    /// two things that only appear once a wireless session is moving its iAP2
    /// control channel onto this one.
    ///
    /// `requestUI` is our own icon on the phone's CarPlay screen being tapped.
    /// On that screen the icon means the head unit's own desktop, so the car is
    /// asked for it. The one exception is `url: videoplayback:`, which is the
    /// phone asking for the screen to play video on — CarLife has no way to give
    /// it, so it is left alone.
    ///
    /// `suggestUI` is the phone offering URLs it thinks we might show. Nothing
    /// here shows them.
    fn command(&self, req: &Request) {
        let Ok(dict) = Self::plist_body(&req.body) else {
            Self::describe(&req.body);
            return;
        };
        let params = dict.get("params").and_then(|p| p.as_dictionary());
        let url = params
            .and_then(|p| p.get("url"))
            .and_then(|v| v.as_string())
            .unwrap_or("");
        match dict.get("type").and_then(|v| v.as_string()) {
            Some("requestUI") if url == "videoplayback:" => {
                tracing::info!("carplay.airplay.request_ui url={url:?} action=not_handled")
            }
            Some("requestUI") => {
                tracing::info!("carplay.airplay.request_ui action=ask_car_for_desktop");
                // Nobody to tell is not a failure: the car may have gone.
                let _ = self.desktop.send(());
            }
            Some("suggestUI") => {
                let urls = params
                    .and_then(|p| p.get("urls"))
                    .and_then(|v| v.as_array())
                    .map_or(0, Vec::len);
                tracing::info!("carplay.airplay.suggest_ui urls={urls} shown=false");
            }
            // The phone's own iAP2 session, arriving down the event channel
            // instead of the data stream. The reference accepts both, and the
            // stream is the one used in practice; the bytes are the same either
            // way, so they go to the same link.
            Some("iAPSendMessage") => {
                let data = params
                    .and_then(|p| p.get("data"))
                    .and_then(|v| v.as_data())
                    .map(|d| d.to_vec());
                match (data, self.tunnel.as_ref()) {
                    (Some(data), Some(tunnel)) => {
                        tracing::debug!("carplay.airplay.iap_message_in bytes={}", data.len());
                        tunnel.feed(data);
                    }
                    (Some(_), None) => {
                        tracing::warn!("carplay.airplay.iap_message_ignored reason=no_tunnel")
                    }
                    (None, _) => {
                        tracing::warn!("carplay.airplay.iap_message_ignored reason=no_data")
                    }
                }
            }
            // The move is done: everything the phone needs now travels over
            // Wi-Fi, so the Bluetooth link is only a way back in. Dropping it is
            // what LIVI does, and what the phone expects — leaving it up costs a
            // page and a second iAP2 session the tunnel cannot survive.
            Some("disableBluetooth") => {
                let named = params
                    .and_then(|p| p.get("deviceID"))
                    .and_then(|v| v.as_string())
                    .unwrap_or("");
                let mut phone = None;
                self.status.with(|s| phone = s.carplay.phone.clone());
                match mac_of(named, phone.as_deref()) {
                    Some(mac) => {
                        tracing::info!(
                            "bt.disconnecting mac={mac} reason=disable_bluetooth"
                        );
                        let adapter = self.cfg.bt_adapter.clone();
                        // Not awaited: the phone does not wait for this answer
                        // before it stops using Bluetooth, and holding the
                        // request open on a D-Bus call serves nobody.
                        tokio::spawn(async move {
                            if let Err(e) = crate::wireless::disconnect_phone(&mac, &adapter).await
                            {
                                tracing::warn!(
                                    "bt.disconnect_failed mac={mac} err={e}"
                                );
                            }
                        });
                    }
                    None => tracing::warn!(
                        "bt.disconnect_skipped reason=no_address device={named:?}"
                    ),
                }
            }
            _ => Self::describe(&req.body),
        }
    }

    /// Prints a request body the phone sent, as one line of JSON.
    ///
    /// Used for the commands nothing here acts on: the body is then the whole of
    /// what there is to know, and every one of them is printed. The per-second
    /// requests never reach here: `/feedback` is answered before this and
    /// OPTIONS carries no body. `setup` and `teardown` print their bodies under
    /// the same event name, so `airplay.body` means one thing throughout; which
    /// request it belongs to is on the line before, so the path is not repeated.
    fn describe(body: &[u8]) {
        match Self::plist_body(body) {
            Ok(dict) => tracing::info!("carplay.airplay.body json={}", to_json(&dict)),
            Err(e) => tracing::warn!(
                "carplay.airplay.body_unparsed err={e} bytes={} data={}",
                body.len(),
                hex::encode(&body[..body.len().min(24)])
            ),
        }
    }

    /// Closes the streams the phone names. It names them by type, and it opens
    /// a fresh one for every track change, so this fires constantly during
    /// normal use — leaving it as a no-op leaked a port, a socket and a task
    /// per song, and kept `/feedback` reporting a stream that was already gone.
    async fn teardown(&mut self, req: &Request) {
        let named = match Self::plist_body(&req.body) {
            Ok(dict) => {
                tracing::info!("carplay.airplay.body json={}", to_json(&dict));
                dict.get("streams").and_then(|v| v.as_array()).cloned()
            }
            Err(e) => {
                tracing::warn!(
                    "carplay.airplay.body_unparsed err={e} bytes={} data={}",
                    req.body.len(),
                    hex::encode(&req.body[..req.body.len().min(24)])
                );
                None
            }
        };

        for stream_type in named
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_dictionary()?.get("type")?.as_signed_integer())
        {
            self.close_stream(stream_type).await;
            tracing::info!("carplay.airplay.stream_torn_down stream={stream_type}");
        }
    }

    /// Closes every stream, without the phone having asked.
    async fn close_streams(&mut self) {
        for stream_type in self.pumps.keys().copied().collect::<Vec<_>>() {
            while self.pumps.contains_key(&stream_type) {
                self.close_stream(stream_type).await;
            }
        }
    }

    /// Lets go of the streams without saying anything about them.
    ///
    /// For a session a newer connection has already replaced: its streams died
    /// with the old connection, but the sink and the audio clock now belong to
    /// the session that took over, and telling them the screen stopped is the
    /// one thing that would break it.
    fn abandon_streams(&mut self) {
        for (_, pumps) in self.pumps.drain() {
            for pump in pumps {
                pump.task.abort();
            }
        }
    }

    /// Closes the oldest instance of one stream type, which is the one the phone
    /// means when it tears a type down.
    ///
    /// Aborting the pump is the only way a UDP stream ever ends, and it means
    /// the pump never reaches its own goodbye — so the sink is told here or not
    /// at all, or whoever mixes keeps a channel for a stream nobody will ever
    /// send again. What the sink is told names the instance, because the channel
    /// may by now be on a newer one of the same type and must not be closed with
    /// it. A type with nothing open says nothing: a teardown that arrives twice,
    /// or once after the stream ended on its own, is not news.
    async fn close_stream(&mut self, stream_type: i64) {
        // The iAP channel is not a pump: it is an iAP2 session of its own, and
        // letting go of it both ends that session and clears the handover gate,
        // which is what lets Bluetooth be paged again.
        if stream_type == 130 {
            if self.tunnel.take().is_some() {
                tracing::info!("carplay.airplay.iap_tunnel_torn_down");
            }
            return;
        }
        let mut closed = None;
        if let Some(pumps) = self.pumps.get_mut(&stream_type) {
            if !pumps.is_empty() {
                let pump = pumps.remove(0);
                pump.task.abort();
                closed = Some(pump.connection_id);
            }
        }
        if self.pumps.get(&stream_type).is_some_and(Vec::is_empty) {
            self.pumps.remove(&stream_type);
        }
        let Some(connection_id) = closed else {
            tracing::debug!("carplay.airplay.stream_already_gone stream={stream_type}");
            return;
        };
        self.audio_clock
            .lock()
            .await
            .close_type(stream_type, connection_id);
        match stream_type {
            110 => self.video_sink.push(StreamEvent::ScreenActive(false)),
            100 | 101 | 102 => self.audio_sink.push(StreamEvent::AudioStopped {
                stream: StreamId {
                    stream_type: stream_type as u8,
                    connection_id,
                },
            }),
            _ => {}
        }
        // Only the main audio stream ever carries a microphone, and a phone that
        // has stopped sending us audio is not listening for any either.
        if stream_type == 100 {
            self.mic.close().await;
        }
    }

    /// The media-clock reply. Silence here is what made the phone stop sending
    /// video a second after the audio stream opened.
    async fn feedback(&mut self) -> Result<Response, String> {
        let clock = self.audio_clock.lock().await;
        if clock.is_empty() {
            return Ok(Response::new(200));
        }
        let now = self.timing.lock().await.now_u64();
        let streams: Vec<Value> = clock
            .report(now)
            .into_iter()
            .map(Value::Dictionary)
            .collect();

        let mut body = Dictionary::new();
        body.insert("streams".into(), Value::Array(streams));
        let plist = to_binary(&Value::Dictionary(body)).map_err(|e| e.to_string())?;

        // Everything but the rate is reported only once the stream is anchored,
        // so the summary is what says whether the phone is being told about a
        // stream at all, and whether it knows where in it we are. Printed on
        // change — it is otherwise once-a-second chatter like the request.
        let summary = clock.summary();
        if self.feedback_log != summary {
            tracing::info!("carplay.airplay.feedback_streams summary={summary:?}");
            self.feedback_log = summary;
        }
        Ok(Response::new(200).plist(plist))
    }

    async fn pair_setup(&mut self, req: &Request) -> Result<Response, String> {
        let setup = self.setup.get_or_insert_with(PairSetup::new);
        let mut pairings = self.pairings.lock().await;
        let reply = setup.handle(&req.body, &mut pairings)?;
        Ok(Response::new(200)
            .header("Content-Type", PAIRING_CONTENT_TYPE)
            .body(reply))
    }

    async fn pair_verify(&mut self, req: &Request) -> Result<Response, String> {
        let verify = self.verify.get_or_insert_with(PairVerify::new);
        let pairings = self.pairings.lock().await;
        let reply = verify.handle(&req.body, &pairings)?;
        if let Some(keys) = verify.keys() {
            self.keys = Some(keys);
            self.event_sink.set_key(keys.events_write()).await;
            self.status.with(|s| s.carplay.airplay.verified = true);
            tracing::info!("carplay.pairing.verified");
        }
        Ok(Response::new(200)
            .header("Content-Type", PAIRING_CONTENT_TYPE)
            .body(reply))
    }

    /// MFiSAP. The phone sends `[1B version][32B controller X25519 public]` and
    /// expects our own ephemeral public key, the MFi certificate, and the
    /// coprocessor signature over both public keys — the signature AES-CTR
    /// encrypted under a key derived from the X25519 secret.
    ///
    /// Without a real answer the phone hangs up immediately after pair-verify,
    /// which looks like a pairing bug if you do not know this step exists.
    async fn auth_setup(&mut self, req: &Request) -> Result<Response, String> {
        if req.body.len() != 33 || req.body[0] != MFISAP_VERSION {
            return Err(format!(
                "bad /auth-setup ({} bytes, version {:?})",
                req.body.len(),
                req.body.first()
            ));
        }
        let peer_pub: [u8; 32] = req.body[1..33].try_into().map_err(|_| "short peer key")?;

        let eph = crypto::Ephemeral::generate();
        let our_pub = eph.public;
        let shared = eph.shared(&peer_pub);
        if shared.iter().all(|b| *b == 0) {
            return Err("degenerate X25519 shared secret".into());
        }

        let aes_key = sha1_all(&[b"AES-KEY", &shared]);
        let aes_iv = sha1_all(&[b"AES-IV", &shared]);

        let cert = self
            .mfi
            .read_certificate()
            .await
            .map_err(|e| e.to_string())?;
        // 2.0 coprocessors sign a SHA-1 digest, 3.0 signs SHA-256. Ours reports 3.
        let major = self.mfi.protocol_major().await.map_err(|e| e.to_string())?;
        let digest = if major == 2 {
            sha1_all(&[&our_pub, &peer_pub]).to_vec()
        } else {
            sha256_all(&[&our_pub, &peer_pub]).to_vec()
        };
        let sig = self.mfi.sign(digest).await.map_err(|e| e.to_string())?;
        let enc_sig = aes_ctr128(&aes_key[..16], &aes_iv[..16], &sig);

        tracing::info!(
            "carplay.airplay.auth_setup_signed cert_bytes={} sig_bytes={} digest=sha{}",
            cert.len(),
            enc_sig.len(),
            if major == 2 { 1 } else { 256 }
        );

        let mut out = Vec::new();
        out.extend_from_slice(&our_pub);
        out.extend_from_slice(&(cert.len() as u32).to_be_bytes());
        out.extend_from_slice(&cert);
        out.extend_from_slice(&(enc_sig.len() as u32).to_be_bytes());
        out.extend_from_slice(&enc_sig);
        Ok(Response::new(200)
            .header("Content-Type", OCTET_CONTENT_TYPE)
            .body(out))
    }

    async fn setup(&mut self, req: &Request) -> Result<Response, String> {
        let dict = Self::plist_body(&req.body)?;
        // The whole body and nothing else: the keys were a table of contents for
        // a body nobody could see, and the body is short enough to print.
        tracing::info!("carplay.airplay.body json={}", to_json(&dict));
        let has_streams = dict
            .get("streams")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);

        if has_streams {
            self.setup_streams(dict).await
        } else {
            self.setup_session(dict).await
        }
    }

    async fn setup_session(&mut self, dict: Dictionary) -> Result<Response, String> {
        let keys = self.keys.ok_or("SETUP before pair-verify")?;
        // The phone tells us where to send our timing requests; it is the only
        // way the two clocks ever agree.
        let peer_timing_port = dict
            .get("timingPort")
            .and_then(|v| v.as_signed_integer())
            .map(|v| v as u16);

        // The phone opens this before it asks for any stream. It is bidirectional:
        // we push input down it, and it posts us requests that each need a 200.
        //
        // Every port here is the kernel's choice, not one of ours. A session that
        // has just ended leaves a socket or two behind for a moment — the event
        // channel's write half in particular is held by `EventSink` until the
        // connection actually goes away — and binding a number one of them still
        // holds fails with `EADDRINUSE`. That is precisely what the second SETUP
        // after a display change used to hit: the ports were handed out from a
        // counter that restarted at 45000 for every session.
        let listener = TcpListener::bind(("::", 0))
            .await
            .map_err(|e| e.to_string())?;
        let event_port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let read_key = SymKey::from_bytes(&keys.events_read()).map_err(|e| e.to_string())?;
        let sink = self.event_sink.clone();
        let responder = sink.clone();
        let event_status = self.status.clone();
        tokio::spawn(async move {
            let Ok((socket, peer)) = listener.accept().await else {
                return;
            };
            tracing::info!("carplay.event.channel_opened peer={peer}");
            event_status.with(|s| s.carplay.airplay.event_port = Some(event_port));
            // Input is latency-sensitive and always written in pairs. With Nagle
            // on, a Wi-Fi link holds the second report until the first is acked
            // and then flushes whatever piled up as one segment, which the phone
            // reads as a single tick — a d-pad nudge and its recentring cancel
            // out.
            let _ = socket.set_nodelay(true);
            let (read, write) = socket.into_split();
            let id = sink.attach_writer(write).await;
            pump_events(read, read_key, responder, id).await;
        });

        let timing_port = self.spawn_timing(peer_timing_port).await?;

        let mut body = Dictionary::new();
        body.insert(
            "eventPort".into(),
            Value::Integer((event_port as i64).into()),
        );
        body.insert(
            "timingPort".into(),
            Value::Integer((timing_port as i64).into()),
        );
        // `viewAreas` is advertised unconditionally by LIVI; omitting it changes
        // what the phone asks for downstream.
        body.insert(
            "enabledFeatures".into(),
            Value::Array(vec![
                Value::String("iAPChannel".into()),
                Value::String("viewAreas".into()),
            ]),
        );

        let plist = to_binary(&Value::Dictionary(body)).map_err(|e| e.to_string())?;
        self.status.with(|s| {
            s.carplay.airplay.event_port = Some(event_port);
            s.carplay.airplay.timing_port = Some(timing_port);
        });
        tracing::info!("carplay.airplay.setup_answered event_port={event_port}");
        Ok(Response::new(200).plist(plist))
    }

    async fn setup_streams(&mut self, dict: Dictionary) -> Result<Response, String> {
        let keys = self.keys.ok_or("stream SETUP before pair-verify")?;
        let streams = dict
            .get("streams")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut replies: Vec<Value> = Vec::new();

        for stream in streams {
            let table = match stream.as_dictionary() {
                Some(d) => d,
                None => continue,
            };
            let type_ = table.get("type").and_then(|v| v.as_signed_integer());
            let stream_id = table
                .get("streamConnectionID")
                .and_then(|v| v.as_signed_integer())
                .unwrap_or(1);
            let key =
                SymKey::from_bytes(&keys.media(stream_id as u64)).map_err(|e| e.to_string())?;

            match type_ {
                Some(110) => {
                    let listener = TcpListener::bind(("::", 0))
                        .await
                        .map_err(|e| e.to_string())?;
                    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
                    let sink = self.video_sink.clone();
                    let status = self.status.clone();
                    let led = self.led.clone();
                    let task = tokio::spawn(async move {
                        let Ok((socket, _)) = listener.accept().await else {
                            return;
                        };
                        tracing::info!("carplay.video.stream_attached");
                        status.with(|s| s.carplay.video.active = true);
                        // The phone's screen is arriving, which is as far as
                        // this side of the bridge gets on its own: the light
                        // goes steady here rather than at pair-verify, because
                        // a handshake that stalls would leave a phone that is
                        // not being mirrored looking connected.
                        led.carplay(Carplay::Linked);
                        Self::pump_video(socket, key, sink, status).await;
                    });
                    self.open_pump(
                        110,
                        Pump {
                            task,
                            connection_id: stream_id,
                            _control: None,
                        },
                    );
                    self.streams.insert(stream_id as u64, port);
                    let mut entry = Dictionary::new();
                    entry.insert("type".into(), Value::Integer(110.into()));
                    entry.insert("dataPort".into(), Value::Integer((port as i64).into()));
                    replies.push(Value::Dictionary(entry));
                }
                Some(100) | Some(101) | Some(102) => {
                    let socket = UdpSocket::bind(("::", 0))
                        .await
                        .map_err(|e| e.to_string())?;
                    let port = socket.local_addr().map_err(|e| e.to_string())?.port();
                    let fmt = table
                        .get("audioFormat")
                        .and_then(|v| v.as_signed_integer())
                        .unwrap_or(0);
                    let codec = AudioCodec::from_format(fmt);
                    let stream_type = type_.unwrap_or(0);
                    // The phone labels each stream itself — `media`, `telephony`,
                    // `speechRecognition` — and each name is a channel of its
                    // own at the far end, so the label is carried through as-is.
                    let audio_type = table
                        .get("audioType")
                        .and_then(|v| v.as_string())
                        .unwrap_or("default")
                        .to_ascii_lowercase();
                    // The phone's jitter buffer: how far the sound lags the
                    // samples it has already handed us, and so what `/feedback`
                    // has to subtract. Not sending it means there is none — and
                    // a stream the phone calls buffered is sent in lumps sized
                    // against exactly this, so the number is worth printing.
                    let latency = table
                        .get("audioLatencyMs")
                        .and_then(|v| v.as_signed_integer())
                        .filter(|v| *v > 0)
                        .map(|v| v as u64)
                        .unwrap_or(0);
                    // How many frames of the codec the phone packs into each
                    // packet. Nothing here takes a packet apart, so it is
                    // reported rather than acted on: it is the one number that
                    // says whether the frames arriving at the media channel are
                    // the single frames its decoder expects.
                    let frames_per_packet = table
                        .get("framesPerPacket")
                        .and_then(|v| v.as_signed_integer())
                        .unwrap_or(0) as u32;
                    // The whole table, because the codec's own name for a stream
                    // can be as much as 8% away from the rate it carries, and a
                    // field naming the rate outright would settle it.
                    tracing::debug!("carplay.audio.setup_table stream={stream_type} table={table:?}");
                    tracing::info!(
                        "carplay.audio.stream_ready stream={stream_type} type={audio_type:?} \
                         format=0x{fmt:x} codec={codec:?} frames_per_packet={frames_per_packet} \
                         latency_ms={latency}"
                    );
                    {
                        *self.codec.lock().await = codec;
                    }
                    // One entry per stream type: the phone re-opens its audio
                    // stream on every track change, so a type already listed is
                    // replaced rather than added to.
                    self.status.with(|s| {
                        s.carplay
                            .audio
                            .retain(|a| a.stream_type != stream_type as u8);
                        s.carplay.audio.push(Audio {
                            stream_type: stream_type as u8,
                            name: audio_type.clone(),
                            codec: codec_name(codec).to_string(),
                            rate: codec.rate(),
                            channels: codec.channels(),
                            frames_per_packet,
                            latency_ms: latency,
                            ..Audio::default()
                        });
                    });
                    // MainAudio is bidirectional, and this is how the phone says
                    // so: a send port in its own SETUP, alongside an audio type
                    // that has someone talking at the other end. It is the only
                    // inbound stream that names a port — for every other one it
                    // is we who do.
                    let mic_port = table
                        .get("dataPort")
                        .and_then(|v| v.as_signed_integer())
                        .unwrap_or(0);
                    if stream_type == 100
                        && matches!(audio_type.as_str(), "telephony" | "speechrecognition")
                        && (1..=65535).contains(&mic_port)
                    {
                        self.open_mic(stream_id, mic_port as u16, codec, frames_per_packet, keys)
                            .await;
                    }
                    // Which of the car's channels this stream feeds is decided by
                    // `audio_type`, downstream; what the car is told its samples
                    // are is `delivered`, which is the same as the negotiated
                    // codec until a prompt's Opus is decoded into 16 kHz PCM.
                    let delivered = audio::delivered(codec, &audio_type);
                    let name = audio_type.clone();
                    self.audio_sink.push(StreamEvent::AudioConfig {
                        stream: StreamId {
                            stream_type: stream_type as u8,
                            connection_id: stream_id,
                        },
                        codec: delivered,
                        name: audio_type,
                    });
                    // The clock counts the samples the phone is sending, so it
                    // is told the rate on the wire — not the one the car is
                    // given. Telling it 16 kHz for a 48 kHz stream would put the
                    // reported playout position 3× off.
                    let id = self.audio_clock.lock().await.open(
                        stream_type,
                        stream_id,
                        codec.rate(),
                        latency,
                    );
                    let sink = self.audio_sink.clone();
                    let clock = self.audio_clock.clone();
                    let status = self.status.clone();
                    let stream = StreamId {
                        stream_type: stream_type as u8,
                        connection_id: stream_id,
                    };
                    let task = tokio::spawn(async move {
                        tracing::info!("carplay.audio.stream_attached");
                        Self::pump_audio(socket, key, stream, &name, codec, sink, clock, id, status)
                            .await;
                    });
                    // Bound but never read; a failure here is not worth failing
                    // the whole setup over.
                    let control = match UdpSocket::bind(("::", 0)).await {
                        Ok(s) => Some(s),
                        Err(e) => {
                            tracing::warn!("carplay.audio.control_port_unbound err={e}");
                            None
                        }
                    };
                    let control_port = control
                        .as_ref()
                        .and_then(|s| s.local_addr().ok())
                        .map_or(0, |a| a.port());
                    self.open_pump(
                        stream_type,
                        Pump {
                            task,
                            connection_id: stream_id,
                            _control: control,
                        },
                    );
                    self.streams.insert(stream_id as u64, port);
                    let mut entry = Dictionary::new();
                    entry.insert("type".into(), Value::Integer(type_.unwrap().into()));
                    entry.insert("dataPort".into(), Value::Integer((port as i64).into()));
                    entry.insert(
                        "controlPort".into(),
                        Value::Integer((control_port as i64).into()),
                    );
                    entry.insert(
                        "streamConnectionID".into(),
                        Value::Integer(stream_id.into()),
                    );
                    replies.push(Value::Dictionary(entry));
                }
                // 130 is iAP-over-CarPlay: the phone moving its iAP2 control
                // channel onto this session. Only the iAP channel is this — the
                // same number is used for video settings and video overlay,
                // whose bodies mean something else entirely, and taking one of
                // those for the control channel would strand the phone. Without
                // a wireless session behind it there is nowhere to put iAP2, so
                // the answer stays the port nobody listens on.
                Some(130) => {
                    let named = table
                        .get("clientTypeUUID")
                        .and_then(|v| v.as_string())
                        .unwrap_or("");
                    let seed = table.get("seed").and_then(seed_text);
                    let mut entry = Dictionary::new();
                    entry.insert("type".into(), Value::Integer(130.into()));
                    entry.insert("streamID".into(), Value::Integer(stream_id.into()));
                    match self.open_tunnel(named, seed, keys.shared).await {
                        Ok(port) => {
                            entry.insert("dataPort".into(), Value::Integer((port as i64).into()));
                        }
                        Err(why) => {
                            tracing::warn!("carplay.airplay.iap_tunnel_skipped reason={why}");
                            entry.insert("dataPort".into(), Value::Integer(UNUSED_PORT.into()));
                        }
                    }
                    replies.push(Value::Dictionary(entry));
                }
                other => {
                    let mut entry = Dictionary::new();
                    entry.insert("type".into(), Value::Integer(other.unwrap_or(0).into()));
                    entry.insert("dataPort".into(), Value::Integer(UNUSED_PORT.into()));
                    replies.push(Value::Dictionary(entry));
                }
            }
        }

        let mut body = Dictionary::new();
        body.insert("streams".into(), Value::Array(replies));
        let plist = to_binary(&Value::Dictionary(body)).map_err(|e| e.to_string())?;
        Ok(Response::new(200).plist(plist))
    }

    /// Opens the iAP2 data stream the phone just asked for, and answers with the
    /// port to connect to.
    ///
    /// Nothing more than a listener is started here: the phone connects to it
    /// during the media handshake, and the session on it begins once the reply
    /// carrying the port has been written — see `run`. Declining is not a
    /// failure — a wired run, or a stream that is not the iAP channel, is
    /// answered exactly as it was before there was a tunnel at all.
    async fn open_tunnel(
        &mut self,
        named: &str,
        seed: Option<String>,
        shared: [u8; 32],
    ) -> Result<u16, String> {
        let Some(setup) = self.tunnel_setup.clone() else {
            return Err("not a wireless session".into());
        };
        if named != tunnel::IAP_DATASTREAM_UUID {
            return Err(format!("client {named:?} is not the iAP channel"));
        }
        let Some(seed) = seed else {
            return Err("the SETUP carried no seed".into());
        };
        if self.tunnel.is_some() {
            return Err("a tunnel is already open on this session".into());
        }
        let (opened, port) = tunnel::Tunnel::open(shared, &seed, setup)
            .await
            .map_err(|e| e.to_string())?;
        self.tunnel = Some(opened);
        // The port is only reachable once the phone has read it, so the session
        // is armed here and started by the writer below.
        self.tunnel_start_pending = true;
        Ok(port)
    }

    /// Opens the microphone uplink for a stream the phone says it will hear on.
    ///
    /// The packets go back to the phone's own address on the port it named. A
    /// failure here is not worth failing the SETUP over: the downlink is already
    /// running, and the worst case is an uplink whose microphone does nothing.
    async fn open_mic(
        &self,
        stream_id: i64,
        port: u16,
        codec: AudioCodec,
        frames_per_packet: u32,
        keys: VerifyKeys,
    ) {
        let key = match SymKey::from_bytes(&keys.media_input(stream_id as u64)) {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!("carplay.mic.key_unusable stream={stream_id} err={e}");
                return;
            }
        };
        let config = MicConfig::from_codec(codec, frames_per_packet);
        let peer = SocketAddr::new(self.peer.ip(), port);
        match self.mic.open(peer, 100, key, config).await {
            Ok(()) => {
                self.status.with(|s| {
                    s.carplay.mic = Some(Mic {
                        peer: Some(peer.to_string()),
                        codec: config.codec.name().to_uppercase(),
                        rate: config.sample_rate,
                        channels: config.channels,
                    });
                });
                tracing::info!(
                    "carplay.mic.uplink_open peer={peer} codec={:?} rate_hz={} channels={} \
                     samples_per_packet={}",
                    config.codec,
                    config.sample_rate,
                    config.channels,
                    config.samples_per_packet
                );
            }
            Err(e) => tracing::warn!("carplay.mic.uplink_failed peer={peer} err={e}"),
        }
    }

    /// Registers a running stream under the type the phone will name it with.
    ///
    /// A second stream of a type that is already open is added rather than put
    /// in place of the first: the phone re-opens its audio without tearing the
    /// old stream down first, and the two are told apart by their connection —
    /// the teardown that follows names only the type, and it is the older of the
    /// two it means. Closing the older one here instead took the sound with it
    /// whenever a prompt opened.
    fn open_pump(&mut self, stream_type: i64, pump: Pump) {
        self.pumps.entry(stream_type).or_default().push(pump);
    }

    fn plist_body(bytes: &[u8]) -> Result<Dictionary, String> {
        if bytes.is_empty() {
            return Ok(Dictionary::new());
        }
        match from_bytes::<Value>(bytes) {
            Ok(Value::Dictionary(d)) => Ok(d),
            Ok(_) => Ok(Dictionary::new()),
            Err(e) => Err(format!("unreadable plist body: {e}")),
        }
    }

    /// Video arrives on TCP as a stream of length-framed messages.
    async fn pump_video(
        socket: TcpStream,
        key: SymKey,
        sink: Arc<dyn MediaSink>,
        status: Arc<Status>,
    ) {
        let mut rx = VideoReceiver::new(key, sink.clone());
        sink.push(StreamEvent::ScreenActive(true));
        let mut socket = socket;
        let mut header = [0u8; screen::HEADER_LEN];
        let mut frames: u64 = 0;
        loop {
            if let Err(e) = socket.read_exact(&mut header).await {
                ended("carplay.video", "header", &e);
                break;
            }
            let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
            if len > 8 * 1024 * 1024 {
                tracing::warn!("carplay.video.frame_rejected len={len} reason=implausible");
                break;
            }
            // KeepAlive, ForceKeyFrame and friends carry no payload at all.
            // Skipping them is routine — treating it as fatal dropped the whole
            // stream the first time the phone sent one.
            if len == 0 {
                continue;
            }
            let mut body = vec![0u8; len];
            if let Err(e) = socket.read_exact(&mut body).await {
                ended("carplay.video", "body", &e);
                break;
            }
            // The codec description is the one message that says which profile
            // and level the stream is in, and it says it once.
            if let Some(format) = rx.feed(&header, &body) {
                status.with(|s| {
                    s.video.profile = Some(format.profile);
                    s.video.level = Some(format.level);
                });
            }
            frames += 1;
        }
        sink.push(StreamEvent::ScreenActive(false));
        // Both counts, because they differ: `frames` counts messages read off
        // the socket, and the codec description and the keepalives are messages
        // too. Only the second is pictures.
        tracing::info!(
            "carplay.video.stream_closed messages={frames} frames={}",
            rx.frames()
        );
        status.with(|s| {
            s.carplay.video.active = false;
            s.carplay.video.messages = frames;
            s.carplay.video.frames = rx.frames();
        });
    }

    /// Audio arrives on UDP, one frame per datagram.
    async fn pump_audio(
        socket: UdpSocket,
        key: SymKey,
        stream: StreamId,
        name: &str,
        codec: AudioCodec,
        sink: Arc<dyn MediaSink>,
        clock: Arc<Mutex<AudioClock>>,
        id: u64,
        status: Arc<Status>,
    ) {
        let stream_type = stream.stream_type;
        let mut rx = AudioReceiver::new(key, stream, codec, name, sink.clone());
        let mut buf = [0u8; 2048];
        let mut packets: u64 = 0;
        // Sound that stutters is either packets that never arrived or ones that
        // arrived in lumps. The sequence number counts the first, the gap
        // between arrivals measures the second, and neither is visible from
        // anywhere else — so both are reported, worst gap and all. A counter
        // that steps backwards is a third thing: a packet that overtook one
        // already here, which the decoder sees as frames out of turn unless the
        // queue puts them back in the order they were stamped.
        let mut last_seq: Option<u16> = None;
        let mut missing: u64 = 0;
        let mut out_of_order: u64 = 0;
        let mut duplicates: u64 = 0;
        let mut worst_gap = Duration::ZERO;
        let mut last_at = Instant::now();
        // The buffered stream is re-timed: frames wait here for the moment the
        // phone's own timestamp gives them and leave one at a time, so the
        // consumer never sees the lump they arrived in. Every other stream is
        // already arriving at its own playback rate and goes straight through.
        let paced = stream_type == BUFFERED_AUDIO;
        let mut queue: VecDeque<Pending> = VecDeque::new();
        // Where the frame before this one was due; the next is measured from it.
        let mut aired = (0u64, tokio::time::Instant::now());
        // The frame the car was given last, by the phone's own counter. Anything
        // stamped at or before it is a frame whose moment has passed — playing it
        // now would put it after sound that has already gone out.
        let mut sent: Option<u64> = None;
        loop {
            // A frame waiting to go out is the other thing this loop waits on.
            // With none there is nothing to wait for but the next packet.
            let front = queue.front().map(|f| f.timestamp_us);
            tokio::select! {
                biased;
                _ = wait_due(&mut aired, front.unwrap_or(0)), if front.is_some() => {
                    if let Some(frame) = queue.pop_front() {
                        sent = Some(frame.timestamp_us);
                        let opened = rx.decode(&frame.sealed);
                        if let Some((timestamp_us, data)) = opened {
                            sink.push(StreamEvent::AudioPacket {
                                stream,
                                timestamp_us,
                                data,
                            });
                        }
                    }
                }
                got = socket.recv(&mut buf) => {
                    let n = match got {
                        Ok(n) => n,
                        // UDP has no end of file, so anything here is the socket
                        // itself failing — worth a line, unlike a close.
                        Err(e) => {
                            tracing::warn!("carplay.audio.stream_recv_failed stream={} err={}", name, e);
                            break;
                        }
                    };
                    let now = Instant::now();
                    let gap = now - last_at;
                    worst_gap = worst_gap.max(gap);
                    last_at = now;
                    // The RTP timestamp is the phone's own sample counter; it is
                    // what /feedback reports back as the playout position.
                    if n >= audio::RTP_HEADER_LEN {
                        let ts = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                        clock.lock().await.note(id, ts);
                        let seq = u16::from_be_bytes([buf[2], buf[3]]);
                        if let Some(prev) = last_seq {
                            // Wrapping subtraction, so a counter that rolls over
                            // reads as the one packet it is. Half the space is
                            // forward and half backwards, which is what tells a
                            // gap apart from a packet that overtook another.
                            match seq.wrapping_sub(prev) {
                                0 => duplicates += 1,
                                step if step <= u16::MAX / 2 => missing += u64::from(step - 1),
                                _ => out_of_order += 1,
                            }
                        }
                        last_seq = Some(seq);
                    }
                    if paced {
                        // Held for its moment rather than decoded here: the
                        // counter is in the clear, so a frame can be timed
                        // before it is opened, and the decode it needs is
                        // spent when the moment comes.
                        if audio::could_be_a_frame(&buf[..n]) {
                            let frame = Pending {
                                timestamp_us: audio::sample_stamp(codec.rate(), &buf[..n]),
                                sealed: buf[..n].to_vec(),
                            };
                            hold(&mut queue, frame, sent);
                        }
                    } else {
                        rx.feed(&buf[..n]);
                    }
                    packets += 1;
                    // Once, and only to settle a question: if this appears right
                    // after `carplay.audio.stream_attached` but sound comes seconds
                    // later, the wait is in the player. If this line itself is
                    // late, the wait was the phone opening the audio stream.
                    if packets == 1 {
                        tracing::debug!("carplay.audio.first_packet");
                    }
                }
            }
        }
        // Whatever ended it — the phone tearing the stream down, or the socket
        // failing — stop announcing it. A stream that is gone but still named
        // in `/feedback` is one the phone keeps asking about.
        clock.lock().await.close(id);
        sink.push(StreamEvent::AudioStopped { stream });
        tracing::info!("carplay.audio.stream_closed packets={packets} missing={missing}");
        // The counters only exist here, and the stream is over, so this is the
        // last moment they can be reported at all.
        status.with(|s| {
            if let Some(a) = s
                .carplay
                .audio
                .iter_mut()
                .find(|a| a.stream_type == stream_type)
            {
                a.packets = packets;
                a.missing = missing;
                a.out_of_order = out_of_order;
                a.duplicates = duplicates;
                a.worst_gap_ms = worst_gap.as_millis() as u64;
            }
        });
    }

    /// Clock sync on the timing port.
    ///
    /// This is *not* NTP: it is a 32-byte RTCP-shaped packet (`0x80`, then type
    /// 210 for a request and 211 for a reply) with three 64-bit timestamps.
    /// The accessory drives it — it sends a request every second to the port the
    /// phone named in its SETUP. Without that the phone drops the session after
    /// a few seconds, which looks exactly like a codec failure.
    /// Also returns the port it landed on: the phone is told to send here, so
    /// the kernel's choice has to be read back rather than assumed.
    async fn spawn_timing(&mut self, peer_timing_port: Option<u16>) -> Result<u16, String> {
        let socket = Arc::new(
            UdpSocket::bind(("::", 0))
                .await
                .map_err(|e| e.to_string())?,
        );
        let port = socket.local_addr().map_err(|e| e.to_string())?.port();

        // Two jobs on one socket: answer the phone's requests, and read the
        // replies to ours. Both stamp with the same steered clock.
        let rx = socket.clone();
        let rx_clock = self.timing.clone();
        let rx_status = self.status.clone();
        let rx_heard = self.heard.clone();
        let reader = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let Ok((n, from)) = rx.recv_from(&mut buf).await else {
                    break;
                };
                if n < 32 {
                    continue;
                }
                // Either direction of the exchange is the phone being there, and
                // this is the heartbeat the silence check leans on: the phone
                // answers once a second for as long as it has a network.
                if let Ok(mut at) = rx_heard.lock() {
                    *at = Some(Instant::now());
                }
                if buf[1] == TIMING_REQUEST {
                    let mut resp = [0u8; 32];
                    resp[0] = 0x80;
                    resp[1] = TIMING_RESPONSE;
                    resp[2..4].copy_from_slice(&7u16.to_be_bytes());
                    // Its transmit time becomes our originate time.
                    resp[8..16].copy_from_slice(&buf[24..32]);
                    let now = rx_clock.lock().await.now();
                    resp[16..24].copy_from_slice(&now);
                    resp[24..32].copy_from_slice(&now);
                    let _ = rx.send_to(&resp, from).await;
                } else if buf[1] == TIMING_RESPONSE {
                    let at = |r: std::ops::Range<usize>| -> u64 {
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&buf[r]);
                        u64::from_be_bytes(b)
                    };
                    // T1 is what we sent, echoed back; T2 and T3 are the phone's
                    // receive and transmit; T4 is our arrival. Offset and
                    // round-trip time fall out of the four.
                    let (t1, t2, t3) = (at(8..16), at(16..24), at(24..32));
                    let mut clock = rx_clock.lock().await;
                    let t4 = clock.now_u64();
                    let offset = (ntp_diff(t2, t1) + ntp_diff(t3, t4)) / 2.0;
                    let rtt = ntp_diff(t4, t1) - ntp_diff(t3, t2);
                    // Applied, not just reported: `/feedback` has to answer in
                    // the phone's domain, and this is the only place it is
                    // learned. The clock decides what a sample is worth.
                    clock.sample(offset, rtt);
                    // Kept as well as applied: how far apart the two clocks are,
                    // and over how long a round trip that was measured, is the
                    // pair of numbers that says whether audio timing can be
                    // trusted at all.
                    rx_status.with(|s| {
                        s.carplay.airplay.clock_offset_us = Some(offset as i64);
                        s.carplay.airplay.clock_rtt_us = Some(rtt as i64);
                    });
                }
            }
        });
        self._timing.push(StopOnDrop(reader));

        // And drive our own requests, so the phone has a clock to work against.
        if let Some(p) = peer_timing_port.filter(|p| *p > 0) {
            let mut target = self.peer;
            target.set_port(p);
            let tx = socket.clone();
            let tx_clock = self.timing.clone();
            tracing::info!("carlife.clock.driving target={target}");
            let ticker = tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                loop {
                    tick.tick().await;
                    let mut pkt = [0u8; 32];
                    pkt[0] = 0x80;
                    pkt[1] = TIMING_REQUEST;
                    pkt[2..4].copy_from_slice(&7u16.to_be_bytes());
                    // Our transmit time; the phone echoes it back as originate.
                    pkt[24..32].copy_from_slice(&tx_clock.lock().await.now());
                    if tx.send_to(&pkt, target).await.is_err() {
                        break;
                    }
                }
            });
            self._timing.push(StopOnDrop(ticker));
        }
        Ok(port)
    }
}

/// The seed as the phone wrote it, which the key derivation turns into a salt.
///
/// It arrives as a number in one version and as a string in another, and both
/// spell the same salt — so both are read the same way rather than one of them
/// being refused as malformed.
///
/// A number is read as unsigned. The phone draws the seed from the whole 64-bit
/// range and spells the salt from it as `PRIu64`, while a plist integer reaches
/// us as the same eight bytes read as signed: half of all seeds are negative
/// here, and each of those names a salt the phone never computes.
fn seed_text(value: &Value) -> Option<String> {
    match value {
        Value::Integer(n) => n
            .as_signed()
            .map(|v| (v as u64).to_string())
            .or_else(|| n.as_unsigned().map(|v| v.to_string())),
        // A whole number written as a real is still the same digits, as long as
        // a real can hold all of them: above 2^53 it cannot, so one that large
        // is written as the real it is rather than as digits it may not have.
        Value::Real(f) if f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 => {
            Some(format!("{}", *f as i64))
        }
        Value::Real(f) => Some(f.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// The address `disableBluetooth` is about.
///
/// The phone names one, but not always: the field carries its own identifier as
/// often as an address. So the name is used when it is an address and the phone
/// the Bluetooth session belonged to is used otherwise — that one is known from
/// the link the message arrived to say goodbye to.
fn mac_of(named: &str, fallback: Option<&str>) -> Option<String> {
    for candidate in [Some(named), fallback].into_iter().flatten() {
        if is_mac(candidate) {
            return Some(candidate.to_uppercase());
        }
    }
    None
}

/// Six pairs of hex digits and nothing else: `AA:BB:CC:DD:EE:FF`.
fn is_mac(text: &str) -> bool {
    let mut parts = 0;
    for part in text.split(':') {
        if part.len() != 2 || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
        parts += 1;
    }
    parts == 6
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(width: u32, height: u32, ui_scale: u32) -> ScreenParams {
        ScreenParams {
            width,
            height,
            fps: 60,
            ui_scale,
        }
    }

    #[test]
    fn forgetting_the_streams_keeps_the_numbering_going() {
        let mut clock = AudioClock::default();
        let first = clock.open(102, 1, 44_100, 0);
        clock.forget_streams();
        assert!(clock.is_empty(), "the last phone's streams are gone");
        let second = clock.open(102, 1, 44_100, 0);
        assert_ne!(
            first, second,
            "an id the replaced session still holds is never handed out again"
        );
    }

    /// The phone names an address as often as it names its own identifier, so
    /// what it says is used when it is an address and the link's own peer is
    /// used otherwise.
    #[test]
    fn disable_bluetooth_names_a_mac() {
        assert_eq!(
            mac_of("aa:bb:cc:dd:ee:ff", Some("11:22:33:44:55:66")).as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        assert_eq!(
            mac_of(
                "8A3B4C5D-0000-0000-0000-000000000000",
                Some("11:22:33:44:55:66")
            )
            .as_deref(),
            Some("11:22:33:44:55:66")
        );
        assert_eq!(mac_of("", None), None);
        assert_eq!(mac_of("aa:bb:cc:dd:ee", Some("nonsense")), None);
    }

    /// A seed above `i64::MAX` arrives as the same eight bytes read as signed,
    /// while the phone spells the salt from the unsigned value.
    #[test]
    fn a_seed_above_the_signed_range_is_spelled_unsigned() {
        assert_eq!(
            seed_text(&Value::Integer((-1_687_929_222_165_462_259i64).into())).as_deref(),
            Some("16758814851544089357")
        );
        assert_eq!(
            seed_text(&Value::Integer(16_758_814_851_544_089_357u64.into())).as_deref(),
            Some("16758814851544089357")
        );
        assert_eq!(
            seed_text(&Value::Integer(42.into())).as_deref(),
            Some("42")
        );
        assert_eq!(
            seed_text(&Value::String("42".into())).as_deref(),
            Some("42")
        );
    }

    /// A percentage turns into a request for a canvas of `100/percent` the size.
    /// These are the values PlayPort's `CarPlayUiScale.apply` produces for the
    /// same input, worked out by hand from its expression — a mismatch would move
    /// every touch by the same ratio, which is the kind of bug that looks like a
    /// broken touchscreen.
    #[test]
    fn canvas_matches_the_ui_scale_presets() {
        assert_eq!(params(1280, 720, 100).canvas(), (1280, 720));
        assert_eq!(params(1280, 720, 75).canvas(), (1706, 960));
        assert_eq!(params(1280, 720, 85).canvas(), (1506, 848));
        assert_eq!(params(1280, 720, 115).canvas(), (1114, 626));
    }

    /// Anything the panel does not offer is the default, rather than a scale
    /// nothing has tested. The panel's input is a number field.
    #[test]
    fn an_unknown_scale_is_the_default() {
        assert_eq!(params(1280, 720, 57).ui_scale(), 100);
        assert_eq!(params(1280, 720, 0).canvas(), (1280, 720));
    }

    /// Enlarging past 4K is refused, the way the reference refuses it: nothing
    /// on either end could render it, and the request would only cost a session.
    #[test]
    fn an_enlarged_canvas_stops_at_4k() {
        assert_eq!(params(3840, 2160, 75).canvas(), (3840, 2160));
    }

    fn frame(timestamp_us: u64) -> Pending {
        Pending {
            timestamp_us,
            sealed: Vec::new(),
        }
    }

    fn stamps(queue: &VecDeque<Pending>) -> Vec<u64> {
        queue.iter().map(|f| f.timestamp_us).collect()
    }

    /// Frames are decoded in the order the phone stamped them, whatever order
    /// they arrived in: one that overtook another is behind it again by the time
    /// the decoder sees it, which is what keeps the twenty milliseconds it
    /// carries in the place they belong.
    #[test]
    fn a_frame_that_arrived_out_of_turn_waits_its_turn() {
        let mut queue = VecDeque::new();
        assert!(hold(&mut queue, frame(300), None));
        assert!(hold(&mut queue, frame(100), None));
        assert!(hold(&mut queue, frame(200), None));
        assert_eq!(stamps(&queue), vec![100, 200, 300]);
    }

    /// Two frames there is no point in: one the queue already holds, which would
    /// only be decoded twice, and one whose moment has gone by, which the car
    /// has already been given the sound up to.
    #[test]
    fn a_frame_that_has_no_turn_left_is_refused() {
        let mut queue = VecDeque::new();
        assert!(hold(&mut queue, frame(200), None));
        assert!(!hold(&mut queue, frame(200), None), "the same frame twice");
        assert!(
            !hold(&mut queue, frame(100), Some(100)),
            "a frame stamped where the car has already been taken to"
        );
        assert!(
            hold(&mut queue, frame(100), Some(99)),
            "one behind the watermark still has a place"
        );
        assert_eq!(stamps(&queue), vec![100, 200]);
    }
}
