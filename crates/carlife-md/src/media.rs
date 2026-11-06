//! What the media, Navi TTS and voice channels carry.
//!
//! The camera side takes PCM, so the format travels on the queue with the
//! samples rather than being a fixed property of the channel.

/// One item for the car's media channel (`MSG_MEDIA_*`).
#[derive(Debug)]
pub enum Media {
    Format { rate: u32, channels: u16 },
    /// 16-bit signed little-endian PCM in the format above.
    Pcm { timestamp_ms: u32, data: Vec<u8> },
    Ended,
}

/// One item for the car's Navi TTS channel (`MSG_NAVI_TTS_*`).
///
/// Same shape as `Media`. `Ended` is also sent on a timeout: a head unit that
/// took the channel up holds the music down until it hears the prompt is over.
#[derive(Debug)]
pub enum Navi {
    Format { rate: u32, channels: u16 },
    Pcm { timestamp_ms: u32, data: Vec<u8> },
    Ended,
}

/// One item for the car's voice channel (`MSG_VR_*`).
///
/// The same shape again, and the only channel of the three that runs both ways:
/// the microphone comes back up it as `MSG_VR_MIC_DATA`. `Ended` is not sent on
/// a timeout here — silence in a call is silence, not the end of one, and the
/// phone closes the stream when it is over.
#[derive(Debug)]
pub enum Voice {
    Format { rate: u32, channels: u16 },
    Pcm { timestamp_ms: u32, data: Vec<u8> },
    Ended,
}
