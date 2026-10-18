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
//! **AAC-LC is decoded here, and PCM is not.** The car at the other end of the
//! bridge takes 16-bit PCM on its media channel, and a phone on wireless CarPlay
//! sends its music as AAC-LC, so the codec comes off on this side of the
//! `MediaSink` — the stream's codec is known exactly here and nowhere later. It
//! is the one stream in the program that is decoded rather than forwarded; see
//! `aac`. Opus is left alone: it is what the two low-bandwidth voice streams
//! use, they belong to the car's own voice channels, and there is no decoder for
//! it here.

use super::crypto::{open, SymKey};
use crate::aac;
use crate::sink::{AudioCodec, MediaSink, StreamEvent};
use std::sync::Arc;

pub const RTP_HEADER_LEN: usize = 12;
const TAG_LEN: usize = 16;
const NONCE_LEN: usize = 8;

pub struct AudioReceiver {
    key: SymKey,
    /// Which of the phone's audio streams this is; the far end mixes one channel
    /// per stream, so it has to travel with every frame.
    stream_type: u8,
    codec: AudioCodec,
    sink: Arc<dyn MediaSink>,
    /// What takes the codec off, when what the phone sends is not already what
    /// the car takes. `None` for PCM, which needs nothing, and for Opus, which
    /// is not decoded here at all.
    decoder: Option<aac::Decoder>,
    /// Whether a frame has already been reported as dropped. A frame the decoder
    /// refuses is dropped, which is a moment of sound rather than the stream —
    /// but a stream it refuses *every* frame of would otherwise be a line per
    /// frame instead of one line.
    dropped: bool,
}

impl AudioReceiver {
    pub fn new(key: SymKey, stream_type: u8, codec: AudioCodec, sink: Arc<dyn MediaSink>) -> Self {
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
        Self {
            key,
            stream_type,
            codec,
            sink,
            decoder,
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

        // Read before the frame is decoded: decoding borrows the receiver
        // mutably, and so does reporting a frame that did not survive it.
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
                        if !self.dropped {
                            self.dropped = true;
                            tracing::warn!(
                                "[audio] this stream's frames are being dropped; the car is \
                                 being given no sound from it"
                            );
                        }
                        return None;
                    }
                }
            }
            // Only the voice streams, which are the car's own; see the note at
            // the top of this file.
            AudioCodec::Opus { .. } => return None,
        };
        Some((at, data))
    }

    pub fn feed(&mut self, packet: &[u8]) {
        if let Some((timestamp_us, data)) = self.decode(packet) {
            self.sink.push(StreamEvent::AudioPacket {
                stream_type: self.stream_type,
                timestamp_us,
                data,
            });
        }
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
}
