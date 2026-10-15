//! Where the media goes once CarPlay has handed it over.
//!
//! One consumer takes the streams: the head unit on the other end of a CarLife
//! session. The events below carry the *description* of a stream as well as its
//! bytes, because the phone sends the codec's parameter sets after the stream has
//! already started, and a consumer has to know where to put them.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

/// What the audio pipeline agreed to deliver, which decides how the frames are
/// framed for whoever decodes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// 16-bit PCM, already swapped to little-endian for the consumer.
    Pcm { rate: u32, channels: u16 },
    /// Raw AAC-LC frames with no ADTS header: the framing is stated alongside
    /// them, so an ADTS header would only be in the way.
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
    ///
    /// Both AAC-LC bits mean 48 kHz, whatever the table calls them. The phone
    /// sets the lower one whatever it was offered and then sends frames 1024
    /// samples apart 46.9 times a second — 48 kHz on the wire, measured. Reading
    /// that as 44.1 kHz is what made the pacer run 8.8% slower than the sound it
    /// was pacing, and so what made the queue in front of it grow for as long as
    /// the music played.
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
            return Self::AacLc {
                rate: 48_000,
                channels: 2,
            };
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

/// One thing a stream produced, on its way to the car.
///
/// The events carry the *description* of a stream as well as its bytes, because
/// the phone sends the codec's parameter sets after the stream has already
/// started, and a consumer has to know where to put them.
///
/// Not all of it has a reader: the car takes the frames and the sound, and has
/// no use for the keyframe flag — what it reads is an elementary stream, where
/// the frames say for themselves what they are — nor for whether the screen
/// stream is up, which the video channel says by going quiet. Both are carried
/// for whoever does need them, which is what the allow is for.
#[allow(dead_code)]
#[derive(Debug)]
pub enum StreamEvent {
    /// A video stream's codec description, sent once before any frame.
    ///
    /// `parameter_sets` are the SPS/PPS the avcC record holds, already converted
    /// to annex-B, which is what a consumer reading an elementary stream has to
    /// put in front of the frames. That is what CarLife's video channel is.
    VideoConfig {
        parameter_sets: Vec<Vec<u8>>,
    },
    /// One access unit as annex-B, and whether it is a keyframe.
    VideoFrame { keyframe: bool, data: Vec<u8> },
    /// The screen stream opened or closed. Says whether a session is up at all.
    ScreenActive(bool),
    /// An audio stream's format. `name` is the phone's own `audioType` from the
    /// SETUP — `media`, `telephony`, `speechrecognition` — which the car hears
    /// as its own separate channels.
    AudioConfig {
        stream_type: u8,
        codec: AudioCodec,
        name: String,
    },
    /// One audio frame. `timestamp_us` is the phone's RTP sample counter turned
    /// into microseconds, which is what playback has to be scheduled against.
    ///
    /// What `data` holds is the codec named above, except on the way to the car:
    /// the media channel takes PCM and the phone sends AAC-LC, so that one is
    /// decoded on the way past. See `airplay::audio`.
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

/// One thing on its way to the car's media channel, which is where the phone's
/// music goes.
///
/// The channel takes PCM, so the codec is already off whatever gets here. What
/// is left is what the car has to be told about the samples before it is given
/// them, which is why the format travels on the queue rather than being a fixed
/// property of the channel: a stream that opens after a silence starts again
/// from an announcement of its own.
#[derive(Debug)]
pub enum Media {
    /// What the samples that follow are: a rate and a channel count, which the
    /// car hears as `MSG_MEDIA_INIT` before it is given any of them.
    Format { rate: u32, channels: u16 },
    /// 16-bit signed little-endian PCM, in the format above.
    Pcm { timestamp_ms: u32, data: Vec<u8> },
    /// The stream that was feeding the channel has ended.
    Ended,
}

/// The phone's name for the one audio stream the car's media channel carries.
///
/// The other two it sends are `telephony` and `speechrecognition`, which are a
/// call and a voice assistant: the car has its own channels for both, and they
/// are not this one.
const MEDIA_STREAM: &str = "media";

/// How many access units may wait for the car. Bounded and dropped past, so a
/// session that is behind glitches instead of accumulating latency.
const CL_BACKLOG: usize = 8;

/// The two channels towards the car, and their receiving halves.
///
/// The car's session is what drains these, and it comes and goes with the car:
/// both receiving halves are handed out when a session starts and given back
/// when it ends, so the next one gets them. Frames that arrive in between have
/// nobody to go to and are dropped, which is what the sink says once rather
/// than per frame.
pub struct ClOutlet {
    video_tx: mpsc::Sender<Vec<u8>>,
    video_rx: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    media_tx: mpsc::Sender<Media>,
    media_rx: Mutex<Option<mpsc::Receiver<Media>>>,
}

impl ClOutlet {
    pub fn new() -> Self {
        let (video_tx, video_rx) = mpsc::channel(CL_BACKLOG);
        let (media_tx, media_rx) = mpsc::channel(CL_BACKLOG);
        Self {
            video_tx,
            video_rx: Mutex::new(Some(video_rx)),
            media_tx,
            media_rx: Mutex::new(Some(media_rx)),
        }
    }

    /// The sink the phone's streams are handed to.
    ///
    /// One object takes all of them: a video event and an audio event differ in
    /// which channel they end up on and in nothing else, and both channels are
    /// the same connection to the same car.
    pub fn sink(&self) -> Arc<ClSink> {
        Arc::new(ClSink {
            video: self.video_tx.clone(),
            media: self.media_tx.clone(),
            pending: Mutex::new(Vec::new()),
            media_stream: AtomicU8::new(NO_STREAM),
            no_video_reader: AtomicBool::new(false),
            video_behind: AtomicBool::new(false),
            no_media_reader: AtomicBool::new(false),
            media_behind: AtomicBool::new(false),
        })
    }

    /// The video half, for the CarLife session. One car, one owner: a second
    /// call gets nothing.
    pub fn take_video(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.video_rx.lock().ok()?.take()
    }

    /// Given back when a session ends so the next one can use it.
    pub fn return_video(&self, rx: mpsc::Receiver<Vec<u8>>) {
        if let Ok(mut guard) = self.video_rx.lock() {
            *guard = Some(rx);
        }
    }

    /// The media half. Taken and given back on the same terms as the video one.
    pub fn take_media(&self) -> Option<mpsc::Receiver<Media>> {
        self.media_rx.lock().ok()?.take()
    }

    pub fn return_media(&self, rx: mpsc::Receiver<Media>) {
        if let Ok(mut guard) = self.media_rx.lock() {
            *guard = Some(rx);
        }
    }
}

/// No audio stream is feeding the media channel. No stream type is numbered
/// below a hundred, so this cannot be mistaken for one.
const NO_STREAM: u8 = 0;

/// Where the CarPlay streams go when the car is the one reading them.
pub struct ClSink {
    video: mpsc::Sender<Vec<u8>>,
    media: mpsc::Sender<Media>,
    /// Parameter sets from the newest codec description, put in front of the
    /// next frame. Held outside the queue deliberately: the queue can be full,
    /// and a dropped SPS/PPS costs everything that follows it.
    pending: Mutex<Vec<u8>>,
    /// Which of the phone's audio streams the media channel is being fed from,
    /// once one of them has said it is the music. A packet carries no name —
    /// only the stream's opening does — so this is what the ones that follow
    /// are measured against.
    media_stream: AtomicU8,
    /// Each of these is said once. A stream that keeps arriving with nowhere to
    /// go would otherwise print a line per frame.
    no_video_reader: AtomicBool,
    video_behind: AtomicBool,
    no_media_reader: AtomicBool,
    media_behind: AtomicBool,
}

impl ClSink {
    /// One thing on its way to the car, on one of the two channels.
    ///
    /// A channel nobody is reading, and one that is full, are each worth a line
    /// once and not once per frame.
    fn send<T>(
        queue: &mpsc::Sender<T>,
        item: T,
        channel: &str,
        no_reader: &AtomicBool,
        behind: &AtomicBool,
    ) {
        match queue.try_send(item) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                if !no_reader.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "[cl] no CarLife session is reading the {channel} channel; \
                         what arrives is dropped"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if !behind.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "[cl] the car is behind on the {channel} channel; dropping rather \
                         than falling further behind"
                    );
                }
            }
        }
    }

    /// The video channel's half of the above, which is most of the callers.
    fn send_video(&self, access_unit: Vec<u8>) {
        Self::send(
            &self.video,
            access_unit,
            "video",
            &self.no_video_reader,
            &self.video_behind,
        );
    }

    fn send_media(&self, item: Media) {
        Self::send(
            &self.media,
            item,
            "media",
            &self.no_media_reader,
            &self.media_behind,
        );
    }
}

impl MediaSink for ClSink {
    fn push(&self, event: StreamEvent) {
        match event {
            // The car reads an elementary stream, so the parameter sets have to
            // be in it. There is nothing else in the event for the car's sake.
            StreamEvent::VideoConfig { parameter_sets } => {
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
                self.send_video(access_unit);
            }
            // Only the music is the car's media channel, and which stream that
            // is is said once, when the stream opens.
            StreamEvent::AudioConfig {
                stream_type,
                codec,
                name,
            } => {
                if name != MEDIA_STREAM {
                    tracing::debug!("[cl] the {name} audio stream is not the car's media channel");
                    return;
                }
                self.media_stream.store(stream_type, Ordering::Relaxed);
                // The format the car is told is the one the phone negotiated.
                // Decoding does not change the rate or the channel count, so
                // those are the two numbers the PCM under it arrives with; see
                // `aac`.
                self.send_media(Media::Format {
                    rate: codec.rate(),
                    channels: codec.channels(),
                });
            }
            StreamEvent::AudioPacket {
                stream_type,
                timestamp_us,
                data,
            } => {
                if stream_type != self.media_stream.load(Ordering::Relaxed) {
                    return;
                }
                self.send_media(Media::Pcm {
                    timestamp_ms: (timestamp_us / 1_000) as u32,
                    data,
                });
            }
            StreamEvent::AudioStopped { stream_type } => {
                if stream_type != self.media_stream.swap(NO_STREAM, Ordering::Relaxed) {
                    return;
                }
                self.send_media(Media::Ended);
            }
            // The car learns nothing from this: it is the video channel that
            // tells it whether there is a picture.
            StreamEvent::ScreenActive(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The phone mixes three different things onto one stream of events and only
    /// one of them is the car's media channel. A call is the car's own
    /// telephone, and the music playing under it is not the call.
    #[test]
    fn only_the_music_finds_its_way_to_the_media_channel() {
        let outlet = ClOutlet::new();
        let sink = outlet.sink();
        let mut media = outlet.take_media().expect("nothing has taken it");

        let opened = |stream_type, name: &str| StreamEvent::AudioConfig {
            stream_type,
            codec: AudioCodec::Pcm {
                rate: 44_100,
                channels: 2,
            },
            name: name.to_string(),
        };
        let sound = |stream_type| StreamEvent::AudioPacket {
            stream_type,
            timestamp_us: 1_234_567,
            data: vec![0x11, 0x22],
        };

        sink.push(opened(100, "telephony"));
        sink.push(sound(100));
        assert!(media.try_recv().is_err(), "a call is not the car's music");

        sink.push(opened(102, "media"));
        sink.push(sound(102));
        assert!(
            matches!(
                media.try_recv(),
                Ok(Media::Format {
                    rate: 44_100,
                    channels: 2
                })
            ),
            "the car is told the format before the samples"
        );
        match media.try_recv() {
            Ok(Media::Pcm { timestamp_ms, data }) => {
                assert_eq!(timestamp_ms, 1_234);
                assert_eq!(data, vec![0x11, 0x22]);
            }
            other => panic!("expected sound on the media channel, got {other:?}"),
        }
    }

    /// Whichever of the two AAC-LC bits the phone sets, the stream is the 48 kHz
    /// one it then sends — and the rate has to be right here, because it is what
    /// the pacer, the decoder and the car are all told.
    #[test]
    fn an_aac_stream_is_48k_whichever_bit_the_phone_names_it_with() {
        for bits in [0x40_0000, 0x80_0000, 0xc0_0000] {
            assert_eq!(
                AudioCodec::from_format(bits),
                AudioCodec::AacLc {
                    rate: 48_000,
                    channels: 2
                },
                "0x{bits:x}"
            );
        }
    }
}
