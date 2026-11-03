//! The AAC a phone on wireless CarPlay sends, taken off on its way to the car.
//!
//! The car's media channel takes 16-bit PCM and the phone sends AAC-LC, so the
//! codec has to be decoded somewhere between the two. This is that place, and
//! it is the only place in the program where a stream is decoded rather than
//! forwarded — which also makes it the only cost that is set by the phone's
//! encoder rather than by the wire.
//!
//! The frames are the codec's own, with no ADTS header: what the phone put in
//! each packet is what `airplay::audio` hands over, so the format is told to
//! the decoder rather than read out of the stream. Decoding changes neither the
//! rate nor the channel count, which is what lets the car be told the same two
//! numbers the phone negotiated with us.

use symphonia::core::audio::{Channels, SampleBuffer, SignalSpec};
use symphonia::core::codecs::{CodecParameters, Decoder as _, DecoderOptions, CODEC_TYPE_AAC};
use symphonia::core::formats::Packet;
use symphonia::default::codecs::AacDecoder;

/// The samples one AAC-LC frame decodes to. The decoder taken here refuses
/// anything that decodes to a different number of them, so this is a fact about
/// the codec rather than a guess.
const FRAME: u64 = 1024;

/// One AAC-LC stream, decoded a frame at a time.
///
/// The format stays put for the life of a stream, so one of these is made when
/// the stream opens and dropped when it ends; a stream that changes rate is a
/// new stream.
pub struct Decoder {
    decoder: AacDecoder,
    /// Where one frame's samples land, sized for the stream's channel count. It
    /// is sized once because a frame of sound is the same size every time, and
    /// allocating per frame is the one thing that would make this expensive.
    samples: SampleBuffer<i16>,
    /// The same samples, as the bytes the car takes.
    pcm: Vec<u8>,
}

impl Decoder {
    /// A decoder for AAC-LC at a rate and a channel count.
    ///
    /// `None` is a format this decoder does not take — a rate of zero, or more
    /// channels than the car is listening to — which leaves the stream to be
    /// dropped rather than half decoded.
    pub fn new(rate: u32, channels: u16) -> Option<Self> {
        let channels = match channels {
            1 => Channels::FRONT_LEFT,
            2 => Channels::FRONT_LEFT | Channels::FRONT_RIGHT,
            _ => return None,
        };
        if rate == 0 {
            return None;
        }
        // No extra data: symphonia reads the audio specific config when a
        // container supplies one, and falls back to these two numbers when it
        // does not — which is this case, and exactly the two the phone named.
        let mut params = CodecParameters::new();
        params
            .for_codec(CODEC_TYPE_AAC)
            .with_sample_rate(rate)
            .with_channels(channels);
        Some(Self {
            decoder: AacDecoder::try_new(&params, &DecoderOptions::default()).ok()?,
            samples: SampleBuffer::new(FRAME, SignalSpec::new(rate, channels)),
            pcm: Vec::new(),
        })
    }

    /// One frame, decoded into interleaved 16-bit little-endian PCM.
    ///
    /// `None` is a frame the decoder would not take. It is dropped, which is one
    /// frame of sound rather than the stream, and the caller is what says so.
    pub fn decode(&mut self, frame: &[u8]) -> Option<&[u8]> {
        // An empty frame is not a frame. The decoder answers one with a frame's
        // worth of silence rather than an error, and silence the phone did not
        // send is not something to play at the car.
        if frame.is_empty() {
            return None;
        }
        let decoded = self
            .decoder
            .decode(&Packet::new_from_slice(0, 0, 0, frame))
            .ok()?;
        // A frame that decodes to more samples than the buffer holds panics
        // inside the copy below, and a panic here takes the session with it.
        // Nothing the phone sends should reach this, so it is refused rather
        // than sized for.
        if decoded.frames() * decoded.spec().channels.count() > self.samples.capacity() {
            return None;
        }
        self.samples.copy_interleaved_ref(decoded);
        self.pcm.clear();
        self.pcm.reserve(self.samples.len() * 2);
        for sample in self.samples.samples() {
            self.pcm.extend_from_slice(&sample.to_le_bytes());
        }
        Some(&self.pcm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One AAC-LC frame as a phone sends it: 1024 samples of two channels at
    /// 44.1 kHz, 177 bytes, and no ADTS header.
    ///
    /// The bytes come from ffmpeg — a 440 Hz sine a fiftieth of a second long,
    /// encoded at 64 kbit/s and then taken out of its ADTS framing, which is the
    /// part the phone does not send.
    const FRAME_OF_A_TONE: [u8; 177] = [
        0x21, 0x4a, 0xda, 0xfe, 0x1f, 0x87, 0xe1, 0x33, 0x65, 0x3a, 0xb2, 0x38, 0x9d, 0x92, 0xbb,
        0x1e, 0x3f, 0x5e, 0xb8, 0xd7, 0x1a, 0xbd, 0x7f, 0xfb, 0x7c, 0xfe, 0xfd, 0x71, 0xae, 0x35,
        0xad, 0x7f, 0xf5, 0xfb, 0x7f, 0x3e, 0x78, 0xb7, 0x15, 0xde, 0x84, 0x73, 0xd8, 0x10, 0x8f,
        0x65, 0x81, 0x7b, 0x3a, 0x94, 0xb6, 0x54, 0x70, 0x19, 0xce, 0x03, 0x38, 0x48, 0x31, 0x27,
        0x70, 0x90, 0x90, 0x60, 0x67, 0x70, 0x90, 0x90, 0x60, 0x67, 0x70, 0x90, 0x90, 0x60, 0x60,
        0x67, 0x77, 0x09, 0x09, 0x09, 0x06, 0x06, 0x06, 0x77, 0x70, 0x90, 0x92, 0xb9, 0x1f, 0xf1,
        0x51, 0x19, 0x50, 0xda, 0x5b, 0xa4, 0x50, 0x35, 0x4d, 0xe7, 0x9b, 0xf9, 0xcc, 0xfe, 0x7a,
        0xb5, 0x8f, 0xe7, 0xfc, 0xf5, 0x80, 0xf9, 0x7c, 0x9e, 0x60, 0x03, 0xe5, 0xf2, 0xf9, 0x3c,
        0xf3, 0x00, 0x1f, 0x2f, 0x97, 0xca, 0x04, 0xbc, 0xad, 0x6b, 0xa3, 0x32, 0xf0, 0xc3, 0x0a,
        0x15, 0x47, 0x35, 0x1f, 0x2a, 0x0d, 0xf2, 0x54, 0xc8, 0xef, 0x83, 0xbe, 0x0e, 0xd8, 0xf1,
        0xfa, 0xf5, 0xc6, 0x83, 0xfa, 0x7c, 0xfe, 0xfd, 0x71, 0xa0, 0xfe, 0x9f, 0x6f, 0xe7, 0xcf,
        0x16, 0xe2, 0xbb, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xe0,
    ];

    #[test]
    fn a_frame_of_aac_becomes_the_pcm_the_car_takes() {
        let mut decoder = Decoder::new(44_100, 2).expect("44.1 kHz stereo is taken");
        let pcm = decoder.decode(&FRAME_OF_A_TONE).expect("the frame decodes");

        // 1024 samples in each of two channels, two bytes each.
        assert_eq!(pcm.len(), 1024 * 2 * 2);
        // And it is a signal rather than silence, which is what a decoder that
        // produced the right length of nothing would also pass.
        let loudest = pcm
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]).unsigned_abs())
            .max()
            .unwrap_or(0);
        assert!(loudest > 1000, "the tone is only {loudest} loud");
    }

    #[test]
    fn a_frame_that_is_not_one_is_refused_rather_than_believed() {
        let mut decoder = Decoder::new(44_100, 2).expect("44.1 kHz stereo is taken");
        // Truncated partway: the decoder runs out of bits rather than making
        // the rest up.
        assert!(decoder.decode(&FRAME_OF_A_TONE[..8]).is_none());
        // Nothing at all, which is not a frame and is not played as silence.
        assert!(decoder.decode(&[]).is_none());
    }

    #[test]
    fn a_format_this_decoder_does_not_take_is_refused_up_front() {
        assert!(Decoder::new(0, 2).is_none());
        assert!(Decoder::new(44_100, 6).is_none());
    }
}
