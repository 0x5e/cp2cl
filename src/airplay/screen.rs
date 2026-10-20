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
//! length, no start codes). CarLife's video channel takes annex-B, so we convert,
//! and the codec description goes out the same way: the SPS/PPS the avcC record
//! holds, in front of the frames rather than out of band.

use super::crypto::{open, SymKey};
use crate::sink::{MediaSink, StreamEvent};
use std::sync::Arc;

pub const HEADER_LEN: usize = 128;

/// The one coding this receiver reads. `parse_video_config` understands an avcC
/// record and nothing else, and CarLife's head units decode `video/avc`, so the
/// name is a property of this side rather than of any one session. It is written
/// once here and read from here by the log line and the status page.
pub const CODEC: &str = "H.264";

/// Everything unpacking one TCP connection needs.
pub struct VideoReceiver {
    key: SymKey,
    counter: u64,
    /// Frames handed on. Its only reader is the one line that says a stream
    /// carried pictures at all: everything else about a frame is per-frame.
    frames: u64,
    sink: Arc<dyn MediaSink>,
    /// Bytes in front of each NAL unit; whatever the avcC record said.
    nal_length_size: usize,
}

impl VideoReceiver {
    pub fn new(key: SymKey, sink: Arc<dyn MediaSink>) -> Self {
        // The stream is H.264 from the first frame: that is the only family
        // `/info` offers, and waiting for the config message before handling
        // frames meant a config we could not parse silently discarded *every*
        // frame — a black picture with no error anywhere. Frames normally carry
        // their own SPS/PPS anyway.
        Self {
            key,
            counter: 0,
            frames: 0,
            sink,
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
    ///
    /// Answers with the coding when the message was the codec description, and
    /// with nothing for every other message: the profile and the level are said
    /// once, in that one message, and are worth reporting where the page can
    /// read them.
    pub fn feed(&mut self, header: &[u8], body: &[u8]) -> Option<H264Format> {
        let opcode = header[4];
        match opcode {
            1 => {
                match parse_video_config(body) {
                    Some(config) => {
                        tracing::info!(
                            "video.config profile={} level={} parameter_sets={} nal_length_size={}",
                            config.format.profile,
                            config.format.level,
                            config.parameter_sets.len(),
                            config.nal_length_size
                        );
                        self.nal_length_size = config.nal_length_size;
                        self.sink.push(StreamEvent::VideoConfig {
                            parameter_sets: config.parameter_sets,
                        });
                        Some(config.format)
                    }
                    None => {
                        // The only config this side can use is H.264's avcC, which
                        // is also the only one `/info` asks for. Anything else
                        // means every frame after it would be read as something it
                        // is not, so name what arrived rather than leave a black
                        // picture with nothing printed.
                        tracing::warn!(
                            "video.config_unreadable bytes={} data={} want=avcC",
                            body.len(),
                            hex::encode(&body[..body.len().min(32)])
                        );
                        None
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
                        let Some(annex_b) = length_prefixed_to_annex_b(&plain, nal) else {
                            return None;
                        };
                        if !annex_b.is_empty() {
                            let keyframe = is_key_frame(&annex_b);
                            // The frame count is only printed when the stream
                            // closes, which for a stream that never delivers
                            // anything is a line nobody waits for. This one says
                            // the phone is sending; the page decoding it or not
                            // is a separate question.
                            if self.frames == 0 {
                                tracing::info!(
                                    "video.first_frame keyframe={keyframe} bytes={} \
                                     nal_length_size={}",
                                    annex_b.len(),
                                    self.nal_length_size
                                );
                            }
                            self.frames += 1;
                            self.sink.push(StreamEvent::VideoFrame { keyframe, data: annex_b });
                        }
                    }
                    Err(e) => {
                        tracing::warn!("video.frame_open_failed frame={} err={e}", self.counter);
                    }
                }
                // A frame says nothing about the coding: that came in the
                // description, and it has not changed since.
                None
            }
            other => {
                tracing::warn!("video.unknown_opcode opcode={other}");
                None
            }
        }
    }
}

/// The coding itself, as the codec description names it.
///
/// An avcC record writes both down in its first three bytes — `profile_idc`,
/// the constraint flags and `level_idc`, which are the same three an annex-B
/// SPS carries after its NAL header — so nothing has to be read out of the
/// parameter set bit by bit. Which of them a phone picks is its own decision,
/// and it is the one thing about the stream that cannot be named in advance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264Format {
    /// `Baseline`, `Main`, `High`, … A profile the table below does not name is
    /// reported by its number rather than guessed at.
    pub profile: String,
    /// As H.264 writes it: `3.1`, `4`, and `1b` for the one level whose name is
    /// not its number.
    pub level: String,
}

/// What the codec description says.
struct VideoConfig {
    /// The parameter sets inside the avcC record, converted to annex-B.
    parameter_sets: Vec<Vec<u8>>,
    /// Bytes in front of each NAL unit. Read from the avcC record rather than
    /// assumed: guessing four when the stream uses two garbles every frame, and
    /// the decoder calls it "probably corrupt input".
    nal_length_size: usize,
    /// The profile and the level the record declares.
    format: H264Format,
}

/// Locates the avcC record inside the config blob and pulls SPS/PPS out of it.
/// Ported from LIVI's `configToCodecData` plus the avcC layout. The record is
/// passed on from after its fourcc to the end of the blob, the same slice
/// MacPlay and PlayPort hand their decoders.
fn parse_video_config(body: &[u8]) -> Option<VideoConfig> {
    let finish = |avcc: &[u8]| -> Option<VideoConfig> {
        let params = avcc_to_annex_b(avcc)?;
        // avcC byte 4: 0xFC | (lengthSizeMinusOne), so +1 for the real width.
        let nal_length_size = (avcc[4] & 0x03) as usize + 1;
        Some(VideoConfig {
            parameter_sets: params,
            nal_length_size,
            format: h264_format(avcc[1], avcc[2], avcc[3]),
        })
    };
    for i in 4..body.len().saturating_sub(4) {
        if &body[i..i + 4] == b"avcC" {
            return finish(&body[i + 4..]);
        }
    }
    // Fall back: a bare avcC with no fourcc in front.
    finish(body)
}

/// Names the profile and the level from the three bytes that hold them.
///
/// `constraints` is the second of them, which is a byte of flags rather than a
/// number: the one that matters here is `constraint_set3_flag`, which is what
/// makes level 1b 1b. The other flag worth knowing about is `constraint_set1`,
/// which turns Baseline into *constrained* Baseline — a subset rather than a
/// profile of its own, and not worth ten characters on a page read at arm's
/// length.
fn h264_format(profile_idc: u8, constraints: u8, level_idc: u8) -> H264Format {
    let profile = match profile_idc {
        66 => "Baseline".to_string(),
        77 => "Main".to_string(),
        88 => "Extended".to_string(),
        100 => "High".to_string(),
        110 => "High 10".to_string(),
        122 => "High 4:2:2".to_string(),
        244 => "High 4:4:4".to_string(),
        other => format!("profile {other}"),
    };
    let level = match level_idc {
        // The one level whose name is not its number: 1b is level_idc 11 with
        // `constraint_set3_flag` set, and it is not 1.1.
        11 if constraints & 0x10 != 0 => "1b".to_string(),
        // Whole levels are written without a fraction — 40 is 4, not 4.0.
        whole if whole % 10 == 0 => (whole / 10).to_string(),
        other => format!("{}.{}", other / 10, other % 10),
    };
    H264Format { profile, level }
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

fn with_start_code(nal: &[u8]) -> Vec<u8> {
    let mut out = vec![0x00, 0x00, 0x00, 0x01];
    out.extend_from_slice(nal);
    out
}

/// CarPlay's length-prefixed NAL run → annex-B, which is what a decoder eats.
///
/// `nal_len` is how many bytes the length prefix takes. It comes from the avcC
/// record, not from a guess: getting it wrong shifts every NAL boundary.
fn length_prefixed_to_annex_b(payload: &[u8], nal_len: usize) -> Option<Vec<u8>> {
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
    Some(out)
}

/// True when an annex-B access unit opens on an H.264 IDR picture. Only the
/// leading NALs are inspected: a keyframe says so at the front, and a long frame
/// should not be walked to prove it. Ported from PlayPort's `Wire.isKeyFrame`.
fn is_key_frame(annex_b: &[u8]) -> bool {
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
        if (header & 0x1f) == 5 {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The description names the profile and the level outright, and that is
    /// what the page shows: reading them out of the SPS itself would be the
    /// same three bytes after more work.
    #[test]
    fn the_codec_description_names_the_profile_and_the_level() {
        // Four bytes of nothing in front of the fourcc, which is where the
        // search starts: a bare record is the fallback, not the usual shape.
        let mut body = vec![0u8; 4];
        body.extend_from_slice(b"avcC");
        // version, Baseline with constraint_set1, level 3.1, NAL length 4,
        // one SPS of two bytes, and no PPS.
        body.extend_from_slice(&[1, 0x42, 0xe0, 0x1f, 0xff, 0xe1, 0x00, 0x02, 0x67, 0x00, 0x00]);

        let config = parse_video_config(&body).expect("an avcC record");
        assert_eq!(
            config.format,
            H264Format {
                profile: "Baseline".to_string(),
                level: "3.1".to_string(),
            }
        );
        assert_eq!(config.nal_length_size, 4);
    }

    /// The two names that are not simply their number.
    #[test]
    fn the_profiles_and_levels_are_named_as_h264_names_them() {
        assert_eq!(h264_format(66, 0x40, 40).profile, "Baseline", "constrained or not");
        assert_eq!(h264_format(100, 0, 40).profile, "High");
        assert_eq!(h264_format(100, 0, 40).level, "4", "a whole level has no fraction");
        assert_eq!(h264_format(77, 0x10, 11).level, "1b", "which is not 1.1");
        assert_eq!(h264_format(200, 0, 51).profile, "profile 200");
    }
}
