//! Audio arrives as UDP packets, each one self-contained and sealed.
//!
//! Layout per packet:
//!
//! ```text
//! [12 byte RTP header][ciphertext][16 byte tag][8 byte nonce]
//! ```
//!
//! The additional authenticated data is `packet[4..12]` — the RTP timestamp and
//! SSRC, notably *not* the leading four bytes. The nonce is four zero bytes
//! followed by those trailing eight, little endian. Unlike video there is no
//! counter to track, which is one fewer way to get out of step.
//!
//! What comes out is the RTP payload, and what happens next depends entirely on
//! what was negotiated in the audio SETUP: raw PCM, an AAC-LC frame, or an Opus
//! frame. Frames go out exactly as the codec packs them — a decoder is told the
//! codec out of band, so wrapping them (ADTS, once) would only be in the way.
//!
//! **AAC-LC and the navigation prompt's Opus are decoded here, and PCM is not.**
//! The car at the other end of the bridge takes 16-bit PCM on its media channel
//! and its navigation channel, a phone on wireless CarPlay sends its music as
//! AAC-LC and its prompts as Opus, so both codecs come off on this side of the
//! `MediaSink` — the stream's codec is known exactly here and nowhere later. The
//! two voice streams that are not prompts, a call and the voice assistant,
//! belong to the car's own channels; they arrive as Opus too and are not decoded
//! at all, because nothing downstream of here is going to be given them.
//!
//! A prompt is also the one stream whose rate changes on the way past: Opus is
//! 48 kHz on the wire and the car's navigation channel is given 16 kHz, so what
//! comes out of the decoder is decimated 3:1. See `Decimator`.

use rusty_opus::OpusDecoder;

use super::crypto::{open, SymKey};
use crate::aac;
use crate::sink::{self, AudioCodec, MediaSink, StreamEvent, StreamId};
use std::sync::Arc;

pub const RTP_HEADER_LEN: usize = 12;
const TAG_LEN: usize = 16;
const NONCE_LEN: usize = 8;

/// The rate every Opus stream AirPlay carries is encoded at, whatever the car is
/// later given.
const OPUS_RATE: u32 = 48_000;

/// What the car's navigation channel is given.
const SPEECH_RATE: u32 = 16_000;

/// The longest frame an Opus packet may carry: 120 ms at 48 kHz.
const OPUS_FRAME: usize = 5_760;

pub struct AudioReceiver {
    key: SymKey,
    /// Which instance of which stream this is; the far end has a channel per
    /// stream and mixes one at a time, so it has to travel with every frame.
    stream: StreamId,
    /// What the phone sends, which decides how a packet's counter is turned into
    /// a moment.
    codec: AudioCodec,
    sink: Arc<dyn MediaSink>,
    /// What takes the codec off, when what the phone sends is not already what
    /// the car takes. `None` for PCM, which needs nothing, and for the Opus of a
    /// stream the car is never given.
    decoder: Option<aac::Decoder>,
    /// The same, for a prompt: Opus off, and out at the rate the car's channel
    /// takes.
    speech: Option<Speech>,
    /// Whether a frame has already been reported as dropped. A frame the decoder
    /// refuses is dropped, which is a moment of sound rather than the stream —
    /// but a stream it refuses *every* frame of would otherwise be a line per
    /// frame instead of one line.
    dropped: bool,
}

impl AudioReceiver {
    /// `name` is the phone's `audioType` for the stream, which is what decides
    /// whether it is a prompt; `stream` is the instance, which is what every
    /// frame is stamped with.
    pub fn new(
        key: SymKey,
        stream: StreamId,
        codec: AudioCodec,
        name: &str,
        sink: Arc<dyn MediaSink>,
    ) -> Self {
        let delivered = delivered(codec, name);
        let decoder = match codec {
            AudioCodec::AacLc { rate, channels } => match aac::Decoder::new(rate, channels) {
                Some(decoder) => Some(decoder),
                None => {
                    tracing::warn!(
                        "[audio] AAC-LC at {rate} Hz in {channels} channel(s) is not something \
                         this decoder takes; the stream's frames are dropped"
                    );
                    None
                }
            },
            _ => None,
        };
        let speech = match delivered {
            AudioCodec::Pcm { .. } if matches!(codec, AudioCodec::Opus { .. }) => Speech::new(),
            _ => None,
        };
        Self {
            key,
            stream,
            codec,
            sink,
            decoder,
            speech,
            dropped: false,
        }
    }

    /// One packet's frame, decoded but not yet handed anywhere.
    ///
    /// The buffered stream is re-timed before it reaches the consumer, so the
    /// pacer wants the frame and its timestamp apart from the sink.
    pub fn decode(&mut self, packet: &[u8]) -> Option<(u64, Vec<u8>)> {
        if !could_be_a_frame(packet) {
            return None;
        }
        let end = packet.len() - NONCE_LEN;
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&packet[end..]);
        let aad = &packet[4..RTP_HEADER_LEN];
        let sealed = &packet[RTP_HEADER_LEN..end];

        let Ok(payload) = open(&self.key, &nonce, sealed, aad) else {
            tracing::warn!("[audio] packet failed to open");
            return None;
        };
        if payload.is_empty() {
            return None;
        }

        // Read before the frame is decoded, and from the *wire* codec: decoding
        // does not move the phone's counter, and the car's rate is not the
        // counter's.
        let at = sample_stamp(self.codec.rate(), packet);
        let data = match self.codec {
            // Raw samples, but every consumer wants them little-endian and
            // CarPlay sends them big-endian. Swapping every pair is the whole
            // fix — skip it and the sound is pure noise.
            AudioCodec::Pcm { .. } => to_le16(&payload),
            AudioCodec::AacLc { .. } => {
                let decoded = match self.decoder.as_mut() {
                    Some(decoder) => decoder.decode(&payload).map(<[u8]>::to_vec),
                    None => None,
                };
                match decoded {
                    Some(pcm) => pcm,
                    None => {
                        self.say_dropped();
                        return None;
                    }
                }
            }
            AudioCodec::Opus { .. } => match self.speech.as_mut() {
                Some(speech) => match speech.decode(&payload) {
                    Some(pcm) => pcm,
                    None => {
                        self.say_dropped();
                        return None;
                    }
                },
                // A call or the voice assistant, which the car has channels of
                // its own for: there is nothing to decode, and nothing is being
                // dropped that anyone wanted.
                None => return None,
            },
        };
        Some((at, data))
    }

    pub fn feed(&mut self, packet: &[u8]) {
        if let Some((timestamp_us, data)) = self.decode(packet) {
            self.sink.push(StreamEvent::AudioPacket {
                stream: self.stream,
                timestamp_us,
                data,
            });
        }
    }

    /// Says once that this stream's frames are going nowhere.
    fn say_dropped(&mut self) {
        if !self.dropped {
            self.dropped = true;
            tracing::warn!(
                "[audio] this stream's frames are being dropped; the car is being given no \
                 sound from it"
            );
        }
    }
}

/// What the car is given for a stream.
///
/// The music arrives as AAC-LC and is decoded, which changes neither the rate nor
/// the channel count. A prompt arrives as Opus at 48 kHz and the car's navigation
/// channel takes 16 kHz mono, which changes both — so the format the car is told
/// is the format this returns, not the one the phone negotiated. Everything else
/// is passed through as it arrives: a wired session sends PCM, and the car is
/// told whatever that was.
pub fn delivered(codec: AudioCodec, name: &str) -> AudioCodec {
    match codec {
        AudioCodec::Opus { .. } if sink::is_navigation(name) => AudioCodec::Pcm {
            rate: SPEECH_RATE,
            channels: 1,
        },
        _ => codec,
    }
}

/// Whether a datagram is shaped like one of the phone's sealed frames at all.
///
/// The length is all that can be judged from outside the crypto; `decode` is
/// what answers for the contents. The pacer asks this before it decides to hold
/// a packet back for a later moment, which is the only thing it can ask without
/// spending the decode.
pub fn could_be_a_frame(packet: &[u8]) -> bool {
    packet.len() > RTP_HEADER_LEN + TAG_LEN + NONCE_LEN
}

/// The moment the frame in this packet is due, in microseconds.
///
/// The counter is in the clear, ahead of the ciphertext, so a packet can be
/// timed before it is opened — which is what lets the pacer put a frame on the
/// schedule the phone's own timestamps give it, and decode it only when the
/// moment arrives.
pub fn sample_stamp(rate: u32, packet: &[u8]) -> u64 {
    let sample = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]) as u64;
    sample * 1_000_000 / rate.max(1) as u64
}

/// Big-endian s16 → little-endian s16, which is what every consumer expects.
fn to_le16(payload: &[u8]) -> Vec<u8> {
    let mut out = payload.to_vec();
    for pair in out.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
    out
}

/// Takes a prompt off Opus and out at the rate the car's channel takes.
struct Speech {
    decoder: OpusDecoder,
    /// What the decoder writes, one channel at 48 kHz. Kept between frames so
    /// that a frame costs no allocation once the stream is running.
    samples: Vec<f32>,
    decimator: Decimator,
}

impl Speech {
    fn new() -> Option<Self> {
        match OpusDecoder::new(OPUS_RATE as i32, 1) {
            Ok(decoder) => Some(Self {
                decoder,
                samples: vec![0.0; OPUS_FRAME],
                decimator: Decimator::default(),
            }),
            Err(e) => {
                tracing::warn!("[audio] no Opus decoder for the prompt stream: {e}");
                None
            }
        }
    }

    /// One packet's worth of PCM, already at `SPEECH_RATE` and little-endian.
    fn decode(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        // `OPUS_FRAME` is the most a packet may carry rather than what this one
        // does: the decoder returns how much it actually wrote, which is 960
        // samples for the 20 ms frames a prompt is sent in.
        let written = match self.decoder.decode(payload, OPUS_FRAME, &mut self.samples) {
            Ok(n) => n.min(OPUS_FRAME),
            Err(e) => {
                // Once per frame at debug: the warning is the caller's, and says
                // it once.
                tracing::debug!("[audio] a prompt frame did not decode: {e}");
                return None;
            }
        };
        Some(self.decimator.push(&self.samples[..written]))
    }
}

/// 48 kHz → 16 kHz, 3:1, with a low-pass in front of the decimation.
///
/// Fixed at 3:1 because that is the one ratio this program needs, and a general
/// resampler is a much larger thing than a prompt stream is worth. The filter is
/// 31 taps of a Hamming-windowed sinc at 6.5 kHz: flat to 4 kHz, and 20 dB down
/// by 8 kHz, which is where what the decimation cannot represent would otherwise
/// fold back into the speech.
struct Decimator {
    /// The tail of the frame before this one. The filter reaches back further
    /// than a kept sample's own frame in its first few samples, and this is what
    /// it reaches into.
    history: [f32; TAPS / 2],
}

/// Taps in the anti-aliasing filter, and the coefficients, oldest first. Odd, so
/// that the filter is symmetric about its middle tap and the delay is a whole
/// number of input samples.
const TAPS: usize = 31;
const COEFFICIENTS: [f32; TAPS] = [
    0.000331842,
    -0.001248850,
    -0.002932027,
    -0.003154483,
    0.000440317,
    0.007843788,
    0.013827723,
    0.009805489,
    -0.008612661,
    -0.033499635,
    -0.044050316,
    -0.017495533,
    0.053874149,
    0.151813971,
    0.237375089,
    0.271362277,
    0.237375089,
    0.151813971,
    0.053874149,
    -0.017495533,
    -0.044050316,
    -0.033499635,
    -0.008612661,
    0.009805489,
    0.013827723,
    0.007843788,
    0.000440317,
    -0.003154483,
    -0.002932027,
    -0.001248850,
    0.000331842,
];

impl Default for Decimator {
    fn default() -> Self {
        Self {
            history: [0.0; TAPS / 2],
        }
    }
}

impl Decimator {
    /// One frame in, every third sample out, as little-endian s16.
    fn push(&mut self, frame: &[f32]) -> Vec<u8> {
        let kept = frame.len() / 3;
        let mut out = Vec::with_capacity(kept * 2);
        for k in 0..kept {
            let centre = (k * 3) as isize;
            let mut acc = 0.0f32;
            for (tap, weight) in COEFFICIENTS.iter().enumerate() {
                let at = centre + tap as isize - (TAPS as isize / 2);
                // The filter is centred on the sample being kept, so its last
                // few taps reach into the frame that has not arrived yet. The
                // last sample is held across them: eight samples of a 960 in
                // one frame, which is a fifth of a millisecond of speech and
                // cheaper than a frame of delay.
                let sample = if at < 0 {
                    self.history[(at + TAPS as isize / 2) as usize]
                } else {
                    frame[(at as usize).min(frame.len() - 1)]
                };
                acc += weight * sample;
            }
            out.extend_from_slice(&to_i16(acc).to_le_bytes());
        }
        let keep = self.history.len();
        if frame.len() >= keep {
            self.history.copy_from_slice(&frame[frame.len() - keep..]);
        }
        out
    }
}

/// One decoded sample, which the decoder gives in -1..1.
fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the pacer does before it decodes anything: one datagram, shaped like
    /// a sealed frame, with the counter it is to be timed by in the clear.
    #[test]
    fn a_sealed_frame_is_timed_before_it_is_opened() {
        let mut packet = vec![0u8; RTP_HEADER_LEN + TAG_LEN + NONCE_LEN + 4];
        // The counter is the phone's sample index: one second at 44.1 kHz.
        packet[4..8].copy_from_slice(&44_100u32.to_be_bytes());
        assert!(could_be_a_frame(&packet));
        assert_eq!(sample_stamp(44_100, &packet), 1_000_000);

        // Header, tag and nonce with nothing carried between them: not a frame,
        // and nothing in it to time either.
        packet.truncate(RTP_HEADER_LEN + TAG_LEN + NONCE_LEN);
        assert!(!could_be_a_frame(&packet));
    }

    /// The car's navigation channel takes 16 kHz mono, and a prompt arrives as
    /// 48 kHz Opus. What the car is *told* has to be what it is *given*, so this
    /// is the mapping between them — and the three streams that are not prompts
    /// are left alone.
    #[test]
    fn a_prompt_is_delivered_at_the_rate_the_cars_channel_takes() {
        let opus = AudioCodec::Opus {
            rate: 48_000,
            channels: 1,
        };
        assert_eq!(
            delivered(opus, "default"),
            AudioCodec::Pcm {
                rate: 16_000,
                channels: 1
            }
        );
        assert_eq!(
            delivered(opus, "alert"),
            AudioCodec::Pcm {
                rate: 16_000,
                channels: 1
            }
        );
        // A call and the voice assistant are Opus as well, and neither is
        // bridged: they are delivered as they arrive and dropped downstream.
        for name in ["telephony", "speechrecognition"] {
            assert_eq!(delivered(opus, name), opus, "{name}");
        }
        // The music is decoded, but decoding changes neither number.
        let aac = AudioCodec::AacLc {
            rate: 48_000,
            channels: 2,
        };
        assert_eq!(delivered(aac, "media"), aac);
        // And a wired session's PCM is already what the car takes.
        let pcm = AudioCodec::Pcm {
            rate: 24_000,
            channels: 1,
        };
        assert_eq!(delivered(pcm, "default"), pcm);
    }

    /// The decimation has to keep every third sample and nothing else: a frame
    /// of 960 comes out as one of 320, and a tone that fits under the filter's
    /// knee comes out at the amplitude it went in at.
    #[test]
    fn every_third_sample_is_kept_and_the_tone_survives_it() {
        let mut decimator = Decimator::default();
        let mut bytes = Vec::new();
        // Three frames of a 1 kHz tone, so that the filter is past its history
        // by the end, at half of full scale.
        for frame in 0..3 {
            let base = frame * 960;
            let samples: Vec<f32> = (0..960)
                .map(|i| {
                    let t = (base + i) as f32 / OPUS_RATE as f32;
                    0.5 * (2.0 * std::f32::consts::PI * 1_000.0 * t).sin()
                })
                .collect();
            bytes = decimator.push(&samples);
        }
        assert_eq!(bytes.len(), 320 * 2, "one sample in three, as s16");

        let out: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|p| i16::from_le_bytes([p[0], p[1]]))
            .collect();
        // The middle of the frame, past the filter's history and clear of the
        // held tail: a 1 kHz tone is well inside the passband.
        let peak = out[40..280].iter().map(|s| i32::from(*s).abs()).max();
        let expected = (0.5 * f32::from(i16::MAX)) as i32;
        assert!(
            (peak.unwrap() - expected).abs() < expected / 20,
            "peak {peak:?} is not near {expected}"
        );
    }
}
