//! The screen stream: one TCP connection carrying every video frame.
//!
//! Layout per message (see LIVI's `screenStream.ts`):
//!
//! ```text
//! [128 byte header][body]
//!   header[0..4]  = body length, little endian
//!   header[4]     = opcode
//! ```
//!
//! Opcode 1 is the codec description and is sent in the clear; opcode 0 is a
//! frame. Every frame is sealed with ChaCha20-Poly1305 using the whole 128-byte
//! header as additional authenticated data, and the nonce is a counter that
//! goes up by one per frame — after a *successful* open only. Getting that
//! detail wrong desynchronises everything after the first packet you handle
//! badly.
//!
//! The plaintext is a run of length-prefixed NAL units (four byte big-endian
//! length, no start codes). ffmpeg's h264 demuxer wants annex-B, so we convert.

use super::crypto::{SymKey, open};
use crate::sink::PlayerSink;
use std::sync::Arc;

pub const HEADER_LEN: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

/// Everything unpacking one TCP connection needs.
pub struct VideoReceiver {
    key: SymKey,
    counter: u64,
    sink: Arc<PlayerSink>,
    codec: Option<Codec>,
    /// Bytes in front of each NAL unit; whatever the avcC record said.
    nal_length_size: usize,
    saw_keyframe: bool,
}

impl VideoReceiver {
    pub fn new(key: SymKey, sink: Arc<PlayerSink>) -> Self {
        // We only ever advertise H.264, so assume it from the start. Waiting for
        // the config message before handling frames meant a config we could not
        // parse silently discarded *every* frame — a black picture with no error
        // anywhere. Frames normally carry their own SPS/PPS anyway.
        Self {
            key,
            counter: 0,
            sink,
            codec: Some(Codec::H264),
            // Only a guess until the config arrives; four is the usual value.
            nal_length_size: 4,
            saw_keyframe: false,
        }
    }

    /// Handles one complete message, having already split it into header + body.
    pub fn feed(&mut self, header: &[u8], body: &[u8]) {
        let opcode = header[4];
        match opcode {
            1 => {
                match parse_video_config(body) {
                    Some(config) => {
                        println!(
                            "[video] config: {:?}, {} parameter set(s), {}-byte NAL lengths",
                            config.codec,
                            config.parameters.len(),
                            config.nal_length_size
                        );
                        self.codec = Some(config.codec);
                        self.nal_length_size = config.nal_length_size;
                        // Parameter sets must go out before any frame references
                        // them, otherwise the first frame is undecodable.
                        for param in config.parameters {
                            self.sink.push(param);
                        }
                    }
                    None => {
                        eprintln!(
                            "[video] config not understood ({}B): {}",
                            body.len(),
                            hex::encode(&body[..body.len().min(32)])
                        );
                    }
                }
            }
            0 => {
                if self.codec.is_none() {
                    // Nothing to do: we cannot even tell the decoder what it is.
                    return;
                }
                match open(&self.key, &super::crypto::nonce_counter(self.counter), body, header) {
                    Ok(plain) => {
                        // Only advance on success — see the module note.
                        self.counter += 1;
                        let nal = self.nal_length_size;
                        let Some(annex_b) =
                            self.codec.and_then(|c| length_prefixed_to_annex_b(&plain, c, nal))
                        else {
                            return;
                        };
                        if is_idr(&plain, self.nal_length_size) {
                            self.saw_keyframe = true;
                        }
                        if !annex_b.is_empty() {
                            self.sink.push(annex_b);
                        }
                    }
                    Err(e) => {
                        eprintln!("[video] frame {} failed to open: {e}", self.counter);
                    }
                }
            }
            other => tracing::warn!(opcode = other, "unknown video opcode"),
        }
    }

    pub fn is_streaming(&self) -> bool {
        self.saw_keyframe
    }
}

/// The codec description, with parameter sets already converted to annex-B.
struct VideoConfig {
    codec: Codec,
    parameters: Vec<Vec<u8>>,
    /// Bytes in front of each NAL unit. Read from the avcC record rather than
    /// assumed: guessing four when the stream uses two garbles every frame, and
    /// the decoder calls it "probably corrupt input".
    nal_length_size: usize,
}

/// Locates the avcC / hvcC record inside the config blob and pulls SPS/PPS out
/// of it. Ported from LIVI's `configToCodecData` plus the avcC layout.
fn parse_video_config(body: &[u8]) -> Option<VideoConfig> {
    let finish = |avcc: &[u8]| -> Option<VideoConfig> {
        let params = avcc_to_annex_b(avcc)?;
        // avcC byte 4: 0xFC | (lengthSizeMinusOne), so +1 for the real width.
        let nal_length_size = (avcc[4] & 0x03) as usize + 1;
        Some(VideoConfig { codec: Codec::H264, parameters: params, nal_length_size })
    };
    for i in 4..body.len().saturating_sub(4) {
        let tag = &body[i..i + 4];
        if tag == b"avcC" {
            return finish(&body[i + 4..]);
        }
        if tag == b"hvcC" {
            let hvcc = &body[i + 4..];
            // Returning no parameter sets here broke HEVC outright: the decoder
            // needs VPS/SPS/PPS before the first slice, and without them it
            // reports "PPS id out of range" and skips every NAL.
            return Some(VideoConfig {
                codec: Codec::H265,
                parameters: hvcc_to_annex_b(hvcc).unwrap_or_default(),
                nal_length_size: (hvcc[21] & 0x03) as usize + 1,
            });
        }
    }
    // Fall back: a bare avcC with no fourcc in front.
    finish(body)
}

/// Converts an avcC record into annex-B SPS/PPS chunks.
fn avcc_to_annex_b(avcc: &[u8]) -> Option<Vec<Vec<u8>>> {
    if avcc.len() < 6 {
        return None;
    }
    let nb_sps = (avcc[5] & 0x1f) as usize;
    let mut out = Vec::new();
    let mut i = 6;
    for _ in 0..nb_sps {
        if i + 2 > avcc.len() {
            return None;
        }
        let len = u16::from_be_bytes([avcc[i], avcc[i + 1]]) as usize;
        i += 2;
        if i + len > avcc.len() {
            return None;
        }
        out.push(with_start_code(&avcc[i..i + len]));
        i += len;
    }
    if i >= avcc.len() {
        return Some(out);
    }
    let nb_pps = avcc[i] as usize;
    i += 1;
    for _ in 0..nb_pps {
        if i + 2 > avcc.len() {
            return None;
        }
        let len = u16::from_be_bytes([avcc[i], avcc[i + 1]]) as usize;
        i += 2;
        if i + len > avcc.len() {
            return None;
        }
        out.push(with_start_code(&avcc[i..i + len]));
        i += len;
    }
    Some(out)
}

/// Pulls VPS / SPS / PPS out of an hvcC record as annex-B chunks.
///
/// After a 22-byte header comes `numOfArrays`, then for each array a byte of
/// `(array_completeness << 7) | nal_unit_type`, a u16 count, and that many
/// u16-length-prefixed NALs. Only the parameter set types are wanted — 32 VPS,
/// 33 SPS, 34 PPS; the rest travel with the frames themselves.
fn hvcc_to_annex_b(hvcc: &[u8]) -> Option<Vec<Vec<u8>>> {
    if hvcc.len() < 23 {
        return None;
    }
    let arrays = hvcc[22] as usize;
    let mut out = Vec::new();
    let mut i = 23;
    for _ in 0..arrays {
        if i + 3 > hvcc.len() {
            return None;
        }
        let nal_type = hvcc[i] & 0x3f;
        let count = u16::from_be_bytes([hvcc[i + 1], hvcc[i + 2]]) as usize;
        i += 3;
        for _ in 0..count {
            if i + 2 > hvcc.len() {
                return None;
            }
            let len = u16::from_be_bytes([hvcc[i], hvcc[i + 1]]) as usize;
            i += 2;
            if i + len > hvcc.len() {
                return None;
            }
            if matches!(nal_type, 32 | 33 | 34) {
                out.push(with_start_code(&hvcc[i..i + len]));
            }
            i += len;
        }
    }
    Some(out)
}

fn with_start_code(nal: &[u8]) -> Vec<u8> {
    let mut out = vec![0x00, 0x00, 0x00, 0x01];
    out.extend_from_slice(nal);
    out
}

/// Whether this frame can be decoded without anything before it.
fn is_idr(payload: &[u8], nal_len: usize) -> bool {
    // Same walk as the conversion, just looking at the NAL type.
    let mut i = 0;
    while i + nal_len <= payload.len() {
        let mut len = 0usize;
        for b in &payload[i..i + nal_len] {
            len = (len << 8) | *b as usize;
        }
        let start = i + nal_len;
        if start < payload.len() {
            let hdr = payload[start];
            let kind = hdr & 0x1f;
            if kind == 5 {
                return true;
            }
        }
        i = start + len;
    }
    false
}

/// CarPlay's length-prefixed NAL run → annex-B, which is what a decoder eats.
///
/// `nal_len` is how many bytes the length prefix takes. It comes from the avcC
/// record, not from a guess: getting it wrong shifts every NAL boundary.
fn length_prefixed_to_annex_b(payload: &[u8], codec: Codec, nal_len: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(payload.len() + 16);
    let mut i = 0;
    let mut wrote = false;
    while i + nal_len <= payload.len() {
        let mut len = 0usize;
        for b in &payload[i..i + nal_len] {
            len = (len << 8) | *b as usize;
        }
        let start = i + nal_len;
        let end = start.saturating_add(len);
        if end > payload.len() || len == 0 {
            break;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&payload[start..end]);
        wrote = true;
        i = end;
    }
    if !wrote {
        return None;
    }
    if codec == Codec::H265 {
        tracing::debug!("h265 frame converted");
    }
    Some(out)
}
