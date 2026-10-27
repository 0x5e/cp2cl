//! The names behind the numbers a line carries.
//!
//! A line says what was on the wire the way the protocol names it: a service
//! type as `0x00018007(MSG_CMD_VIDEO_ENCODER_INIT)`, an enumerated field as
//! `3(music)`. A number with no name in the guide reads `UNKNOWN`, which says
//! the log has run past what this side knows rather than inventing a name for
//! it.
//!
//! The numbers and the names are CarLifeVehicleLib's: `CTranRecvPackageProcess.h`
//! for the service types and `CCarLifeLibWrapper.h` for the field enums, which
//! are what the guide's §10–13 describe. Only what this side sends, receives or
//! may be handed is listed; the rest of the protocol's tables — the Bluetooth
//! phone family, the microphone — are left out rather than guessed at.

use std::fmt;

/// A service type as a line writes it: the number and the protocol's name.
///
/// One value rather than two format arguments, so the number and the name
/// cannot drift apart at a call site.
pub struct ServiceType(pub u32);

impl fmt::Display for ServiceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#010x}({})", self.0, name(self.0))
    }
}

/// The protocol's name for a service type, `UNKNOWN` for one not listed above.
fn name(service_type: u32) -> &'static str {
    match service_type {
        // The command channel, car to phone.
        0x0001_8001 => "MSG_CMD_HU_PROTOCOL_VERSION",
        0x0001_8003 => "MSG_CMD_HU_INFO",
        0x0001_8005 => "MSG_CMD_HU_BT_PAIR_INFO",
        0x0001_8007 => "MSG_CMD_VIDEO_ENCODER_INIT",
        0x0001_8009 => "MSG_CMD_VIDEO_ENCODER_START",
        0x0001_800A => "MSG_CMD_VIDEO_ENCODER_PAUSE",
        0x0001_800B => "MSG_CMD_VIDEO_ENCODER_RESET",
        0x0001_800C => "MSG_CMD_VIDEO_ENCODER_FRAME_RATE_CHANGE",
        0x0001_800E => "MSG_CMD_PAUSE_MEDIA",
        0x0001_800F => "MSG_CMD_CAR_VELOCITY",
        0x0001_8010 => "MSG_CMD_CAR_GPS",
        0x0001_8011 => "MSG_CMD_CAR_GYROSCOPE",
        0x0001_8012 => "MSG_CMD_CAR_ACCELERATION",
        0x0001_8013 => "MSG_CMD_CAR_OIL",
        0x0001_801D => "MSG_CMD_LAUNCH_MODE_NORMAL",
        0x0001_801E => "MSG_CMD_LAUNCH_MODE_PHONE",
        0x0001_801F => "MSG_CMD_LAUNCH_MODE_MAP",
        0x0001_8020 => "MSG_CMD_LAUNCH_MODE_MUSIC",
        0x0001_8025 => "MSG_CMD_GO_TO_FOREGROUND",
        0x0001_8027 => "MSG_CMD_STATISTIC_INFO",
        0x0001_8028 => "MSG_CMD_MODULE_CONTROL",
        0x0001_8029 => "MSG_CMD_GEAR_INFO",
        0x0001_8032 => "MSG_CMD_CAR_DATA_SUBSCRIBE_DONE",
        0x0001_8043 => "MSG_CMD_CARLIFE_DATA_SUBSCRIBE",
        0x0001_8045 => "MSG_CMD_CARLIFE_DATA_SUBSCRIBE_START",
        0x0001_8046 => "MSG_CMD_CARLIFE_DATA_SUBSCRIBE_STOP",
        0x0001_8048 => "MSG_CMD_HU_AUTHEN_REQUEST",
        0x0001_804A => "MSG_CMD_HU_AUTHEN_RESULT",
        0x0001_8052 => "MSG_CMD_HU_FEATURE_CONFIG_RESPONSE",
        0x0001_8055 => "MSG_CMD_ERROR_CODE",
        0x0001_8056 => "MSG_CMD_VIDEO_ENCODER_JPEG",
        // The command channel, phone to car.
        0x0001_0002 => "MSG_CMD_PROTOCOL_VERSION_MATCH_STATUS",
        0x0001_0004 => "MSG_CMD_MD_INFO",
        0x0001_0008 => "MSG_CMD_VIDEO_ENCODER_INIT_DONE",
        0x0001_000D => "MSG_CMD_VIDEO_ENCODER_FRAME_RATE_CHANGE_DONE",
        0x0001_0018 => "MSG_CMD_SCREEN_ON",
        0x0001_0019 => "MSG_CMD_SCREEN_OFF",
        0x0001_001B => "MSG_CMD_FOREGROUND",
        0x0001_001C => "MSG_CMD_BACKGROUND",
        0x0001_0021 => "MSG_CMD_GO_TO_DESKTOP",
        0x0001_0026 => "MSG_CMD_MODULE_STATUS",
        0x0001_0030 => "MSG_CMD_NAVI_NEXT_TURN_INFO",
        0x0001_0031 => "MSG_CMD_CAR_DATA_SUBSCRIBE",
        0x0001_0033 => "MSG_CMD_CAR_DATA_SUBSCRIBE_START",
        0x0001_0035 => "MSG_CMD_MEDIA_INFO",
        0x0001_0036 => "MSG_CMD_MEDIA_PROGRESS_BAR",
        0x0001_0044 => "MSG_CMD_CARLIFE_DATA_SUBSCRIBE_DONE",
        0x0001_0049 => "MSG_CMD_MD_AUTHEN_RESPONSE",
        0x0001_004B => "MSG_CMD_MD_AUTHEN_RESULT",
        0x0001_004C => "MSG_CMD_GO_TO_FOREGROUND_RESPONSE",
        0x0001_0051 => "MSG_CMD_MD_FEATURE_CONFIG_REQUEST",
        // The video channel.
        0x0002_0001 => "MSG_VIDEO_DATA",
        0x0002_0002 => "MSG_VIDEO_HEARTBEAT",
        // The media channel.
        0x0003_0001 => "MSG_MEDIA_INIT",
        0x0003_0002 => "MSG_MEDIA_STOP",
        0x0003_0003 => "MSG_MEDIA_PAUSE",
        0x0003_0004 => "MSG_MEDIA_RESUME_PLAY",
        0x0003_0005 => "MSG_MEDIA_SEEK_TO",
        0x0003_0006 => "MSG_MEDIA_DATA",
        // The navigation prompt channel.
        0x0004_0001 => "MSG_NAVI_TTS_INIT",
        0x0004_0002 => "MSG_NAVI_TTS_END",
        0x0004_0003 => "MSG_NAVI_TTS_DATA",
        // The input channel.
        0x0006_8001 => "MSG_TOUCH_ACTION",
        0x0006_8002 => "MSG_TOUCH_ACTION_DOWN",
        0x0006_8003 => "MSG_TOUCH_ACTION_UP",
        0x0006_8004 => "MSG_TOUCH_ACTION_MOVE",
        0x0006_8005 => "MSG_TOUCH_SINGLE_CLICK",
        0x0006_8006 => "MSG_TOUCH_DOUBLE_CLICK",
        0x0006_8007 => "MSG_TOUCH_LONG_PRESS",
        0x0006_8008 => "MSG_TOUCH_CAR_HARD_KEY_CODE",
        _ => "UNKNOWN",
    }
}

/// The module a `moduleID` names, in the protocol's own vocabulary.
pub fn module(module: i32) -> &'static str {
    match module {
        1 => "phone",
        2 => "navigation",
        3 => "music",
        4 => "vr",
        5 => "connect",
        6 => "mic",
        _ => "UNKNOWN",
    }
}

/// The state a `statusID` names. The field reads against an enum of its own per
/// module — idle and running for the song, idle and in a call for the phone,
/// where the voice is taken from for the microphone — so the module is part of
/// the lookup.
pub fn module_state(module: i32, state: i32) -> &'static str {
    match (module, state) {
        (1, 0) => "idle",
        (1, 1) => "incoming",
        (1, 2) => "outing",
        (2, 0) => "idle",
        (2, 1) => "running",
        (3, 0) => "idle",
        (3, 1) => "running",
        (4, 0) => "idle",
        (4, 1) => "running",
        (4, 2) => "not_supported",
        (6, 0) => "vehicle_mic",
        (6, 1) => "mobile_mic",
        (6, 2) => "not_supported",
        _ => "UNKNOWN",
    }
}

/// The panel key a `keycode` names.
pub fn keycode(key: i32) -> &'static str {
    match key {
        0x01 => "KEYCODE_HOME",
        0x02 => "KEYCODE_PHONE_CALL",
        0x03 => "KEYCODE_PHONE_END",
        0x06 => "KEYCODE_SELECTOR_NEXT",
        0x07 => "KEYCODE_SELECTOR_PREVIOUS",
        0x0E => "KEYCODE_BACK",
        0x0F => "KEYCODE_SEEK_SUB",
        0x10 => "KEYCODE_SEEK_ADD",
        0x13 => "KEYCODE_MUTE",
        0x14 => "KEYCODE_OK",
        0x15 => "KEYCODE_MOVE_LEFT",
        0x16 => "KEYCODE_MOVE_RIGHT",
        0x17 => "KEYCODE_MOVE_UP",
        0x18 => "KEYCODE_MOVE_DOWN",
        0x1E => "KEYCODE_MAIN",
        0x1F => "KEYCODE_MEDIA_START",
        0x20 => "KEYCODE_MEDIA_STOP",
        0x21 => "KEYCODE_VR_START",
        _ => "UNKNOWN",
    }
}

/// The coordinate system a GPS module's `flag` names. The speed module's flag is
/// not one of these, and is only ever handed back as it was given.
pub fn coordinate_system(flag: i32) -> &'static str {
    match flag {
        1 => "WGS84",
        2 => "GCJ02",
        _ => "UNKNOWN",
    }
}

/// The CarLife data a `MSG_CMD_CARLIFE_DATA_SUBSCRIBE` names a module of.
pub fn carlife_data(module: i32) -> &'static str {
    match module {
        0 => "turn_by_turn",
        1 => "assistant_guide",
        _ => "UNKNOWN",
    }
}

/// The module of the car's own body data that a `moduleID` names.
pub fn car_data(module: i32) -> &'static str {
    match module {
        0 => "gps",
        1 => "velocity",
        2 => "gyroscope",
        3 => "acceleration",
        4 => "gear",
        5 => "oil",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        car_data, carlife_data, coordinate_system, keycode, module, module_state, ServiceType,
    };

    #[test]
    fn a_service_type_is_the_number_and_the_name_the_protocol_gives_it() {
        assert_eq!(
            ServiceType(0x0001_8007).to_string(),
            "0x00018007(MSG_CMD_VIDEO_ENCODER_INIT)"
        );
        assert_eq!(
            ServiceType(0x0003_0001).to_string(),
            "0x00030001(MSG_MEDIA_INIT)"
        );
    }

    #[test]
    fn a_number_with_no_name_is_said_to_be_unknown_rather_than_named() {
        assert_eq!(ServiceType(0x0001_9999).to_string(), "0x00019999(UNKNOWN)");
        // The direction bit is not a name: the same keyword in the other
        // direction is another message, and one this side does not have.
        assert_eq!(ServiceType(0x0001_0007).to_string(), "0x00010007(UNKNOWN)");
    }

    #[test]
    fn a_module_and_its_state_are_read_together() {
        assert_eq!(module(3), "music");
        assert_eq!(module(0xff), "UNKNOWN");
        assert_eq!(module_state(3, 1), "running");
        assert_eq!(module_state(2, 1), "running");
        assert_eq!(module_state(1, 1), "incoming");
        assert_eq!(module_state(6, 1), "mobile_mic");
        assert_eq!(module_state(4, 9), "UNKNOWN");
    }

    #[test]
    fn the_enumerated_fields_are_named_or_unknown() {
        assert_eq!(keycode(0x1E), "KEYCODE_MAIN");
        assert_eq!(keycode(0x99), "UNKNOWN");
        assert_eq!(coordinate_system(2), "GCJ02");
        assert_eq!(coordinate_system(0), "UNKNOWN");
        assert_eq!(carlife_data(0), "turn_by_turn");
        assert_eq!(carlife_data(7), "UNKNOWN");
        assert_eq!(car_data(1), "velocity");
        assert_eq!(car_data(9), "UNKNOWN");
    }
}
