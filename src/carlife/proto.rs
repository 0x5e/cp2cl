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

use crate::playing::Track;

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
    put_bytes(out, number, value.as_bytes());
}

/// A length-delimited field of bytes that are not text — which is the same
/// thing on the wire, and a different thing to read.
pub fn put_bytes(out: &mut Vec<u8>, number: u32, value: &[u8]) {
    tag(out, number, 2);
    put_varint(out, value.len() as u64);
    out.extend_from_slice(value);
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

/// `MSG_CMD_MD_AUTHEN_RESPONSE` — the answer to the car's authentication
/// challenge, which is `carlife::auth`'s business to produce.
pub fn authen_response(encrypt_value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    put_str(&mut out, 1, encrypt_value);
    out
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

/// `MSG_MEDIA_INIT` — the media channel's `CarlifeMusicInit`, which is what the
/// PCM that follows is. All three fields are required by the phone's own schema,
/// so all three are written.
///
/// `MSG_NAVI_TTS_INIT` carries `CarlifeTTSInit`, which is the same three fields
/// under another name: a rate, a channel count and a sample width, and the car
/// sets a player up from them the same way. One function writes both.
///
/// Nothing else about either channel is negotiated, and the car sets a player up
/// from these two numbers alone, so how it reads them is the whole of it.
/// CarLife-Android-Vehicle's `AudioTrackManagerSingle` reads each one exactly one
/// way: `channelConfig` is the channel count, with 1 mono and 2 stereo; and
/// `sampleFormat` is the width of a sample in bits, with 8 meaning 8-bit and
/// anything else meaning 16 — which is why the width is written down rather than
/// a code. Both readings are still unconfirmed against a real car.
pub fn media_init(rate: u32, channels: u16) -> Vec<u8> {
    let mut out = Vec::new();
    put_int(&mut out, 1, rate as i32);
    put_int(&mut out, 2, i32::from(channels));
    put_int(&mut out, 3, 16);
    out
}

/// `MSG_CMD_MEDIA_PROGRESS_BAR` — where the panel's bar belongs in the song.
///
/// `CarlifeMediaProgressBar` is the position in milliseconds and an optional
/// line of lyric. The lyric is left out: the phone this side bridges has none,
/// and a field that says nothing is better absent than empty.
///
/// Milliseconds is what the phone's own app writes for every car but a handful
/// of Hyundai and Kia channels, which it divides down to seconds
/// (`MusicInfoSender.isSecondVehi`). This side has no such list and writes the
/// unit the schema names.
pub fn media_progress_bar(elapsed_ms: i32) -> Vec<u8> {
    let mut out = Vec::new();
    put_int(&mut out, 1, elapsed_ms);
    out
}

/// The largest cover the car's media message may carry.
///
/// The song and its cover travel in one command-channel message, whose body the
/// car caps at 40 KB. The reference app keeps a cover under 30 KB, re-compressing
/// it until it fits and falling back to a built-in placeholder when it will not.
/// This layer does not re-compress: a cover is made to fit before it gets here
/// (`crate::artwork`), and one that still does not is left out rather than sent
/// as a message the car cannot take.
pub const MAX_ART: usize = 30 * 1024;

/// `MSG_CMD_MEDIA_INFO` — what the car's panel is told about the song that is
/// playing. The guide calls this one "当前歌曲播放信息" and says the refresh is
/// "开始播放或者切换歌曲时" (guide §13.2).
///
/// `CarlifeMediaInfo` is nine fields and the guide calls every one of them
/// required. What goes out is what has a value: the four strings when they are
/// not empty, `albumArt` when there is a cover, `duration` when the phone has
/// said it. The other three — `playlistNum`, `songId` and `mode` — never,
/// because the phone has nothing behind them: see `crate::playing` for which
/// subscription is missing and why a zero would be a claim rather than a
/// silence. A field left out carries its default, which is the same value an
/// empty one would carry, and the car's parser hands its callbacks those
/// defaults for whatever the body does not say. `albumArt` is written only when
/// there is a picture: an empty one is not a picture, and a field that says
/// nothing is better left out than sent with a length of zero.
///
/// `duration` is in seconds, which is the unit the guide gives it, and
/// `albumArt` is the cover as the phone transferred it — a file, not a bitmap
/// this side drew.
pub fn media_info(track: &Track, album_art: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    put_str(&mut out, 1, track.source.as_deref().unwrap_or_default());
    put_str(&mut out, 2, track.song.as_deref().unwrap_or_default());
    put_str(&mut out, 3, track.artist.as_deref().unwrap_or_default());
    put_str(&mut out, 4, track.album.as_deref().unwrap_or_default());
    if let Some(art) = album_art {
        put_bytes(&mut out, 5, art);
    }
    if let Some(seconds) = track.duration_s {
        put_int(&mut out, 6, seconds);
    }
    out
}

/// `MSG_CMD_MODULE_STATUS` — the state of this device's own modules, which the
/// car hears when it changes and not per message.
///
/// The body is `CarlifeModuleStatusList`: a count and that many
/// `CarlifeModuleStatus`, which is two required ints — which module, and what
/// state it is in. A change of one module's state is a list of one entry; what a
/// session opens with is the whole list at rest. The guide's `ModuleId` numbers
/// the phone's modules and `NaviState` the states navigation is in (guide §10.2,
/// table in the command-channel list).
pub fn module_status(module_id: i32, status_id: i32) -> Vec<u8> {
    module_statuses(&[(module_id, status_id)])
}

/// The same list, with one entry per module.
pub fn module_statuses(states: &[(i32, i32)]) -> Vec<u8> {
    let mut out = Vec::new();
    put_int(&mut out, 1, states.len() as i32);
    for (module_id, status_id) in states {
        let mut entry = Vec::new();
        put_int(&mut entry, 1, *module_id);
        put_int(&mut entry, 2, *status_id);
        put_bytes(&mut out, 2, &entry);
    }
    out
}

/// `CARLIFE_PHONE_MODULE_ID`: the module a call rings on. Nothing here answers
/// one, so its state is only ever the idle one — see `modules_at_rest`.
pub const PHONE_MODULE: i32 = 1;

/// `CARLIFE_NAVI_MODULE_ID`: the navigation module, the one module this device
/// has a state of its own for.
pub const NAVI_MODULE: i32 = 2;

/// `CARLIFE_MUSIC_MODULE_ID`: the module playing the song, whose state follows
/// whether the song is moving.
pub const MUSIC_MODULE: i32 = 3;

/// `CARLIFE_VR_MODULE_ID`: the assistant's module, idle here for the same reason
/// the phone's is — see `modules_at_rest`.
pub const VR_MODULE: i32 = 4;

/// `MusicState`. `IDLE` is a song that is not moving through its own time and
/// `RUNNING` one that is — the same distinction the car's own pause and play
/// messages carry.
pub const MUSIC_IDLE: i32 = 0;
pub const MUSIC_RUNNING: i32 = 1;

/// `NaviState`. `IDLE` is a navigation that is not running and `START` one that
/// is. The third, `STOP`, is a navigation the phone has ended — which the phone
/// this side bridges has no way to say, so it is never sent.
pub const NAVI_IDLE: i32 = 0;
pub const NAVI_START: i32 = 1;

/// `PhoneState.IDLE` and `VRState.IDLE`: the state the two modules above are in
/// whenever this side has anything to say about them.
pub const MODULE_IDLE: i32 = 0;

/// The whole of what this device has to say about its own modules when a session
/// opens: the phone, the navigation, the song and the assistant, all four idle,
/// in one message rather than four.
///
/// The microphone is not in the list. Its state says where the voice is taken
/// from, and nothing here has a microphone of its own to report one for.
pub fn modules_at_rest() -> Vec<u8> {
    module_statuses(&[
        (PHONE_MODULE, MODULE_IDLE),
        (NAVI_MODULE, NAVI_IDLE),
        (MUSIC_MODULE, MUSIC_IDLE),
        (VR_MODULE, MODULE_IDLE),
    ])
}

/// `ActionType`: what a turn message does to the car's strip.
pub const ACTION_SHOW: i32 = 1;
pub const ACTION_UPDATE: i32 = 2;
pub const ACTION_HIDE: i32 = 3;

/// What the car's turn-by-turn strip is to show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NextTurn {
    /// The car's own `TurnKind`, once `crate::navigation::turn_kind` has put
    /// the phone's `EManeuverType` into the car's vocabulary.
    pub kind: Option<i32>,
    /// The road the turn leads onto.
    pub road: Option<String>,
    /// Metres to the turn.
    pub remain_m: Option<i32>,
}

/// `MSG_CMD_NAVI_NEXT_TURN_INFO` — the turn the car's strip is to show, or the
/// word to take it down. The guide calls this one the simple guidance and has
/// it refreshed whenever the next turn changes (guide §13.1).
///
/// `CarlifeNaviNextTurnInfo` is six fields and the guide calls every one of them
/// required. Three are written here. `totalDistance` (field 4) and `time`
/// (field 6) are not, because the phone has nothing behind them, and `naviTurn`
/// (field 2) is only written when the two vocabularies have a word in common —
/// see `crate::navigation` for both. The car's own parser reads a body that
/// stops early and hands its callbacks the defaults, so an absent field is the
/// same empty value, said honestly.
pub fn navi_next_turn_info(action: i32, turn: &NextTurn) -> Vec<u8> {
    let mut out = Vec::new();
    put_int(&mut out, 1, action);
    if let Some(kind) = turn.kind {
        put_int(&mut out, 2, kind);
    }
    if let Some(road) = turn.road.as_deref() {
        put_str(&mut out, 3, road);
    }
    if let Some(metres) = turn.remain_m {
        put_int(&mut out, 5, metres);
    }
    out
}

/// `MSG_CMD_CARLIFE_DATA_SUBSCRIBE_DONE` — this device's answer to the car's
/// question about the data it can subscribe to.
///
/// `CarlifeSubscribeMobileCarLifeInfoList` is a count and that many
/// `{moduleID, supportFlag}` entries, and the answer carries one entry per
/// module the car asked about. `frequency` is left out: the car sets the rate
/// for the data it subscribes to, and nothing here produces a rate of its own.
pub fn data_subscribe_done(modules: &[(i32, bool)]) -> Vec<u8> {
    let mut out = Vec::new();
    put_int(&mut out, 1, modules.len() as i32);
    for (module, supported) in modules {
        let mut entry = Vec::new();
        put_int(&mut entry, 1, *module);
        put_bool(&mut entry, 2, *supported);
        put_bytes(&mut out, 2, &entry);
    }
    out
}

/// `MSG_CMD_CAR_DATA_SUBSCRIBE_START` — this device asking the car to start
/// reporting one module of its own data.
///
/// The body is a bare `CarlifeVehicleInfo` and not the list the same family's
/// answer carries: that is what the phone sends, and the head unit's own parser
/// reads it as one entry (`CarLife-Android-Vehicle`'s `CarlifeActivity`).
/// `frequency` is 0 for the same reason — it is what the phone sends, and the
/// reference head unit reads it as "on every change".
pub fn car_data_subscribe_start(module: i32, flag: i32, frequency: i32) -> Vec<u8> {
    let mut out = Vec::new();
    put_int(&mut out, 1, module);
    put_int(&mut out, 2, flag);
    put_int(&mut out, 3, frequency);
    out
}

/// `ModuleID.CAR_DATA_GPS`: the car's own position, which is the one module this
/// bridge does anything with. The other five — speed, gyroscope, acceleration,
/// gear, oil — are read by the phone's own navigation and are left alone.
pub const CAR_DATA_GPS: i32 = 0;

/// `ModuleID.CAR_DATA_VELOCITY`: the same answer's second module. It is asked for
/// as well, because the phone asks: this car reports it unasked, and one that
/// reports only what it was asked for would otherwise never send a speed.
pub const CAR_DATA_VELOCITY: i32 = 1;

// --------------------------------------------------------- what comes down ---

/// `MSG_CMD_HU_INFO`: what the car says about itself.
///
/// The same `CarlifeDeviceInfo` we send in `MSG_CMD_MD_INFO`, filled in on the
/// head unit's side. The field numbers are not the same set: the car's `os` is
/// 1 like ours, but its make is `manufacturer` at 13 where ours is `brand` at
/// 4, and its Bluetooth pair is at 23 and 24. Nothing here acts on any of it —
/// it is reported, and that is all.
#[derive(Debug, Default, Clone)]
pub struct HuInfo<'a> {
    pub os: &'a str,
    pub manufacturer: &'a str,
    pub model: &'a str,
    pub release: &'a str,
    pub bt_address: &'a str,
    pub bt_name: &'a str,
}

pub fn decode_hu_info(body: &[u8]) -> HuInfo<'_> {
    HuInfo {
        os: str_of(body, 1).unwrap_or_default(),
        manufacturer: str_of(body, 13).unwrap_or_default(),
        model: str_of(body, 14).unwrap_or_default(),
        release: str_of(body, 19).unwrap_or_default(),
        bt_address: str_of(body, 23).unwrap_or_default(),
        bt_name: str_of(body, 24).unwrap_or_default(),
    }
}

/// The car's `MSG_CMD_MODULE_CONTROL`: which of this device's modules it wants
/// changed, and to what state. The two fields are the same pair
/// `MSG_CMD_MODULE_STATUS` reports in, read as a request rather than as news.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModuleControl {
    pub module: i32,
    pub state: i32,
}

pub fn decode_module_control(body: &[u8]) -> ModuleControl {
    ModuleControl {
        module: int_of(body, 1).unwrap_or(0) as i32,
        state: int_of(body, 2).unwrap_or(0) as i32,
    }
}

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
/// here acts on it beyond reporting it and answering the car with a verdict on
/// the channel it carries — see `carlife::auth`.
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

/// One entry in the car's answer to `MSG_CMD_CAR_DATA_SUBSCRIBE`: a module it can
/// report, the system its coordinates are in, and the rate it means to report at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VehicleInfo {
    /// `ModuleID` — 0 GPS, 1 vehicle speed, 2 gyroscope, 3 acceleration, 4 gear,
    /// 5 oil.
    pub module: i32,
    /// For GPS this is the coordinate system: 1 is WGS84 and 2 is GCJ-02. The
    /// phone treats a GPS module with any other value as one the car cannot
    /// actually report, and so does this device.
    pub flag: i32,
    /// Hertz, or 0 for "on change".
    pub frequency: i32,
}

/// `MSG_CMD_CAR_DATA_SUBSCRIBE_DONE`: what the car has to report. Its body is a
/// count and that many `CarlifeVehicleInfo` entries, which is the same shape the
/// question arrived in — see `decode_data_subscribe`, which reads the other two
/// messages of the family.
pub fn decode_vehicle_infos(body: &[u8]) -> Vec<VehicleInfo> {
    let mut out = Vec::new();
    let mut fields = Fields::new(body);
    while let Some(field) = fields.next_field() {
        let Field::Bytes { number: 2, value } = field else {
            continue;
        };
        out.push(VehicleInfo {
            module: int_of(value, 1).unwrap_or(0) as i32,
            flag: int_of(value, 2).unwrap_or(0) as i32,
            frequency: int_of(value, 3).unwrap_or(0) as i32,
        });
    }
    out
}

/// `MSG_CMD_CAR_VELOCITY`: the car's own speed, which it reports on a clock of its
/// own — once a second on the car in the log this was written against, without
/// being asked and without anything being subscribed to.
///
/// `speed` is kilometres an hour. The schema names no unit — the car's library
/// carries it as a bare `u32` — so that is the one the page and the log read it
/// in, and nothing is converted on the way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CarSpeed {
    pub speed: i32,
    /// Milliseconds since the epoch, and optional in the schema: 0 is a car that
    /// did not stamp one.
    pub timestamp_ms: u64,
}

pub fn decode_car_speed(body: &[u8]) -> CarSpeed {
    CarSpeed {
        speed: int_of(body, 1).unwrap_or(0) as i32,
        timestamp_ms: int_of(body, 2).unwrap_or(0) as u64,
    }
}

/// `MSG_CMD_CAR_GPS`: the car's position, in the units the message is defined in.
///
/// Every field is `required` in the car's own schema, so a body that stops early
/// is a body holding zeroes, and that is how it is read. The units are the
/// guide's: coordinates in a millionth of a degree, height in a tenth of a metre,
/// speed in a hundredth of a kilometre an hour, and heading in a tenth of a
/// degree. Nothing here is converted; the page turns them into degrees and
/// metres, and so would anything that hands the fix on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CarGps {
    /// 1 while the receiver has a fix. A car with none sends `-1`, which is the
    /// unsigned field's `0xFFFF_FFFF`.
    pub antenna_state: i32,
    pub latitude: i32,
    pub longitude: i32,
    pub height: i32,
    pub speed: u32,
    pub heading: u32,
    /// Metres, and the two the receiver is least likely to fill.
    pub hor_pos_error: u32,
    pub vert_pos_error: u32,
    /// The date and time of the fix, in UTC — a GPS receiver reports UTC, and
    /// nothing in the message names a zone.
    pub year: i32,
    pub month: i32,
    pub day: i32,
    pub hour: i32,
    pub minute: i32,
    pub second: i32,
    /// The same instant in milliseconds since the epoch, which is the field the
    /// phone's own consumer reads.
    pub timestamp_ms: u64,
}

pub fn decode_car_gps(body: &[u8]) -> CarGps {
    let int = |number: u32| int_of(body, number).unwrap_or(0) as i32;
    let uint = |number: u32| int_of(body, number).unwrap_or(0) as u32;
    CarGps {
        antenna_state: int(1),
        latitude: int(3),
        longitude: int(4),
        height: int(5),
        speed: uint(6),
        heading: uint(7),
        year: int(8),
        month: int(9),
        day: int(10),
        hour: int(11),
        minute: int(12),
        second: int(13),
        hor_pos_error: uint(20),
        vert_pos_error: uint(21),
        timestamp_ms: int_of(body, 25).unwrap_or(0) as u64,
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

/// `MSG_CMD_CARLIFE_DATA_SUBSCRIBE`: the module IDs the car is asking about, in
/// the order it asked. Its body is a count and that many `{moduleID}` entries.
///
/// The three messages that carry this list — the question, the start and the
/// stop — are the same body, so one reader serves all three.
pub fn decode_data_subscribe(body: &[u8]) -> Vec<i32> {
    let mut out = Vec::new();
    let mut fields = Fields::new(body);
    while let Some(field) = fields.next_field() {
        let Field::Bytes { number: 2, value } = field else {
            continue;
        };
        if let Some(module) = int_of(value, 1) {
            out.push(module as i32);
        }
    }
    out
}

/// `CARLIFE_DATA_TURNBYTURN`: the turn-by-turn strip, which is the one of the
/// two subscribed kinds of data this device produces. The other is the assistant
/// guide — speed cameras and the like — which has nothing behind it on the
/// CarPlay side; see `crate::navigation`.
pub const DATA_TURN_BY_TURN: i32 = 0;

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
    fn the_media_channel_is_announced_as_the_pcm_it_is() {
        let body = media_init(44_100, 2);
        assert_eq!(int_of(&body, 1), Some(44_100));
        assert_eq!(int_of(&body, 2), Some(2));
        assert_eq!(int_of(&body, 3), Some(16));
        // Field by field, and nothing else: a body the car's parser cannot read
        // makes the message disappear rather than fail.
        assert_eq!(body, vec![0x08, 0xc4, 0xd8, 0x02, 0x10, 0x02, 0x18, 0x10]);
    }

    /// The bar's body is one required field: the position in milliseconds.
    #[test]
    fn the_progress_bar_body_is_the_position_and_nothing_else() {
        let body = media_progress_bar(43_500);
        assert_eq!(int_of(&body, 1), Some(43_500));
        // Field 1, varint: 43 500 is 0x0000A9EC, in three varint bytes.
        assert_eq!(body, vec![0x08, 0xec, 0xd3, 0x02]);
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
    fn the_cars_own_info_is_read_at_the_field_numbers_the_car_uses() {
        // The head unit's half of `CarlifeDeviceInfo`: `manufacturer` at 13
        // rather than our `brand` at 4, which is the one pair that would read
        // as an empty string if the two halves were taken for the same set.
        let mut body = Vec::new();
        put_str(&mut body, 1, "Linux");
        put_str(&mut body, 13, "Bosch");
        put_str(&mut body, 14, "HU-1");
        put_str(&mut body, 19, "1.2.3");
        put_str(&mut body, 23, "AA:BB:CC:DD:EE:FF");
        put_str(&mut body, 24, "HU-BT");

        let info = decode_hu_info(&body);
        assert_eq!(info.os, "Linux");
        assert_eq!(info.manufacturer, "Bosch");
        assert_eq!(info.model, "HU-1");
        assert_eq!(info.release, "1.2.3");
        assert_eq!(info.bt_address, "AA:BB:CC:DD:EE:FF");
        assert_eq!(info.bt_name, "HU-BT");
    }

    #[test]
    fn a_car_that_leaves_its_info_out_reads_as_empty_strings() {
        let info = decode_hu_info(&[]);
        assert_eq!(info.os, "");
        assert_eq!(info.manufacturer, "");
        assert_eq!(info.model, "");
    }

    #[test]
    fn the_cars_module_control_is_the_pair_a_status_reports() {
        // The one message, not the list: the control names a single module and
        // the state it wants, where a status report carries a count and entries.
        let mut body = Vec::new();
        put_int(&mut body, 1, MUSIC_MODULE);
        put_int(&mut body, 2, MUSIC_IDLE);

        assert_eq!(
            decode_module_control(&body),
            ModuleControl {
                module: MUSIC_MODULE,
                state: MUSIC_IDLE,
            }
        );
        // A body that names neither is read as zeros rather than refused: the
        // caller is the one that knows what an unknown pair means.
        assert_eq!(decode_module_control(&[]), ModuleControl::default());
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
    fn the_authentication_answer_goes_out_as_the_string_field_it_is() {
        let body = authen_response("QUJD");
        assert_eq!(body, vec![0x0a, 0x04, b'Q', b'U', b'J', b'D']);
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

    /// The car's answer to the data question is a count and that many entries,
    /// and each entry is the module, its coordinate system and its rate.
    #[test]
    fn the_cars_data_answer_is_read_entry_by_entry() {
        let mut body = Vec::new();
        put_int(&mut body, 1, 2);
        for (module, flag, frequency) in [(CAR_DATA_GPS, 1, 0), (1, 0, 10)] {
            let mut entry = Vec::new();
            put_int(&mut entry, 1, module);
            put_int(&mut entry, 2, flag);
            put_int(&mut entry, 3, frequency);
            put_bytes(&mut body, 2, &entry);
        }

        assert_eq!(
            decode_vehicle_infos(&body),
            vec![
                VehicleInfo {
                    module: 0,
                    flag: 1,
                    frequency: 0
                },
                VehicleInfo {
                    module: 1,
                    flag: 0,
                    frequency: 10
                },
            ]
        );
        // A car that reports nothing is an empty answer, not a missing one.
        assert!(decode_vehicle_infos(&[]).is_empty());
    }

    /// The subscribe message is one bare entry, and the head unit reads it as
    /// one — a list here would be read as a count with no entries behind it.
    #[test]
    fn the_subscription_is_a_single_entry_and_not_a_list() {
        assert_eq!(
            car_data_subscribe_start(CAR_DATA_GPS, 1, 0),
            vec![0x08, 0x00, 0x10, 0x01, 0x18, 0x00]
        );
    }

    /// Every field the car fills is read in the unit the guide states, including
    /// the negative coordinate and the `-1` that stands for "no fix".
    #[test]
    fn a_position_is_read_in_the_units_the_message_is_defined_in() {
        let mut body = Vec::new();
        // `antennaState` is a `uint32`, so "no fix" is its `0xFFFF_FFFF` and not
        // a signed -1.
        tag(&mut body, 1, 0);
        put_varint(&mut body, 0xFFFF_FFFF);
        put_int(&mut body, 3, 30_317_880);
        put_int(&mut body, 4, -120_128_094);
        put_int(&mut body, 5, 456);
        put_int(&mut body, 6, 1234);
        put_int(&mut body, 7, 900);
        put_int(&mut body, 8, 2026);
        put_int(&mut body, 9, 10);
        put_int(&mut body, 10, 20);
        put_int(&mut body, 11, 14);
        put_int(&mut body, 12, 33);
        put_int(&mut body, 13, 57);
        put_int(&mut body, 20, 35);
        put_int(&mut body, 21, 20);
        // `timeStamp` is a `uint64`, which is wider than anything else here.
        tag(&mut body, 25, 0);
        put_varint(&mut body, 1_761_065_637_000);

        assert_eq!(
            decode_car_gps(&body),
            CarGps {
                antenna_state: -1,
                latitude: 30_317_880,
                longitude: -120_128_094,
                height: 456,
                speed: 1234,
                heading: 900,
                hor_pos_error: 35,
                vert_pos_error: 20,
                year: 2026,
                month: 10,
                day: 20,
                hour: 14,
                minute: 33,
                second: 57,
                timestamp_ms: 1_761_065_637_000,
            }
        );

        // A body that stops early is a car that left the fields out, and the
        // schema's defaults are what it says.
        assert_eq!(decode_car_gps(&[]), CarGps::default());
    }

    /// The speed is a number and the moment it was measured at, and the moment is
    /// optional: a car that leaves it out reads as one that did not stamp it.
    #[test]
    fn the_speed_is_read_as_the_number_and_the_moment_beside_it() {
        let mut body = Vec::new();
        put_int(&mut body, 1, 42);
        tag(&mut body, 2, 0);
        put_varint(&mut body, 1_761_065_637_000);
        assert_eq!(
            decode_car_speed(&body),
            CarSpeed {
                speed: 42,
                timestamp_ms: 1_761_065_637_000,
            }
        );

        assert_eq!(
            decode_car_speed(&[0x08, 0x00]),
            CarSpeed {
                speed: 0,
                timestamp_ms: 0
            }
        );
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

    #[test]
    fn the_panel_is_told_the_song_field_by_field() {
        let track = Track {
            source: Some("Music".into()),
            song: Some("Reeperbahn".into()),
            artist: Some("Hamburg".into()),
            album: Some("Stadt".into()),
            duration_s: Some(239),
        };
        let body = media_info(&track, Some(&[0xff, 0xd8, 0xff]));

        assert_eq!(str_of(&body, 1), Some("Music"));
        assert_eq!(str_of(&body, 2), Some("Reeperbahn"));
        assert_eq!(str_of(&body, 3), Some("Hamburg"));
        assert_eq!(str_of(&body, 4), Some("Stadt"));
        assert_eq!(bytes_of(&body, 5), Some(&[0xffu8, 0xd8, 0xff][..]));
        assert_eq!(int_of(&body, 6), Some(239));

        // The three the phone has nothing to say about are absent rather than
        // zero: a `mode` of 0 would be the panel told single-track repeat.
        assert_eq!(int_of(&body, 7), None);
        assert_eq!(str_of(&body, 8), None);
        assert_eq!(int_of(&body, 9), None);
    }

    /// Nothing is written for a field there is nothing behind, so a song and
    /// nothing else is a body of one field — and no cover at all is no cover
    /// field, not an empty one.
    #[test]
    fn a_song_with_nothing_said_about_it_carries_only_its_name() {
        let body = media_info(
            &Track {
                song: Some("Only".into()),
                ..Track::default()
            },
            None,
        );
        assert_eq!(body, vec![0x12, 0x04, b'O', b'n', b'l', b'y']);
    }

    /// A cover is a file the phone sent, so it goes out byte for byte however
    /// long it is — the length is a varint like every other.
    #[test]
    fn the_cover_goes_out_as_the_bytes_the_phone_sent() {
        let art = vec![0xab; 200];
        let body = media_info(
            &Track {
                song: Some("Song".into()),
                ..Track::default()
            },
            Some(&art),
        );
        assert_eq!(bytes_of(&body, 5), Some(&art[..]));
        // The song's field, then the cover's: a tag of its own and a length
        // that no longer fits in one byte's worth of value.
        assert_eq!(body[6], 0x2a);
        assert_eq!(body[7], 0xc8);
    }

    #[test]
    fn the_strip_is_told_the_turn_field_by_field() {
        let turn = NextTurn {
            kind: Some(0x07),
            road: Some("Elbchaussee".into()),
            remain_m: Some(350),
        };
        let body = navi_next_turn_info(ACTION_SHOW, &turn);

        assert_eq!(int_of(&body, 1), Some(1));
        assert_eq!(int_of(&body, 2), Some(0x07));
        assert_eq!(str_of(&body, 3), Some("Elbchaussee"));
        assert_eq!(int_of(&body, 5), Some(350));

        // The two the phone has nothing behind are absent rather than zero, and
        // so is a kind the two vocabularies have no word in common for.
        assert_eq!(int_of(&body, 4), None);
        assert_eq!(int_of(&body, 6), None);
        let wordless = navi_next_turn_info(
            ACTION_UPDATE,
            &NextTurn {
                kind: None,
                ..turn.clone()
            },
        );
        assert_eq!(int_of(&wordless, 2), None);
        assert_eq!(str_of(&wordless, 3), Some("Elbchaussee"));
    }

    /// Taking the strip down is the one field that says so: there is no turn to
    /// describe and nothing about the last one is still true.
    #[test]
    fn taking_the_strip_down_is_the_action_on_its_own() {
        let body = navi_next_turn_info(ACTION_HIDE, &NextTurn::default());
        assert_eq!(body, vec![0x08, 0x03]);
        assert_eq!(int_of(&body, 1), Some(3));
    }

    /// The car reads this one as a *list* of module statuses, so a bare pair of
    /// ints would be read as a count and nothing else — the message would arrive
    /// and mean nothing.
    #[test]
    fn the_module_state_goes_out_as_the_list_the_car_reads_it_as() {
        let body = module_status(NAVI_MODULE, NAVI_START);

        let mut fields = Fields::new(&body);
        assert_eq!(
            fields.next_field(),
            Some(Field::Varint {
                number: 1,
                value: 1
            }),
            "a count of one entry"
        );
        let Some(Field::Bytes { number: 2, value }) = fields.next_field() else {
            panic!("the entry is a nested message, not a scalar");
        };
        assert_eq!(int_of(value, 1), Some(i64::from(NAVI_MODULE)));
        assert_eq!(int_of(value, 2), Some(i64::from(NAVI_START)));
        assert_eq!(fields.next_field(), None, "and nothing after it");
    }

    /// The list a session opens with is one message holding the four modules
    /// this device has, each idle, and no microphone among them.
    #[test]
    fn the_modules_at_rest_are_the_four_this_device_has() {
        let body = modules_at_rest();

        let mut fields = Fields::new(&body);
        assert_eq!(
            fields.next_field(),
            Some(Field::Varint {
                number: 1,
                value: 4
            }),
            "four entries"
        );
        let mut listed = Vec::new();
        while let Some(Field::Bytes { number: 2, value }) = fields.next_field() {
            listed.push((
                int_of(value, 1).expect("a module") as i32,
                int_of(value, 2).expect("a state") as i32,
            ));
        }
        assert_eq!(
            listed,
            vec![
                (PHONE_MODULE, MODULE_IDLE),
                (NAVI_MODULE, NAVI_IDLE),
                (MUSIC_MODULE, MUSIC_IDLE),
                (VR_MODULE, MODULE_IDLE),
            ]
        );
    }

    /// The car's question about the subscribed data and this side's answer are
    /// the same list read the same way, from opposite directions.
    #[test]
    fn the_subscribed_data_is_the_list_of_modules_both_ways() {
        let mut asked = Vec::new();
        put_int(&mut asked, 1, 2);
        for module in [0, 1] {
            let mut entry = Vec::new();
            put_int(&mut entry, 1, module);
            put_bytes(&mut asked, 2, &entry);
        }
        assert_eq!(decode_data_subscribe(&asked), vec![0, 1]);

        let answer = data_subscribe_done(&[(0, true), (1, false)]);
        assert_eq!(decode_data_subscribe(&answer), vec![0, 1]);
        // The count is what the car reads the entries with, so it has to be
        // there and has to be right.
        assert_eq!(int_of(&answer, 1), Some(2));
        assert_eq!(int_of(&answer, 2), None, "the entries are not ints");
    }
}
