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
//! it here. See `PLAN-CL.md` §4.2.

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
        if packet.len() <= RTP_HEADER_LEN + TAG_LEN + NONCE_LEN {
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
        let timestamp_us = self.timestamp_us(packet);
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
        Some((timestamp_us, data))
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

    /// The RTP timestamp is the phone's own sample counter. Turning it into
    /// microseconds here is what lets playback be scheduled; for PCM it also
    /// happens to be exact, since the counter *is* the sample index.
    fn timestamp_us(&self, packet: &[u8]) -> u64 {
        let sample = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]) as u64;
        sample * 1_000_000 / self.codec.rate().max(1) as u64
    }
}

/// Big-endian s16 → little-endian s16, which is what every consumer expects.
fn to_le16(payload: &[u8]) -> Vec<u8> {
    let mut out = payload.to_vec();
    for pair in out.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
    out
}
