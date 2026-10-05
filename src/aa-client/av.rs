// The AV channel messages the transport handles itself: media data, the start
// indication it takes the session id from, and the ack every media message needs.

use super::consts::AV_MEDIA_WITH_TIMESTAMP;
use super::proto::{varint_field, varint_fields};

/// The media bytes and their timestamp. Only `AV_MEDIA_WITH_TIMESTAMP` carries
/// one, as eight big-endian bytes ahead of the data.
pub fn media(msg_id: u16, payload: &[u8]) -> (Option<u64>, &[u8]) {
    if msg_id == AV_MEDIA_WITH_TIMESTAMP && payload.len() >= 8 {
        let ts = u64::from_be_bytes(payload[..8].try_into().unwrap());
        return (Some(ts), &payload[8..]);
    }
    (None, payload)
}

/// `Start { session_id = 1 }`.
pub fn start_session_id(payload: &[u8]) -> Option<u32> {
    varint_field(payload, 1).map(|v| v as u32)
}

/// `AVMediaAckIndication { session_id = 1, ack_count = 2, receive_timestamps = 3 }`.
///
/// This one travels head unit → phone: it acknowledges frames *we* sent, so we
/// only ever decode it. (The head-unit side of the protocol encodes it, which
/// is why it looks like the mirror of everything else in this file.)
///
/// `session_id` is the one the channel was started with, compared modulo 256.
/// The timestamps are nanoseconds at which the head unit received each frame,
/// and they take priority over the count when both are present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaAck {
    pub session_id: u32,
    /// Frames acknowledged. The fallback when no timestamps are carried.
    pub ack_count: u32,
    /// Receive timestamps in nanoseconds, one per acknowledged frame.
    pub receive_timestamps: Vec<u64>,
}

pub fn parse_media_ack(body: &[u8]) -> MediaAck {
    MediaAck {
        session_id: varint_field(body, 1).unwrap_or(0) as u32,
        ack_count: varint_field(body, 2).unwrap_or(0) as u32,
        receive_timestamps: varint_fields(body, 3),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aa_client::consts::AV_MEDIA_INDICATION;
    use crate::aa_client::proto::pb_varint;

    #[test]
    fn timestamped_media_is_split() {
        let mut payload = 12345u64.to_be_bytes().to_vec();
        payload.extend_from_slice(b"nal");
        assert_eq!(
            media(AV_MEDIA_WITH_TIMESTAMP, &payload),
            (Some(12345), &b"nal"[..])
        );
    }

    #[test]
    fn plain_media_has_no_timestamp() {
        assert_eq!(media(AV_MEDIA_INDICATION, b"sps"), (None, &b"sps"[..]));
    }

    #[test]
    fn session_id_comes_out_of_start() {
        let mut body = pb_varint(1, 300);
        body.extend(pb_varint(2, 0));
        assert_eq!(start_session_id(&body), Some(300));
    }

    #[test]
    fn an_ack_names_the_session_and_the_frames_it_covers() {
        let mut body = pb_varint(1, 1);
        body.extend(pb_varint(2, 3));
        let ack = parse_media_ack(&body);
        assert_eq!(ack.session_id, 1);
        assert_eq!(ack.ack_count, 3);
        assert!(ack.receive_timestamps.is_empty());
    }

    #[test]
    fn per_frame_timestamps_come_through_an_ack() {
        let mut body = pb_varint(1, 7);
        body.extend(pb_varint(3, 1000));
        body.extend(pb_varint(3, 2000));
        assert_eq!(parse_media_ack(&body).receive_timestamps, vec![1000, 2000]);
    }
}
