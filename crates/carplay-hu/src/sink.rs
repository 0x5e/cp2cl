//! Where the phone's streams go once the receiver has decoded them.
//!
//! The events carry the *description* of a stream as well as its bytes, because
//! the phone sends the codec's parameter sets after the stream has already
//! started, and a consumer has to know where to put them.
//!
//! The phone's audio does not all go to one place: the music is the car's media
//! channel, a navigation prompt is its TTS channel, and a call and the voice
//! assistant are channels of the car's own that this bridge has nothing to say
//! to. Which is which is decided by the name the phone gives the stream in its
//! SETUP — see `is_media` and `is_navigation` — and what a consumer does with
//! that is its own business.

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
/// Not all of it has a reader: the car takes the frames and the sound, and has
/// no use for the keyframe flag — what it reads is an elementary stream, where
/// the frames say for themselves what they are — nor for whether the screen
/// stream is up, which the video channel says by going quiet.
#[allow(dead_code)]
#[derive(Debug)]
pub enum StreamEvent {
    /// A video stream's codec description, sent once before any frame.
    ///
    /// `parameter_sets` are the SPS/PPS the avcC record holds, already converted
    /// to annex-B, which is what a consumer reading an elementary stream has to
    /// put in front of the frames.
    VideoConfig { parameter_sets: Vec<Vec<u8>> },
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
    /// decoded on the way past.
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
/// it that way is shorter than a list of names that keeps growing.
///
/// The two that are left out are a call and Siri. The car has channels of its
/// own for both, and nothing here is bridged to them.
pub fn is_navigation(name: &str) -> bool {
    !is_media(name) && !matches!(name, "telephony" | "speechrecognition")
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A call and an assistant are the car's own channels; everything else the
    /// phone mixes onto its audio streams is either the music or a prompt.
    #[test]
    fn the_phones_stream_names_are_read_as_the_cars_channels() {
        assert!(is_media("media"));
        assert!(!is_navigation("media"));
        assert!(is_navigation("default"));
        assert!(is_navigation("alert"));
        assert!(!is_navigation("telephony"));
        assert!(!is_navigation("speechrecognition"));
    }
}
