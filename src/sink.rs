//! Where the media goes once CarPlay has handed it over.
//!
//! Exactly one consumer takes the streams: the browser client, or — one day — an
//! Android Auto head unit. The events below carry the *description* of a stream
//! as well as its bytes, which is what the ffplay sink this replaced never
//! needed: ffplay was told what it was reading on its command line, while a
//! browser decoding with WebCodecs has to be told out of band, because the
//! description arrives on the stream itself after decoding has already started.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc::{self, channel, Sender};

use crate::aa_client::session::MediaFrame;

/// Which family a video stream is in, which decides both how its parameter sets
/// are read out of the config record and how the browser names the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
}

/// What the audio pipeline agreed to deliver, which decides how the frames are
/// framed for whoever decodes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// 16-bit PCM, already swapped to little-endian for the consumer.
    Pcm { rate: u32, channels: u16 },
    /// Raw AAC-LC frames with no ADTS header: the browser is told the framing
    /// separately, so an ADTS header would only be in the way.
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

    pub fn channels(&self) -> u16 {
        match self {
            Self::Pcm { channels, .. } => *channels,
            Self::AacLc { channels, .. } => *channels,
            Self::Opus { channels, .. } => *channels,
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

/// One thing a stream produced, on its way to whichever consumer asked for it.
#[derive(Debug)]
pub enum StreamEvent {
    /// A video stream's codec description, sent once before any frame.
    ///
    /// Both shapes are carried because the two consumers want different ones and
    /// neither can cheaply rebuild the other: `codec_data` is the raw avcC /
    /// hvcC record, which is what WebCodecs wants as its decoder `description`,
    /// while `parameter_sets` is the same information as annex-B chunks, which
    /// is what a plain elementary stream needs.
    VideoConfig {
        codec: VideoCodec,
        codec_data: Vec<u8>,
        parameter_sets: Vec<Vec<u8>>,
    },
    /// One access unit as annex-B, and whether it is a keyframe.
    VideoFrame { keyframe: bool, data: Vec<u8> },
    /// The screen stream opened or closed. The browser ignores it today; it is
    /// what the head unit would need to stop waiting for a picture.
    ScreenActive(bool),
    /// An audio stream's format. `name` is the phone's own `audioType` from the
    /// SETUP — `media`, `telephony`, `speechRecognition` — which the browser
    /// shows as one mixer channel per name.
    AudioConfig {
        stream_type: u8,
        codec: AudioCodec,
        name: String,
    },
    /// One audio frame. `timestamp_us` is the phone's RTP sample counter turned
    /// into microseconds, the only clock a browser has to schedule against.
    AudioPacket {
        stream_type: u8,
        timestamp_us: u64,
        data: Vec<u8>,
    },
    AudioStopped { stream_type: u8 },
}

/// Wherever a decoded stream ends up.
///
/// Never blocks: both implementations drop instead, because a live stream should
/// glitch rather than fall behind, and a consumer that is gone for good says so
/// once instead of forever.
pub trait MediaSink: Send + Sync {
    fn push(&self, event: StreamEvent);
}

/// How many frames may wait for the head unit. Bounded and dropped past, so a
/// head unit that is behind glitches instead of accumulating latency.
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

/// A stream on its way to the head unit, as one continuous elementary stream.
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
    fn push(&self, event: StreamEvent) {
        match event {
            // A head unit reads an annex-B elementary stream, so each parameter
            // set goes out on its own ahead of the frames that reference it. The
            // raw config record is for decoders that are told out of band and is
            // dropped here.
            StreamEvent::VideoConfig { parameter_sets, .. } => {
                for parameter_set in parameter_sets {
                    self.send(parameter_set);
                }
            }
            StreamEvent::VideoFrame { data, .. } => self.send(data),
            StreamEvent::AudioPacket { data, .. } => self.send(data),
            // Nothing the head unit acts on yet: it learns the audio format from
            // its own negotiation, and the screen-open signal has no channel of
            // its own.
            StreamEvent::ScreenActive(_)
            | StreamEvent::AudioConfig { .. }
            | StreamEvent::AudioStopped { .. } => {}
        }
    }
}

impl AaSink {
    fn send(&self, bytes: Vec<u8>) {
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
