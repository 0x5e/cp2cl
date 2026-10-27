//! Where the media goes once CarPlay has handed it over.
//!
//! One consumer takes the streams: the head unit on the other end of a CarLife
//! session. The events below carry the *description* of a stream as well as its
//! bytes, because the phone sends the codec's parameter sets after the stream has
//! already started, and a consumer has to know where to put them.
//!
//! The phone's audio does not all go to one place. The music is the car's media
//! channel; the navigation prompt is its Navi TTS channel; a call and the voice
//! assistant are channels of the car's own, which this bridge has nothing to say
//! to. Which is which is decided by the name the phone gives the stream in its
//! SETUP — see `is_media` and `is_navigation`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::navigation::Guidance;
use crate::playing::Playing;

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

/// Which of the phone's streams is feeding one of the car's channels: the type
/// the phone names it with, and the instance it opened for it.
///
/// The type on its own is not enough, and that is the one thing about stream
/// identity worth stating outright. The phone opens a fresh stream for every
/// track change and every prompt, and it re-uses the type number when it does;
/// the teardown for the instance it replaced arrives *after* the replacement has
/// said hello often enough to matter. The two sides of a track change are then
/// two streams with one name, and anything that treats the name as the identity
/// closes the new one when the old one is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamId {
    pub stream_type: u8,
    pub connection_id: i64,
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
    /// SETUP, which decides which of the car's channels the stream feeds.
    AudioConfig {
        stream: StreamId,
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
        stream: StreamId,
        timestamp_us: u64,
        data: Vec<u8>,
    },
    /// One instance of a stream has ended. Which instance it was matters: see
    /// `StreamId`.
    AudioStopped { stream: StreamId },
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

/// One thing on its way to the car's Navi TTS channel, which is where the
/// phone's navigation prompt goes.
///
/// The shape is the media channel's, because the car's messages are: a format,
/// then samples in it, then an end. What is not the same is what happens when
/// the end is missed — a head unit that took the channel up holds the music
/// down until it is told the prompt is over, so `Ended` is sent on a timeout as
/// well as when the phone says so. See `carlife::session`.
#[derive(Debug)]
pub enum Navi {
    /// What the samples that follow are, which the car hears as
    /// `MSG_NAVI_TTS_INIT` before it is given any of them.
    Format { rate: u32, channels: u16 },
    /// 16-bit signed little-endian PCM, in the format above.
    Pcm { timestamp_ms: u32, data: Vec<u8> },
    /// The prompt is over, or is taken to be.
    Ended,
}

/// The phone's name for the one audio stream the car's media channel carries.
const MEDIA_STREAM: &str = "media";

/// Whether this stream is the music.
pub fn is_media(name: &str) -> bool {
    name == MEDIA_STREAM
}

/// Whether this stream is a navigation prompt.
///
/// The phone names the music, a call and the voice assistant, and it labels a
/// prompt as `default` or `alert` — or, on a wired session, does not label it at
/// all, which the SETUP turns into `default`. So everything that is neither the
/// music nor one of the two things the car does itself is a prompt, and writing
/// it that way is shorter than a list of names that keeps growing: LIVI reads
/// the same field the same way, and calls the leftovers `Alert`.
///
/// The two that are left out are a call and Siri. The car has channels of its
/// own for both, and nothing here is bridged to them.
pub fn is_navigation(name: &str) -> bool {
    !is_media(name) && !matches!(name, "telephony" | "speechrecognition")
}

/// Which of the car's channels an audio stream feeds.
enum Role {
    Media,
    Navi,
    /// The car's own: a call or the voice assistant.
    Other,
}

fn role(name: &str) -> Role {
    if is_media(name) {
        Role::Media
    } else if is_navigation(name) {
        Role::Navi
    } else {
        Role::Other
    }
}

/// How many access units may wait for the car. Bounded and dropped past, so a
/// session that is behind glitches instead of accumulating latency.
const CL_BACKLOG: usize = 8;

/// The three streams towards the car, and their receiving halves.
///
/// The car's session is what drains these, and it comes and goes with the car:
/// both receiving halves are handed out when a session starts and given back
/// when it ends, so the next one gets them. Frames that arrive in between have
/// nobody to go to and are dropped, which is what the sink says once rather
/// than per frame.
///
/// The media and navigation channels are separate on purpose. They are separate
/// at the car — one is music and one is speech, and the car mixes them itself —
/// and keeping them apart here is what lets a prompt arrive without the music
/// stopping.
///
/// There are two more queues beside them for what the phone is playing and for
/// where it is going — see `playing` and `guidance` — because a session takes
/// those and gives them back on exactly these terms, and nothing about that
/// changes for them being records rather than streams.
pub struct ClOutlet {
    video_tx: mpsc::Sender<Vec<u8>>,
    video_rx: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    media_tx: mpsc::Sender<Media>,
    media_rx: Mutex<Option<mpsc::Receiver<Media>>>,
    navi_tx: mpsc::Sender<Navi>,
    navi_rx: Mutex<Option<mpsc::Receiver<Navi>>>,
    playing_tx: mpsc::Sender<Playing>,
    playing_rx: Mutex<Option<mpsc::Receiver<Playing>>>,
    guidance_tx: mpsc::Sender<Guidance>,
    guidance_rx: Mutex<Option<mpsc::Receiver<Guidance>>>,
}

impl ClOutlet {
    pub fn new() -> Self {
        let (video_tx, video_rx) = mpsc::channel(CL_BACKLOG);
        let (media_tx, media_rx) = mpsc::channel(CL_BACKLOG);
        let (navi_tx, navi_rx) = mpsc::channel(CL_BACKLOG);
        let (playing_tx, playing_rx) = mpsc::channel(CL_BACKLOG);
        let (guidance_tx, guidance_rx) = mpsc::channel(CL_BACKLOG);
        Self {
            video_tx,
            video_rx: Mutex::new(Some(video_rx)),
            media_tx,
            media_rx: Mutex::new(Some(media_rx)),
            navi_tx,
            navi_rx: Mutex::new(Some(navi_rx)),
            playing_tx,
            playing_rx: Mutex::new(Some(playing_rx)),
            guidance_tx,
            guidance_rx: Mutex::new(Some(guidance_rx)),
        }
    }

    /// The sink the phone's streams are handed to.
    ///
    /// One object takes all of them: a video event and an audio event differ in
    /// which channel they end up on and in nothing else, and both channels are
    /// the same connection to the same car. What the phone is playing, and where
    /// it is going, go through the same object for the same reason.
    pub fn sink(&self) -> Arc<ClSink> {
        Arc::new(ClSink {
            video: self.video_tx.clone(),
            media: self.media_tx.clone(),
            navi: self.navi_tx.clone(),
            playing: self.playing_tx.clone(),
            guidance: self.guidance_tx.clone(),
            pending: Mutex::new(Vec::new()),
            media_stream: Mutex::new(None),
            navi_stream: Mutex::new(None),
            no_video_reader: AtomicBool::new(false),
            video_behind: AtomicBool::new(false),
            no_media_reader: AtomicBool::new(false),
            media_behind: AtomicBool::new(false),
            no_navi_reader: AtomicBool::new(false),
            navi_behind: AtomicBool::new(false),
            playing_dropped: AtomicBool::new(false),
            guidance_dropped: AtomicBool::new(false),
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

    /// The navigation half, on the same terms as the two above.
    pub fn take_navi(&self) -> Option<mpsc::Receiver<Navi>> {
        self.navi_rx.lock().ok()?.take()
    }

    pub fn return_navi(&self, rx: mpsc::Receiver<Navi>) {
        if let Ok(mut guard) = self.navi_rx.lock() {
            *guard = Some(rx);
        }
    }

    /// What the phone is playing. Taken and given back on the same terms as the
    /// three above.
    pub fn take_playing(&self) -> Option<mpsc::Receiver<Playing>> {
        self.playing_rx.lock().ok()?.take()
    }

    pub fn return_playing(&self, rx: mpsc::Receiver<Playing>) {
        if let Ok(mut guard) = self.playing_rx.lock() {
            *guard = Some(rx);
        }
    }

    /// Where the phone is going. Taken and given back on the same terms as the
    /// four above.
    pub fn take_guidance(&self) -> Option<mpsc::Receiver<Guidance>> {
        self.guidance_rx.lock().ok()?.take()
    }

    pub fn return_guidance(&self, rx: mpsc::Receiver<Guidance>) {
        if let Ok(mut guard) = self.guidance_rx.lock() {
            *guard = Some(rx);
        }
    }
}

/// Whether one of the car's channels is being fed from this stream.
fn feeding(slot: &Mutex<Option<StreamId>>, stream: StreamId) -> bool {
    slot.lock().map(|held| *held == Some(stream)).unwrap_or(false)
}

/// Points a channel at a stream. A stream that opens replaces what the channel
/// was on, which is what the phone does when it moves its music to a new one.
fn feed(slot: &Mutex<Option<StreamId>>, stream: StreamId) {
    if let Ok(mut held) = slot.lock() {
        *held = Some(stream);
    }
}

/// Lets go of a channel if this is the stream feeding it, and says whether it
/// was — an instance that ends after its replacement took over has nothing to
/// say to the car.
fn starve(slot: &Mutex<Option<StreamId>>, stream: StreamId) -> bool {
    match slot.lock() {
        Ok(mut held) if *held == Some(stream) => {
            *held = None;
            true
        }
        _ => false,
    }
}

/// Where the CarPlay streams go when the car is the one reading them.
pub struct ClSink {
    video: mpsc::Sender<Vec<u8>>,
    media: mpsc::Sender<Media>,
    navi: mpsc::Sender<Navi>,
    playing: mpsc::Sender<Playing>,
    guidance: mpsc::Sender<Guidance>,
    /// Parameter sets from the newest codec description, put in front of the
    /// next frame. Held outside the queue deliberately: the queue can be full,
    /// and a dropped SPS/PPS costs everything that follows it.
    pending: Mutex<Vec<u8>>,
    /// Which instance of which stream each of the two audio channels is being
    /// fed from. A packet carries no name — only the stream's opening does — so
    /// this is what the ones that follow are measured against. The two are kept
    /// apart so that one stream ending cannot take the other channel with it.
    media_stream: Mutex<Option<StreamId>>,
    navi_stream: Mutex<Option<StreamId>>,
    /// Each of these is said once. A stream that keeps arriving with nowhere to
    /// go would otherwise print a line per frame.
    no_video_reader: AtomicBool,
    video_behind: AtomicBool,
    no_media_reader: AtomicBool,
    media_behind: AtomicBool,
    no_navi_reader: AtomicBool,
    navi_behind: AtomicBool,
    /// And once for what the phone is playing, which is dropped in its own way.
    playing_dropped: AtomicBool,
    /// And once for where it is going, which is dropped the same way.
    guidance_dropped: AtomicBool,
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
                    tracing::warn!("carlife.outlet_dropped channel={channel} reason=no_reader");
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if !behind.swap(true, Ordering::Relaxed) {
                    tracing::warn!("carlife.outlet_backlog_dropped channel={channel}");
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

    fn send_navi(&self, item: Navi) {
        Self::send(
            &self.navi,
            item,
            "navi",
            &self.no_navi_reader,
            &self.navi_behind,
        );
    }

    /// What the phone is playing, on its way to the car's panel.
    ///
    /// Its own queue, and its own way of losing things. A track is a record
    /// rather than a flow, so a car that is not reading is not *behind*: what it
    /// did not hear is stale, and the next thing the phone says replaces it. So
    /// the two ways of having nowhere to go are one line here, and one line
    /// only — a phone playing to nobody would otherwise print a song per song.
    pub fn playing(&self, item: Playing) {
        if self.playing.try_send(item).is_err()
            && !self.playing_dropped.swap(true, Ordering::Relaxed)
        {
            tracing::warn!("carlife.media_dropped reason=no_reader");
        }
    }

    /// Where the phone is going, on its way to the car's turn-by-turn strip.
    ///
    /// Dropped on exactly the terms of the one above, and for the same reason: a
    /// description of a route is a record rather than a flow, so a car that is
    /// not reading is not behind — the phone describes the route again as it
    /// goes, and the newest description is the one worth having.
    pub fn guidance(&self, item: Guidance) {
        if self.guidance.try_send(item).is_err()
            && !self.guidance_dropped.swap(true, Ordering::Relaxed)
        {
            tracing::warn!("carlife.navi_dropped reason=no_reader");
        }
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
            // Which of the car's channels a stream feeds is said once, when the
            // stream opens, and by the name the phone gave it.
            StreamEvent::AudioConfig {
                stream,
                codec,
                name,
            } => match role(&name) {
                Role::Media => {
                    feed(&self.media_stream, stream);
                    // The format the car is told is the one the phone
                    // negotiated. Decoding does not change the rate or the
                    // channel count, so those are the two numbers the PCM under
                    // it arrives with; see `aac`.
                    self.send_media(Media::Format {
                        rate: codec.rate(),
                        channels: codec.channels(),
                    });
                }
                Role::Navi => {
                    feed(&self.navi_stream, stream);
                    // Here decoding does change both: the prompt arrives as
                    // Opus at 48 kHz and the car is given 16 kHz mono. The
                    // codec on the event is what the car gets, not what the
                    // phone sends — see `airplay::audio::delivered`.
                    self.send_navi(Navi::Format {
                        rate: codec.rate(),
                        channels: codec.channels(),
                    });
                }
                Role::Other => {
                    tracing::debug!("carlife.audio_unbridged name={name:?}");
                }
            },
            StreamEvent::AudioPacket {
                stream,
                timestamp_us,
                data,
            } => {
                let at = (timestamp_us / 1_000) as u32;
                if feeding(&self.media_stream, stream) {
                    self.send_media(Media::Pcm {
                        timestamp_ms: at,
                        data,
                    });
                } else if feeding(&self.navi_stream, stream) {
                    self.send_navi(Navi::Pcm {
                        timestamp_ms: at,
                        data,
                    });
                }
            }
            // One instance ended. If it is the one the channel is on, the
            // channel ends with it; if it is the one it replaced, the channel
            // has nothing to say, which is the whole reason an event names an
            // instance rather than a type.
            StreamEvent::AudioStopped { stream } => {
                if starve(&self.media_stream, stream) {
                    self.send_media(Media::Ended);
                }
                if starve(&self.navi_stream, stream) {
                    self.send_navi(Navi::Ended);
                }
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

    /// One stream: a type the phone names it with, and the instance it opened.
    fn id(stream_type: u8, connection_id: i64) -> StreamId {
        StreamId {
            stream_type,
            connection_id,
        }
    }

    /// The phone mixes three different things onto one stream of events and only
    /// one of them is the car's media channel. A call is the car's own
    /// telephone, and the music playing under it is not the call.
    #[test]
    fn only_the_music_finds_its_way_to_the_media_channel() {
        let outlet = ClOutlet::new();
        let sink = outlet.sink();
        let mut media = outlet.take_media().expect("nothing has taken it");
        let mut navi = outlet.take_navi().expect("nothing has taken it");

        let opened = |stream, name: &str| StreamEvent::AudioConfig {
            stream,
            codec: AudioCodec::Pcm {
                rate: 44_100,
                channels: 2,
            },
            name: name.to_string(),
        };
        let sound = |stream| StreamEvent::AudioPacket {
            stream,
            timestamp_us: 1_234_567,
            data: vec![0x11, 0x22],
        };

        sink.push(opened(id(100, 1), "telephony"));
        sink.push(sound(id(100, 1)));
        assert!(media.try_recv().is_err(), "a call is not the car's music");
        assert!(navi.try_recv().is_err(), "nor is it a prompt");

        sink.push(opened(id(102, 2), "media"));
        sink.push(sound(id(102, 2)));
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

    /// A prompt is a stream of its own, and it goes to the car's other channel
    /// rather than through the music's — which is what lets sound come out of
    /// the navigation while the music keeps playing.
    #[test]
    fn a_prompt_goes_to_the_navigation_channel_and_the_music_carries_on() {
        let outlet = ClOutlet::new();
        let sink = outlet.sink();
        let mut media = outlet.take_media().expect("nothing has taken it");
        let mut navi = outlet.take_navi().expect("nothing has taken it");

        sink.push(StreamEvent::AudioConfig {
            stream: id(102, 7),
            codec: AudioCodec::AacLc {
                rate: 48_000,
                channels: 2,
            },
            name: "media".into(),
        });
        // The prompt opens as its own type and its own instance, with the codec
        // it is delivered in rather than the one on the wire.
        sink.push(StreamEvent::AudioConfig {
            stream: id(100, 8),
            codec: AudioCodec::Pcm {
                rate: 16_000,
                channels: 1,
            },
            name: "default".into(),
        });
        sink.push(StreamEvent::AudioPacket {
            stream: id(100, 8),
            timestamp_us: 2_000_000,
            data: vec![0xaa],
        });

        assert!(
            matches!(
                navi.try_recv(),
                Ok(Navi::Format {
                    rate: 16_000,
                    channels: 1
                })
            ),
            "the prompt channel is told what its samples are"
        );
        assert!(matches!(navi.try_recv(), Ok(Navi::Pcm { data, .. }) if data == vec![0xaa]));
        // The music is still where it was: opening a prompt says nothing on the
        // media channel at all.
        assert!(matches!(media.try_recv(), Ok(Media::Format { .. })));
        assert!(media.try_recv().is_err(), "the prompt is not the music");
    }

    /// The phone re-opens a type for every track change, so an instance that
    /// ends can be one the channel has already moved off — and then the channel
    /// has nothing to say. Ending the music whenever an older instance of it
    /// says goodbye is what used to stop the sound for good.
    #[test]
    fn an_instance_that_was_replaced_cannot_end_the_channel() {
        let outlet = ClOutlet::new();
        let sink = outlet.sink();
        let mut media = outlet.take_media().expect("nothing has taken it");

        let opened = |connection| StreamEvent::AudioConfig {
            stream: id(102, connection),
            codec: AudioCodec::AacLc {
                rate: 48_000,
                channels: 2,
            },
            name: "media".into(),
        };
        sink.push(opened(3));
        assert!(matches!(media.try_recv(), Ok(Media::Format { .. })));

        // A newer instance of the same type takes the channel over.
        sink.push(opened(4));
        assert!(matches!(media.try_recv(), Ok(Media::Format { .. })));

        // The one it replaced is torn down late. Nothing is said to the car.
        sink.push(StreamEvent::AudioStopped { stream: id(102, 3) });
        assert!(
            media.try_recv().is_err(),
            "the teardown of a replaced instance is not the end of the music"
        );

        // And the music still reaches it.
        sink.push(StreamEvent::AudioPacket {
            stream: id(102, 4),
            timestamp_us: 1_000,
            data: vec![0x55],
        });
        assert!(matches!(media.try_recv(), Ok(Media::Pcm { .. })));

        // The instance the channel is actually on is a different matter.
        sink.push(StreamEvent::AudioStopped { stream: id(102, 4) });
        assert!(matches!(media.try_recv(), Ok(Media::Ended)));
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
