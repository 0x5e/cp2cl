// The control channel's protobuf: what a phone says to be let in.
//
// Field numbers follow AACS's `proto/*.proto`, which is the closest thing to a
// public description of this conversation. Encoding is by hand rather than
// generated: the messages we need are small, and hand-rolled varints keep this
// crate's dependency list the size it already was.

use super::consts::{is_video_codec, MEDIA_CODEC_AUDIO_PCM, SETUP_STATUS_OK};
use super::proto::{bytes_field, bytes_fields, pb_varint, varint_field, varint_fields};

/// `ServiceDiscoveryRequest { model = 4, manufacturer = 5 }`.
///
/// The phone introduces itself; the head unit answers with what it can do.
pub fn service_discovery_request(manufacturer: &str, model: &str) -> Vec<u8> {
    let mut out = super::proto::pb_string(4, model);
    out.extend(super::proto::pb_string(5, manufacturer));
    out
}

/// `ChannelOpenRequest { unknown_field = 1, channel_id = 2 }`.
///
/// Always zero in every capture there is; it is carried because the head unit
/// wants it present, not because anyone knows what it means.
pub fn channel_open_request(ch: u8) -> Vec<u8> {
    let mut out = pb_varint(1, 0);
    out.extend(pb_varint(2, ch as u64));
    out
}

/// `PingResponse { timestamp = 1 }`, echoing what the request carried.
pub fn ping_response(timestamp: u64) -> Vec<u8> {
    pb_varint(1, timestamp)
}

/// `InputChannelHandshakeRequest { available_buttons = 1 }`, echoing back the
/// subset of the head unit's buttons we can act on.
pub fn input_handshake_request(buttons: &[u32]) -> Vec<u8> {
    buttons
        .iter()
        .flat_map(|b| pb_varint(1, *b as u64))
        .collect()
}

/// `AudioFocusResponse { focus_state = 1 }`; 1 is "gained".
pub fn audio_focus_response(state: u32) -> Vec<u8> {
    pb_varint(1, state as u64)
}

/// `NavigationFocusResponse { focus_type = 1 }`.
pub fn nav_focus_response(focus: u32) -> Vec<u8> {
    pb_varint(1, focus as u64)
}

/// `AVChannelSetupRequest { media_codec_type = 1 }`.
///
/// Field 1 is a `MediaCodecType`, not a stream type: 3 is H.264 baseline, 7 is
/// H.265, 1 is raw PCM. It has to name what we are about to send, because the
/// head unit decodes by it — declaring H.264 while streaming H.265 is a picture
/// that never comes together.
pub fn av_setup_request(codec: u64) -> Vec<u8> {
    pb_varint(1, codec)
}

/// `AVChannelStartIndication { session = 1, config = 2 }`.
///
/// Sent once the head unit hands us focus, and it is what opens the channel for
/// media: every packet after it is acked against the session id it carries.
/// Field 2 is the index into the configs the head unit offered, not a spare.
pub fn av_start_indication(session: u64) -> Vec<u8> {
    let mut out = pb_varint(1, session);
    out.extend(pb_varint(2, 0));
    out
}

pub fn setup_request_video(codec: u64) -> Vec<u8> {
    av_setup_request(codec)
}

pub fn setup_request_audio() -> Vec<u8> {
    av_setup_request(MEDIA_CODEC_AUDIO_PCM)
}

/// What the head unit offered in its `ServiceDiscoveryResponse`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub video: Option<VideoOffer>,
    pub audio: Vec<AudioOffer>,
    pub input: Option<InputOffer>,
    /// Channels we do not drive ourselves but must not choke on.
    pub sensor: Option<u8>,
    pub navigation: Option<u8>,
    pub bluetooth: Option<u8>,
    pub vendor: Vec<u8>,
    pub other: Vec<u8>,
}

impl Discovered {
    /// The channel to send video on, and the first config the head unit listed.
    pub fn video_channel(&self) -> Option<u8> {
        self.video.as_ref().map(|v| v.ch)
    }

    /// The media audio channel, if the head unit takes one.
    pub fn media_audio_channel(&self) -> Option<u8> {
        self.audio
            .iter()
            .find(|a| a.audio_type == 3)
            .or_else(|| self.audio.first())
            .map(|a| a.ch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoOffer {
    pub ch: u8,
    /// `MediaCodecType` the head unit takes here: 3 is H.264, 7 is H.265.
    pub codec: u32,
    pub configs: Vec<VideoConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoConfig {
    pub resolution: u32,
    pub fps: u32,
    pub margin_width: u32,
    pub margin_height: u32,
    pub dpi: u32,
    pub additional_depth: Option<u32>,
}

impl VideoConfig {
    /// The pixels behind `VideoResolution.Enum`.
    pub fn pixels(&self) -> Option<(u32, u32)> {
        match self.resolution {
            1 => Some((800, 480)),
            2 => Some((1280, 720)),
            3 => Some((1920, 1080)),
            _ => None,
        }
    }

    /// The frames per second behind `VideoFps.Enum`.
    pub fn fps(&self) -> Option<u32> {
        match self.fps {
            1 => Some(30),
            2 => Some(60),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOffer {
    pub ch: u8,
    /// `AudioType.Enum`: speech, system, media, alarm.
    pub audio_type: u32,
    pub configs: Vec<AudioConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub bits_per_sample: u32,
    pub channel_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputOffer {
    pub ch: u8,
    /// `ButtonCode.Enum` values the head unit has, see `input::ButtonCode`.
    pub buttons: Vec<u32>,
    pub screen: Option<(u32, u32)>,
}

/// Reads `ServiceDiscoveryResponse { channels = 1 }`.
///
/// Each entry is a `Channel { channel_id = 1, ... }` whose body says what the
/// channel is. Unknown ones are collected rather than dropped, so a head unit
/// with something new still gets answered.
pub fn parse_service_discovery_response(body: &[u8]) -> Discovered {
    let mut out = Discovered::default();
    for channel in bytes_fields(body, 1) {
        let Some(ch) = varint_field(channel, 1) else {
            continue;
        };
        let ch = ch as u8;
        if let Some(media) = bytes_field(channel, 3) {
            // Field 1 is the codec the channel takes (aasdk names it
            // `available_type`): a video channel says 3, 5, 6 or 7, an audio
            // one says 1, 2 or 4. Reading it as a stream type, where 3 happens
            // to be "video", works until a head unit offers H.265 and says 7.
            let codec = varint_field(media, 1).unwrap_or(0) as u32;
            let audio_type = varint_field(media, 2).unwrap_or(0) as u32;
            let video_configs = bytes_fields(media, 4)
                .iter()
                .map(|c| video_config(c))
                .collect();
            let audio_configs = bytes_fields(media, 3)
                .iter()
                .map(|c| audio_config(c))
                .collect();
            if is_video_codec(codec as u64) {
                out.video = Some(VideoOffer {
                    ch,
                    codec,
                    configs: video_configs,
                });
            } else {
                out.audio.push(AudioOffer {
                    ch,
                    audio_type,
                    configs: audio_configs,
                });
            }
            continue;
        }
        if let Some(input) = bytes_field(channel, 4) {
            let buttons = varint_fields(input, 1).iter().map(|b| *b as u32).collect();
            let screen = bytes_field(input, 2)
                .and_then(|t| Some((varint_field(t, 1)? as u32, varint_field(t, 2)? as u32)));
            out.input = Some(InputOffer {
                ch,
                buttons,
                screen,
            });
            continue;
        }
        if bytes_field(channel, 2).is_some() {
            out.sensor = Some(ch);
            continue;
        }
        if bytes_field(channel, 8).is_some() {
            out.navigation = Some(ch);
            continue;
        }
        if bytes_field(channel, 6).is_some() {
            out.bluetooth = Some(ch);
            continue;
        }
        if bytes_field(channel, 12).is_some() {
            out.vendor.push(ch);
            continue;
        }
        out.other.push(ch);
    }
    out
}

fn video_config(body: &[u8]) -> VideoConfig {
    VideoConfig {
        resolution: varint_field(body, 1).unwrap_or(0) as u32,
        fps: varint_field(body, 2).unwrap_or(0) as u32,
        margin_width: varint_field(body, 3).unwrap_or(0) as u32,
        margin_height: varint_field(body, 4).unwrap_or(0) as u32,
        dpi: varint_field(body, 5).unwrap_or(0) as u32,
        additional_depth: varint_field(body, 6).map(|v| v as u32),
    }
}

fn audio_config(body: &[u8]) -> AudioConfig {
    AudioConfig {
        sample_rate: varint_field(body, 1).unwrap_or(0) as u32,
        bits_per_sample: varint_field(body, 2).unwrap_or(0) as u32,
        channel_count: varint_field(body, 3).unwrap_or(0) as u32,
    }
}

/// `PingRequest { timestamp = 1 }`.
pub fn parse_ping_request(body: &[u8]) -> Option<u64> {
    varint_field(body, 1)
}

/// `ChannelOpenResponse { status = 1 }`; zero means the channel is open.
pub fn parse_channel_open_response(body: &[u8]) -> Option<u32> {
    varint_field(body, 1).map(|v| v as u32)
}

/// `AudioFocusRequest { audio_type = 1, focus_state = 2 }` — what the head unit
/// grants us. Only the two numbers matter for answering it.
pub fn parse_audio_focus_request(body: &[u8]) -> (u32, u32) {
    (
        varint_field(body, 1).unwrap_or(0) as u32,
        varint_field(body, 2).unwrap_or(0) as u32,
    )
}

/// `AVChannelSetupResponse { media_status = 1, max_unacked = 2, configs = 3 }`.
///
/// The status is the head unit's verdict on the codec we asked for, so it is
/// the thing that decides whether media may start. `max_unacked` is the flow
/// control window: how many frames may be outstanding before we have to slow
/// down. `configs` names the display or stream indices it settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvSetupResponse {
    /// `AVChannelSetupStatus.Enum`. Zero is not a valid wire value and counts
    /// as a failure, so compare with `ok()` rather than against zero.
    pub status: u32,
    pub max_unacked: u32,
    pub configs: Vec<u32>,
}

impl AvSetupResponse {
    /// Whether the head unit took the channel and the codec it was offered.
    pub fn ok(&self) -> bool {
        self.status == SETUP_STATUS_OK
    }
}

pub fn parse_av_setup_response(body: &[u8]) -> AvSetupResponse {
    AvSetupResponse {
        status: varint_field(body, 1).unwrap_or(0) as u32,
        max_unacked: varint_field(body, 2).unwrap_or(0) as u32,
        configs: varint_fields(body, 3).iter().map(|c| *c as u32).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aa_client::consts::{
        MEDIA_CODEC_VIDEO_H264_BP, MEDIA_CODEC_VIDEO_H265, SETUP_STATUS_FAIL,
    };
    use crate::aa_client::proto::{pb_bytes, pb_string, pb_varint};

    /// A channel entry as a head unit writes it: id, then the body saying which
    /// kind it is.
    fn channel(ch: u8, field: u32, body: Vec<u8>) -> Vec<u8> {
        let mut inner = pb_varint(1, ch as u64);
        inner.extend(pb_bytes(field, &body));
        pb_bytes(1, &inner)
    }

    /// A media sink service body: field 1 is the codec, field 4 the video
    /// configs.
    fn media_body(codec: u64) -> Vec<u8> {
        let mut cfg = pb_varint(1, 2); // 720p
        cfg.extend(pb_varint(2, 1)); // 30 fps
        cfg.extend(pb_varint(3, 0));
        cfg.extend(pb_varint(4, 0));
        cfg.extend(pb_varint(5, 160));
        let mut media = pb_varint(1, codec);
        media.extend(pb_bytes(4, &cfg));
        media
    }

    fn video_body() -> Vec<u8> {
        media_body(MEDIA_CODEC_VIDEO_H264_BP)
    }

    #[test]
    fn a_video_channel_is_found_with_its_config() {
        let mut body = channel(3, 3, video_body());
        body.extend(channel(8, 4, pb_varint(1, 0x17)));

        let d = parse_service_discovery_response(&body);
        let video = d.video.as_ref().expect("video channel");
        assert_eq!(video.ch, 3);
        assert_eq!(video.configs.len(), 1);
        assert_eq!(video.configs[0].pixels(), Some((1280, 720)));
        assert_eq!(video.configs[0].fps(), Some(30));
        assert_eq!(d.media_audio_channel(), None);
    }

    #[test]
    fn an_input_channel_keeps_its_buttons_and_screen() {
        let mut input = pb_varint(1, 0x13);
        input.extend(pb_varint(1, 0x17));
        let mut screen = pb_varint(1, 800);
        screen.extend(pb_varint(2, 480));
        input.extend(pb_bytes(2, &screen));

        let d = parse_service_discovery_response(&channel(8, 4, input));
        let input = d.input.expect("input channel");
        assert_eq!(input.ch, 8);
        assert_eq!(input.buttons, vec![0x13, 0x17]);
        assert_eq!(input.screen, Some((800, 480)));
    }

    #[test]
    fn unknown_channels_are_kept_not_dropped() {
        let d = parse_service_discovery_response(&channel(13, 10, Vec::new()));
        assert_eq!(d.other, vec![13]);
        assert!(d.video.is_none());
    }

    #[test]
    fn the_requests_match_the_reference_shape() {
        // AACS sends the fields in this order and the head units take it.
        let sdr = service_discovery_request("TAG", "AAServer");
        assert_eq!(sdr, concat(pb_string(4, "AAServer"), pb_string(5, "TAG")));
        assert_eq!(channel_open_request(3), vec![0x08, 0x00, 0x10, 0x03]);
        assert_eq!(
            setup_request_video(MEDIA_CODEC_VIDEO_H264_BP),
            vec![0x08, 0x03]
        );
        assert_eq!(av_start_indication(0), vec![0x08, 0x00, 0x10, 0x00]);
        assert_eq!(ping_response(12345), pb_varint(1, 12345));
    }

    /// A head unit that takes H.265 says 7, not 3, in the same field.
    #[test]
    fn a_head_unit_offering_h265_is_still_a_video_channel() {
        let body = channel(3, 3, media_body(MEDIA_CODEC_VIDEO_H265));
        let d = parse_service_discovery_response(&body);
        let video = d.video.as_ref().expect("video channel");
        assert_eq!(video.codec, MEDIA_CODEC_VIDEO_H265 as u32);
        assert_eq!(d.media_audio_channel(), None);
    }

    /// Raw PCM is an audio channel and keeps its `AudioType`.
    #[test]
    fn a_pcm_channel_is_audio_not_video() {
        let mut media = pb_varint(1, 1); // PCM
        media.extend(pb_varint(2, 3)); // media audio
        let d = parse_service_discovery_response(&channel(4, 3, media));
        assert!(d.video.is_none());
        assert_eq!(d.media_audio_channel(), Some(4));
    }

    #[test]
    fn setup_response_carries_the_verdict_and_the_window() {
        let mut body = pb_varint(1, SETUP_STATUS_OK as u64);
        body.extend(pb_varint(2, 24));
        let r = parse_av_setup_response(&body);
        assert!(r.ok());
        assert_eq!(r.max_unacked, 24);

        // A refusal has to be recognisable: zero is not "unset", it is a fail.
        let refused = parse_av_setup_response(&pb_varint(1, SETUP_STATUS_FAIL as u64));
        assert!(!refused.ok());
        assert!(!parse_av_setup_response(&[]).ok());
    }

    fn concat(mut a: Vec<u8>, b: Vec<u8>) -> Vec<u8> {
        a.extend(b);
        a
    }
}
