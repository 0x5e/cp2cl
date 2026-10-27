//! The microphone uplink: what was captured at this end, on its way to the phone.
//!
//! CarPlay has no microphone stream of its own. The phone asks for one by
//! naming a *send* port in the SETUP of its main audio stream (type 100) and
//! labelling that stream `telephony` or `speechRecognition`; audio then travels
//! the other way over UDP, sealed with the mirror of the key the phone seals its
//! own audio with. Nothing here decodes: whoever captures encodes, and the phone
//! says what it will take, which is exactly how the downlink works.
//!
//! The receiver fills the sink in as the phone opens and closes such a stream,
//! and the capture side reads the published state to decide whether to start at
//! all — one object, because both sides have to change together.
//!
//! Nothing captures anything yet, so the half that takes a captured payload and
//! sends it is the one the `dead_code` allowances below are for: the microphone
//! will be the CarLife side's.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::sync::{watch, Mutex};

use super::crypto::{seal, SymKey};
use crate::sink::AudioCodec;

/// The RTP-shaped header the phone reads before the sealed payload.
#[allow(dead_code)]
const RTP_HEADER_LEN: usize = 12;
/// How much audio one packet carries when the phone does not say: twenty
/// milliseconds, the same default both reference implementations use.
const DEFAULT_FRAME_MS: u32 = 20;
/// Eight bytes of the nonce follow the Poly1305 tag, so the phone can tell a
/// replayed packet from a late one.
#[allow(dead_code)]
const NONCE_LEN: usize = 8;

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
    #[allow(dead_code)]
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
    /// into a duration, or 20 ms when it did not say.
    pub samples_per_packet: u32,
    /// Opus only. The low-latency tiers the reference implementations pick.
    pub bitrate: Option<u32>,
}

impl MicConfig {
    /// Reads the uplink's shape off the codec the downlink already agreed on.
    ///
    /// There is no separate negotiation: a stream that arrived as Opus is one
    /// the capture side is asked to produce in Opus, at the same rate and with
    /// the single channel a microphone has. `frames_per_packet` is the phone's
    /// own packet timing, which is what keeps the RTP timestamp right.
    pub fn from_codec(codec: AudioCodec, frames_per_packet: u32) -> Self {
        let (mic, channels) = match codec {
            AudioCodec::Opus { .. } => (MicCodec::Opus, 1),
            _ => (MicCodec::Lpcm, codec.channels()),
        };
        let sample_rate = codec.rate();
        let frame_ms = if frames_per_packet > 0 && sample_rate > 0 {
            (frames_per_packet * 1000 / sample_rate).clamp(5, 60)
        } else {
            DEFAULT_FRAME_MS
        };
        Self {
            codec: mic,
            sample_rate,
            channels,
            samples_per_packet: (sample_rate * frame_ms / 1000).max(1),
            bitrate: (mic == MicCodec::Opus).then(|| opus_bitrate(sample_rate)),
        }
    }

    /// How far the RTP timestamp moves for one payload.
    ///
    /// Whole frames, not bytes — and for Opus a fixed frame rather than a count,
    /// because the encoder decides its own frame size. Getting this wrong is not
    /// fatal, but it is what the phone positions the audio against.
    #[allow(dead_code)]
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
#[allow(dead_code)]
struct Uplink {
    socket: UdpSocket,
    key: SymKey,
    /// The RTP payload type, which is the stream type the phone named.
    payload_type: u8,
    config: MicConfig,
    /// RTP sequence and timestamp, and the ChaCha nonce. All three advance once
    /// per packet and none is derived from the others.
    sequence: u16,
    timestamp: u32,
    nonce: u64,
}

#[allow(dead_code)]
impl Uplink {
    /// How far the RTP timestamp moves for one payload: the phone's own packet
    /// size, which for Opus is a fixed frame rather than a byte count.
    fn samples(&self, payload: &[u8]) -> u32 {
        self.config.samples(payload)
    }
}

/// Where audio captured at this end goes.
///
/// One of these is built at startup and shared: the receiver opens and closes it
/// as the phone asks for bidirectional audio, and the capture side reads the
/// state to know whether to start — which is why the two change under one lock.
pub struct MicSink {
    uplink: Mutex<Option<Uplink>>,
    /// Published so the capture side is told to start and stop. `None` means the
    /// phone is not listening, and packets are dropped without a word.
    state: watch::Sender<Option<MicConfig>>,
}

impl MicSink {
    pub fn new() -> Arc<Self> {
        let (state, _) = watch::channel(None);
        Arc::new(Self {
            uplink: Mutex::new(None),
            state,
        })
    }

    /// Opens the uplink for one audio stream. Replaces whatever was there: the
    /// phone re-opens its audio stream on every track change.
    pub(crate) async fn open(
        &self,
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
        *self.uplink.lock().await = Some(Uplink {
            socket,
            key,
            payload_type,
            config,
            sequence: 0,
            timestamp: 0,
            nonce: 0,
        });
        let _ = self.state.send(Some(config));
        Ok(())
    }

    /// Closes it: the phone's audio stream is gone, so nothing we send would be
    /// heard. Dropping the socket is the whole of the cleanup.
    pub(crate) async fn close(&self) {
        if self.uplink.lock().await.take().is_none() {
            return;
        }
        let _ = self.state.send(None);
    }

    /// What to tell the capture side now, and a subscription for later changes.
    #[allow(dead_code)]
    pub fn current(&self) -> Option<MicConfig> {
        *self.state.borrow()
    }

    #[allow(dead_code)]
    pub fn subscribe(&self) -> watch::Receiver<Option<MicConfig>> {
        self.state.subscribe()
    }

    /// Seals one captured payload and sends it.
    ///
    /// Silently does nothing when there is no uplink: a capture may still be
    /// running when the phone stops listening, and racing it to a stop would
    /// only lose the last packet either way.
    #[allow(dead_code)]
    pub async fn send(&self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let mut guard = self.uplink.lock().await;
        let Some(uplink) = guard.as_mut() else {
            return;
        };

        let samples = uplink.samples(payload);
        // The header is clear and its last eight bytes are authenticated as
        // additional data — the timestamp the phone checks the payload against.
        let mut header = [0u8; RTP_HEADER_LEN];
        header[0] = 0x80;
        header[1] = uplink.payload_type & 0x7f;
        header[2..4].copy_from_slice(&uplink.sequence.to_be_bytes());
        header[4..8].copy_from_slice(&uplink.timestamp.to_be_bytes());

        // Twelve bytes, with the packet counter in the low eight: the same
        // convention as the media streams, and the counter is repeated after the
        // tag as well as being the nonce.
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&uplink.nonce.to_le_bytes());
        let sealed = match seal(&uplink.key, &nonce, payload, &header[4..]) {
            Ok(sealed) => sealed,
            Err(e) => {
                tracing::warn!("carplay.mic.seal_failed err={e}");
                return;
            }
        };

        let mut packet = Vec::with_capacity(RTP_HEADER_LEN + sealed.len() + NONCE_LEN);
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&sealed);
        packet.extend_from_slice(&uplink.nonce.to_le_bytes());

        uplink.sequence = uplink.sequence.wrapping_add(1);
        uplink.timestamp = uplink.timestamp.wrapping_add(samples);
        uplink.nonce += 1;

        if let Err(e) = uplink.socket.send(&packet).await {
            tracing::warn!("carplay.mic.send_failed err={e}");
        }
    }
}

/// Opus's low-latency tiers: the reference implementations raise the bitrate
/// with the rate rather than fixing it.
fn opus_bitrate(rate: u32) -> u32 {
    if rate <= 24_000 {
        48_000
    } else if rate <= 32_000 {
        64_000
    } else {
        96_000
    }
}

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
        assert_eq!(pcm.bitrate, None);

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

    /// Opus is the wireless answer and always mono; its bitrate moves with the
    /// rate rather than being fixed.
    #[test]
    fn opus_is_mono_and_raises_its_bitrate_with_the_rate() {
        let opus = MicConfig::from_codec(AudioCodec::Opus { rate: 48_000, channels: 1 }, 0);
        assert_eq!(opus.codec, MicCodec::Opus);
        assert_eq!(opus.channels, 1);
        assert_eq!(opus.samples_per_packet, 960);
        assert_eq!(opus.bitrate, Some(96_000));

        let narrow = MicConfig::from_codec(AudioCodec::Opus { rate: 16_000, channels: 1 }, 0);
        assert_eq!(narrow.bitrate, Some(48_000));
        assert_eq!(narrow.samples_per_packet, 320);
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
}
