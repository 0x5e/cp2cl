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

use super::crypto::{open, SymKey};
use crate::sink::{AudioCodec, MediaSink, StreamEvent};
use std::sync::Arc;

pub const RTP_HEADER_LEN: usize = 12;
const TAG_LEN: usize = 16;
const NONCE_LEN: usize = 8;

pub struct AudioReceiver {
    key: SymKey,
    /// Which of the phone's audio streams this is; the browser mixes one channel
    /// per stream, so it has to travel with every frame.
    stream_type: u8,
    codec: AudioCodec,
    sink: Arc<dyn MediaSink>,
}

impl AudioReceiver {
    pub fn new(key: SymKey, stream_type: u8, codec: AudioCodec, sink: Arc<dyn MediaSink>) -> Self {
        Self {
            key,
            stream_type,
            codec,
            sink,
        }
    }

    pub fn feed(&self, packet: &[u8]) {
        if packet.len() <= RTP_HEADER_LEN + TAG_LEN + NONCE_LEN {
            return;
        }
        let end = packet.len() - NONCE_LEN;
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&packet[end..]);
        let aad = &packet[4..RTP_HEADER_LEN];
        let sealed = &packet[RTP_HEADER_LEN..end];

        let Ok(payload) = open(&self.key, &nonce, sealed, aad) else {
            eprintln!("[audio] packet failed to open");
            return;
        };
        if payload.is_empty() {
            return;
        }

        let data = match self.codec {
            // Raw samples, but every consumer wants them little-endian and
            // CarPlay sends them big-endian. Swapping every pair is the whole
            // fix — skip it and the sound is pure noise.
            AudioCodec::Pcm { .. } => to_le16(&payload),
            _ => payload,
        };
        self.sink.push(StreamEvent::AudioPacket {
            stream_type: self.stream_type,
            timestamp_us: self.timestamp_us(packet),
            data,
        });
    }

    /// The RTP timestamp is the phone's own sample counter. Turning it into
    /// microseconds here is what lets a browser schedule playback; for PCM it
    /// also happens to be exact, since the counter *is* the sample index.
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
