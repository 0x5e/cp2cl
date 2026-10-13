//! The CarLife message bodies, in as much protobuf as they need.
//!
//! Every body on every channel is a protobuf message, and the car's own library
//! ships the `.proto` for each one — they are the source of the field numbers
//! below, and a copy sits in `../CarLifePort/vendor/carlife/…/protobuf/proto`
//! for anyone checking one. What is written here is the smallest amount of
//! protobuf that can carry them: tags, varints and length-delimited fields, no
//! code generator and no dependency.
//!
//! That is affordable because the messages are small and flat. The largest is
//! `MD_INFO`, which is twenty-three optional strings, and we fill six of them.
//! Everything else is one to eight scalars. What the encoders are *not* is
//! forgiving: the car reads these with a generated parser, so a body that does
//! not decode makes the message disappear rather than fail loudly.

/// One field, as it appears on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field<'a> {
    /// A tag whose wire type is 0: integers of every width, and booleans.
    Varint { number: u32, value: u64 },
    /// A tag whose wire type is 2: strings, bytes, and embedded messages.
    Bytes { number: u32, value: &'a [u8] },
}

/// Walks the fields of a body, skipping over anything it does not name.
///
/// Iteration stops at the first field it cannot read, which is what the
/// generated parsers do too — a body is only ever trusted as far as it decodes.
pub struct Fields<'a> {
    rest: &'a [u8],
}

impl<'a> Fields<'a> {
    pub fn new(body: &'a [u8]) -> Self {
        Self { rest: body }
    }

    pub fn next_field(&mut self) -> Option<Field<'a>> {
        let tag = take_varint(&mut self.rest)? as u32;
        let number = tag >> 3;
        if number == 0 {
            return None;
        }
        match tag & 0x7 {
            0 => Some(Field::Varint {
                number,
                value: take_varint(&mut self.rest)?,
            }),
            2 => {
                let len = take_varint(&mut self.rest)? as usize;
                if self.rest.len() < len {
                    return None;
                }
                let (value, rest) = self.rest.split_at(len);
                self.rest = rest;
                Some(Field::Bytes { number, value })
            }
            // Wire types 1 and 5 are fixed-width integers and 3 and 4 are
            // groups; CarLife uses none of them.
            _ => None,
        }
    }
}

fn take_varint(rest: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let (byte, tail) = rest.split_first()?;
        value |= ((byte & 0x7f) as u64) << shift;
        *rest = tail;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    // Longer than a u64 can hold, so the body is not what it claims to be.
    None
}

/// The value of a single scalar field, read as the signed integer it is.
fn int_of(body: &[u8], number: u32) -> Option<i64> {
    let mut fields = Fields::new(body);
    while let Some(field) = fields.next_field() {
        if let Field::Varint { number: n, value } = field {
            if n == number {
                // `int32` on the wire is the value sign-extended to 64 bits, so
                // the cast back is what recovers a negative number.
                return Some(value as i64);
            }
        }
    }
    None
}

/// The value of a single string field, left as bytes.
fn bytes_of<'a>(body: &'a [u8], number: u32) -> Option<&'a [u8]> {
    let mut fields = Fields::new(body);
    while let Some(field) = fields.next_field() {
        if let Field::Bytes { number: n, value } = field {
            if n == number {
                return Some(value);
            }
        }
    }
    None
}

/// The value of a single string field, as the text it is.
fn str_of<'a>(body: &'a [u8], number: u32) -> Option<&'a str> {
    std::str::from_utf8(bytes_of(body, number)?).ok()
}

// ---------------------------------------------------------------- encoding ---

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn tag(out: &mut Vec<u8>, number: u32, wire_type: u8) {
    put_varint(out, ((number as u64) << 3) | wire_type as u64);
}

pub fn put_int(out: &mut Vec<u8>, number: u32, value: i32) {
    tag(out, number, 0);
    // A negative `int32` is sign-extended to ten bytes, which is what the
    // generated encoders emit and therefore what the car's parser expects.
    put_varint(out, value as i64 as u64);
}

fn put_bool(out: &mut Vec<u8>, number: u32, value: bool) {
    tag(out, number, 0);
    put_varint(out, u64::from(value));
}

pub fn put_str(out: &mut Vec<u8>, number: u32, value: &str) {
    if value.is_empty() {
        return;
    }
    tag(out, number, 2);
    put_varint(out, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

// --------------------------------------------------------- what we send up ---

/// `MSG_CMD_PROTOCOL_VERSION_MATCH_STATUS` — its single field, and the reply to
/// the car's opening version message.
pub fn protocol_version_match_status(match_status: i32) -> Vec<u8> {
    let mut out = Vec::new();
    put_int(&mut out, 1, match_status);
    out
}

/// There is one value, and the reference sends it.
pub const MATCH: i32 = 1;

/// What we claim to be in `MSG_CMD_MD_INFO`.
///
/// `CarlifeDeviceInfo` is twenty-three optional fields. Only the ones the
/// reference implementations fill in are offered; the car treats all of them as
/// optional, so leaving the rest out is not a shortcut.
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub os: String,
    pub brand: String,
    pub model: String,
    pub release: String,
    pub sdk_int: i32,
    pub bt_address: String,
}

pub fn device_info(info: &DeviceInfo) -> Vec<u8> {
    let mut out = Vec::new();
    put_str(&mut out, 1, &info.os);
    put_str(&mut out, 4, &info.brand);
    put_str(&mut out, 14, &info.model);
    put_str(&mut out, 19, &info.release);
    if info.sdk_int != 0 {
        put_int(&mut out, 21, info.sdk_int);
    }
    put_str(&mut out, 23, &info.bt_address);
    out
}

/// What the car asks for and what we agree to, the same `CarlifeVideoEncoderInfo`
/// in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoEncoderInfo {
    pub width: i32,
    pub height: i32,
    pub frame_rate: i32,
}

pub fn video_encoder_info(info: VideoEncoderInfo) -> Vec<u8> {
    let mut out = Vec::new();
    put_int(&mut out, 1, info.width);
    put_int(&mut out, 2, info.height);
    put_int(&mut out, 3, info.frame_rate);
    out
}

pub fn decode_video_encoder_info(body: &[u8]) -> VideoEncoderInfo {
    VideoEncoderInfo {
        width: int_of(body, 1).unwrap_or(0) as i32,
        height: int_of(body, 2).unwrap_or(0) as i32,
        frame_rate: int_of(body, 3).unwrap_or(0) as i32,
    }
}

/// `MSG_CMD_MD_AUTHEN_RESULT`.
pub fn authen_result(authenticated: bool) -> Vec<u8> {
    let mut out = Vec::new();
    put_bool(&mut out, 1, authenticated);
    out
}

/// `MSG_CMD_VIDEO_ENCODER_FRAME_RATE_CHANGE_DONE`.
pub fn frame_rate(rate: i32) -> Vec<u8> {
    let mut out = Vec::new();
    put_int(&mut out, 1, rate);
    out
}

// --------------------------------------------------------- what comes down ---

pub fn decode_protocol_version(body: &[u8]) -> (i32, i32) {
    (
        int_of(body, 1).unwrap_or(0) as i32,
        int_of(body, 2).unwrap_or(0) as i32,
    )
}

/// The car's `randomValue` from `MSG_CMD_HU_AUTHEN_REQUEST`.
///
/// A string, not bytes: the reference builds it as `"<random>_<minimum phone
/// version>"` and the device is expected back with it encrypted.
pub fn decode_authen_request(body: &[u8]) -> Option<&str> {
    str_of(body, 1)
}

/// The car's verdict from `MSG_CMD_HU_AUTHEN_RESULT`.
pub fn decode_authen_result(body: &[u8]) -> Option<bool> {
    int_of(body, 1).map(|value| value != 0)
}

/// `MSG_CMD_HU_STATISTIC_INFO`: who the car thinks it is talking to. Nothing
/// here acts on it beyond reporting it, but it is the one message that says
/// whether the car has accepted the channel number we are supposed to carry.
#[derive(Debug, Default, Clone)]
pub struct StatisticInfo<'a> {
    pub cuid: &'a str,
    pub version_name: &'a str,
    pub version_code: i32,
    pub channel: &'a str,
    pub connect_count: i32,
    pub connect_success_count: i32,
    pub connect_time: i32,
}

pub fn decode_statistic_info(body: &[u8]) -> StatisticInfo<'_> {
    StatisticInfo {
        cuid: str_of(body, 1).unwrap_or_default(),
        version_name: str_of(body, 2).unwrap_or_default(),
        version_code: int_of(body, 3).unwrap_or(0) as i32,
        channel: str_of(body, 4).unwrap_or_default(),
        connect_count: int_of(body, 5).unwrap_or(0) as i32,
        connect_success_count: int_of(body, 6).unwrap_or(0) as i32,
        connect_time: int_of(body, 7).unwrap_or(0) as i32,
    }
}

/// `MSG_CMD_TOUCH_ACTION`: one finger, moving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchAction {
    /// 0 down, 1 up, 2 move.
    pub action: i32,
    pub x: i32,
    pub y: i32,
}

pub fn decode_touch_action(body: &[u8]) -> Option<TouchAction> {
    Some(TouchAction {
        action: int_of(body, 1)? as i32,
        x: int_of(body, 2)? as i32,
        y: int_of(body, 3)? as i32,
    })
}

/// The panel keys, `MSG_CMD_TOUCH_CAR_HARD_KEY_CODE`. The values are the
/// `KEYCODE_*` constants in CarLifeVehicleLib and are listed in the plan.
pub fn decode_hard_key(body: &[u8]) -> Option<i32> {
    int_of(body, 1).map(|value| value as i32)
}

/// `MSG_CMD_HU_FEATURE_CONFIG_RESPONSE`: the car's answers to the switches we
/// asked about, as the key-value list the message is.
pub fn decode_feature_configs(body: &[u8]) -> Vec<(String, i32)> {
    let mut out = Vec::new();
    let mut fields = Fields::new(body);
    while let Some(field) = fields.next_field() {
        // Every entry is field 2, and `CarlifeFeatureConfig` is a key and a
        // value — so the walk has to be over the body, not over one entry.
        let Field::Bytes { number: 2, value } = field else {
            continue;
        };
        if let (Some(key), Some(setting)) = (str_of(value, 1), int_of(value, 2)) {
            out.push((key.to_string(), setting as i32));
        }
    }
    out
}

/// `MSG_CMD_VIDEO_ENCODER_FRAME_RATE_CHANGE`.
pub fn decode_frame_rate(body: &[u8]) -> Option<i32> {
    int_of(body, 1).map(|value| value as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_message_round_trips_as_the_two_ints_it_is() {
        let body = [0x08, 0x01, 0x10, 0x00];
        assert_eq!(decode_protocol_version(&body), (1, 0));
    }

    #[test]
    fn the_match_status_body_is_the_field_the_reference_sends() {
        assert_eq!(protocol_version_match_status(MATCH), vec![0x08, 0x01]);
        assert_eq!(int_of(&protocol_version_match_status(MATCH), 1), Some(1));
    }

    #[test]
    fn device_info_carries_the_fields_it_was_given_and_nothing_else() {
        let info = DeviceInfo {
            os: "Android".into(),
            brand: "cp2cl".into(),
            model: "cp2cl".into(),
            release: "10".into(),
            sdk_int: 29,
            bt_address: "00:11:22:33:44:55".into(),
        };
        let body = device_info(&info);

        assert_eq!(str_of(&body, 1), Some("Android"));
        assert_eq!(str_of(&body, 4), Some("cp2cl"));
        assert_eq!(str_of(&body, 14), Some("cp2cl"));
        assert_eq!(str_of(&body, 19), Some("10"));
        assert_eq!(int_of(&body, 21), Some(29));
        assert_eq!(str_of(&body, 23), Some("00:11:22:33:44:55"));
        // Untouched fields are absent rather than empty.
        assert_eq!(str_of(&body, 2), None);
    }

    #[test]
    fn an_unset_sdk_int_is_left_out_rather_than_written_as_zero() {
        let body = device_info(&DeviceInfo {
            os: "Android".into(),
            ..DeviceInfo::default()
        });
        assert_eq!(int_of(&body, 21), None);
    }

    #[test]
    fn the_encoder_info_is_the_same_message_in_both_directions() {
        let asked = video_encoder_info(VideoEncoderInfo {
            width: 1280,
            height: 720,
            frame_rate: 30,
        });
        assert_eq!(
            decode_video_encoder_info(&asked),
            VideoEncoderInfo {
                width: 1280,
                height: 720,
                frame_rate: 30
            }
        );
        // 1280 needs two bytes, so the field is longer than a one-byte value.
        assert_eq!(asked[..2], [0x08, 0x80]);
    }

    #[test]
    fn the_authentication_request_is_read_as_the_string_it_is() {
        let body = [0x0a, 0x03, b'A', b'_', b'B'];
        assert_eq!(decode_authen_request(&body), Some("A_B"));
    }

    #[test]
    fn the_authentication_result_is_a_boolean() {
        assert_eq!(authen_result(true), vec![0x08, 0x01]);
        assert_eq!(decode_authen_result(&authen_result(false)), Some(false));
    }

    #[test]
    fn statistic_info_is_read_field_by_field() {
        let mut body = Vec::new();
        put_str(&mut body, 1, "cuid-1");
        put_str(&mut body, 2, "7.0.0");
        put_int(&mut body, 3, 700);
        put_str(&mut body, 4, "20000000");
        put_int(&mut body, 5, 3);
        put_int(&mut body, 6, 1);
        put_int(&mut body, 7, 900);

        let info = decode_statistic_info(&body);
        assert_eq!(info.cuid, "cuid-1");
        assert_eq!(info.channel, "20000000");
        assert_eq!(info.connect_count, 3);
        assert_eq!(info.connect_time, 900);
    }

    #[test]
    fn a_touch_action_needs_all_three_numbers() {
        let mut body = Vec::new();
        put_int(&mut body, 1, 0);
        put_int(&mut body, 2, 640);
        put_int(&mut body, 3, 360);
        assert_eq!(
            decode_touch_action(&body),
            Some(TouchAction {
                action: 0,
                x: 640,
                y: 360
            })
        );

        let mut short = Vec::new();
        put_int(&mut short, 1, 0);
        assert_eq!(decode_touch_action(&short), None);
    }

    #[test]
    fn the_hard_key_is_the_one_scalar_it_is() {
        let body = [0x08, 0x14];
        assert_eq!(decode_hard_key(&body), Some(0x14));
    }

    #[test]
    fn the_feature_config_list_is_walked_as_the_repeated_field_it_is() {
        // CarlifeFeatureConfigList { cnt = 2, featureConfig = [ {key,value} × 2 ] }
        let mut first = Vec::new();
        put_str(&mut first, 1, "VOICE_MIC");
        put_int(&mut first, 2, 0);
        let mut second = Vec::new();
        put_str(&mut second, 1, "AUDIO_TRANSMISSION_MODE");
        put_int(&mut second, 2, 0);

        let mut body = Vec::new();
        put_int(&mut body, 1, 2);
        for entry in [&first, &second] {
            tag(&mut body, 2, 2);
            put_varint(&mut body, entry.len() as u64);
            body.extend_from_slice(entry);
        }

        assert_eq!(
            decode_feature_configs(&body),
            vec![
                ("VOICE_MIC".to_string(), 0),
                ("AUDIO_TRANSMISSION_MODE".to_string(), 0)
            ]
        );
    }

    #[test]
    fn a_negative_int_is_written_sign_extended_the_way_an_int32_is() {
        let mut out = Vec::new();
        put_int(&mut out, 1, -1);
        assert_eq!(out, vec![0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
    }

    #[test]
    fn a_body_that_stops_mid_field_yields_what_came_before_it() {
        let mut body = Vec::new();
        put_str(&mut body, 1, "ok");
        put_str(&mut body, 2, "truncated");
        body.truncate(body.len() - 3);

        assert_eq!(str_of(&body, 1), Some("ok"));
        assert_eq!(str_of(&body, 2), None);
    }
}
