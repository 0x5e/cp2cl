//! The airplay receiver: a small RTSP server the phone connects back to.
//!
//! After CarPlayStartSession goes out over iAP2 the phone opens a *second*,
//! unrelated connection to the port we advertised and speaks a different
//! protocol entirely. Nothing in this module shares code with iAP2; the only
//! thing carried across is the port number.
//!
//! The ordering matters and is enforced by which fields each step requires:
//!
//! ```text
//! OPTIONS
//!   POST /pair-setup   (first pairing only)
//!   POST /pair-verify  -> shared secret -> every later key
//!   POST /auth-setup   -> MFiSAP: prove the licence, or the phone hangs up
//! SETUP                (session: announces timing + event ports)
//!   GET /info          -> who we are; incomplete here means no picture
//! SETUP                (streams: announces video/audio data ports)
//! RECORD               -> media starts flowing
//!   POST /feedback     (once a second, forever — see below)
//! ```

pub mod audio;
pub mod bplist;
pub mod clock;
pub mod crypto;
pub mod event;
pub mod info;
pub mod pairing;
pub mod rtsp;
pub mod screen;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use plist::{dictionary::Dictionary, from_bytes, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Mutex;

use livi_runtime::AsyncAuth;

use crate::airplay::audio::AudioReceiver;
use crate::airplay::bplist::to_binary;
use crate::airplay::clock::{ntp_diff, SyncedClock};
use crate::airplay::crypto::{aes_ctr128, nonce_counter, open, seal, sha1_all, sha256_all, SymKey};
use crate::airplay::event::EventChannel;
use crate::airplay::pairing::{PairSetup, PairVerify, Pairings, VerifyKeys};
use crate::airplay::rtsp::{Request, Response, OCTET_CONTENT_TYPE, PAIRING_CONTENT_TYPE};
use crate::airplay::screen::VideoReceiver;
use crate::commands::Command;
use crate::config::Config;
use crate::sink::{AudioCodec, MediaSink};

/// How much audio the phone keeps buffered before playing it. Subtracted from
/// the reported position so `sampleTime` describes what the listener is hearing
/// now, not what we last received.
const PLAYOUT_LATENCY_SEC: f64 = 1.0;

/// Enough of an audio clock to answer `/feedback`.
///
/// The phone asks once a second and expects the playback position of every
/// active audio stream. An empty reply once audio is running tells it the stream
/// is dead — it stops sending video and tears the session down shortly after.
#[derive(Default)]
pub struct AudioClock {
    stream_type: i64,
    sample_rate: u32,
    connection_id: i64,
    /// RTP timestamp of the first packet, and when it arrived.
    origin: Option<(u32, std::time::Instant)>,
}

impl AudioClock {
    pub fn start(&mut self, stream_type: i64, connection_id: i64, sample_rate: u32) {
        self.stream_type = stream_type;
        self.connection_id = connection_id;
        self.sample_rate = sample_rate;
        self.origin = None;
    }

    pub fn note(&mut self, rtp_timestamp: u32) {
        if self.sample_rate == 0 {
            return;
        }
        if self.origin.is_none() {
            self.origin = Some((rtp_timestamp, std::time::Instant::now()));
        }
    }

    /// Whether there is something worth reporting.
    pub fn active(&self) -> bool {
        self.sample_rate > 0 && self.origin.is_some()
    }

    /// First sample plus real time elapsed since, less the playout buffer.
    pub fn sample_time(&self) -> Option<u64> {
        let (first, started) = self.origin?;
        let elapsed = started.elapsed().as_secs_f64() - PLAYOUT_LATENCY_SEC;
        Some(first as u64 + (elapsed.max(0.0) * self.sample_rate as f64) as u64)
    }

    pub fn stream_type(&self) -> i64 {
        self.stream_type
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn connection_id(&self) -> i64 {
        self.connection_id
    }
}

/// What the phone should render at, when something other than the configuration
/// knows better — an Android Auto head unit's declared resolution, say.
///
/// `GET /info` prefers this to `Config` once it is set. It only lands if the
/// head unit is heard from before the iPhone asks: the iPhone reads `/info` once
/// at the start of a session and does not ask again.
#[derive(Debug, Clone, Copy)]
pub struct ScreenParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Shared with whoever negotiates on our behalf; `None` means "use `Config`".
pub type SharedScreen = Arc<std::sync::Mutex<Option<ScreenParams>>>;

/// Input only becomes possible after pair-verify derives the keys, so the sink
/// exists the whole time and fills itself in as pieces arrive.
pub struct EventSink {
    key: Mutex<Option<SymKey>>,
    counter: Mutex<u64>,
    writer: Mutex<Option<tokio::net::tcp::OwnedWriteHalf>>,
}

impl EventSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            key: Mutex::new(None),
            counter: Mutex::new(0),
            writer: Mutex::new(None),
        })
    }

    async fn set_key(&self, raw: [u8; 32]) {
        *self.key.lock().await = SymKey::from_bytes(&raw).ok();
    }

    pub async fn dispatch(self: &Arc<Self>, cmd: Command) -> std::io::Result<()> {
        let key = self.key.lock().await.clone();
        let Some(key) = key else {
            eprintln!("[event] not paired yet — press ignored");
            return Ok(());
        };
        let mut guard = self.writer.lock().await;
        let Some(writer) = guard.as_mut() else {
            eprintln!("[event] phone has not opened the event channel yet");
            return Ok(());
        };
        let mut counter = self.counter.lock().await;
        let mut ch = EventChannel::new(key, &mut *writer);
        ch.set_counter(*counter);
        let outcome = ch.key(cmd).await;
        *counter = ch.counter();
        outcome
    }

    /// Answers one request the phone posted us on the event channel.
    ///
    /// The channel runs both ways: alongside our HID reports the phone sends its
    /// own requests, and each one needs a 200. Ignoring them is not neutral —
    /// the phone concludes the accessory stopped listening and stops sending
    /// video a couple of seconds later.
    pub async fn reply(&self, cseq: u32) -> std::io::Result<()> {
        let Some(key) = self.key.lock().await.clone() else {
            return Ok(());
        };
        let mut guard = self.writer.lock().await;
        let Some(writer) = guard.as_mut() else {
            return Ok(());
        };
        let mut counter = self.counter.lock().await;
        let plain = Response::new(200).encode(cseq);
        let mut out = Vec::new();
        for chunk in plain.chunks(MAX_PAYLOAD) {
            let header = (chunk.len() as u16).to_le_bytes();
            let sealed = seal(&key, &nonce_counter(*counter), chunk, &header)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            *counter += 1;
            out.extend_from_slice(&header);
            out.extend_from_slice(&sealed);
        }
        writer.write_all(&out).await
    }
}

/// Reads the phone's side of the event connection and answers every request.
async fn pump_events(
    mut read: tokio::net::tcp::OwnedReadHalf,
    read_key: SymKey,
    sink: Arc<EventSink>,
) {
    let mut buf = [0u8; 4096];
    let mut enc: Vec<u8> = Vec::new();
    let mut plain: Vec<u8> = Vec::new();
    let mut ctr: u64 = 0;

    loop {
        let n = match read.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        enc.extend_from_slice(&buf[..n]);

        // Same framing as the control channel: [2B length][ciphertext][16B tag].
        loop {
            if enc.len() < 2 {
                break;
            }
            let len = u16::from_le_bytes([enc[0], enc[1]]) as usize;
            let total = 2 + len + TAG_LEN;
            if enc.len() < total {
                break;
            }
            let aad = enc[..2].to_vec();
            let sealed = enc[2..total].to_vec();
            enc.drain(..total);
            match open(&read_key, &nonce_counter(ctr), &sealed, &aad) {
                Ok(p) => {
                    ctr += 1;
                    plain.extend_from_slice(&p);
                }
                Err(e) => {
                    eprintln!("[event] frame {ctr} failed to open: {e}");
                    return;
                }
            }
        }

        while let Ok(Some((req, used))) = rtsp::parse(&plain) {
            plain.drain(..used);
            // The phone answers our commands on this same connection. Those
            // answers are chatter, and replying to one would make it answer
            // back — a loop that drowns the log in identical lines.
            if req.method.starts_with("RTSP/") || req.method.starts_with("HTTP/") {
                if req.path != "200" {
                    eprintln!("[event] response {} {}", req.path, req.version);
                }
                continue;
            }
            println!(
                "[event] <- {} {} ({}B)",
                req.method,
                req.path,
                req.body.len()
            );
            if let Err(e) = sink.reply(req.cseq()).await {
                eprintln!("[event] reply failed: {e}");
                return;
            }
        }
    }
    println!("[airplay] event channel closed");
}

/// Everything the airplay side needs, owned once and shared per connection.
pub struct Shared {
    pub cfg: Arc<Config>,
    pub pairings: Arc<Mutex<Pairings>>,
    pub video_sink: Arc<dyn MediaSink>,
    pub audio_sink: Arc<dyn MediaSink>,
    /// Set by the Android Auto bridge once a head unit has said what it can
    /// show. Read by `GET /info`.
    pub screen: SharedScreen,
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
    /// Where input lands; stdin dispatches into this once pairing completes.
    pub event_sink: Arc<EventSink>,
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
        let pairings = shared.pairings.clone();
        let video_sink = shared.video_sink.clone();
        let audio_sink = shared.audio_sink.clone();
        let codec = shared.audio_codec.clone();
        let event_sink = shared.event_sink.clone();
        let mfi = shared.mfi.clone();
        let audio_clock = shared.audio_clock.clone();
        let timing = shared.timing.clone();

        tokio::spawn(async move {
            let mut session = Session {
                cfg,
                screen,
                pairings,
                video_sink,
                audio_sink,
                codec,
                event_sink,
                mfi,
                audio_clock,
                timing,
                peer,
                setup: None,
                verify: None,
                keys: None,
                cipher_read: None,
                cipher_write: None,
                next_port: NextPort::default(),
                streams: HashMap::new(),
                in_buf: Vec::new(),
                frame_buf: Vec::new(),
                plain_buf: Vec::new(),
                read_ctr: 0,
                write_ctr: 0,
                cseq: 0,
                feedback_n: 0,
            };
            if let Err(e) = session.run(socket).await {
                println!("[airplay] {peer} ended: {e}");
            }
        });
    }
}

/// A ChaCha20-Poly1305 tag, which sits after the ciphertext in every frame but
/// is not counted by the two-byte length in front of it.
const TAG_LEN: usize = 16;
/// Every /auth-setup request starts with this byte.
const MFISAP_VERSION: u8 = 0x01;
/// Timing-port packet types (RTCP-shaped, not NTP): request and reply.
const TIMING_REQUEST: u8 = 210;
const TIMING_RESPONSE: u8 = 211;
/// The largest plaintext chunk the phone will accept in one frame. The event
/// channel shares the framing, so it shares the limit.
pub(crate) const MAX_PAYLOAD: usize = 0x4000;

/// Ports are allocated from a range so each stream is easy to tell apart in
/// `lsof`, and because the phone expects them to be distinct.
struct NextPort(u16);

impl Default for NextPort {
    fn default() -> Self {
        Self(45000)
    }
}

impl NextPort {
    fn take(&mut self) -> u16 {
        let p = self.0;
        self.0 = self.0.wrapping_add(1);
        if self.0 > 45999 {
            self.0 = 45000;
        }
        p
    }
}

struct Session {
    cfg: Arc<Config>,
    /// What a head unit asked for, if one has. Read when answering `/info`.
    screen: SharedScreen,
    pairings: Arc<Mutex<Pairings>>,
    video_sink: Arc<dyn MediaSink>,
    audio_sink: Arc<dyn MediaSink>,
    codec: Arc<Mutex<AudioCodec>>,
    event_sink: Arc<EventSink>,
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
    next_port: NextPort,
    /// stream id -> port, kept only for logging.
    streams: HashMap<u64, u16>,
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
    /// Only for throttling the /feedback log line.
    feedback_n: u32,
}

impl Session {
    async fn run(
        &mut self,
        socket: TcpStream,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (mut reader, mut writer) = socket.into_split();
        let mut buf = [0u8; 8192];

        loop {
            let n = reader.read(&mut buf).await?;
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
                    Some(key) => {
                        let mut out = Vec::new();
                        for chunk in plaintext.chunks(MAX_PAYLOAD) {
                            let header = (chunk.len() as u16).to_le_bytes();
                            let sealed =
                                seal(&key, &nonce_counter(self.write_ctr), chunk, &header)?;
                            self.write_ctr += 1;
                            out.extend_from_slice(&header);
                            out.extend_from_slice(&sealed);
                        }
                        out
                    }
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
            Some(key) => {
                loop {
                    // The length counts the ciphertext only, so the tag has to
                    // be added back to find where the next frame starts.
                    if self.frame_buf.len() >= 2 {
                        let len =
                            u16::from_le_bytes([self.frame_buf[0], self.frame_buf[1]]) as usize;
                        let total = 2 + len + TAG_LEN;
                        if self.frame_buf.len() >= total {
                            let aad = self.frame_buf[..2].to_vec();
                            let sealed = self.frame_buf[2..total].to_vec();
                            self.frame_buf.drain(..total);
                            let plain =
                                match open(&key, &nonce_counter(self.read_ctr), &sealed, &aad) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        eprintln!(
                                            "[airplay] frame {} failed to open ({} bytes): {e}",
                                            self.read_ctr, len,
                                        );
                                        return Err(e.into());
                                    }
                                };
                            self.read_ctr += 1;
                            self.plain_buf.extend_from_slice(&plain);
                            continue;
                        }
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
                }
            }
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
                match Self::plist_body(&req.body) {
                    Ok(d) if !d.is_empty() => println!("[airplay] TEARDOWN body: {d:?}"),
                    _ => println!(
                        "[airplay] TEARDOWN body (not a plist): {}",
                        hex::encode(&req.body[..req.body.len().min(48)])
                    ),
                }
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

    fn info(&self) -> Result<Response, String> {
        // A head unit's declared resolution wins: it is the end that has to
        // decode the picture, so asking the phone for something else just makes
        // it scale.
        let negotiated = self.screen.lock().ok().and_then(|g| *g);
        let (width, height, fps) = match negotiated {
            Some(s) => (s.width, s.height, s.fps),
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
            hevc: self.cfg.hevc,
        })
        .map_err(|e| e.to_string())?;
        println!("[airplay] answered GET /info");
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

    /// The media-clock reply. Silence here is what made the phone stop sending
    /// video a second after the audio stream opened.
    async fn feedback(&mut self) -> Result<Response, String> {
        let clock = self.audio_clock.lock().await;
        if !clock.active() {
            return Ok(Response::new(200));
        }
        let mut stream = Dictionary::new();
        stream.insert("type".into(), Value::Integer(clock.stream_type().into()));
        stream.insert(
            "sampleRate".into(),
            Value::Integer((clock.sample_rate() as i64).into()),
        );
        stream.insert(
            "streamConnectionID".into(),
            Value::Integer(clock.connection_id().into()),
        );
        stream.insert(
            "timestamp".into(),
            // In the phone's domain, not ours: it is what places `sampleTime`
            // against the audio the phone sent. `AudioClock` counts samples, so
            // this is the one field that has to come from the steered clock.
            Value::Integer(self.timing.lock().await.now_u64().into()),
        );
        if let Some(at) = clock.sample_time() {
            stream.insert("sampleTime".into(), Value::Integer(at.into()));
        }

        let at = clock.sample_time();
        let mut body = Dictionary::new();
        body.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let plist = to_binary(&Value::Dictionary(body)).map_err(|e| e.to_string())?;

        // Once, to show the reported position is advancing; after that it is
        // once-a-second chatter like the request it answers.
        self.feedback_n += 1;
        if self.feedback_n == 1 {
            println!(
                "[airplay] /feedback now reporting type {} at {} Hz, sampleTime {:?}",
                clock.stream_type(),
                clock.sample_rate(),
                at
            );
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

    async fn setup_session(
        &mut self,
        dict: plist::dictionary::Dictionary,
    ) -> Result<Response, String> {
        let event_port = self.next_port.take();
        let timing_port = self.next_port.take();
        let keys = self.keys.ok_or("SETUP before pair-verify")?;
        // The phone tells us where to send our timing requests; it is the only
        // way the two clocks ever agree.
        let peer_timing_port = dict
            .get("timingPort")
            .and_then(|v| v.as_signed_integer())
            .map(|v| v as u16);

        // The phone opens this before it asks for any stream. It is bidirectional:
        // we push input down it, and it posts us requests that each need a 200.
        let listener = TcpListener::bind(("::", event_port))
            .await
            .map_err(|e| e.to_string())?;
        let read_key = SymKey::from_bytes(&keys.events_read()).map_err(|e| e.to_string())?;
        let sink = self.event_sink.clone();
        let responder = sink.clone();
        tokio::spawn(async move {
            let Ok((socket, peer)) = listener.accept().await else {
                return;
            };
            println!("[airplay] event channel from {peer}");
            let (read, write) = socket.into_split();
            *sink.writer.lock().await = Some(write);
            pump_events(read, read_key, responder).await;
        });

        self.spawn_timing(timing_port, peer_timing_port).await?;

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
        let features: Vec<Value> = if self.cfg.hevc {
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

    async fn setup_streams(
        &mut self,
        dict: plist::dictionary::Dictionary,
    ) -> Result<Response, String> {
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
                    let port = self.next_port.take();
                    let listener = TcpListener::bind(("::", port))
                        .await
                        .map_err(|e| e.to_string())?;
                    let sink = self.video_sink.clone();
                    tokio::spawn(async move {
                        let Ok((socket, _)) = listener.accept().await else {
                            return;
                        };
                        println!("[airplay] video stream attached");
                        Self::pump_video(socket, key, sink).await;
                    });
                    self.streams.insert(stream_id as u64, port);
                    let mut entry = Dictionary::new();
                    entry.insert("type".into(), Value::Integer(110.into()));
                    entry.insert("dataPort".into(), Value::Integer((port as i64).into()));
                    replies.push(Value::Dictionary(entry));
                }
                Some(100) | Some(101) | Some(102) => {
                    let port = self.next_port.take();
                    let control_port = self.next_port.take();
                    let socket = UdpSocket::bind(("::", port))
                        .await
                        .map_err(|e| e.to_string())?;
                    let fmt = table
                        .get("audioFormat")
                        .and_then(|v| v.as_signed_integer())
                        .unwrap_or(0);
                    let codec = AudioCodec::from_format(fmt);
                    println!(
                        "[airplay] audio stream type {} (format 0x{fmt:x} -> {codec:?})",
                        type_.unwrap_or(0)
                    );
                    {
                        *self.codec.lock().await = codec;
                    }
                    self.audio_clock.lock().await.start(
                        type_.unwrap_or(0),
                        stream_id,
                        codec.rate(),
                    );
                    let sink = self.audio_sink.clone();
                    let clock = self.audio_clock.clone();
                    tokio::spawn(async move {
                        println!("[airplay] audio stream attached");
                        Self::pump_audio(socket, key, codec, sink, clock).await;
                    });
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
                    let port = self.next_port.take();
                    let mut entry = Dictionary::new();
                    entry.insert("type".into(), Value::Integer(other.unwrap_or(0).into()));
                    entry.insert("dataPort".into(), Value::Integer((port as i64).into()));
                    replies.push(Value::Dictionary(entry));
                }
            }
        }

        let mut body = Dictionary::new();
        body.insert("streams".into(), Value::Array(replies));
        let plist = to_binary(&Value::Dictionary(body)).map_err(|e| e.to_string())?;
        Ok(Response::new(200).plist(plist))
    }

    fn plist_body(bytes: &[u8]) -> Result<plist::dictionary::Dictionary, String> {
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
        let mut rx = VideoReceiver::new(key, sink);
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
        println!("[airplay] video stream closed after {frames} frames");
    }

    /// Audio arrives on UDP, one frame per datagram.
    async fn pump_audio(
        socket: UdpSocket,
        key: SymKey,
        codec: AudioCodec,
        sink: Arc<dyn MediaSink>,
        clock: Arc<Mutex<AudioClock>>,
    ) {
        let rx = AudioReceiver::new(key, codec, sink);
        let mut buf = [0u8; 2048];
        let mut packets: u64 = 0;
        loop {
            match socket.recv(&mut buf).await {
                Ok(n) => {
                    // The RTP timestamp is the phone's own sample counter; it is
                    // what /feedback reports back as the playout position.
                    if n >= audio::RTP_HEADER_LEN {
                        let ts = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                        clock.lock().await.note(ts);
                    }
                    rx.feed(&buf[..n]);
                    packets += 1;
                    // Once, and only to settle a question: if this appears right
                    // after "audio stream attached" but sound comes seconds
                    // later, the wait is in the player. If this line itself is
                    // late, the wait was the phone opening the audio stream.
                    if packets == 1 {
                        println!("[airplay] audio: first packet handed to the player");
                    }
                }
                Err(_) => break,
            }
        }
        println!("[airplay] audio stream closed after {packets} packets");
    }

    /// Clock sync on the timing port.
    ///
    /// This is *not* NTP: it is a 32-byte RTCP-shaped packet (`0x80`, then type
    /// 210 for a request and 211 for a reply) with three 64-bit timestamps.
    /// The accessory drives it — it sends a request every second to the port the
    /// phone named in its SETUP. Without that the phone drops the session after
    /// a few seconds, which looks exactly like a codec failure.
    async fn spawn_timing(
        &mut self,
        port: u16,
        peer_timing_port: Option<u16>,
    ) -> Result<(), String> {
        let socket = Arc::new(
            UdpSocket::bind(("::", port))
                .await
                .map_err(|e| e.to_string())?,
        );

        // Two jobs on one socket: answer the phone's requests, and read the
        // replies to ours. Both stamp with the same steered clock.
        let rx = socket.clone();
        let rx_clock = self.timing.clone();
        tokio::spawn(async move {
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

        // And drive our own requests, so the phone has a clock to work against.
        if let Some(p) = peer_timing_port.filter(|p| *p > 0) {
            let mut target = self.peer;
            target.set_port(p);
            let tx = socket.clone();
            let tx_clock = self.timing.clone();
            println!("[airplay] driving clock sync to {target}");
            tokio::spawn(async move {
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
        }
        Ok(())
    }
}

