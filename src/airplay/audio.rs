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
//! frame.

use super::crypto::{SymKey, open};
use crate::sink::{AudioCodec, PlayerSink};
use std::sync::Arc;

pub const RTP_HEADER_LEN: usize = 12;
const TAG_LEN: usize = 16;
const NONCE_LEN: usize = 8;

pub struct AudioReceiver {
    key: SymKey,
    codec: AudioCodec,
    sink: Arc<PlayerSink>,
}

impl AudioReceiver {
    pub fn new(key: SymKey, codec: AudioCodec, sink: Arc<PlayerSink>) -> Self {
        Self { key, codec, sink }
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

        match self.codec {
            // Raw samples, but WAV wants them little-endian and CarPlay sends
            // them big-endian. Swapping every pair is the whole fix — skip it
            // and the sound is pure noise.
            AudioCodec::Pcm { .. } => self.sink.push(to_le16(&payload)),
            AudioCodec::Opus { .. } => self.sink.push(payload),
            AudioCodec::AacLc { rate, channels } => {
                match adts_frame(&payload, rate, channels) {
                    Some(frame) => self.sink.push(frame),
                    None => eprintln!("[audio] aac frame too small for adts"),
                }
            }
        }
    }
}

/// Big-endian s16 → little-endian s16, which is what a WAV payload holds.
fn to_le16(payload: &[u8]) -> Vec<u8> {
    let mut out = payload.to_vec();
    for pair in out.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
    out
}

/// Wraps a raw AAC frame in the ADTS header ffmpeg's aac demuxer expects.
fn adts_frame(payload: &[u8], rate: u32, channels: u16) -> Option<Vec<u8>> {
    let freq_idx = frequency_index(rate)?;
    let channels = channels.clamp(1, 8) as u8;
    let frame_len = payload.len() + 7;
    if frame_len > 0x1fff {
        return None;
    }

    let mut out = Vec::with_capacity(frame_len);
    // Sync word + MPEG-4, protection absent.
    out.push(0xff);
    out.push(0xf1);
    // profile (AAC-LC = 2, minus one), frequency index, channel config.
    out.push(((2 - 1) << 6) | ((freq_idx & 0x0f) << 2) | ((channels >> 2) & 0x01));
    out.push(((channels & 0x03) << 6) | ((frame_len >> 11) as u8 & 0x03));
    out.push((frame_len >> 3) as u8);
    out.push((((frame_len & 0x07) << 5) | 0x1f) as u8);
    out.push(0xfc);
    out.extend_from_slice(payload);
    Some(out)
}

fn frequency_index(rate: u32) -> Option<u8> {
    Some(match rate {
        96_000 => 0,
        88_200 => 1,
        64_000 => 2,
        48_000 => 3,
        44_100 => 4,
        32_000 => 5,
        24_000 => 6,
        22_050 => 7,
        16_000 => 8,
        12_000 => 9,
        11_025 => 10,
        8_000 => 11,
        _ => return None,
    })
}
