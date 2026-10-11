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
//! length, no start codes). A decoder wants annex-B, so we convert. The codec
//! description goes out both ways at once: as annex-B parameter sets, and as the
//! raw avcC / hvcC record, which is what a browser needs to configure its
//! decoder before the first frame arrives.

use super::crypto::{open, SymKey};
use crate::sink::{MediaSink, StreamEvent, VideoCodec};
use std::sync::Arc;

pub const HEADER_LEN: usize = 128;

/// Everything unpacking one TCP connection needs.
pub struct VideoReceiver {
    key: SymKey,
    counter: u64,
    /// Frames handed on. Its only reader is the one line that says a stream
    /// carried pictures at all: everything else about a frame is per-frame.
    frames: u64,
    sink: Arc<dyn MediaSink>,
    codec: VideoCodec,
    /// Bytes in front of each NAL unit; whatever the avcC record said.
    nal_length_size: usize,
}

impl VideoReceiver {
    pub fn new(key: SymKey, sink: Arc<dyn MediaSink>) -> Self {
        // We only ever advertise H.264, so assume it from the start. Waiting for
        // the config message before handling frames meant a config we could not
        // parse silently discarded *every* frame — a black picture with no error
        // anywhere. Frames normally carry their own SPS/PPS anyway.
        Self {
            key,
            counter: 0,
            frames: 0,
            sink,
            codec: VideoCodec::H264,
            // Only a guess until the config arrives; four is the usual value.
            nal_length_size: 4,
        }
    }

    /// How many frames this connection has carried. Zero means the phone
    /// opened the stream and sent nothing that decodes.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Handles one complete message, having already split it into header + body.
    pub fn feed(&mut self, header: &[u8], body: &[u8]) {
        let opcode = header[4];
        match opcode {
            1 => {
                match parse_video_config(body) {
                    Some(config) => {
                        tracing::info!(
                            "[video] config: {:?}, {} parameter set(s), {}-byte NAL lengths",
                            config.codec,
                            config.parameter_sets.len(),
                            config.nal_length_size
                        );
                        self.codec = config.codec;
                        self.nal_length_size = config.nal_length_size;
                        self.sink.push(StreamEvent::VideoConfig {
                            codec: config.codec,
                            codec_data: config.codec_data,
                        });
                    }
                    None => {
                        tracing::warn!(
                            "[video] config not understood ({}B): {}",
                            body.len(),
                            hex::encode(&body[..body.len().min(32)])
                        );
                    }
                }
            }
            0 => {
                match open(
                    &self.key,
                    &super::crypto::nonce_counter(self.counter),
                    body,
                    header,
                ) {
                    Ok(plain) => {
                        // Only advance on success — see the module note.
                        self.counter += 1;
                        let nal = self.nal_length_size;
                        let Some(annex_b) = length_prefixed_to_annex_b(&plain, self.codec, nal)
                        else {
                            return;
                        };
                        if !annex_b.is_empty() {
                            let keyframe = is_key_frame(&annex_b, self.codec);
                            // The frame count is only printed when the stream
                            // closes, which for a stream that never delivers
                            // anything is a line nobody waits for. This one says
                            // the phone is sending; the page decoding it or not
                            // is a separate question.
                            if self.frames == 0 {
                                tracing::info!(
                                    "[video] first frame: {:?}, keyframe={keyframe}, \
                                     {} bytes, {}-byte NAL lengths",
                                    self.codec,
                                    annex_b.len(),
                                    self.nal_length_size
                                );
                            }
                            self.frames += 1;
                            self.sink.push(StreamEvent::VideoFrame { keyframe, data: annex_b });
                        }
                    }
                    Err(e) => {
                        tracing::warn!("[video] frame {} failed to open: {e}", self.counter);
                    }
                }
            }
            other => tracing::warn!(opcode = other, "unknown video opcode"),
        }
    }
}

/// The codec description, in both shapes a consumer might want.
struct VideoConfig {
    codec: VideoCodec,
    /// The raw avcC / hvcC record, exactly as it arrived after its fourcc.
    codec_data: Vec<u8>,
    /// The parameter sets inside it, converted to annex-B.
    parameter_sets: Vec<Vec<u8>>,
    /// Bytes in front of each NAL unit. Read from the avcC record rather than
    /// assumed: guessing four when the stream uses two garbles every frame, and
    /// the decoder calls it "probably corrupt input".
    nal_length_size: usize,
}

/// Locates the avcC / hvcC record inside the config blob and pulls SPS/PPS out
/// of it. Ported from LIVI's `configToCodecData` plus the avcC layout. The
/// record is passed on from after its fourcc to the end of the blob, the same
/// slice MacPlay and PlayPort hand their decoders.
fn parse_video_config(body: &[u8]) -> Option<VideoConfig> {
    let finish = |avcc: &[u8]| -> Option<VideoConfig> {
        let params = avcc_to_annex_b(avcc)?;
        // avcC byte 4: 0xFC | (lengthSizeMinusOne), so +1 for the real width.
        let nal_length_size = (avcc[4] & 0x03) as usize + 1;
        Some(VideoConfig {
            codec: VideoCodec::H264,
            codec_data: avcc.to_vec(),
            parameter_sets: params,
            nal_length_size,
        })
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
                codec: VideoCodec::H265,
                codec_data: hvcc.to_vec(),
                parameter_sets: hvcc_to_annex_b(hvcc).unwrap_or_default(),
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

/// CarPlay's length-prefixed NAL run → annex-B, which is what a decoder eats.
///
/// `nal_len` is how many bytes the length prefix takes. It comes from the avcC
/// record, not from a guess: getting it wrong shifts every NAL boundary.
fn length_prefixed_to_annex_b(payload: &[u8], codec: VideoCodec, nal_len: usize) -> Option<Vec<u8>> {
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
    // Per frame, so it says nothing that the frame count does not — and at
    // debug level it buried the input and audio lines, which are the ones that
    // level is for. Trace keeps it reachable without that.
    if codec == VideoCodec::H265 {
        tracing::trace!("h265 frame converted");
    }
    Some(out)
}

/// True when an annex-B access unit opens on an H.264 IDR or an H.265 IRAP
/// picture. Only the leading NALs are inspected: a keyframe says so at the
/// front, and a long frame should not be walked to prove it. Ported from
/// PlayPort's `Wire.isKeyFrame`.
fn is_key_frame(annex_b: &[u8], codec: VideoCodec) -> bool {
    let mut index = 0;
    let mut inspected = 0;
    while index + 4 <= annex_b.len() && inspected < 8 {
        let start_length = start_code_length(annex_b, index);
        if start_length == 0 {
            index += 1;
            continue;
        }
        let nal_start = index + start_length;
        if nal_start >= annex_b.len() {
            return false;
        }
        let header = annex_b[nal_start];
        let key = match codec {
            VideoCodec::H264 => (header & 0x1f) == 5,
            VideoCodec::H265 => (16..=21).contains(&((header >> 1) & 0x3f)),
        };
        if key {
            return true;
        }
        inspected += 1;
        index = nal_start + 1;
    }
    false
}

fn start_code_length(bytes: &[u8], index: usize) -> usize {
    if index + 3 > bytes.len() || bytes[index] != 0 || bytes[index + 1] != 0 {
        return 0;
    }
    if index + 4 <= bytes.len() && bytes[index + 2] == 0 && bytes[index + 3] == 1 {
        4
    } else if bytes[index + 2] == 1 {
        3
    } else {
        0
    }
}
