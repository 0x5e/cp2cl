//! Plays the decoded media with ffplay, one process per stream.
//!
//! ffplay is doing real work here, not just being a convenience: it turns our
//! elementary streams into a picture and sound without us writing a decoder, an
//! A/V synchroniser or a window. Doing that in Rust would be thousands of lines
//! for no gain in a demo.
//!
//! Two processes, deliberately independent:
//!
//! * audio and video arrive on separate connections and start at different
//!   times — the phone may open the screen stream well before (or without) the
//!   audio one, and a stalled side must never stop the other;
//! * they need different input formats anyway.
//!
//! An earlier version pushed to go2rtc over RTSP. That added a server, a muxer
//! and a whole class of "black screen with no error" failures; feeding ffplay
//! directly cuts the problem in half when something does not show up.

use std::io::{Error as IoError, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc::{self, channel, Sender};

use crate::aa_client::session::MediaFrame;

/// How much may queue ahead of the player before we start dropping. Bounded
/// with a drop policy on purpose: this is a live stream, so falling behind
/// should cost a glitch, not seconds of latency that accumulate forever.
const VIDEO_QUEUE: usize = 64;
const AUDIO_QUEUE: usize = 512;

/// Where to copy video frames, if anywhere. Read once from the environment.
fn video_dump() -> Option<&'static std::path::PathBuf> {
    use std::sync::OnceLock;
    static DUMP: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    DUMP.get_or_init(|| {
        std::env::var("CP2AA_DUMP_VIDEO")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
    })
    .as_ref()
}

/// Wherever a decoded stream ends up.
///
/// Two implementations: `PlayerSink`, which feeds ffplay, and `AaSink`, which
/// hands the same bytes to the Android Auto side. Neither blocks — both drop
/// instead, because a live stream should glitch rather than fall behind, and a
/// receiver that is gone for good says so once instead of forever.
pub trait MediaSink: Send + Sync {
    fn push(&self, bytes: Vec<u8>);
}

/// A running ffplay, fed from a channel so a slow player never blocks parsing.
pub struct PlayerSink {
    label: &'static str,
    tx: Sender<Vec<u8>>,
    /// Set the first time a push fails. The player dying takes the channel with
    /// it, and every later frame would otherwise print the same complaint.
    dead: AtomicBool,
    /// Set the first time the queue is full. Dropping is the right behaviour for
    /// a live stream, but it is still worth knowing it is happening.
    dropped: AtomicBool,
}

impl PlayerSink {
    /// `header` is written once before the first payload — see `wav_header`.
    /// Owned, because it moves onto the blocking task that owns stdin.
    pub fn spawn(
        program: &Path,
        args: &[String],
        header: Option<Vec<u8>>,
        label: &'static str,
    ) -> Result<Self, IoError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| IoError::new(e.kind(), format!("failed to start ffplay: {e}")))?;

        let mut stdin = child.stdin.take().expect("stdin was piped");
        let capacity = if label == "video" {
            VIDEO_QUEUE
        } else {
            AUDIO_QUEUE
        };
        let (tx, mut rx) = channel::<Vec<u8>>(capacity);

        tokio::task::spawn_blocking(move || {
            if let Some(h) = header {
                if let Err(e) = stdin.write_all(&h).and_then(|_| stdin.flush()) {
                    eprintln!("[{label}] ffplay stdin closed on header: {e}");
                    let _ = child.kill();
                    return;
                }
            }
            while let Some(bytes) = rx.blocking_recv() {
                if let Err(e) = stdin.write_all(&bytes).and_then(|_| stdin.flush()) {
                    eprintln!("[{label}] ffplay stdin closed: {e}");
                    break;
                }
            }
            let _ = child.kill();
        });

        Ok(Self {
            label,
            tx,
            dead: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
        })
    }
}

impl MediaSink for PlayerSink {
    fn push(&self, bytes: Vec<u8>) {
        // `CP2AA_DUMP_VIDEO=/tmp/carplay.h264` writes the annex-B stream to disk
        // as it goes. Playing that file settles whether the bytes are wrong or
        // the player's options are.
        if self.label == "video" {
            if let Some(path) = video_dump() {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = f.write_all(&bytes);
                }
            }
        }
        match self.tx.try_send(bytes) {
            Ok(()) => {}
            // The player exited; the receiver is gone for good.
            Err(mpsc::error::TrySendError::Closed(_)) => {
                if !self.dead.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "[{}] ffplay is gone — did it fail to open a window? \
                         Further frames are dropped silently.",
                        self.label
                    );
                }
            }
            // Full: the player is behind. Skipping is better than queueing,
            // which is how several seconds of latency built up unnoticed.
            Err(mpsc::error::TrySendError::Full(_)) => {
                if !self.dropped.swap(true, Ordering::Relaxed) {
                    eprintln!("[{}] player is behind; dropping to stay live", self.label);
                }
            }
        }
    }
}

/// What the audio pipeline agreed to deliver, which decides ffmpeg's `-f`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// 16-bit big-endian PCM. Only ever offered over wired CarPlay.
    Pcm { rate: u32, channels: u16 },
    /// AAC-LC frames, which ffmpeg wants wrapped in ADTS.
    AacLc { rate: u32, channels: u16 },
    /// Raw Opus frames.
    Opus { rate: u32, channels: u16 },
}

impl Default for AudioCodec {
    fn default() -> Self {
        Self::Pcm {
            rate: 48_000,
            channels: 2,
        }
    }
}

impl AudioCodec {
    pub fn rate(&self) -> u32 {
        match self {
            Self::Pcm { rate, .. } => *rate,
            Self::AacLc { rate, .. } => *rate,
            Self::Opus { rate, .. } => *rate,
        }
    }

    /// Reads the single `audioFormat` bitfield the phone echoes back in the
    /// stream SETUP. Bits are LIVI's `kAirPlayAudioFormat_*` constants: OPUS and
    /// AAC-LC sit high up, the LPCM rates are one bit each below.
    pub fn from_format(fmt: i64) -> Self {
        let bits = fmt as u64;
        const OPUS: u64 = 0x7000_0000;
        const AAC_LC_44K: u64 = 0x40_0000;
        const AAC_LC_48K: u64 = 0x80_0000;

        if bits & OPUS != 0 {
            return Self::Opus {
                rate: 48_000,
                channels: 1,
            };
        }
        if bits & (AAC_LC_44K | AAC_LC_48K) != 0 {
            let rate = if bits & AAC_LC_48K != 0 {
                48_000
            } else {
                44_100
            };
            return Self::AacLc { rate, channels: 2 };
        }
        let (rate, channels) = match bits {
            0x4 => (8_000, 1),
            0x8 => (8_000, 2),
            0x10 => (16_000, 1),
            0x20 => (16_000, 2),
            0x40 => (24_000, 1),
            0x80 => (24_000, 2),
            0x100 => (32_000, 1),
            0x200 => (32_000, 2),
            0x400 => (44_100, 1),
            0x800 => (44_100, 2),
            0x4000 => (48_000, 1),
            0x8000 => (48_000, 2),
            _ => (48_000, 2),
        };
        Self::Pcm { rate, channels }
    }
}

/// The two ffplay command lines. Kept as data so they can be printed verbatim,
/// which is the fastest way to debug a pipeline you cannot see.
pub struct Pipelines {
    pub video: Vec<String>,
    pub audio: Vec<String>,
    /// Written ahead of the audio samples. Raw PCM has no self-describing
    /// header, and ffplay refuses the `-ar` / `-ac` / `-channels` options that
    /// would otherwise tell it what it is looking at — so we supply one.
    pub audio_header: Option<Vec<u8>>,
}

/// A RIFF/WAVE header for raw PCM, with the sizes left open-ended so a stream
/// of unknown length can follow it.
fn wav_header(rate: u32, channels: u16, bits: u16) -> Vec<u8> {
    let block_align = channels * bits / 8;
    let byte_rate = rate * block_align as u32;
    // Largest value that fits: a pipe has no seekable end to report.
    let data_len: u32 = 0x7fff_ffff;

    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(data_len.wrapping_add(36)).to_le_bytes());
    h.extend_from_slice(b"WAVE");
    h.extend_from_slice(b"fmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&byte_rate.to_le_bytes());
    h.extend_from_slice(&block_align.to_le_bytes());
    h.extend_from_slice(&bits.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_len.to_le_bytes());
    h
}

impl Pipelines {
    /// Two ffplay command lines, each reading one elementary stream from stdin.
    ///
    /// `window` is the size to open the video window at, in whatever units the
    /// player's SDL takes them for. Left unset it opens at the backing pixel
    /// count, which on a HiDPI display is twice the size it should be — and
    /// since those units are points there, halving it halves the window.
    pub fn build(codec: AudioCodec, fps: u32, window: (u32, u32), hevc: bool) -> Self {
        let video_demuxer = if hevc { "hevc" } else { "h264" };
        let video = vec![
            "-hide_banner".into(),
            // Not `warning`: libswscale prints "deprecated pixel format used"
            // on the first frame and, at 60 fps with the banner gone, that one
            // line is the only thing the player ever says. Real decode errors
            // are still reported — they are `error`, not `warning`.
            "-loglevel".into(),
            "error".into(),
            // As plain as it gets, deliberately. Dumping the same bytes to a file
            // and playing that works with no options at all, so the bytes are
            // fine — what broke it on stdin was the tuning: probesize,
            // analyzeduration, nobuffer, low_delay and framedrop all assume they
            // can look ahead or hold data back, which a pipe cannot do. Latency
            // tuning can come back once there is a picture to tune.
            //
            // The demuxer has to match what we asked the phone for: an annex-B
            // stream carries no headers saying which codec it is.
            "-f".into(),
            video_demuxer.into(),
            "-framerate".into(),
            fps.to_string(),
            "-i".into(),
            "pipe:0".into(),
            "-x".into(),
            window.0.to_string(),
            "-y".into(),
            window.1.to_string(),
        ];

        // The rate and channel count are bound per branch below: WAV needs them
        // in its header, the framed codecs do not need them at all.
        let input_fmt = match codec {
            AudioCodec::Pcm { .. } => "s16be",
            AudioCodec::AacLc { .. } => "aac",
            AudioCodec::Opus { .. } => "opus",
        };
        // WAV carries the rate and channel count itself, so ffplay needs no
        // format options at all — which is the only way past its refusal to
        // accept them for a raw stream. AAC and Opus are self-describing.
        let (audio, audio_header) = match codec {
            AudioCodec::Pcm { rate, channels } => (
                vec![
                    "-hide_banner".into(),
                    "-loglevel".into(),
                    "error".into(),
                    // Sound only — a window here would just be an empty box.
                    "-nodisp".into(),
                    // Low latency is safe here: with no picture there is no
                    // scaler to trip over, and these keep the player from
                    // holding samples back the way it does for a file.
                    "-probesize".into(),
                    "32".into(),
                    "-analyzeduration".into(),
                    "0".into(),
                    "-i".into(),
                    "pipe:0".into(),
                    "-fflags".into(),
                    "nobuffer".into(),
                ],
                Some(wav_header(rate, channels, 16)),
            ),
            _ => (
                vec![
                    "-hide_banner".into(),
                    "-loglevel".into(),
                    "error".into(),
                    "-nodisp".into(),
                    "-f".into(),
                    input_fmt.into(),
                    "-i".into(),
                    "pipe:0".into(),
                    "-fflags".into(),
                    "nobuffer".into(),
                ],
                None,
            ),
        };

        Self {
            video,
            audio,
            audio_header,
        }
    }
}

/// How many frames may wait for the head unit. Bounded and dropped past, for
/// the same reason the player's queue is.
const AA_BACKLOG: usize = 256;

/// The Android Auto end of the bridge.
///
/// A head unit chooses the channel numbers during its handshake, so they are
/// unknown when this is built: both stay 0 until service discovery answers and
/// `set_channels` fills them in, and frames that arrive before then are dropped
/// rather than sent to a channel that does not exist yet.
pub struct AaOutlet {
    video_ch: Arc<AtomicU8>,
    audio_ch: Arc<AtomicU8>,
    tx: Sender<MediaFrame>,
    /// Taken once, by whoever starts the AA session.
    rx: Mutex<Option<mpsc::Receiver<MediaFrame>>>,
}

// `set_channels` and `take_receiver` belong to whoever starts the AA session,
// which is the far end of the bridge. Nothing calls them yet.
#[allow(dead_code)]
impl AaOutlet {
    pub fn new() -> Self {
        let (tx, rx) = channel(AA_BACKLOG);
        Self {
            video_ch: Arc::new(AtomicU8::new(0)),
            audio_ch: Arc::new(AtomicU8::new(0)),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    pub fn video(&self) -> Arc<AaSink> {
        Arc::new(AaSink::new("video", self.video_ch.clone(), self.tx.clone()))
    }

    pub fn audio(&self) -> Arc<AaSink> {
        Arc::new(AaSink::new("audio", self.audio_ch.clone(), self.tx.clone()))
    }

    /// The channels the head unit offered. `None` is a stream it did not ask
    /// for, which leaves that sink dropping.
    pub fn set_channels(&self, video: Option<u8>, audio: Option<u8>) {
        self.video_ch.store(video.unwrap_or(0), Ordering::Relaxed);
        self.audio_ch.store(audio.unwrap_or(0), Ordering::Relaxed);
    }

    /// The receiving half, for the AA session. One link, one owner: a second
    /// call gets nothing, which is how a second head unit is turned away.
    pub fn take_receiver(&self) -> Option<mpsc::Receiver<MediaFrame>> {
        self.rx.lock().ok()?.take()
    }

    /// Given back when a session ends so the next one can use it.
    pub fn return_receiver(&self, rx: mpsc::Receiver<MediaFrame>) {
        if let Ok(mut guard) = self.rx.lock() {
            *guard = Some(rx);
        }
    }
}

impl Default for AaOutlet {
    fn default() -> Self {
        Self::new()
    }
}

/// A stream on its way to the head unit.
pub struct AaSink {
    label: &'static str,
    /// 0 until the handshake says which channel this is.
    ch: Arc<AtomicU8>,
    tx: Sender<MediaFrame>,
    /// Each of these is reported once: a stream that starts before the
    /// handshake finishes, or keeps going after it ends, would otherwise print
    /// the same line for every frame.
    no_channel: AtomicBool,
    gone: AtomicBool,
    dropped: AtomicBool,
}

impl AaSink {
    fn new(label: &'static str, ch: Arc<AtomicU8>, tx: Sender<MediaFrame>) -> Self {
        Self {
            label,
            ch,
            tx,
            no_channel: AtomicBool::new(false),
            gone: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
        }
    }
}

impl MediaSink for AaSink {
    fn push(&self, bytes: Vec<u8>) {
        let ch = self.ch.load(Ordering::Relaxed);
        if ch == 0 {
            if !self.no_channel.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[aa] {}: no Android Auto channel negotiated yet; dropping",
                    self.label
                );
            }
            return;
        }
        let frame = MediaFrame {
            ch,
            ts_us: Some(now_us()),
            data: bytes,
        };
        match self.tx.try_send(frame) {
            Ok(()) => {}
            // No session is draining the other end.
            Err(mpsc::error::TrySendError::Closed(_)) => {
                if !self.gone.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "[aa] {}: no Android Auto session is reading; dropping",
                        self.label
                    );
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if !self.dropped.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "[aa] {}: head unit is behind; dropping to stay live",
                        self.label
                    );
                }
            }
        }
    }
}

/// Microseconds since the process started, which is the unit an AV channel
/// timestamp carries. CarPlay's own RTP timestamps should replace this once the
/// two clocks are bridged; until then a monotonic local clock at least gives
/// the head unit something to play out in order.
fn now_us() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}
