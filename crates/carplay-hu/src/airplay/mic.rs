//! The microphone uplink: what was captured at this end, on its way to the phone.
//!
//! CarPlay has no microphone stream of its own. The phone asks for one by
//! naming a *send* port in the SETUP of an audio stream; audio then travels the
//! other way over UDP, sealed with the mirror of the key the phone seals its own
//! audio with. Nothing here decodes: whoever captures encodes, and the phone
//! says what it will take, which is exactly how the downlink works.
//!
//! The port is the whole of the signal. It is the only inbound stream that names
//! one — for every other one it is we who do — so neither the stream's type nor
//! the name it is labelled with is asked: a call and an assistant have been seen
//! on more than one type, and a wired call has arrived labelled `media`.
//!
//! The capture side is the CarLife half's: its VR channel carries the car's
//! microphone at 16 kHz mono, and `send_pcm` is where that arrives. What the
//! phone asked for is not what the car produces — a wireless session wants Opus
//! at 48 kHz, a wired one the PCM it negotiated, which for a stream that runs
//! both ways has been 24 kHz — so the conversions the uplink needs, one of two
//! resamples and sometimes an Opus encoder, live here beside the sealing rather
//! than anywhere upstream.
//!
//! The receiver opens and closes this as the phone opens and closes such a
//! stream. The car is not told from here: it records for as long as its voice
//! channel is open, which is the same span, and that is the CarLife half's to
//! decide.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rusty_opus::{Application, OpusEncoder};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use super::crypto::{seal, SymKey};
use crate::sink::AudioCodec;

/// The RTP-shaped header the phone reads before the sealed payload.
const RTP_HEADER_LEN: usize = 12;
/// How much audio one packet carries when the phone does not say: twenty
/// milliseconds, the same default both reference implementations use.
const DEFAULT_FRAME_MS: u32 = 20;
/// Eight bytes of the nonce follow the Poly1305 tag, so the phone can tell a
/// replayed packet from a late one.
const NONCE_LEN: usize = 8;

/// The rate the car's microphone is captured at. CarLife's VR channel fixes it:
/// 16 kHz, mono, signed 16-bit little-endian, around a kilobyte a packet.
const MIC_RATE: u32 = 16_000;

/// The rate Opus runs at, which is what a wireless session's uplink is coded at.
const OPUS_RATE: u32 = 48_000;

/// The rate a wired session negotiates for a stream that runs both ways, and so
/// the rate its uplink has to be produced at. Measured from the SETUP: the car
/// is told `MSG_VR_AUDIO_INIT sampleRate=24000` for exactly these streams.
const WIRED_SPEECH_RATE: u32 = 24_000;

/// The duration of one Opus packet. Fixed rather than taken from the phone:
/// `framesPerPacket` counts codec frames, and one frame of speech is 20 ms.
const OPUS_FRAME_MS: u32 = 20;

/// The most one Opus frame may be: 1275 bytes of payload, plus the table of
/// contents the encoder puts in front of it (RFC 6716 §3.2.1). One frame is one
/// packet here, so this is the whole of what the encoder can write.
const MAX_OPUS_PACKET: usize = 1_276;

/// Which family the captured audio is in. The phone decides by transport rather
/// than by preference — PCM over the cable, Opus over Wi-Fi — and the capture
/// side is told which one to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicCodec {
    Lpcm,
    Opus,
}

impl MicCodec {
    /// The name the capture path knows it by.
    pub fn name(self) -> &'static str {
        match self {
            Self::Lpcm => "lpcm",
            Self::Opus => "opus",
        }
    }
}

/// What a capture path needs to set itself up, and what the uplink needs to
/// seal what comes back. Derived once, from the phone's own audio SETUP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicConfig {
    pub codec: MicCodec,
    pub sample_rate: u32,
    pub channels: u16,
    /// How many samples one packet carries: the phone's `framesPerPacket` turned
    /// into a duration, or 20 ms when it did not say. Per channel, which is what
    /// the RTP timestamp counts.
    pub samples_per_packet: u32,
}

impl MicConfig {
    /// Reads the uplink's shape off the codec the downlink already agreed on.
    ///
    /// There is no separate negotiation. A stream that arrived as Opus is one
    /// the capture side is asked to produce in Opus, and Opus is 48 kHz mono
    /// whatever the stream was described as — the decoder on the way in runs at
    /// 48 kHz for the same reason. PCM is sent back at the rate and channel
    /// count the phone named, `frames_per_packet` being the phone's own packet
    /// timing, which is what keeps the RTP timestamp right.
    pub fn from_codec(codec: AudioCodec, frames_per_packet: u32) -> Self {
        let (mic, channels) = match codec {
            AudioCodec::Opus { .. } => (MicCodec::Opus, 1),
            _ => (MicCodec::Lpcm, codec.channels()),
        };
        let sample_rate = match mic {
            MicCodec::Opus => OPUS_RATE,
            MicCodec::Lpcm => codec.rate(),
        };
        let frame_ms = if frames_per_packet > 0 && sample_rate > 0 {
            (frames_per_packet * 1000 / sample_rate).clamp(5, 60)
        } else {
            DEFAULT_FRAME_MS
        };
        // Opus is the exception: its `framesPerPacket` counts codec frames, so
        // the usual value of one works out as `1000 / 48000`, which truncates to
        // zero and clamps to 5 ms. One CarPlay frame is 20 ms whatever the field
        // says, and the timestamp has to move by what is actually in the packet.
        let samples_per_packet = match mic {
            MicCodec::Opus => (OPUS_RATE / (1_000 / OPUS_FRAME_MS)).max(1),
            MicCodec::Lpcm => (sample_rate * frame_ms / 1000).max(1),
        };
        Self {
            codec: mic,
            sample_rate,
            channels,
            samples_per_packet,
        }
    }

    /// How far the RTP timestamp moves for one payload.
    ///
    /// Whole frames, not bytes — and for Opus a fixed frame rather than a count,
    /// because the encoder decides its own frame size. Getting this wrong is not
    /// fatal, but it is what the phone positions the audio against.
    fn samples(&self, payload: &[u8]) -> u32 {
        match self.codec {
            MicCodec::Opus => self.samples_per_packet,
            MicCodec::Lpcm => {
                let frames = payload.len() / 2 / usize::from(self.channels.max(1));
                frames as u32
            }
        }
    }
}

/// One open uplink: where the packets go and how they are sealed.
struct Uplink {
    socket: UdpSocket,
    key: SymKey,
    /// The stream that named the port, so that a teardown of some other audio
    /// stream does not close an uplink that is still wanted.
    stream: i64,
    /// The RTP payload type, which is the stream type the phone named.
    payload_type: u8,
    config: MicConfig,
    /// RTP sequence and timestamp, and the ChaCha nonce. All three advance once
    /// per packet and none is derived from the others.
    sequence: u16,
    timestamp: u32,
    nonce: u64,
    /// What turns the car's 16 kHz into what the phone asked for. `None` when
    /// the phone asked for something that cannot be produced here.
    coder: Option<Coder>,
    /// Whether the socket has already been reported full. A live stream that is
    /// behind for a moment is ordinary; one that says so every packet is not
    /// worth a line each.
    full: bool,
}

impl Uplink {
    /// Seals one payload and sends it.
    ///
    /// The write does not wait: this runs inside the CarLife session's own
    /// reader, and a packet that cannot go out now is one the phone's jitter
    /// buffer will not get. Dropping it is the same outcome as the network
    /// doing it, and cheaper than stalling the session.
    fn emit(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let samples = self.config.samples(payload);
        // The header is clear and its last eight bytes are authenticated as
        // additional data — the timestamp the phone checks the payload against.
        let mut header = [0u8; RTP_HEADER_LEN];
        header[0] = 0x80;
        header[1] = self.payload_type & 0x7f;
        header[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        header[4..8].copy_from_slice(&self.timestamp.to_be_bytes());

        // Twelve bytes, with the packet counter in the low eight: the same
        // convention as the media streams, and the counter is repeated after the
        // tag as well as being the nonce.
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.nonce.to_le_bytes());
        let sealed = match seal(&self.key, &nonce, payload, &header[4..]) {
            Ok(sealed) => sealed,
            Err(e) => {
                tracing::warn!("carplay.mic.seal_failed err={e}");
                return;
            }
        };

        let mut packet = Vec::with_capacity(RTP_HEADER_LEN + sealed.len() + NONCE_LEN);
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&sealed);
        packet.extend_from_slice(&self.nonce.to_le_bytes());

        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(samples);
        self.nonce += 1;

        match self.socket.try_send(&packet) {
            Ok(_) => self.full = false,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if !self.full {
                    self.full = true;
                    tracing::warn!("carplay.mic.send_blocked consequence=packets_dropped");
                }
            }
            Err(e) => tracing::warn!("carplay.mic.send_failed err={e}"),
        }
    }
}

/// The uplink's coding: the car's 16 kHz mono in, packets of what the phone
/// asked for out.
///
/// The car's microphone arrives in blocks of about a kilobyte, which is 32 ms —
/// neither the packet duration nor the rate either kind of uplink wants. So
/// samples are buffered here until a whole packet is due, and both conversions
/// happen on the way in rather than at the socket.
struct Coder {
    /// `None` for a wired session, whose uplink is PCM, re-framed, resampled if
    /// the phone named a rate other than the car's, and byte-swapped.
    encoder: Option<OpusEncoder>,
    /// Samples at the phone's rate that are not yet a whole packet.
    pending: Vec<i16>,
    /// What the car's 16 kHz has to become before it is what the phone asked for.
    rate: Rate,
    resampler: Upsampler,
    /// One packet's worth of output, reused across packets.
    packet: Vec<u8>,
}

/// What has to happen to the car's 16 kHz before it is what the phone asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rate {
    /// Nothing: a wired session whose stream is the same 16 kHz.
    Same,
    /// Three output samples per input sample, which is 48 kHz — the rate Opus
    /// codes at, and one a wired session may also name.
    Triple,
    /// Three per two, which is 24 kHz: the rate a wired CarPlay voice stream is
    /// negotiated at, measured (`MSG_VR_AUDIO_INIT sampleRate=24000`).
    ThreeHalves,
}

impl Coder {
    /// Sets itself up for what the phone asked for, or says why it cannot.
    fn new(config: &MicConfig) -> Option<Self> {
        // The car's microphone is 16 kHz mono and cannot be made into anything
        // else that is not one of these. Opus codes at 48 kHz whatever the
        // stream was described as; the PCM of a wired session is sent back at
        // the rate the phone named, which has been 24 kHz mono and 48 kHz stereo.
        // A phone that asked for two channels is given the one in both: a
        // microphone is one channel, and that is how a hands-free call is
        // carried anyway.
        let rate = match (config.codec, config.sample_rate) {
            (MicCodec::Opus, _) | (MicCodec::Lpcm, OPUS_RATE) => Some(Rate::Triple),
            (MicCodec::Lpcm, MIC_RATE) => Some(Rate::Same),
            (MicCodec::Lpcm, WIRED_SPEECH_RATE) => Some(Rate::ThreeHalves),
            _ => None,
        };
        let Some(rate) = rate.filter(|_| matches!(config.channels, 1 | 2)) else {
            tracing::warn!(
                "carplay.mic.coding_unsupported codec={:?} rate_hz={} channels={} \
                 consequence=uplink_silent",
                config.codec,
                config.sample_rate,
                config.channels
            );
            return None;
        };
        let encoder = match config.codec {
            MicCodec::Lpcm => None,
            MicCodec::Opus => match OpusEncoder::new(OPUS_RATE as i32, 1, Application::Voip) {
                Ok(mut encoder) => {
                    encoder.bitrate_bps = OPUS_BITRATE as i32;
                    Some(encoder)
                }
                Err(e) => {
                    tracing::warn!("carplay.mic.encoder_unavailable err={e}");
                    return None;
                }
            },
        };
        // Only Opus writes through this, and a wired session has nothing to put
        // in it.
        let packet = if encoder.is_some() {
            vec![0u8; MAX_OPUS_PACKET]
        } else {
            Vec::new()
        };
        Some(Self {
            encoder,
            pending: Vec::with_capacity(config.samples_per_packet as usize),
            rate,
            resampler: Upsampler::default(),
            packet,
        })
    }

    /// One block of the car's microphone, as whole packets.
    fn packets(&mut self, pcm: &[u8], config: &MicConfig) -> Vec<Vec<u8>> {
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        match self.rate {
            Rate::Same => self.pending.extend_from_slice(&samples),
            Rate::Triple => self.resampler.triple(&samples, &mut self.pending),
            Rate::ThreeHalves => self.resampler.three_halves(&samples, &mut self.pending),
        }

        let want = config.samples_per_packet as usize;
        let mut out = Vec::new();
        while self.pending.len() >= want {
            let frame: Vec<i16> = self.pending.drain(..want).collect();
            match self.encoder.as_mut() {
                Some(encoder) => {
                    // The encoder takes its input on the -1..1 scale the
                    // decoder gives back, so whole-scale stays whole-scale.
                    let input: Vec<f32> = frame
                        .iter()
                        .map(|sample| f32::from(*sample) / f32::from(i16::MAX))
                        .collect();
                    match encoder.encode(&input, want, &mut self.packet) {
                        Ok(n) => out.push(self.packet[..n].to_vec()),
                        Err(e) => {
                            tracing::warn!("carplay.mic.encode_failed err={e}");
                            return out;
                        }
                    }
                }
                // Big-endian: the phone's PCM is in the same order as the
                // downlink's, which is also what a wired session arrives in. The
                // one channel the car produced is written once per channel the
                // phone asked for.
                None => {
                    let channels = usize::from(config.channels.max(1));
                    let mut bytes = Vec::with_capacity(want * 2 * channels);
                    for sample in &frame {
                        for _ in 0..channels {
                            bytes.extend_from_slice(&sample.to_be_bytes());
                        }
                    }
                    out.push(bytes);
                }
            }
        }
        out
    }
}

/// The car's 16 kHz made into a multiple of itself, with a low-pass in the
/// interpolation.
///
/// Zero-stuffing alone leaves the 8 kHz the source cannot carry as the images of
/// everything under it, so each output sample is the whole filter centred on a
/// fraction of an input sample — three phases of one 31-tap kernel.
///
/// Two ratios, both of them three output samples per some input samples: 3:1
/// for 48 kHz and 3:2 for 24 kHz. A general resampler is a much larger thing
/// than a microphone uplink is worth, and these are the two CarPlay names.
struct Upsampler {
    /// The input samples the filter still reaches back into.
    history: [i16; TAPS / 3],
    /// The input sample 3:2 is holding: its partner is the next one, and the
    /// three output samples come out of the pair.
    waiting: Option<i16>,
}

impl Default for Upsampler {
    fn default() -> Self {
        Self {
            history: [0; TAPS / 3],
            waiting: None,
        }
    }
}

impl Upsampler {
    /// One block in, three times as many samples out, appended to `out`.
    fn triple(&mut self, input: &[i16], out: &mut Vec<i16>) {
        for &sample in input {
            for phase in 0..3 {
                self.emit(sample, phase, out);
            }
            self.remember(sample);
        }
    }

    /// One block in, half again as many samples out: three for every two.
    ///
    /// The same kernel read at every other one of its three phases, which is
    /// what taking every second sample of the 3:1 stream amounts to. The output
    /// moments are the input sample itself, two thirds of one later, and a third
    /// of one past its partner.
    fn three_halves(&mut self, input: &[i16], out: &mut Vec<i16>) {
        for &sample in input {
            let Some(first) = self.waiting.take() else {
                self.waiting = Some(sample);
                continue;
            };
            self.emit(first, 0, out);
            self.emit(first, 2, out);
            self.remember(first);
            self.emit(sample, 1, out);
            self.remember(sample);
        }
    }

    /// One output sample, `phase` thirds of an input sample after `sample`.
    fn emit(&self, sample: i16, phase: usize, out: &mut Vec<i16>) {
        let mut acc = 0.0f32;
        let mut tap = phase;
        let mut back = 0;
        while tap < TAPS {
            // Phase 0 reaches one sample further back than the others, which is
            // why the history is the longest of the three.
            let taken = if back == 0 {
                sample
            } else {
                self.history[self.history.len() - back]
            };
            acc += COEFFICIENTS[tap] * f32::from(taken);
            tap += 3;
            back += 1;
        }
        out.push(saturate(acc));
    }

    /// Ages `sample` into the history the filter reaches into.
    fn remember(&mut self, sample: i16) {
        self.history.copy_within(1.., 0);
        if let Some(last) = self.history.last_mut() {
            *last = sample;
        }
    }
}

/// One interpolated sample, in the 16-bit units the channel carries. The sum can
/// leave the range by a little — the filter rings — so it is held at the ends
/// rather than wrapped.
fn saturate(sample: f32) -> i16 {
    sample.round().clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// Taps in the interpolation filter, and the coefficients. Odd, so that the
/// kernel is symmetric about its middle tap and each of the three phases sums to
/// one — which is what makes a constant input come out at its own amplitude.
///
/// A Hamming-windowed sinc at 7.4 kHz, the top of what a 16 kHz source holds.
const TAPS: usize = 31;
const COEFFICIENTS: [f32; TAPS] = [
    0.004694120,
    0.005139221,
    0.000229753,
    -0.010780613,
    -0.018956406,
    -0.007643601,
    0.027350403,
    0.058258082,
    0.038189470,
    -0.049171571,
    -0.145455237,
    -0.135109047,
    0.067619086,
    0.427009462,
    0.777223863,
    0.922806032,
    0.777223863,
    0.427009462,
    0.067619086,
    -0.135109047,
    -0.145455237,
    -0.049171571,
    0.038189470,
    0.058258082,
    0.027350403,
    -0.007643601,
    -0.018956406,
    -0.010780613,
    0.000229753,
    0.005139221,
    0.004694120,
];

/// Where audio captured at this end goes.
///
/// One of these is built at startup and shared: the receiver opens and closes it
/// as the phone asks for bidirectional audio, and the capture side hands it what
/// the car heard.
pub struct MicSink {
    uplink: Mutex<Option<Uplink>>,
    /// Whether a block has already been dropped for want of an uplink. A phone
    /// that never names a send port is a fault worth one line, not one per
    /// block of a conversation.
    unheard: AtomicBool,
}

impl MicSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            uplink: Mutex::new(None),
            unheard: AtomicBool::new(false),
        })
    }

    /// Opens the uplink for one audio stream. Replaces whatever was there: the
    /// phone re-opens its audio stream on every call and every request.
    pub(crate) async fn open(
        &self,
        stream: i64,
        peer: SocketAddr,
        payload_type: u8,
        key: SymKey,
        config: MicConfig,
    ) -> std::io::Result<()> {
        // Bound to the same address family as the phone, or the reply goes out
        // from an address it never asked to hear from.
        let bind = if peer.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let socket = UdpSocket::bind(bind).await?;
        socket.connect(peer).await?;
        // Built before the first packet rather than on it, so a stream nothing
        // can be produced for says so once, at the SETUP, rather than per block.
        let coder = Coder::new(&config);
        self.unheard.store(false, Ordering::Relaxed);
        *self.uplink.lock().await = Some(Uplink {
            socket,
            key,
            stream,
            payload_type,
            config,
            sequence: 0,
            timestamp: 0,
            nonce: 0,
            coder,
            full: false,
        });
        Ok(())
    }

    /// Closes it, if it is the one this stream opened: the phone's audio stream
    /// is gone, so nothing we send on it would be heard. Dropping the socket is
    /// the whole of the cleanup.
    pub(crate) async fn close(&self, stream: i64) {
        let mut guard = self.uplink.lock().await;
        if guard.as_ref().is_some_and(|uplink| uplink.stream == stream) {
            *guard = None;
        }
    }

    /// One block of the car's microphone, as the phone asked for it.
    ///
    /// Silently does nothing when there is no uplink: the car keeps recording
    /// until it is told to stop, and racing that stop would only lose the last
    /// block either way. The first such block is reported, though — a phone
    /// that never offered a send port is the one failure this cannot see.
    ///
    /// Not `async`, and it does not wait: this is called from the CarLife
    /// session's reader, which must not be held up by the encoder or by a socket
    /// that is momentarily full.
    pub fn send_pcm(&self, pcm: &[u8]) {
        if pcm.is_empty() {
            return;
        }
        let Ok(mut guard) = self.uplink.try_lock() else {
            return;
        };
        let Some(uplink) = guard.as_mut() else {
            if !self.unheard.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "carplay.mic.no_uplink bytes={} consequence=voice_silent",
                    pcm.len()
                );
            }
            return;
        };
        // Copied out first: the coder is borrowed mutably, and the frame it is
        // handed needs the same struct.
        let config = uplink.config;
        let packets = match uplink.coder.as_mut() {
            Some(coder) => coder.packets(pcm, &config),
            None => return,
        };
        for packet in &packets {
            uplink.emit(packet);
        }
    }
}

/// The uplink's bitrate, which is the reference implementations' tier for
/// 48 kHz speech. Opus always codes at 48 kHz here, so there is no other tier.
const OPUS_BITRATE: u32 = 96_000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::AudioCodec;

    /// The packet size is the phone's own `framesPerPacket` timing, and 20 ms
    /// when it did not say — the same default both reference implementations
    /// fall back to.
    #[test]
    fn packet_size_follows_the_phones_timing() {
        let pcm = MicConfig::from_codec(AudioCodec::Pcm { rate: 16_000, channels: 1 }, 0);
        assert_eq!(pcm.samples_per_packet, 320);
        assert_eq!(pcm.channels, 1);

        // 480 samples at 48 kHz is 10 ms, which is what the phone asked for.
        let told = MicConfig::from_codec(AudioCodec::Pcm { rate: 48_000, channels: 2 }, 480);
        assert_eq!(told.samples_per_packet, 480);
        assert_eq!(told.channels, 2);
    }

    /// A frame duration outside anything plausible is clamped rather than
    /// trusted: it arrives as a number the phone chose, and a zero would make
    /// every RTP timestamp identical.
    #[test]
    fn an_implausible_frame_is_clamped() {
        let too_short = MicConfig::from_codec(AudioCodec::Pcm { rate: 48_000, channels: 1 }, 1);
        assert_eq!(too_short.samples_per_packet, 48_000 * 5 / 1000);
        let too_long = MicConfig::from_codec(AudioCodec::Pcm { rate: 48_000, channels: 1 }, 48_000);
        assert_eq!(too_long.samples_per_packet, 48_000 * 60 / 1000);
    }

    /// Opus is the wireless answer, always mono and always at the rate Opus codes
    /// at, whatever the stream was described as.
    #[test]
    fn opus_is_always_mono_at_forty_eight_kilohertz() {
        for named in [16_000, 24_000, 48_000] {
            let opus =
                MicConfig::from_codec(AudioCodec::Opus { rate: named, channels: 1 }, 0);
            assert_eq!(opus.codec, MicCodec::Opus, "{named}");
            assert_eq!(opus.channels, 1, "{named}");
            assert_eq!(opus.sample_rate, OPUS_RATE, "{named}");
            assert_eq!(opus.samples_per_packet, 960, "{named}");
        }
    }

    /// The RTP timestamp advances by whole frames, not by bytes: a stereo PCM
    /// payload carries the same number of frames in twice the bytes.
    #[test]
    fn the_timestamp_advances_by_frames() {
        let stereo = MicConfig::from_codec(AudioCodec::Pcm { rate: 48_000, channels: 2 }, 0);
        assert_eq!(stereo.samples(&[0u8; 960 * 2 * 2]), 960);
        let mono = MicConfig::from_codec(AudioCodec::Pcm { rate: 48_000, channels: 1 }, 0);
        assert_eq!(mono.samples(&[0u8; 960 * 2]), 960);
    }

    /// `framesPerPacket` counts codec frames, so the usual one works out as a
    /// five-millisecond clamp if it is read as samples. One CarPlay frame is
    /// 20 ms, and the timestamp has to move by what the packet holds — a fifth
    /// of it puts the phone's playout position out by the same factor.
    #[test]
    fn one_opus_frame_is_twenty_milliseconds_whatever_the_field_says() {
        for frames in 0..=2 {
            let opus =
                MicConfig::from_codec(AudioCodec::Opus { rate: 48_000, channels: 1 }, frames);
            assert_eq!(opus.samples_per_packet, 960, "framesPerPacket={frames}");
        }
    }

    /// The car's microphone is 16 kHz and the phone's Opus is 48 kHz, so every
    /// input sample becomes three: a tone that fits under the filter's knee has
    /// to come out at the amplitude it went in at.
    #[test]
    fn the_microphone_is_resampled_three_to_one() {
        let mut upsampler = Upsampler::default();
        let mut out = Vec::new();
        for block in 0..4 {
            let samples: Vec<i16> = (0..320)
                .map(|i| {
                    let t = (block * 320 + i) as f32 / MIC_RATE as f32;
                    saturate(0.5 * f32::from(i16::MAX) * (2.0 * std::f32::consts::PI * 1_000.0 * t).sin())
                })
                .collect();
            upsampler.triple(&samples, &mut out);
        }
        assert_eq!(out.len(), 4 * 320 * 3);

        // Past the filter's history, and clear of the held tail: a 1 kHz tone is
        // well inside the passband.
        let peak = out[960..2_880]
            .iter()
            .map(|s| i32::from(*s).abs())
            .max();
        let expected = (0.5 * f32::from(i16::MAX)) as i32;
        assert!(
            (peak.unwrap() - expected).abs() < expected / 10,
            "peak {peak:?} is not near {expected}"
        );
    }

    /// And a wired session's 24 kHz uplink is the same kernel at every other
    /// phase: taking every second sample of the 48 kHz stream. Both are run over
    /// the same tone, and the one has to be the other decimated — which is what
    /// says the phases come out in the right order rather than merely at the
    /// right amplitude.
    #[test]
    fn the_microphone_is_resampled_three_to_two() {
        let tone = |block: usize| -> Vec<i16> {
            (0..320)
                .map(|i| {
                    let t = (block * 320 + i) as f32 / MIC_RATE as f32;
                    saturate(0.5 * f32::from(i16::MAX) * (2.0 * std::f32::consts::PI * 1_000.0 * t).sin())
                })
                .collect()
        };

        let (mut triple, mut halves) = (Upsampler::default(), Upsampler::default());
        let (mut at_48k, mut at_24k) = (Vec::new(), Vec::new());
        for block in 0..4 {
            let samples = tone(block);
            triple.triple(&samples, &mut at_48k);
            halves.three_halves(&samples, &mut at_24k);
        }

        assert_eq!(at_24k.len(), at_48k.len() / 2);
        let decimated: Vec<i16> = at_48k.iter().step_by(2).copied().collect();
        assert_eq!(at_24k, decimated);
    }

    /// What the car gives is a block of about a kilobyte — 32 ms — and what the
    /// phone wants is a whole packet. Re-framing is the same for both codings,
    /// and for the cable's there is nothing else to do but the byte order.
    #[test]
    fn a_block_is_reframed_into_whole_packets() {
        let config = MicConfig::from_codec(AudioCodec::Pcm { rate: 16_000, channels: 1 }, 0);
        let mut coder = Coder::new(&config).expect("16 kHz mono is what the car produces");
        let block = vec![0x01u8; 1024]; // 512 samples, 32 ms

        let first = coder.packets(&block, &config);
        assert_eq!(first.len(), 1, "one whole packet, and 192 samples over");
        assert_eq!(first[0].len(), 320 * 2);
        let second = coder.packets(&block, &config);
        assert_eq!(second.len(), 2, "the rest of the first block, and one more");
    }

    /// Anything the phone asks for that the car's microphone cannot be made
    /// into is refused before the car is told to record, rather than feeding it
    /// a stream that would go nowhere. The wired session's 24 kHz is one of the
    /// three it can, in either channel count — a call has been negotiated at
    /// 48 kHz stereo, and refusing that is a microphone the phone never hears.
    #[test]
    fn a_rate_nothing_here_can_produce_is_refused() {
        let odd = MicConfig::from_codec(AudioCodec::Pcm { rate: 44_100, channels: 2 }, 0);
        assert!(Coder::new(&odd).is_none());
        for rate in [MIC_RATE, WIRED_SPEECH_RATE, OPUS_RATE] {
            for channels in [1, 2] {
                let wired = MicConfig::from_codec(AudioCodec::Pcm { rate, channels }, 0);
                assert!(Coder::new(&wired).is_some(), "{rate} / {channels}");
            }
        }
        let three = MicConfig::from_codec(AudioCodec::Pcm { rate: 16_000, channels: 3 }, 0);
        assert!(Coder::new(&three).is_none(), "there is no third channel");
        let opus = MicConfig::from_codec(AudioCodec::Opus { rate: 48_000, channels: 1 }, 0);
        assert!(Coder::new(&opus).is_some());
    }

    /// A phone that asked for two channels is given the one the car produced in
    /// both: the packets are twice the bytes, and the two halves say the same
    /// thing — a call carried on the right channel only would be heard on one
    /// side of the car.
    #[test]
    fn a_stereo_uplink_carries_the_one_channel_twice() {
        let stereo =
            MicConfig::from_codec(AudioCodec::Pcm { rate: WIRED_SPEECH_RATE, channels: 2 }, 0);
        assert_eq!(stereo.channels, 2);
        assert_eq!(stereo.samples_per_packet, 480);
        let mut coder = Coder::new(&stereo).expect("24 kHz stereo is a wired call");

        // 480 input samples of 16 kHz become 720 of 24 kHz, which is one packet
        // of 480 plus 240 held over.
        let block: Vec<u8> = (0..480)
            .flat_map(|i| saturate(0.5 * f32::from(i16::MAX) * ((i as f32) * 0.01).sin()).to_le_bytes())
            .collect();
        let packets = coder.packets(&block, &stereo);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].len(), 480 * 2 * 2, "frames of two channels");

        let pairs: Vec<(i16, i16)> = packets[0]
            .chunks_exact(4)
            .map(|four| {
                (
                    i16::from_be_bytes([four[0], four[1]]),
                    i16::from_be_bytes([four[2], four[3]]),
                )
            })
            .collect();
        assert_eq!(pairs.len(), 480);
        assert!(pairs.iter().all(|(left, right)| left == right));
    }

    /// A 24 kHz uplink is three samples for every two, so a block of 512 gives
    /// 768 and a 20 ms packet is 480 of them — and the bytes that go out are
    /// that many, in the phone's order.
    #[test]
    fn a_wired_voice_stream_is_resampled_and_reframed() {
        let config = MicConfig::from_codec(AudioCodec::Pcm { rate: WIRED_SPEECH_RATE, channels: 1 }, 0);
        assert_eq!(config.samples_per_packet, 480);
        let mut coder = Coder::new(&config).expect("24 kHz mono is what a wired session names");

        let block = vec![0x01u8; 1024]; // 512 samples, 32 ms
        let first = coder.packets(&block, &config);
        assert_eq!(first.len(), 1, "480 of the 768, and 288 over");
        assert_eq!(first[0].len(), 480 * 2);
        let second = coder.packets(&block, &config);
        assert_eq!(second.len(), 2, "the 288 held over, and one more");
    }

    /// The uplink is coded by the same crate that decodes the downlink, so a
    /// packet it produces has to be one that crate reads back: two packets of a
    /// half-scale tone, 960 samples each, and audible rather than silent.
    ///
    /// This is the whole of what the swap of encoder buys — the encoder and the
    /// decoder are one implementation, and this says they agree.
    #[test]
    fn an_opus_packet_is_one_the_decoder_reads_back() {
        let config = MicConfig::from_codec(
            AudioCodec::Opus {
                rate: 48_000,
                channels: 1,
            },
            0,
        );
        assert_eq!(config.samples_per_packet, 960);
        let mut coder = Coder::new(&config).expect("Opus is mono at 48 kHz");

        // 640 samples of 16 kHz is 1 920 of 48 kHz: exactly two packets.
        let mut block = Vec::with_capacity(1_280);
        for i in 0..640 {
            let t = i as f32 / MIC_RATE as f32;
            let tone = 0.5 * f32::from(i16::MAX) * (2.0 * std::f32::consts::PI * 1_000.0 * t).sin();
            block.extend_from_slice(&saturate(tone).to_le_bytes());
        }
        let packets = coder.packets(&block, &config);
        assert_eq!(packets.len(), 2);

        let mut decoder =
            rusty_opus::OpusDecoder::new(OPUS_RATE as i32, 1).expect("the decoder the downlink uses");
        let mut samples = vec![0.0f32; 5_760];
        let mut peak = 0.0f32;
        for packet in &packets {
            assert!(packet.len() <= MAX_OPUS_PACKET);
            let written = decoder
                .decode(packet, 5_760, &mut samples)
                .expect("a packet this crate wrote");
            assert_eq!(written, 960);
            peak = samples[..written]
                .iter()
                .fold(peak, |loudest, sample| loudest.max(sample.abs()));
        }
        assert!(peak > 0.3, "the tone came back at {peak}");
    }
}
