//! The cable's iAP2 frames, as lines rather than bytes.
//!
//! `Latched` sees the carkit stream as chunks: a read may end in the middle of
//! a frame and a write may carry several. This puts them back together and says
//! what each frame is — a link packet's control bits, sequence and session, and
//! for the control session the message's name and its parameters — instead of
//! printing the first bytes of every chunk in hex.
//!
//! Only the first few frames of each direction are written, and only at debug
//! level: enough to see whether the phone answered at all, not enough to drown
//! a working session. Pure acknowledgements carry nothing but their counters,
//! so they are left out of that count.

use std::fmt::Write as _;

use iap2_csm::{frame_header, split_params};
use iap2_link::{check_checksum, LinkPacketHeader, LinkSynchronizationPayload};

use crate::names::{self, Kind, Message};

const CONTROL_SESSION: u8 = 10;
const EA_SESSION: u8 = 11;
const FILE_TRANSFER_SESSION: u8 = 12;

/// Frames with nothing but a link counter are not worth a line.
const LINES_PER_DIRECTION: usize = 20;
/// A frame's length is 16 bits, so a buffer past two of them has lost sync.
const MAX_BUFFER: usize = 2 * u16::MAX as usize;

/// What the first frames of a cable session say, in both directions.
#[derive(Default)]
pub struct Trace {
    inbound: Decoder,
    outbound: Decoder,
}

impl Trace {
    pub fn inbound(&mut self, data: &[u8]) {
        for line in self.inbound.feed(data) {
            tracing::debug!("carplay.iap2.frame_in {line}");
        }
    }

    pub fn outbound(&mut self, data: &[u8]) {
        for line in self.outbound.feed(data) {
            tracing::debug!("carplay.iap2.frame_out {line}");
        }
    }
}

/// One direction's bytes, held until a whole frame has arrived.
#[derive(Default)]
struct Decoder {
    buf: Vec<u8>,
    lines: usize,
}

impl Decoder {
    fn feed(&mut self, data: &[u8]) -> Vec<String> {
        if self.lines >= LINES_PER_DIRECTION {
            return Vec::new();
        }
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            // The cable opens with a six-byte detection marker, and a frame
            // after any amount of noise begins with FF 5A. Everything before
            // the next marker is neither.
            let Some(at) = marker(&self.buf) else {
                let keep = usize::from(self.buf.last() == Some(&0xff));
                self.buf.drain(..self.buf.len() - keep);
                break;
            };
            if at > 0 {
                self.buf.drain(..at);
            }
            if self.buf.len() < 9 {
                break;
            }
            let mut header = [0u8; 9];
            header.copy_from_slice(&self.buf[..9]);
            let Some(h) = LinkPacketHeader::parse(&header) else {
                self.buf.drain(..1);
                continue;
            };
            let len = h.length as usize;
            if len < 9 {
                self.buf.drain(..1);
                continue;
            }
            if self.buf.len() < len {
                if self.buf.len() > MAX_BUFFER {
                    self.buf.drain(..1);
                    continue;
                }
                break;
            }
            if len > 9 && !check_checksum(&self.buf[9..len]) {
                self.buf.drain(..1);
                continue;
            }
            let payload = if len > 9 {
                self.buf[9..len - 1].to_vec()
            } else {
                Vec::new()
            };
            self.buf.drain(..len);
            for line in describe(&h, &payload) {
                if self.lines >= LINES_PER_DIRECTION {
                    break;
                }
                out.push(line);
                self.lines += 1;
            }
            if self.lines >= LINES_PER_DIRECTION {
                break;
            }
        }
        out
    }
}

/// Where the next frame starts, if a whole marker is here yet.
fn marker(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == [0xff, 0x5a])
}

/// One line per control-session message in a frame, one line otherwise.
fn describe(h: &LinkPacketHeader, payload: &[u8]) -> Vec<String> {
    let kind = control_kind(h.control, payload.is_empty());
    if kind == "ack" {
        return Vec::new();
    }
    let mut base = format!(
        "type={kind} control=0x{:02x} seq=0x{:02x} ack=0x{:02x} session={} bytes={}",
        h.control,
        h.seq,
        h.ack,
        session_name(h.session_id),
        payload.len()
    );
    if h.control & 0x80 != 0 {
        if let Some(lsp) = LinkSynchronizationPayload::parse(payload) {
            let sessions: Vec<String> = lsp.sessions.iter().map(|s| s.id.to_string()).collect();
            let _ = write!(
                base,
                " max_len={} max_outgoing={} sessions={}",
                lsp.max_len,
                lsp.max_outgoing,
                sessions.join(",")
            );
        }
        return vec![base];
    }
    match h.session_id {
        EA_SESSION if payload.len() >= 2 => {
            let _ = write!(
                base,
                " stream_id={}",
                u16::from_be_bytes([payload[0], payload[1]])
            );
            vec![base]
        }
        CONTROL_SESSION => control_messages(&base, payload),
        _ => vec![base],
    }
}

/// The highest bit that sets what a packet does; the raw control byte is on the
/// line too, so a combination is not reduced to one name.
fn control_kind(control: u8, empty: bool) -> &'static str {
    if control & 0x80 != 0 {
        return "syn";
    }
    if control & 0x20 != 0 {
        return "eak";
    }
    if control & 0x10 != 0 {
        return "rst";
    }
    if empty { "ack" } else { "data" }
}

fn session_name(id: u8) -> String {
    match id {
        0 => "link".to_string(),
        CONTROL_SESSION => "control".to_string(),
        EA_SESSION => "ea".to_string(),
        FILE_TRANSFER_SESSION => "file_transfer".to_string(),
        _ => format!("0x{id:02x}"),
    }
}

/// A control-session packet can carry more than one message, one after the
/// other. Each gets a line of its own, with the frame's counters repeated.
fn control_messages(base: &str, payload: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = payload;
    if rest.is_empty() {
        return vec![format!("{base} msg=none")];
    }
    while !rest.is_empty() && out.len() < 8 {
        let Ok((id, body)) = frame_header(rest) else {
            out.push(format!("{base} msg=malformed bytes={}", rest.len()));
            break;
        };
        let params = split_params(body);
        let mut line = format!("{base} msg={} params={}", Message(id), params.len());
        fields(&mut line, "", id, None, &params, 0);
        out.push(line);
        rest = &rest[6 + body.len()..];
    }
    out
}

/// Parameters as ` key=value`. A group opens into the same line under a dotted
/// name, the way a CarLife message's fields are written.
fn fields(
    out: &mut String,
    prefix: &str,
    msg: u16,
    group: Option<names::Group>,
    params: &[(u16, &[u8])],
    depth: usize,
) {
    for (id, value) in params {
        let p = match group {
            Some(g) => names::group_param(g, *id),
            None => names::param(msg, *id),
        };
        let name = if p.name.is_empty() {
            format!("p{id}")
        } else {
            p.name.to_string()
        };
        let key = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}.{name}")
        };
        match p.kind {
            Kind::Group(g) if depth < 2 => {
                fields(out, &key, msg, Some(g), &split_params(value), depth + 1)
            }
            _ => {
                let _ = write!(out, " {key}={}", read(value, p.kind));
            }
        }
    }
}

/// The value of one parameter, in the type the protocol says it is.
fn read(bytes: &[u8], kind: Kind) -> String {
    match kind {
        Kind::Flag => "true".to_string(),
        Kind::Bool => bool_string(bytes.first().is_some_and(|b| *b != 0)),
        Kind::Num(width, signed) => {
            if bytes.len() != width as usize {
                return count(bytes);
            }
            let n = bytes.iter().fold(0i128, |a, b| (a << 8) | *b as i128);
            let bits = width as u32 * 8;
            let n = if signed && n >= 1i128 << (bits - 1) {
                n - (1i128 << bits)
            } else {
                n
            };
            n.to_string()
        }
        Kind::Str => strings(bytes),
        Kind::Bytes => count(bytes),
        Kind::Enum(enumeration) => {
            if bytes.is_empty() {
                return count(bytes);
            }
            bytes
                .iter()
                .map(|b| format!("{}({})", b, names::enum_name(enumeration, *b)))
                .collect::<Vec<_>>()
                .join(",")
        }
        Kind::Group(_) => count(bytes),
        Kind::Auto => {
            if bytes.is_empty() {
                "true".to_string()
            } else if strings(bytes).starts_with('"') {
                strings(bytes)
            } else {
                match bytes.len() {
                    1 | 2 | 4 | 8 => read(bytes, Kind::Num(bytes.len() as u8, false)),
                    _ => count(bytes),
                }
            }
        }
    }
}

fn bool_string(value: bool) -> String {
    if value { "true" } else { "false" }.to_string()
}

/// Opaque bytes are worth their count, not their contents.
fn count(bytes: &[u8]) -> String {
    format!("bytes({})", bytes.len())
}

/// NUL-terminated text, quoted; a list of strings is one value of several.
fn strings(bytes: &[u8]) -> String {
    let parts: Vec<&[u8]> = bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() || !bytes.last().is_some_and(|b| *b == 0) {
        return count(bytes);
    }
    let mut quoted = Vec::with_capacity(parts.len());
    for part in &parts {
        match std::str::from_utf8(part) {
            Ok(text) => quoted.push(format!("{:?}", shorten(text))),
            Err(_) => return count(bytes),
        }
    }
    if quoted.len() == 1 {
        quoted.remove(0)
    } else {
        format!("[{}]", quoted.join(","))
    }
}

/// A name, address or key that would swamp a line is cut, not dropped.
fn shorten(text: &str) -> String {
    const MAX: usize = 96;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(MAX).collect();
    cut.push_str("...");
    cut
}

#[cfg(test)]
mod tests {
    use iap2_csm::put_param;
    use iap2_link::{
        gen_checksum, LinkPacketHeader, LinkSynchronizationPayload, LspSession,
    };

    use super::{Decoder, Trace};

    /// A link packet as LIVI's own writer would build it.
    fn frame(session: u8, control: u8, seq: u8, ack: u8, payload: &[u8]) -> Vec<u8> {
        let length = if payload.is_empty() {
            9
        } else {
            payload.len() as u16 + 10
        };
        let header = LinkPacketHeader {
            length,
            control,
            seq,
            ack,
            session_id: session,
        };
        let mut out = header.pack().to_vec();
        if !payload.is_empty() {
            out.extend_from_slice(payload);
            out.push(gen_checksum(payload));
        }
        out
    }

    /// A control-session message: the 0x4040 header, then its parameters.
    fn csm(id: u16, params: &[u8]) -> Vec<u8> {
        let mut out = vec![0x40, 0x40];
        out.extend_from_slice(&((params.len() + 6) as u16).to_be_bytes());
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(params);
        out
    }

    fn lines(bytes: &[u8]) -> Vec<String> {
        Decoder::default().feed(bytes)
    }

    /// The first control frame in a real cable session, from a phone that has
    /// just answered the accessory's identification request.
    #[test]
    fn a_control_frame_is_named_not_dumped() {
        let real = [
            0xff, 0x5a, 0x00, 0x10, 0x40, 0x8c, 0x63, 0x0a, 0x5e, 0x40, 0x40, 0x00, 0x06,
            0x1d, 0x00, 0x5d,
        ];
        let got = lines(&real);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("msg=0x1d00(StartIdentification)"), "{got:?}");
        assert!(got[0].contains("session=control"), "{got:?}");
        assert!(got[0].contains("params=0"), "{got:?}");
        assert!(!got[0].contains("data="), "{got:?}");
    }

    /// The cable starts with a detection marker before any FF 5A frame.
    #[test]
    fn the_detection_marker_and_a_syn_are_read() {
        let lsp = LinkSynchronizationPayload {
            max_outgoing: 4,
            max_len: 65535,
            retransmission_timeout: 0,
            ack_timeout: 0,
            max_retransmissions: 0,
            max_ack: 0,
            sessions: vec![
                LspSession { id: 10, kind: 0, version: 2 },
                LspSession { id: 11, kind: 2, version: 1 },
                LspSession { id: 12, kind: 2, version: 1 },
            ],
        };
        let mut bytes = vec![0xff, 0x55, 0x02, 0x00, 0xee, 0x10];
        bytes.extend(frame(0, 0x80, 0x63, 0, &lsp.pack()));
        let got = lines(&bytes);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("type=syn"), "{got:?}");
        assert!(got[0].contains("max_len=65535"), "{got:?}");
        assert!(got[0].contains("sessions=10,11,12"), "{got:?}");
    }

    /// A read that ends inside a frame must not produce a line of its own.
    #[test]
    fn a_frame_split_across_two_reads_is_one_line() {
        let frame = frame(10, 0x40, 0x8c, 0x63, &csm(0x1d00, &[]));
        let mut decoder = Decoder::default();
        assert!(decoder.feed(&frame[..9]).is_empty());
        let got = decoder.feed(&frame[9..]);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("msg=0x1d00(StartIdentification)"), "{got:?}");
    }

    /// Two messages in one packet, and a bare acknowledgement, are told apart.
    #[test]
    fn several_messages_in_one_packet_get_one_line_each() {
        let mut payload = csm(0x1d00, &[]);
        payload.extend(csm(0xaa00, &[]));
        let got = lines(&frame(10, 0x40, 1, 2, &payload));
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].contains("StartIdentification"), "{got:?}");
        assert!(got[1].contains("RequestAuthenticationCertificate"), "{got:?}");
        assert!(lines(&frame(0, 0x40, 3, 4, &[])).is_empty());
    }

    /// The nested groups of a now-playing update are the fields a line is read
    /// for, not a blob of bytes.
    #[test]
    fn a_now_playing_update_shows_its_named_fields() {
        let mut media = Vec::new();
        put_param(&mut media, 1, b"And find molasses candy...\0");
        let mut playback = Vec::new();
        put_param(&mut playback, 7, "网易云音乐\0".as_bytes());
        let mut params = Vec::new();
        put_param(&mut params, 0, &media);
        put_param(&mut params, 1, &playback);
        let got = lines(&frame(10, 0x40, 0x9c, 0x6d, &csm(0x5001, &params)));
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("msg=0x5001(NowPlayingUpdate)"), "{got:?}");
        assert!(
            got[0].contains("media_item_attributes.title=\"And find molasses candy...\""),
            "{got:?}"
        );
        assert!(
            got[0].contains("playback_attributes.app_name=\"网易云音乐\""),
            "{got:?}"
        );
    }

    /// Numbers with no name in the table are still said, not guessed at.
    #[test]
    fn an_unknown_message_and_parameter_are_said_to_be_unknown() {
        let mut params = Vec::new();
        put_param(&mut params, 9, &42u32.to_be_bytes());
        let got = lines(&frame(10, 0x40, 1, 2, &csm(0x9999, &params)));
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("msg=0x9999(UNKNOWN)"), "{got:?}");
        assert!(got[0].contains("p9=42"), "{got:?}");
    }

    /// A certificate is hundreds of bytes: only its count belongs on a line.
    #[test]
    fn an_opaque_value_is_its_length() {
        let mut params = Vec::new();
        put_param(&mut params, 0, &[0x30; 607]);
        let got = lines(&frame(10, 0x40, 1, 2, &csm(0xaa01, &params)));
        assert!(got[0].contains("certificate=bytes(607)"), "{got:?}");
        assert!(!got[0].contains("3030"), "{got:?}");
    }

    /// Artwork arrives as several large reads of one frame; not one line each.
    #[test]
    fn a_large_frame_read_in_chunks_is_one_line() {
        let payload = vec![0xd8; 40_000];
        let frame = frame(12, 0x40, 1, 2, &payload);
        let mut decoder = Decoder::default();
        let mut got = Vec::new();
        for chunk in frame.chunks(8192) {
            got.extend(decoder.feed(chunk));
        }
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("session=file_transfer bytes=40000"), "{got:?}");
    }

    /// The first look at a cable is bounded in both directions.
    #[test]
    fn the_trace_stops_after_its_first_few_lines() {
        let mut trace = Trace::default();
        for _ in 0..25 {
            trace.inbound(&frame(10, 0x40, 1, 2, &csm(0x1d00, &[])));
        }
        assert_eq!(trace.inbound.lines, 20);
        assert_eq!(trace.outbound.lines, 0);
    }
}
