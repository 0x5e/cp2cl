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
//! **AAC-LC and the Opus of every speech stream are decoded here, and PCM is
//! not.** The car at the other end of the bridge takes 16-bit PCM on its media
//! channel and on the two it keeps for speech, a phone on wireless CarPlay sends
//! its music as AAC-LC and its speech as Opus, so both codecs come off on this
//! side of the `StreamSink` — the stream's codec is known exactly here and
//! nowhere later.
//!
//! A speech stream is also the one whose rate changes on the way past: the car's
//! speech channels are 16 kHz whatever the phone sent. Wireless, that is Opus at
//! 48 kHz and a 3:1 decimation — see `Decimator`. Wired, it is PCM at 24 kHz and
//! a 3:2 one — see `Thirds`. That covers a navigation prompt, a call and the
//! voice assistant alike — the three the phone names, and the three the car's
//! two speech channels carry.

use rusty_opus::OpusDecoder;

use super::crypto::{open, SymKey};
use crate::aac;
use crate::sink::{self, AudioCodec, StreamSink, StreamEvent, StreamId};
use std::sync::Arc;

pub const RTP_HEADER_LEN: usize = 12;
const TAG_LEN: usize = 16;
const NONCE_LEN: usize = 8;

/// The rate every Opus stream AirPlay carries is encoded at, whatever the car is
/// later given.
const OPUS_RATE: u32 = 48_000;

/// What the car's speech channels are given, and what both of them carry: the
/// CarLife guide writes 16 kHz mono for the VR channel in both directions.
const SPEECH_RATE: u32 = 16_000;

/// The rate a wired session's speech stream is negotiated at. The phone picks
/// it, not us — `audioFormat` 0x40 is 24 kHz mono.
const WIRED_SPEECH_RATE: u32 = 24_000;

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
    sink: Arc<dyn StreamSink>,
    /// What takes the codec off, when what the phone sends is not already what
    /// the car takes. `None` for PCM, which needs nothing, and for the Opus of a
    /// stream the car is never given.
    decoder: Option<aac::Decoder>,
    /// The same, for a stream the car is given as speech: Opus off, and out at
    /// the rate the car's speech channels take.
    speech: Option<Speech>,
    /// The wired speech stream's own way there: PCM, so nothing to decode, but
    /// 24 kHz on the wire against the 16 kHz the car takes.
    wired: Option<Narrow>,
    /// Whether a frame has already been reported as dropped. A frame the decoder
    /// refuses is dropped, which is a moment of sound rather than the stream —
    /// but a stream it refuses *every* frame of would otherwise be a line per
    /// frame instead of one line.
    dropped: bool,
}

impl AudioReceiver {
    /// `name` is the phone's `audioType` for the stream and `two_way` says it
    /// named a send port, which together decide whether it is speech; `stream`
    /// is the instance, which is what every frame is stamped with.
    pub fn new(
        key: SymKey,
        stream: StreamId,
        codec: AudioCodec,
        name: &str,
        two_way: bool,
        sink: Arc<dyn StreamSink>,
    ) -> Self {
        let delivered = delivered(codec, name, two_way);
        let decoder = match codec {
            AudioCodec::AacLc { rate, channels } => match aac::Decoder::new(rate, channels) {
                Some(decoder) => Some(decoder),
                None => {
                    tracing::warn!(
                        "carplay.audio.codec_unsupported codec=aac-lc rate_hz={rate} channels={channels} \
                         consequence=frames_dropped"
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
        // What `delivered` decided, in the form the frames need: a speech stream
        // that arrives wider than the channel, and is not decoded on the way.
        let wired = match (codec, delivered) {
            (AudioCodec::Pcm { rate, channels }, AudioCodec::Pcm { rate: SPEECH_RATE, .. })
                if rate != SPEECH_RATE || channels != 1 =>
            {
                Some(Narrow::new(rate))
            }
            _ => None,
        };
        Self {
            key,
            stream,
            codec,
            sink,
            decoder,
            speech,
            wired,
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
            tracing::warn!("carplay.audio.packet_open_failed");
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
            AudioCodec::Pcm { channels, .. } => {
                let pcm = to_le16(&payload);
                match self.wired.as_mut() {
                    Some(narrow) => narrow.push(&pcm, channels),
                    None => pcm,
                }
            }
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
                // No decoder: the stream is not speech the car is given, or the
                // decoder could not be built. `new` has said which.
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
            tracing::warn!("carplay.audio.stream_dropped consequence=no_sound");
        }
    }
}

/// What the car is given for a stream.
///
/// The music arrives as AAC-LC and is decoded, which changes neither the rate nor
/// the channel count. Everything the car's speech channels carry — a navigation
/// prompt, a call, the voice assistant — is 16 kHz mono there, and the phone
/// sends it wider: 48 kHz Opus on a wireless session, and on a wired one the PCM
/// its `audioFormat` names, which has been 24 kHz mono and may be 48 kHz stereo.
/// So the format the car is told is the format this returns, not the one the
/// phone negotiated, and anything this does not know how to narrow is left as it
/// arrived rather than announced as something it is not.
pub fn delivered(codec: AudioCodec, name: &str, two_way: bool) -> AudioCodec {
    if !(sink::is_navigation(name, two_way) || sink::is_voice(name, two_way)) {
        return codec;
    }
    let speech = AudioCodec::Pcm {
        rate: SPEECH_RATE,
        channels: 1,
    };
    match codec {
        AudioCodec::Opus { .. } => speech,
        // Every rate a wired speech stream has been seen at, mono or stereo.
        // `Narrow` is the other half of this decision and is built from the same
        // two numbers.
        AudioCodec::Pcm {
            rate: WIRED_SPEECH_RATE | OPUS_RATE,
            channels: 1 | 2,
        } => speech,
        // Already at the car's rate and in stereo: only the mix is left to do.
        AudioCodec::Pcm {
            rate: SPEECH_RATE,
            channels: 2,
        } => speech,
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

/// Little-endian s16 in, one channel of -1..1 out: a stereo pair is averaged,
/// which is what a call's two channels are for — the same voice in both.
fn to_mono(pcm: &[u8], channels: u16) -> Vec<f32> {
    let samples: Vec<f32> = pcm
        .chunks_exact(2)
        .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / f32::from(i16::MAX))
        .collect();
    if channels < 2 {
        return samples;
    }
    samples
        .chunks_exact(2)
        .map(|pair| (pair[0] + pair[1]) / 2.0)
        .collect()
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
                tracing::warn!("carplay.audio.prompt_decoder_missing err={e}");
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
                tracing::debug!("carplay.audio.prompt_frame_dropped err={e}");
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

/// A speech stream that arrives wider than the car's channel: mixed to one
/// channel, and brought down in the ratio its rate calls for.
///
/// Only the rates a wired session has been seen to negotiate, because those are
/// the only ones `delivered` will announce as 16 kHz — a stream this does not
/// cover is left at its own rate rather than played at the wrong speed.
enum Narrow {
    /// Already 16 kHz and only in stereo: the mix is all that is left to do.
    Same,
    /// 48 kHz: one sample in three.
    ThreeToOne(Decimator),
    /// 24 kHz: two in three.
    ThreeToTwo(Thirds),
}

impl Narrow {
    fn new(rate: u32) -> Self {
        match rate {
            SPEECH_RATE => Self::Same,
            WIRED_SPEECH_RATE => Self::ThreeToTwo(Thirds::default()),
            _ => Self::ThreeToOne(Decimator::default()),
        }
    }

    /// One packet of little-endian s16 in, mono 16 kHz little-endian s16 out.
    fn push(&mut self, pcm: &[u8], channels: u16) -> Vec<u8> {
        let mono = to_mono(pcm, channels);
        match self {
            Self::Same => mono.iter().flat_map(|s| to_i16(*s).to_le_bytes()).collect(),
            Self::ThreeToOne(decimator) => decimator.push(&mono),
            Self::ThreeToTwo(thirds) => thirds.push(&mono),
        }
    }
}

/// 24 kHz → 16 kHz: three samples in, two out, with a low-pass in front.
///
/// The two kept samples of each triple fall at the first and the middle of the
/// three, and the middle one is half way between two input samples — so the same
/// kernel is needed twice, once centred on a sample and once between two. Both
/// are 41 taps of a Hamming-windowed sinc at 7 kHz over the 24 kHz input: flat to
/// 4 kHz, and 48 dB down by the 8 kHz that is the most a 16 kHz channel can
/// carry, which is where a wired call's sibilants would otherwise fold back.
///
/// Fixed at 3:2 because that is the one ratio a wired speech stream arrives at,
/// for the reason `Decimator` is fixed at 3:1.
struct Thirds {
    /// The tail of the frame before this one, which the kernel's first taps
    /// reach into. Shorter than the kernel: only half of it is behind the centre.
    history: [f32; SPEECH_CENTRE],
}

const SPEECH_TAPS: usize = 41;
const SPEECH_CENTRE: usize = SPEECH_TAPS / 2;

/// Centred on an input sample, oldest first.
const SPEECH_KEEP: [f32; SPEECH_TAPS] = [
    -0.001104340, -0.000372006, 0.001815611, -0.000631626, -0.002896342, 0.003227011, 0.003069759,
    -0.007844337, 0.000000000, 0.013102266, -0.008607479, -0.015327723, 0.023541349, 0.008826670,
    -0.043057651, 0.014278732, 0.062957587, -0.071373490, -0.077904540, 0.306188893, 0.584223308,
    0.306188893, -0.077904540, -0.071373490, 0.062957587, 0.014278732, -0.043057651, 0.008826670,
    0.023541349, -0.015327723, -0.008607479, 0.013102266, 0.000000000, -0.007844337, 0.003069759,
    0.003227011, -0.002896342, -0.000631626, 0.001815611, -0.000372006, -0.001104340,
];

/// The same kernel half a sample later, which is where the second of each pair
/// of output samples falls. Indexed against the same input samples as `KEEP`.
const SPEECH_HALF: [f32; SPEECH_TAPS] = [
    0.001187684, -0.000175598, -0.001563669, 0.001378659, 0.001879823, -0.003870853, -0.000730966,
    0.007289957, -0.003630851, -0.009563581, 0.011993429, 0.007180490, -0.022919452, 0.003702082,
    0.032138398, -0.026089500, -0.032503524, 0.062783364, 0.011736023, -0.126952945, 0.082135174,
    0.507938197, 0.499330365, 0.078017163, -0.116459404, 0.010388710, 0.053563997, -0.026684048,
    -0.020568047, 0.024269331, 0.002669644, -0.015725659, 0.004668237, 0.007355178, -0.005509188,
    -0.001959662, 0.003694948, -0.000352815, -0.001847972, 0.000959367, 0.000847513,
];

impl Default for Thirds {
    fn default() -> Self {
        Self {
            history: [0.0; SPEECH_CENTRE],
        }
    }
}

impl Thirds {
    /// One frame of 24 kHz samples in, two thirds as many at 16 kHz.
    ///
    /// A frame that does not end on a whole triple keeps the samples that did
    /// not make one, which at 20 ms a frame is a frame that never happens.
    fn push(&mut self, frame: &[f32]) -> Vec<u8> {
        let whole = frame.len() - frame.len() % 3;
        let mut out = Vec::with_capacity(whole * 2 / 3 * 2);
        for base in (0..whole).step_by(3) {
            for kernel in [&SPEECH_KEEP, &SPEECH_HALF] {
                out.extend_from_slice(&to_i16(self.tap(&frame, base, kernel)).to_le_bytes());
            }
        }
        if frame.len() >= SPEECH_CENTRE {
            self.history
                .copy_from_slice(&frame[frame.len() - SPEECH_CENTRE..]);
        }
        out
    }

    /// One output sample: the kernel centred on `base`, or half a sample past it.
    ///
    /// Past the end of the frame it holds the last sample, for the reason
    /// `Decimator` does: the kernel's last few taps reach into the frame after
    /// this one, and a frame of 20 ms of speech is worth more than a frame of
    /// delay to wait for them.
    fn tap(&self, frame: &[f32], base: usize, kernel: &[f32; SPEECH_TAPS]) -> f32 {
        let origin = base as isize - SPEECH_CENTRE as isize;
        let mut acc = 0.0f32;
        for (i, weight) in kernel.iter().enumerate() {
            let at = origin + i as isize;
            let sample = if at < 0 {
                self.history[(at + SPEECH_CENTRE as isize) as usize]
            } else {
                frame[(at as usize).min(frame.len() - 1)]
            };
            acc += weight * sample;
        }
        acc
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

    /// The car's speech channels take 16 kHz mono. Wireless, every speech stream
    /// arrives as 48 kHz Opus; on a wired one, as the PCM its `audioFormat`
    /// names. What the car is *told* has to be what it is *given* — and a stream
    /// this cannot narrow, or one that is not speech at all, is left alone.
    #[test]
    fn a_prompt_is_delivered_at_the_rate_the_cars_channel_takes() {
        let opus = AudioCodec::Opus {
            rate: 48_000,
            channels: 1,
        };
        assert_eq!(
            delivered(opus, "default", false),
            AudioCodec::Pcm {
                rate: 16_000,
                channels: 1
            }
        );
        assert_eq!(
            delivered(opus, "alert", false),
            AudioCodec::Pcm {
                rate: 16_000,
                channels: 1
            }
        );
        // A call and the voice assistant are Opus as well, and the bridge gives
        // them the same rate as a prompt: the car has one channel for speech
        // whichever of the two is talking.
        for name in ["telephony", "speechrecognition"] {
            assert_eq!(
                delivered(opus, name, false),
                AudioCodec::Pcm {
                    rate: 16_000,
                    channels: 1
                },
                "{name}"
            );
        }
        // The music is decoded, but decoding changes neither number.
        let aac = AudioCodec::AacLc {
            rate: 48_000,
            channels: 2,
        };
        assert_eq!(delivered(aac, "media", false), aac);
        // A wired speech stream is PCM, and 24 kHz rather than the 16 the car
        // takes, so it is narrowed too.
        let wired = AudioCodec::Pcm {
            rate: 24_000,
            channels: 1,
        };
        assert_eq!(
            delivered(wired, "speechrecognition", false),
            AudioCodec::Pcm {
                rate: 16_000,
                channels: 1
            }
        );
        // The music channel is not a speech channel and keeps its rate.
        assert_eq!(delivered(wired, "media", false), wired);
        // A call that arrived at 48 kHz stereo — which is what a wired one has
        // been seen to do — is both mixed down and decimated.
        let wide = AudioCodec::Pcm {
            rate: 48_000,
            channels: 2,
        };
        assert_eq!(
            delivered(wide, "telephony", false),
            AudioCodec::Pcm {
                rate: 16_000,
                channels: 1
            }
        );
        // And a call the phone labelled with the music's own name is still a
        // call, because it named a port for a microphone.
        assert_eq!(
            delivered(wide, "media", true),
            AudioCodec::Pcm {
                rate: 16_000,
                channels: 1
            }
        );
        // And one this cannot narrow is left as it arrived rather than announced
        // as something it is not.
        let odd = AudioCodec::Pcm {
            rate: 44_100,
            channels: 2,
        };
        assert_eq!(delivered(odd, "default", false), odd);
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

    /// A wired speech stream is 24 kHz and the car's channel is 16 kHz: three
    /// samples in, two out. The order of the pair matters — a tone that came out
    /// with its halves swapped would be a buzz rather than speech — so this
    /// compares the whole stretch against the sine it started as.
    #[test]
    fn a_wired_speech_stream_is_decimated_three_to_two() {
        let mut thirds = Thirds::default();
        let mut bytes = Vec::new();
        for frame in 0..4 {
            let base = frame * 480;
            let samples: Vec<f32> = (0..480)
                .map(|i| {
                    let t = (base + i) as f32 / WIRED_SPEECH_RATE as f32;
                    0.5 * (2.0 * std::f32::consts::PI * 1_000.0 * t).sin()
                })
                .collect();
            bytes.extend_from_slice(&thirds.push(&samples));
        }
        assert_eq!(bytes.len(), 4 * 480 * 2 / 3 * 2, "two for every three");

        let out: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let ideal = |m: usize| {
            0.5 * f32::from(i16::MAX)
                * (2.0 * std::f32::consts::PI * 1_000.0 * m as f32 / SPEECH_RATE as f32).sin()
        };
        // Past the kernel's history and clear of the held tail.
        let worst = out[120..1_100]
            .iter()
            .enumerate()
            .map(|(i, sample)| (f32::from(*sample) - ideal(120 + i)).abs())
            .fold(0.0f32, f32::max);
        assert!(
            worst < 0.05 * f32::from(i16::MAX),
            "worst deviation {worst} is too far from the tone"
        );
    }

    /// A call at 48 kHz stereo is the widest a wired speech stream has been
    /// seen: the pair is mixed to one channel and then decimated, and the mix
    /// has to leave the amplitude alone — a call whose two channels carry the
    /// same voice is not half as loud as a mono one.
    #[test]
    fn a_wider_speech_stream_is_mixed_down_and_decimated() {
        let mut narrow = Narrow::new(48_000);
        let mut bytes = Vec::new();
        for frame in 0..3 {
            let base = frame * 960;
            let mut samples = Vec::with_capacity(960 * 4);
            for i in 0..960 {
                let t = (base + i) as f32 / 48_000.0;
                let sample =
                    to_i16(0.5 * (2.0 * std::f32::consts::PI * 1_000.0 * t).sin()).to_le_bytes();
                samples.extend_from_slice(&sample);
                samples.extend_from_slice(&sample);
            }
            bytes.extend_from_slice(&narrow.push(&samples, 2));
        }
        assert_eq!(bytes.len(), 3 * 960 / 3 * 2, "mono, one in three");

        let out: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let peak = out[40..280].iter().map(|s| i32::from(*s).abs()).max();
        let expected = (0.5 * f32::from(i16::MAX)) as i32;
        assert!(
            (peak.unwrap() - expected).abs() < expected / 20,
            "peak {peak:?} is not near {expected}"
        );
    }
}
