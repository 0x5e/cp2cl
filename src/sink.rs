//! Where the media goes once CarPlay has handed it over.
//!
//! One consumer takes the streams: the browser client, or the head unit on the
//! other end of a CarLife session. The events below carry the *description* of a
//! stream as well as its bytes, because a consumer that has to be told what it is
//! reading needs both: the phone sends the codec's parameter sets after the
//! consumer has already started, and a browser decoding with WebCodecs needs them
//! out of band while anything reading an elementary stream needs them in it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

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

/// One thing a stream produced, on its way to the browser.
#[derive(Debug)]
pub enum StreamEvent {
    /// A video stream's codec description, sent once before any frame.
    ///
    /// The same record in both shapes a consumer might want. `codec_data` is the
    /// raw avcC / hvcC record, which is what WebCodecs wants as its decoder
    /// `description` and is told to the browser out of band; `parameter_sets`
    /// are the SPS/PPS the record holds, already converted to annex-B, which is
    /// what a consumer reading an elementary stream has to put in it.
    VideoConfig {
        codec: VideoCodec,
        codec_data: Vec<u8>,
        parameter_sets: Vec<Vec<u8>>,
    },
    /// One access unit as annex-B, and whether it is a keyframe.
    VideoFrame { keyframe: bool, data: Vec<u8> },
    /// The screen stream opened or closed. The browser uses it to tell whether a
    /// session is up at all.
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
/// Never blocks: the implementation drops instead, because a live stream should
/// glitch rather than fall behind, and a consumer that is gone for good says so
/// once instead of forever.
pub trait MediaSink: Send + Sync {
    fn push(&self, event: StreamEvent);
}

/// How many access units may wait for the car. Bounded and dropped past, so a
/// session that is behind glitches instead of accumulating latency.
const CL_BACKLOG: usize = 8;

/// The CarLife end of the bridge.
///
/// The car's session is what drains this, and it comes and goes with the car:
/// the receiving half is handed out when a session starts and given back when it
/// ends, so the next one gets it. Frames that arrive in between have nobody to go
/// to and are dropped, which is what the sink says once rather than per frame.
pub struct ClOutlet {
    tx: mpsc::Sender<Vec<u8>>,
    rx: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
}

impl ClOutlet {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(CL_BACKLOG);
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// The video half. There is one sink because there is one channel that
    /// carries anything yet: audio lands on the same sink and is dropped there.
    pub fn video(&self) -> Arc<ClSink> {
        Arc::new(ClSink {
            tx: self.tx.clone(),
            pending: Mutex::new(Vec::new()),
            no_reader: AtomicBool::new(false),
            behind: AtomicBool::new(false),
            no_audio: AtomicBool::new(false),
        })
    }

    /// The receiving half, for the CarLife session. One car, one owner: a second
    /// call gets nothing.
    pub fn take_receiver(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.rx.lock().ok()?.take()
    }

    /// Given back when a session ends so the next one can use it.
    pub fn return_receiver(&self, rx: mpsc::Receiver<Vec<u8>>) {
        if let Ok(mut guard) = self.rx.lock() {
            *guard = Some(rx);
        }
    }
}

/// Where the CarPlay streams go when the car is the one reading them.
pub struct ClSink {
    tx: mpsc::Sender<Vec<u8>>,
    /// Parameter sets from the newest codec description, put in front of the
    /// next frame. Held outside the queue deliberately: the queue can be full,
    /// and a dropped SPS/PPS costs everything that follows it.
    pending: Mutex<Vec<u8>>,
    /// Each of these is said once. A stream that keeps arriving in the wrong
    /// state would otherwise print a line per frame.
    no_reader: AtomicBool,
    behind: AtomicBool,
    no_audio: AtomicBool,
}

impl ClSink {
    /// One access unit on its way to the video channel.
    fn send(&self, access_unit: Vec<u8>) {
        match self.tx.try_send(access_unit) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                if !self.no_reader.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "[cl] no CarLife session is reading the video channel; dropping frames"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if !self.behind.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "[cl] the car is behind on video; dropping frames rather than \
                         falling further behind"
                    );
                }
            }
        }
    }
}

impl MediaSink for ClSink {
    fn push(&self, event: StreamEvent) {
        match event {
            // The car reads an elementary stream, so the parameter sets have to
            // be in it. The raw record is for decoders told out of band and is
            // dropped here; `parameter_sets` is the same thing already
            // converted.
            StreamEvent::VideoConfig { parameter_sets, .. } => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.clear();
                    for set in parameter_sets {
                        pending.extend_from_slice(&set);
                    }
                }
            }
            StreamEvent::VideoFrame { data, .. } => {
                let mut access_unit = self
                    .pending
                    .lock()
                    .map(|mut pending| std::mem::take(&mut *pending))
                    .unwrap_or_default();
                access_unit.extend_from_slice(&data);
                self.send(access_unit);
            }
            // The media channel wants PCM, and what a phone on wireless CarPlay
            // sends is AAC-LC, so it has to be decoded before it can go there.
            // Until it is, these are dropped.
            StreamEvent::AudioConfig { .. } | StreamEvent::AudioPacket { .. } => {
                if !self.no_audio.swap(true, Ordering::Relaxed) {
                    tracing::warn!("[cl] the car's media channel is not fed yet; audio is dropped");
                }
            }
            // Neither has a channel of its own: the car learns the audio format
            // from its own negotiation, and a screen opening says nothing the
            // video channel does not already say.
            StreamEvent::ScreenActive(_) | StreamEvent::AudioStopped { .. } => {}
        }
    }
}
