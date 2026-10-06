//! One connection to the phone, from the first OPTIONS to the teardown.
//!
//! `serve` accepts connections and hands each one to a `Session`, which walks
//! the handshake the `airplay` module header describes and then keeps answering
//! for the life of the session. Everything a session needs is owned once in
//! `Shared` and cloned per connection.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plist::{dictionary::Dictionary, from_bytes, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, Mutex};

use livi_runtime::AsyncAuth;

use super::audio::{self, AudioReceiver};
use super::bplist::to_binary;
use super::clock::{ntp_diff, SyncedClock};
use super::crypto::{self, aes_ctr128, open_frames, seal_frames, sha1_all, sha256_all, SymKey};
use super::event::{pump_events, EventSink};
use super::info;
use super::mic::{MicConfig, MicSink};
use super::pairing::{PairSetup, PairVerify, Pairings, VerifyKeys};
use super::rtsp::{self, Request, Response, OCTET_CONTENT_TYPE, PAIRING_CONTENT_TYPE};
use super::screen::{self, VideoReceiver};
use crate::config::Config;
use crate::sink::{AudioCodec, MediaSink, StreamEvent};

/// Every /auth-setup request starts with this byte.
const MFISAP_VERSION: u8 = 0x01;
/// Timing-port packet types (RTCP-shaped, not NTP): request and reply.
const TIMING_REQUEST: u8 = 210;
const TIMING_RESPONSE: u8 = 211;

/// How often the audio pump reports how it is going. Choppy sound is either
/// packets that never arrived or an uneven trickle of them, and nothing else in
/// the program can tell those apart.
const AUDIO_REPORT: Duration = Duration::from_secs(5);

/// An arrival this much later than the one before it closes the burst that was
/// in progress and starts a new one.
///
/// The phone does not send a buffered stream one packet at a time: it hands
/// over a lump and lets the receiver's buffer absorb it. Whether the player can
/// absorb that lump is the difference between sound and stutter, so the lumps
/// are counted instead of inferred from packet totals.
const BURST_GAP: Duration = Duration::from_millis(50);

/// The buffered media stream. It is the one stream the phone hands over in
/// lumps — ~384 ms at a time — rather than at its own playback rate, so it is
/// the one stream that has to be re-timed on the way out.
const BUFFERED_AUDIO: u8 = 102;

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
    /// Registers a stream and hands back a handle to it. Setting up the same
    /// type again replaces the entry: the phone re-opens audio on every track
    /// change rather than reusing the old stream.
    pub fn open(
        &mut self,
        stream_type: i64,
        connection_id: i64,
        sample_rate: u32,
        playout_latency_ms: u64,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.streams.retain(|s| s.stream_type != stream_type);
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

    /// Forgets one stream, by the handle its setup returned.
    pub fn close(&mut self, id: u64) {
        self.streams.retain(|s| s.id != id);
    }

    /// Forgets whichever stream carries this type, which is how the phone names
    /// it in a teardown.
    pub fn close_type(&mut self, stream_type: i64) {
        self.streams.retain(|s| s.stream_type != stream_type);
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

/// What the phone should render at, when something other than the configuration
/// knows better — an Android Auto head unit's declared resolution, or a size the
/// operator picked in the browser's display panel.
///
/// `GET /info` prefers this to `Config` once it is set. It only lands if it is
/// written before the session starts, which is why changing it costs a session:
/// the iPhone reads `/info` once and does not ask again.
#[derive(Debug, Clone, Copy)]
pub struct ScreenParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Canvas scale in percent, the way PlayPort's "UI size" control means it:
    /// the phone is asked for a canvas of `100/percent` the size, and the
    /// browser fits the result back to its own surface, so a larger canvas
    /// draws proportionally *smaller* CarPlay controls. 100 leaves it alone.
    pub ui_scale: u32,
    /// Whether to advertise HEVC. Part of the negotiated display rather than the
    /// environment because it has to match what the far end can decode: a browser
    /// picking H.264, or a head unit that only lists H.264, would show nothing.
    pub hevc: bool,
}

/// The canvas scales the browser's display panel offers. Anything else is the
/// default, which is what PlayPort's own panel does with a typed-in value.
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

/// Shared with whoever negotiates on our behalf; `None` means "use `Config`".
pub type SharedScreen = Arc<std::sync::Mutex<Option<ScreenParams>>>;

/// Everything the airplay side needs, owned once and shared per connection.
pub struct Shared {
    pub cfg: Arc<Config>,
    pub pairings: Arc<Mutex<Pairings>>,
    pub video_sink: Arc<dyn MediaSink>,
    pub audio_sink: Arc<dyn MediaSink>,
    /// Set by the Android Auto bridge once a head unit has said what it can
    /// show, or by the browser's display panel. Read by `GET /info`.
    pub screen: SharedScreen,
    /// Fired when what the phone should be rendering has changed. `/info` is
    /// answered once per session, so dropping the session is the only way a
    /// change lands: the phone reconnects, asks again, and gets the new size.
    pub restart: tokio::sync::broadcast::Sender<()>,
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
    /// Where input lands; the browser and the AA bridge dispatch into this once
    /// pairing completes.
    pub event_sink: Arc<EventSink>,
    /// The one stream that runs both ways. Opened when the phone asks for
    /// bidirectional audio, and where the browser's microphone audio goes.
    pub mic: Arc<MicSink>,
}

/// Accepts connections and runs each one to completion.
pub async fn serve(listener: TcpListener, shared: Shared) {
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[airplay] accept failed: {e}");
                return;
            }
        };
        println!("[airplay] {peer} connected");
        let cfg = shared.cfg.clone();
        let screen = shared.screen.clone();
        let restart = shared.restart.subscribe();
        let pairings = shared.pairings.clone();
        let video_sink = shared.video_sink.clone();
        let audio_sink = shared.audio_sink.clone();
        let codec = shared.audio_codec.clone();
        let event_sink = shared.event_sink.clone();
        let mic = shared.mic.clone();
        let mfi = shared.mfi.clone();
        let audio_clock = shared.audio_clock.clone();
        let timing = shared.timing.clone();

        tokio::spawn(async move {
            let mut session = Session {
                cfg,
                screen,
                restart,
                pairings,
                video_sink,
                audio_sink,
                codec,
                event_sink,
                mic,
                mfi,
                audio_clock,
                timing,
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
            };
            if let Err(e) = session.run(socket).await {
                println!("[airplay] {peer} ended: {e}");
            }
        });
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
struct Pending {
    timestamp_us: u64,
    data: Vec<u8>,
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
    /// The stream's control port, bound but never read: leaving it closed makes
    /// the phone's RTCP bounce back as ICMPv6 port-unreachable, and an open
    /// socket nobody listens on is the cheapest way to answer "nothing to say".
    _control: Option<UdpSocket>,
}

struct Session {
    cfg: Arc<Config>,
    /// What a head unit — or the browser's display panel — asked for, if either
    /// has. Read when answering `/info`.
    screen: SharedScreen,
    /// Fires when the session should end so the phone re-reads `/info`.
    restart: tokio::sync::broadcast::Receiver<()>,
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
    /// Open streams by the type the phone names them with. Teardown looks them
    /// up here to close what it says it is closing.
    pumps: HashMap<i64, Pump>,
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
}

impl Session {
    async fn run(
        &mut self,
        socket: TcpStream,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (mut reader, mut writer) = socket.into_split();
        let mut buf = [0u8; 8192];

        loop {
            let n = tokio::select! {
                result = reader.read(&mut buf) => result?,
                signal = self.restart.recv() => match signal {
                    // `Lagged` means a second change arrived before this one was
                    // read; either way the answer is the same session restart.
                    Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        println!(
                            "[airplay] {}: what to render changed; dropping the session so \
                             the phone asks again",
                            self.peer
                        );
                        self.close_streams().await;
                        return Ok(());
                    }
                    // Unreachable while any sender lives, and dropping a healthy
                    // session over it would be the wrong answer.
                    Err(broadcast::error::RecvError::Closed) => continue,
                },
            };
            if n == 0 {
                println!("[airplay] {} disconnected", self.peer);
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
                        println!("[airplay] control channel is now encrypted");
                    }
                }

                writer.write_all(&out).await?;
            }
        }
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
                    eprintln!("[airplay] frame {} failed to open: {e}", self.read_ctr);
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
            println!(
                "[airplay] <- {} {} ({}B)",
                req.method,
                req.path,
                req.body.len()
            );
        }
        let result = self.route(req).await;
        match result {
            Ok(response) => response,
            Err(e) => {
                eprintln!("[airplay] {} {} failed: {e}", req.method, req.path);
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
                println!("[airplay] RECORD: media is live");
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
                Self::describe("command", &req.body);
                Ok(Response::new(200))
            }
            _ => Ok(Response::new(200)),
        }
    }

    /// Whether HEVC is being advertised. The environment sets the default; a
    /// head unit that cannot decode it, or a browser that picked H.264, writes
    /// the negotiated display and overrides it.
    fn hevc(&self) -> bool {
        self.negotiated().map(|s| s.hevc).unwrap_or(self.cfg.hevc)
    }

    /// What the phone has been asked to render, if anything has asked.
    fn negotiated(&self) -> Option<ScreenParams> {
        self.screen.lock().ok().and_then(|g| *g)
    }

    fn info(&self) -> Result<Response, String> {
        // A head unit's declared resolution wins, and so does the browser's
        // display panel: both are the end that has to decode the picture, so
        // asking the phone for something else just makes it scale.
        let (width, height, fps) = match self.negotiated() {
            Some(s) => {
                let (width, height) = s.canvas();
                (width, height, s.fps)
            }
            None => (self.cfg.width, self.cfg.height, self.cfg.fps),
        };
        let plist = info::build(&info::Static {
            device_id: self.cfg.device_id.clone(),
            bt_mac: self.cfg.bt_mac.clone(),
            name: self.cfg.name.clone(),
            width,
            height,
            fps,
            width_mm: self.cfg.width_mm,
            height_mm: self.cfg.height_mm,
            hevc: self.hevc(),
        })
        .map_err(|e| e.to_string())?;
        println!("[airplay] answered GET /info ({width}x{height}@{fps}, {})",
            if self.hevc() { "HEVC" } else { "H.264" });
        Ok(Response::new(200).plist(plist))
    }

    /// Names what the phone asked for — but only when it is not one of the
    /// routine ones. Those are answered with a 200 exactly as LIVI answers them,
    /// so repeating them says nothing; a type we have never seen is news.
    fn describe(what: &str, body: &[u8]) {
        const ROUTINE: &[&str] = &[
            "modesChanged",
            "disableBluetooth",
            "requestUI",
            "suggestUI",
            "duckAudio",
            "unduckAudio",
            "iAPSendMessage",
        ];
        if body.is_empty() {
            return;
        }
        let Ok(dict) = Self::plist_body(body) else {
            println!(
                "[airplay] {what}: not a plist ({}B) {}",
                body.len(),
                hex::encode(&body[..body.len().min(24)])
            );
            return;
        };
        if dict.is_empty() {
            return;
        }
        // `as_string` hands back a borrowed `&str`, so there is nothing to clone.
        let kind = dict.get("type").and_then(|v| v.as_string()).unwrap_or("");
        if ROUTINE.contains(&kind) {
            return;
        }
        println!(
            "[airplay] {what}: type {kind:?}, keys {:?}",
            dict.keys().collect::<Vec<_>>()
        );
    }

    /// Closes the streams the phone names. It names them by type, and it opens
    /// a fresh one for every track change, so this fires constantly during
    /// normal use — leaving it as a no-op leaked a port, a socket and a task
    /// per song, and kept `/feedback` reporting a stream that was already gone.
    async fn teardown(&mut self, req: &Request) {
        let dict = Self::plist_body(&req.body);
        let named = match &dict {
            Ok(d) if !d.is_empty() => {
                println!("[airplay] TEARDOWN body: {d:?}");
                d.get("streams").and_then(|v| v.as_array())
            }
            _ => {
                println!(
                    "[airplay] TEARDOWN body (not a plist): {}",
                    hex::encode(&req.body[..req.body.len().min(48)])
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
            println!("[airplay] stream {stream_type} torn down");
        }
    }

    /// Closes every stream, without the phone having asked.
    async fn close_streams(&mut self) {
        for stream_type in self.pumps.keys().copied().collect::<Vec<_>>() {
            self.close_stream(stream_type).await;
        }
    }

    /// Closes one stream, named the way the phone names it.
    ///
    /// Aborting the pump is the only way a UDP stream ever ends, and it means
    /// the pump never reaches its own goodbye — so the sink is told here or not
    /// at all, or the browser keeps a mixer channel for a stream nobody will
    /// ever send again.
    async fn close_stream(&mut self, stream_type: i64) {
        self.audio_clock.lock().await.close_type(stream_type);
        if let Some(pump) = self.pumps.remove(&stream_type) {
            pump.task.abort();
        }
        match stream_type {
            110 => self.video_sink.push(StreamEvent::ScreenActive(false)),
            100 | 101 | 102 => self.audio_sink.push(StreamEvent::AudioStopped {
                stream_type: stream_type as u8,
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
            println!("[airplay] /feedback streams: {summary}");
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
            println!("[airplay] pair-verify complete; shared secret ready");
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

        println!(
            "[airplay] /auth-setup signed (cert {}B, sig {}B, sha{})",
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
        let keys: Vec<&String> = dict.keys().collect();
        println!("[airplay] SETUP keys: {keys:?}");
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
        tokio::spawn(async move {
            let Ok((socket, peer)) = listener.accept().await else {
                return;
            };
            println!("[airplay] event channel from {peer}");
            // Input is latency-sensitive and always written in pairs. With Nagle
            // on, a Wi-Fi link holds the second report until the first is acked
            // and then flushes whatever piled up as one segment, which the phone
            // reads as a single tick — a d-pad nudge and its recentring cancel
            // out. The Android Auto socket sets this for the same reason.
            let _ = socket.set_nodelay(true);
            let (read, write) = socket.into_split();
            sink.attach_writer(write).await;
            pump_events(read, read_key, responder).await;
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
        let features: Vec<Value> = if self.hevc() {
            vec![
                Value::String("hevc".into()),
                Value::String("iAPChannel".into()),
                Value::String("viewAreas".into()),
            ]
        } else {
            vec![
                Value::String("iAPChannel".into()),
                Value::String("viewAreas".into()),
            ]
        };
        body.insert("enabledFeatures".into(), Value::Array(features));

        let plist = to_binary(&Value::Dictionary(body)).map_err(|e| e.to_string())?;
        println!("[airplay] session SETUP answered (event {event_port})");
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
                    let task = tokio::spawn(async move {
                        let Ok((socket, _)) = listener.accept().await else {
                            return;
                        };
                        println!("[airplay] video stream attached");
                        Self::pump_video(socket, key, sink).await;
                    });
                    self.open_pump(110, task, None);
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
                    // `speechRecognition` — and the browser shows one mixer
                    // channel per name, so the label is carried through as-is.
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
                    println!(
                        "[airplay] audio stream type {stream_type} {audio_type:?} \
                         (format 0x{fmt:x} -> {codec:?}), latency {latency} ms"
                    );
                    {
                        *self.codec.lock().await = codec;
                    }
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
                        let frames_per_packet = table
                            .get("framesPerPacket")
                            .and_then(|v| v.as_signed_integer())
                            .unwrap_or(0) as u32;
                        self.open_mic(stream_id, mic_port as u16, codec, frames_per_packet, keys)
                            .await;
                    }
                    self.audio_sink.push(StreamEvent::AudioConfig {
                        stream_type: stream_type as u8,
                        codec,
                        name: audio_type,
                    });
                    let id = self
                        .audio_clock
                        .lock()
                        .await
                        .open(stream_type, stream_id, codec.rate(), latency);
                    let sink = self.audio_sink.clone();
                    let clock = self.audio_clock.clone();
                    let task = tokio::spawn(async move {
                        println!("[airplay] audio stream attached");
                        Self::pump_audio(socket, key, stream_type as u8, codec, sink, clock, id).await;
                    });
                    // Bound but never read; a failure here is not worth failing
                    // the whole setup over.
                    let control = match UdpSocket::bind(("::", 0)).await {
                        Ok(s) => Some(s),
                        Err(e) => {
                            eprintln!("[airplay] control port not bound: {e}");
                            None
                        }
                    };
                    let control_port = control
                        .as_ref()
                        .and_then(|s| s.local_addr().ok())
                        .map_or(0, |a| a.port());
                    self.open_pump(stream_type, task, control);
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
                // 130 is iAP-over-CarPlay; ignoring it costs nothing visible.
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

    /// Opens the microphone uplink for a stream the phone says it will hear on.
    ///
    /// The packets go back to the phone's own address on the port it named. A
    /// failure here is not worth failing the SETUP over: the downlink is already
    /// running, and the worst case is a browser whose microphone does nothing.
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
                eprintln!("[mic] input key for stream {stream_id} is unusable: {e}");
                return;
            }
        };
        let config = MicConfig::from_codec(codec, frames_per_packet);
        let peer = SocketAddr::new(self.peer.ip(), port);
        match self.mic.open(peer, 100, key, config).await {
            Ok(()) => println!(
                "[mic] uplink open to {peer} ({:?} {}Hz/{}ch, {} samples per packet)",
                config.codec, config.sample_rate, config.channels, config.samples_per_packet
            ),
            Err(e) => eprintln!("[mic] uplink to {peer} not opened: {e}"),
        }
    }

    /// Registers a running stream under the type the phone will name it with.
    /// A type that is already open is replaced rather than left to rot, because
    /// the phone may re-open a stream without tearing the old one down.
    fn open_pump(        &mut self,
        stream_type: i64,
        task: tokio::task::JoinHandle<()>,
        control: Option<UdpSocket>,
    ) {
        let pump = Pump {
            task,
            _control: control,
        };
        if let Some(old) = self.pumps.insert(stream_type, pump) {
            old.task.abort();
        }
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
    async fn pump_video(socket: TcpStream, key: SymKey, sink: Arc<dyn MediaSink>) {
        let mut rx = VideoReceiver::new(key, sink.clone());
        sink.push(StreamEvent::ScreenActive(true));
        let mut socket = socket;
        let mut header = [0u8; screen::HEADER_LEN];
        let mut frames: u64 = 0;
        loop {
            if socket.read_exact(&mut header).await.is_err() {
                break;
            }
            let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
            if len > 8 * 1024 * 1024 {
                eprintln!("[video] implausible frame length {len}");
                break;
            }
            // KeepAlive, ForceKeyFrame and friends carry no payload at all.
            // Skipping them is routine — treating it as fatal dropped the whole
            // stream the first time the phone sent one.
            if len == 0 {
                continue;
            }
            let mut body = vec![0u8; len];
            if socket.read_exact(&mut body).await.is_err() {
                break;
            }
            rx.feed(&header, &body);
            frames += 1;
        }
        sink.push(StreamEvent::ScreenActive(false));
        println!("[airplay] video stream closed after {frames} frames");
    }

    /// Audio arrives on UDP, one frame per datagram.
    async fn pump_audio(
        socket: UdpSocket,
        key: SymKey,
        stream_type: u8,
        codec: AudioCodec,
        sink: Arc<dyn MediaSink>,
        clock: Arc<Mutex<AudioClock>>,
        id: u64,
    ) {
        let rx = AudioReceiver::new(key, stream_type, codec, sink.clone());
        let mut buf = [0u8; 2048];
        let mut packets: u64 = 0;
        // Sound that stutters is either packets that never arrived or ones that
        // arrived in lumps. The sequence number counts the first, the gap
        // between arrivals measures the second, and neither is visible from
        // anywhere else — so both are reported, worst gap and all.
        let mut last_seq: Option<u16> = None;
        let mut missing: u64 = 0;
        let mut worst_gap = Duration::ZERO;
        let mut last_at = Instant::now();
        let mut report_at = last_at + AUDIO_REPORT;
        // Lumps, counted directly: a gap longer than BURST_GAP closes the one
        // before it, so every closed lump is a burst and its packet count is
        // how much audio the phone handed over at once.
        let mut bursts: u64 = 0;
        let mut burst_sum: u64 = 0;
        let mut burst_min = u32::MAX;
        let mut burst_max: u32 = 0;
        let mut in_burst: u32 = 0;
        // The buffered stream is re-timed: frames wait here for the moment the
        // phone's own timestamp gives them and leave one at a time, so the
        // consumer never sees the lump they arrived in. Every other stream is
        // already arriving at its own playback rate and goes straight through.
        let paced = stream_type == BUFFERED_AUDIO;
        let mut queue: VecDeque<Pending> = VecDeque::new();
        // Where the frame before this one was due; the next is measured from it.
        let mut aired = (0u64, tokio::time::Instant::now());
        loop {
            // A frame waiting to go out is the other thing this loop waits on.
            // With none there is nothing to wait for but the next packet.
            let front = queue.front().map(|f| f.timestamp_us);
            tokio::select! {
                biased;
                _ = wait_due(&mut aired, front.unwrap_or(0)), if front.is_some() => {
                    if let Some(frame) = queue.pop_front() {
                        sink.push(StreamEvent::AudioPacket {
                            stream_type,
                            timestamp_us: frame.timestamp_us,
                            data: frame.data,
                        });
                    }
                }
                got = socket.recv(&mut buf) => {
                    let Ok(n) = got else { break };
                    let now = Instant::now();
                    let gap = now - last_at;
                    worst_gap = worst_gap.max(gap);
                    last_at = now;
                    if gap > BURST_GAP && in_burst > 0 {
                        bursts += 1;
                        burst_sum += u64::from(in_burst);
                        burst_min = burst_min.min(in_burst);
                        burst_max = burst_max.max(in_burst);
                        in_burst = 0;
                    }
                    in_burst += 1;
                    // The RTP timestamp is the phone's own sample counter; it is
                    // what /feedback reports back as the playout position.
                    if n >= audio::RTP_HEADER_LEN {
                        let ts = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                        clock.lock().await.note(id, ts);
                        let seq = u16::from_be_bytes([buf[2], buf[3]]);
                        if let Some(prev) = last_seq {
                            // Wrapping subtraction, so a counter that rolls over
                            // reads as the one packet it is.
                            missing += u64::from(seq.wrapping_sub(prev.wrapping_add(1)));
                        }
                        last_seq = Some(seq);
                    }
                    if paced {
                        if let Some((timestamp_us, data)) = rx.decode(&buf[..n]) {
                            queue.push_back(Pending { timestamp_us, data });
                        }
                    } else {
                        rx.feed(&buf[..n]);
                    }
                    packets += 1;
                    // Once, and only to settle a question: if this appears right
                    // after "audio stream attached" but sound comes seconds
                    // later, the wait is in the player. If this line itself is
                    // late, the wait was the phone opening the audio stream.
                    if packets == 1 {
                        println!("[airplay] audio: first packet handed to the player");
                    }
                    if now >= report_at {
                        // A burst is only counted once the next one starts, so a
                        // window that saw none reports zero rather than a stale
                        // minimum.
                        let (lo, hi, avg) = if bursts == 0 {
                            (0, 0, 0.0)
                        } else {
                            (burst_min, burst_max, burst_sum as f64 / bursts as f64)
                        };
                        // `held` is what the re-timing shows: a buffered stream
                        // leaves at its own rate, so the queue should sit at a
                        // steady depth instead of sawtoothing.
                        println!(
                            "[airplay] audio: {packets} packets, {missing} missing, \
                             worst gap {:.0} ms, {bursts} bursts of {lo}-{hi} \
                             (avg {avg:.1}), {} held",
                            worst_gap.as_secs_f64() * 1000.0,
                            queue.len()
                        );
                        worst_gap = Duration::ZERO;
                        bursts = 0;
                        burst_sum = 0;
                        burst_min = u32::MAX;
                        burst_max = 0;
                        report_at = now + AUDIO_REPORT;
                    }
                }
            }
        }
        // Whatever ended it — the phone tearing the stream down, or the socket
        // failing — stop announcing it. A stream that is gone but still named
        // in `/feedback` is one the phone keeps asking about.
        clock.lock().await.close(id);
        sink.push(StreamEvent::AudioStopped { stream_type });
        println!(
            "[airplay] audio stream closed after {packets} packets, {missing} missing"
        );
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
        let reader = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let Ok((n, from)) = rx.recv_from(&mut buf).await else {
                    break;
                };
                if n < 32 {
                    continue;
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
            println!("[airplay] driving clock sync to {target}");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn params(width: u32, height: u32, ui_scale: u32) -> ScreenParams {
        ScreenParams {
            width,
            height,
            fps: 60,
            ui_scale,
            hevc: false,
        }
    }

    /// The browser's panel sends a percentage and the phone is asked for a
    /// canvas of `100/percent` the size. These are the values PlayPort's
    /// `CarPlayUiScale.apply` produces for the same input, worked out by hand
    /// from its expression — a mismatch would move every touch by the same
    /// ratio, which is the kind of bug that looks like a broken touchscreen.
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
}
