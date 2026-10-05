// Android Auto wire-protocol constants, the subset the transport needs.

pub const TCP_PORT: u16 = 5277;

// Frame flag bits.
pub const FLAG_FIRST: u8 = 0x01;
pub const FLAG_LAST: u8 = 0x02;
pub const FLAG_ENCRYPTED: u8 = 0x08;

// The flag combinations the protocol uses.
pub const FLAGS_PLAINTEXT: u8 = 0x03;
pub const FLAGS_ENC_SIGNAL: u8 = 0x0b;

// Set when the payload belongs to the channel it rides on rather than to the
// control channel's conversation — channel open requests, for one.
pub const FLAG_SPECIFIC: u8 = 0x04;
pub const FLAGS_ENC_SPECIFIC: u8 = 0x0f;

// Channel ids.
pub const CH_CONTROL: u8 = 0;
pub const CH_VIDEO: u8 = 3;
pub const CH_MEDIA_AUDIO: u8 = 4;
pub const CH_SPEECH_AUDIO: u8 = 5;
pub const CH_SYSTEM_AUDIO: u8 = 6;
pub const CH_MIC_INPUT: u8 = 9;
pub const CH_CLUSTER_VIDEO: u8 = 19;

// Control channel message ids. 1..3 are the plaintext handshake; the rest ride
// inside TLS. The numbering is the protocol's own, not ours.
pub const CTRL_VERSION_REQUEST: u16 = 0x0001;
pub const CTRL_VERSION_RESPONSE: u16 = 0x0002;
pub const CTRL_SSL_HANDSHAKE: u16 = 0x0003;
pub const CTRL_AUTH_COMPLETE: u16 = 0x0004;
pub const CTRL_SERVICE_DISCOVERY_REQUEST: u16 = 0x0005;
pub const CTRL_SERVICE_DISCOVERY_RESPONSE: u16 = 0x0006;
pub const CTRL_CHANNEL_OPEN_REQUEST: u16 = 0x0007;
pub const CTRL_CHANNEL_OPEN_RESPONSE: u16 = 0x0008;
pub const CTRL_PING_REQUEST: u16 = 0x000b;
pub const CTRL_PING_RESPONSE: u16 = 0x000c;
pub const CTRL_NAV_FOCUS_REQUEST: u16 = 0x000d;
pub const CTRL_NAV_FOCUS_RESPONSE: u16 = 0x000e;
pub const CTRL_VOICE_SESSION_REQUEST: u16 = 0x0011;
pub const CTRL_AUDIO_FOCUS_REQUEST: u16 = 0x0012;
pub const CTRL_AUDIO_FOCUS_RESPONSE: u16 = 0x0013;

// AV channel message ids.
pub const AV_MEDIA_WITH_TIMESTAMP: u16 = 0x0000;
pub const AV_MEDIA_INDICATION: u16 = 0x0001;
pub const AV_SETUP_REQUEST: u16 = 0x8000;
pub const AV_START_INDICATION: u16 = 0x8001;
pub const AV_SETUP_RESPONSE: u16 = 0x8003;
pub const AV_MEDIA_ACK: u16 = 0x8004;
pub const AV_VIDEO_FOCUS_INDICATION: u16 = 0x8008;

// Input channel message ids, the ones a phone answers or decodes.
pub const INPUT_EVENT: u16 = 0x8001;
pub const INPUT_HANDSHAKE_REQUEST: u16 = 0x8002;
pub const INPUT_HANDSHAKE_RESPONSE: u16 = 0x8003;

// `MediaCodecType.Enum`: what an AV channel carries.
//
// This — not `MediaStreamType` — is what `AVChannelSetupRequest` names in field
// 1, and what a head unit's media sink service calls its `available_type`. The
// values come from the Android Auto APK (see the `oaa/` proto tree), where 3 is
// H.264 baseline and 7 is H.265.
pub const MEDIA_CODEC_AUDIO_PCM: u64 = 1;
pub const MEDIA_CODEC_AUDIO_AAC_LC: u64 = 2;
pub const MEDIA_CODEC_VIDEO_H264_BP: u64 = 3;
pub const MEDIA_CODEC_AUDIO_AAC_LC_ADTS: u64 = 4;
pub const MEDIA_CODEC_VIDEO_VP9: u64 = 5;
pub const MEDIA_CODEC_VIDEO_AV1: u64 = 6;
pub const MEDIA_CODEC_VIDEO_H265: u64 = 7;

/// Video codecs as opposed to audio ones, which is how a sink service's
/// `available_type` has to be read. A head unit offering H.265 says 7, so
/// testing for 3 alone would file it away as an audio channel.
pub fn is_video_codec(codec: u64) -> bool {
    matches!(
        codec,
        MEDIA_CODEC_VIDEO_H264_BP
            | MEDIA_CODEC_VIDEO_VP9
            | MEDIA_CODEC_VIDEO_AV1
            | MEDIA_CODEC_VIDEO_H265
    )
}

// `AVChannelSetupStatus.Enum`: the head unit's answer to a setup request. Zero
// is not a valid value on the wire — it counts as a failure.
pub const SETUP_STATUS_NONE: u32 = 0;
pub const SETUP_STATUS_FAIL: u32 = 1;
pub const SETUP_STATUS_OK: u32 = 2;

pub const VERSION_MAJOR: u16 = 1;
pub const VERSION_MINOR: u16 = 7;
pub const VERSION_STATUS_MISMATCH: u16 = 0xffff;

/// What we answer the version request with.
///
/// The head unit asks for its own version and we name ours, then say the two
/// agree. 1.5 is what AACS's phone side answers and head units take it, so it
/// is a safer thing to claim than the 1.7 a head unit advertises for itself.
pub const PHONE_VERSION_MINOR: u16 = 5;

pub fn is_video_channel(ch: u8) -> bool {
    ch == CH_VIDEO || ch == CH_CLUSTER_VIDEO
}

pub fn is_audio_channel(ch: u8) -> bool {
    ch == CH_MEDIA_AUDIO || ch == CH_SPEECH_AUDIO || ch == CH_SYSTEM_AUDIO
}

pub fn is_media_message(msg_id: u16) -> bool {
    msg_id == AV_MEDIA_WITH_TIMESTAMP || msg_id == AV_MEDIA_INDICATION
}
