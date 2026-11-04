//! The names behind the numbers an iAP2 line carries.
//!
//! A control-session message on the cable is a two-byte ID followed by
//! parameters, each a two-byte ID and a value. A line says them the way the
//! protocol names them: `msg=0x1d01(IdentificationInformation)` and
//! `name="..."`, rather than the bytes they came from. Numbers with no name
//! here read `UNKNOWN` or `p<id>`, which says the log has run past what this
//! side knows rather than inventing a name for it.
//!
//! The table is generated from LIVI's `iap2-csm` message definitions, which
//! are the protocol's own names and field types. It is a copy, not a
//! dependency on the macro's internals: the decoder only needs names, kinds
//! and the nested groups, none of which LIVI exposes at run time.

use std::fmt;

/// A group a parameter's value can be made of. Its parameters are another
/// table, keyed by the group rather than by the message they arrived in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    BluetoothTransportComponent,
    CarPlayAvailabilityWiredAttributes,
    CarPlayAvailabilityWirelessAttributes,
    CarPlayStartSessionAsset,
    CarPlayStartSessionWiredAttributes,
    CarPlayStartSessionWirelessAttributes,
    ExternalAccessoryProtocol,
    LocationInformationComponent,
    MediaItemAttributes,
    PlaybackAttributes,
    RouteGuidanceDisplayComponent,
    SerialTransportComponent,
    StartMediaItemAttributes,
    StartPlaybackAttributes,
    USBDeviceTransportComponent,
    USBHostTransportComponent,
    VehicleInformationComponent,
    VehicleStatusComponent,
    WirelessCarPlayTransportComponent,
}

/// How a parameter's value is to be read. `Auto` is for a parameter that is
/// not in the table, where the shape of the bytes is all there is to go on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Auto,
    /// An empty value: the parameter being there is the whole message.
    Flag,
    Bool,
    /// A big-endian integer: width in bytes, then whether it is signed.
    Num(u8, bool),
    /// NUL-terminated text; a list of strings is one value of these.
    Str,
    /// Opaque bytes: only their count is worth a line.
    Bytes,
    /// One byte, named by the enum it belongs to; a list is several.
    Enum(&'static str),
    /// The value is itself a list of parameters, from this group's table.
    Group(Group),
}

/// A parameter's name and the way its value is read. An empty name means the
/// ID is all that is known, and the line falls back to `p<id>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Param {
    pub name: &'static str,
    pub kind: Kind,
}

const UNKNOWN: Param = Param { name: "", kind: Kind::Auto };

/// A message ID as a line writes it: the number and the protocol's name.
pub struct Message(pub u16);

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#06x}({})", self.0, message(self.0))
    }
}

/// The protocol's name for a control-session message ID.
pub fn message(id: u16) -> &'static str {
    match id {
        0x1d00 => "StartIdentification",
        0x1d01 => "IdentificationInformation",
        0x1d02 => "IdentificationAccepted",
        0x1d03 => "IdentificationRejected",
        0x4154 => "StartCallStateUpdates",
        0x4155 => "CallStateUpdate",
        0x4156 => "StopCallStateUpdates",
        0x4157 => "StartCommunicationsUpdates",
        0x4158 => "CommunicationsUpdate",
        0x4159 => "StopCommunicationsUpdates",
        0x4300 => "CarPlayAvailability",
        0x4301 => "CarPlayStartSession",
        0x4e0b => "DeviceTimeUpdate",
        0x4e0d => "WirelessCarPlayUpdate",
        0x4e0e => "DeviceTransportIdentifierNotification",
        0x5000 => "StartNowPlayingUpdates",
        0x5001 => "NowPlayingUpdate",
        0x5002 => "StopNowPlayingUpdates",
        0x5003 => "SetNowPlayingInformation",
        0x5200 => "StartRouteGuidanceUpdates",
        0x5201 => "RouteGuidanceUpdate",
        0x5202 => "RouteGuidanceManeuverUpdate",
        0x5203 => "StopRouteGuidanceUpdates",
        0x5700 => "RequestWiFiInformation",
        0x5701 => "WiFiInformation",
        0x5702 => "RequestAccessoryWiFiConfigurationInformation",
        0x5703 => "AccessoryWiFiConfigurationInformation",
        0xa100 => "StartVehicleStatusUpdates",
        0xa101 => "VehicleStatusUpdate",
        0xa102 => "StopVehicleStatusUpdates",
        0xaa00 => "RequestAuthenticationCertificate",
        0xaa01 => "AuthenticationCertificate",
        0xaa02 => "RequestAuthenticationChallengeResponse",
        0xaa03 => "AuthenticationResponse",
        0xaa04 => "AuthenticationFailed",
        0xaa05 => "AuthenticationSucceeded",
        0xae00 => "StartPowerUpdates",
        0xae01 => "PowerUpdate",
        0xae02 => "StopPowerUpdates",
        0xae03 => "PowerSourceUpdate",
        0xea00 => "StartExternalAccessoryProtocolSession",
        0xea01 => "StopExternalAccessoryProtocolSession",
        0xea03 => "StatusExternalAccessoryProtocolSession",
        0xfffa => "StartLocationInformation",
        0xfffb => "LocationInformation",
        0xfffc => "StopLocationInformation",
        _ => "UNKNOWN",
    }
}

/// The name and kind of a parameter belonging to a message.
pub fn param(msg: u16, id: u16) -> Param {
    match (msg, id) {
        (0x1d01, 0) => Param { name: "name", kind: Kind::Str },
        (0x1d01, 1) => Param { name: "model_identifier", kind: Kind::Str },
        (0x1d01, 2) => Param { name: "manufacturer", kind: Kind::Str },
        (0x1d01, 3) => Param { name: "serial_number", kind: Kind::Str },
        (0x1d01, 4) => Param { name: "firmware_version", kind: Kind::Str },
        (0x1d01, 5) => Param { name: "hardware_version", kind: Kind::Str },
        (0x1d01, 6) => Param { name: "messages_sent_by_accessory", kind: Kind::Bytes },
        (0x1d01, 7) => Param { name: "messages_received_from_accessory", kind: Kind::Bytes },
        (0x1d01, 8) => Param { name: "power_providing_capability", kind: Kind::Enum("PowerProvidingCapability") },
        (0x1d01, 9) => Param { name: "maximum_current_drawn_from_device", kind: Kind::Num(2, false) },
        (0x1d01, 10) => Param { name: "supported_external_accessory_protocol", kind: Kind::Group(Group::ExternalAccessoryProtocol) },
        (0x1d01, 11) => Param { name: "app_match_team_id", kind: Kind::Str },
        (0x1d01, 12) => Param { name: "current_language", kind: Kind::Str },
        (0x1d01, 13) => Param { name: "supported_language", kind: Kind::Str },
        (0x1d01, 14) => Param { name: "serial_transport_component", kind: Kind::Group(Group::SerialTransportComponent) },
        (0x1d01, 15) => Param { name: "usb_device_transport_component", kind: Kind::Group(Group::USBDeviceTransportComponent) },
        (0x1d01, 16) => Param { name: "usb_host_transport_component", kind: Kind::Group(Group::USBHostTransportComponent) },
        (0x1d01, 17) => Param { name: "bluetooth_transport_component", kind: Kind::Group(Group::BluetoothTransportComponent) },
        (0x1d01, 20) => Param { name: "vehicle_information_component", kind: Kind::Group(Group::VehicleInformationComponent) },
        (0x1d01, 21) => Param { name: "vehicle_status_component", kind: Kind::Group(Group::VehicleStatusComponent) },
        (0x1d01, 22) => Param { name: "location_information_component", kind: Kind::Group(Group::LocationInformationComponent) },
        (0x1d01, 24) => Param { name: "wireless_car_play_transport_component", kind: Kind::Group(Group::WirelessCarPlayTransportComponent) },
        (0x1d01, 30) => Param { name: "route_guidance_display_component", kind: Kind::Group(Group::RouteGuidanceDisplayComponent) },
        (0x1d03, 0) => Param { name: "name", kind: Kind::Flag },
        (0x1d03, 1) => Param { name: "model_identifier", kind: Kind::Flag },
        (0x1d03, 2) => Param { name: "manufacturer", kind: Kind::Flag },
        (0x1d03, 3) => Param { name: "serial_number", kind: Kind::Flag },
        (0x1d03, 4) => Param { name: "fireware_version", kind: Kind::Flag },
        (0x1d03, 5) => Param { name: "hardware_version", kind: Kind::Flag },
        (0x1d03, 6) => Param { name: "messages_sent_by_accessory", kind: Kind::Flag },
        (0x1d03, 7) => Param { name: "messages_received_from_accessory", kind: Kind::Flag },
        (0x1d03, 8) => Param { name: "power_providing_capability", kind: Kind::Flag },
        (0x1d03, 9) => Param { name: "maximum_current_drawn_from_device", kind: Kind::Flag },
        (0x1d03, 10) => Param { name: "supported_external_accessory_protocol", kind: Kind::Flag },
        (0x1d03, 11) => Param { name: "app_match_team_id", kind: Kind::Flag },
        (0x1d03, 12) => Param { name: "current_language", kind: Kind::Flag },
        (0x1d03, 13) => Param { name: "supported_language", kind: Kind::Flag },
        (0x1d03, 14) => Param { name: "serial_transport_component", kind: Kind::Flag },
        (0x1d03, 15) => Param { name: "usb_device_transport_component", kind: Kind::Flag },
        (0x1d03, 16) => Param { name: "usb_host_transport_component", kind: Kind::Flag },
        (0x1d03, 17) => Param { name: "bluetooth_transport_component", kind: Kind::Flag },
        (0x1d03, 20) => Param { name: "vehicle_information_component", kind: Kind::Flag },
        (0x1d03, 21) => Param { name: "vehicle_status_component", kind: Kind::Flag },
        (0x1d03, 22) => Param { name: "location_information_component", kind: Kind::Flag },
        (0x1d03, 24) => Param { name: "wireless_car_play_transport_component", kind: Kind::Flag },
        (0x4154, 0) => Param { name: "remote_id", kind: Kind::Flag },
        (0x4154, 1) => Param { name: "display_name", kind: Kind::Flag },
        (0x4154, 2) => Param { name: "status", kind: Kind::Flag },
        (0x4154, 3) => Param { name: "direction", kind: Kind::Flag },
        (0x4154, 4) => Param { name: "call_uuid", kind: Kind::Flag },
        (0x4154, 6) => Param { name: "address_book_id", kind: Kind::Flag },
        (0x4154, 7) => Param { name: "label", kind: Kind::Flag },
        (0x4154, 8) => Param { name: "service", kind: Kind::Flag },
        (0x4154, 9) => Param { name: "is_conferenced", kind: Kind::Flag },
        (0x4154, 10) => Param { name: "conference_group", kind: Kind::Flag },
        (0x4154, 11) => Param { name: "disconnect_reason", kind: Kind::Flag },
        (0x4154, 12) => Param { name: "start_timestamp", kind: Kind::Flag },
        (0x4155, 0) => Param { name: "remote_id", kind: Kind::Str },
        (0x4155, 1) => Param { name: "display_name", kind: Kind::Str },
        (0x4155, 2) => Param { name: "status", kind: Kind::Num(1, false) },
        (0x4155, 3) => Param { name: "direction", kind: Kind::Num(1, false) },
        (0x4155, 4) => Param { name: "call_uuid", kind: Kind::Str },
        (0x4155, 11) => Param { name: "disconnect_reason", kind: Kind::Num(1, false) },
        (0x4157, 0) => Param { name: "signal_strength", kind: Kind::Flag },
        (0x4157, 1) => Param { name: "registration_status", kind: Kind::Flag },
        (0x4157, 2) => Param { name: "airplane_mode_status", kind: Kind::Flag },
        (0x4157, 4) => Param { name: "carrier_name", kind: Kind::Flag },
        (0x4157, 5) => Param { name: "cellular_supported", kind: Kind::Flag },
        (0x4158, 0) => Param { name: "signal_strength", kind: Kind::Bytes },
        (0x4158, 1) => Param { name: "registration_status", kind: Kind::Bytes },
        (0x4158, 2) => Param { name: "airplane_mode_status", kind: Kind::Bool },
        (0x4158, 4) => Param { name: "carrier_name", kind: Kind::Str },
        (0x4158, 5) => Param { name: "cellular_supported", kind: Kind::Bool },
        (0x4300, 0) => Param { name: "wired_attributes", kind: Kind::Group(Group::CarPlayAvailabilityWiredAttributes) },
        (0x4300, 1) => Param { name: "wireless_attributes", kind: Kind::Group(Group::CarPlayAvailabilityWirelessAttributes) },
        (0x4301, 0) => Param { name: "wired_attributes", kind: Kind::Group(Group::CarPlayStartSessionWiredAttributes) },
        (0x4301, 1) => Param { name: "wireless_attributes", kind: Kind::Group(Group::CarPlayStartSessionWirelessAttributes) },
        (0x4301, 2) => Param { name: "port", kind: Kind::Num(4, false) },
        (0x4301, 3) => Param { name: "device_identifier", kind: Kind::Str },
        (0x4301, 4) => Param { name: "public_key", kind: Kind::Str },
        (0x4301, 5) => Param { name: "source_version", kind: Kind::Str },
        (0x4301, 6) => Param { name: "sdk_version", kind: Kind::Str },
        (0x4301, 7) => Param { name: "asset", kind: Kind::Group(Group::CarPlayStartSessionAsset) },
        (0x4301, 8) => Param { name: "supports_mutual_authentication", kind: Kind::Bool },
        (0x4e0b, 0) => Param { name: "seconds_since_reference_date", kind: Kind::Num(8, true) },
        (0x4e0b, 1) => Param { name: "time_zone_offset_minutes", kind: Kind::Num(2, true) },
        (0x4e0b, 2) => Param { name: "daylight_savings_offset_minutes", kind: Kind::Num(1, true) },
        (0x4e0d, 0) => Param { name: "status", kind: Kind::Enum("WirelessCarPlayStatus") },
        (0x4e0e, 0) => Param { name: "bluetooth_transport_id", kind: Kind::Str },
        (0x4e0e, 1) => Param { name: "usb_transport_id", kind: Kind::Str },
        (0x5000, 0) => Param { name: "media_item_attributes", kind: Kind::Group(Group::StartMediaItemAttributes) },
        (0x5000, 1) => Param { name: "playback_attributes", kind: Kind::Group(Group::StartPlaybackAttributes) },
        (0x5001, 0) => Param { name: "media_item_attributes", kind: Kind::Group(Group::MediaItemAttributes) },
        (0x5001, 1) => Param { name: "playback_attributes", kind: Kind::Group(Group::PlaybackAttributes) },
        (0x5003, 0) => Param { name: "elapsed_ms", kind: Kind::Num(4, false) },
        (0x5200, 0) => Param { name: "display_component_id", kind: Kind::Num(2, false) },
        (0x5201, 0) => Param { name: "display_component_id", kind: Kind::Num(2, false) },
        (0x5201, 1) => Param { name: "state", kind: Kind::Num(1, false) },
        (0x5201, 2) => Param { name: "maneuver_state", kind: Kind::Num(1, false) },
        (0x5201, 3) => Param { name: "current_road_name", kind: Kind::Str },
        (0x5201, 4) => Param { name: "destination_name", kind: Kind::Str },
        (0x5201, 5) => Param { name: "eta", kind: Kind::Num(8, false) },
        (0x5201, 6) => Param { name: "time_remaining", kind: Kind::Num(8, false) },
        (0x5201, 7) => Param { name: "distance_remaining", kind: Kind::Num(4, false) },
        (0x5201, 10) => Param { name: "distance_to_maneuver", kind: Kind::Num(4, false) },
        (0x5201, 13) => Param { name: "current_maneuver_list", kind: Kind::Bytes },
        (0x5202, 0) => Param { name: "display_component_id", kind: Kind::Num(2, false) },
        (0x5202, 1) => Param { name: "index", kind: Kind::Num(2, false) },
        (0x5202, 3) => Param { name: "maneuver_type", kind: Kind::Num(1, false) },
        (0x5202, 4) => Param { name: "after_maneuver_road_name", kind: Kind::Str },
        (0x5202, 8) => Param { name: "driving_side", kind: Kind::Num(1, false) },
        (0x5202, 9) => Param { name: "junction_type", kind: Kind::Num(1, false) },
        (0x5202, 11) => Param { name: "exit_angle", kind: Kind::Num(2, true) },
        (0x5203, 0) => Param { name: "display_component_id", kind: Kind::Num(2, false) },
        (0x5701, 0) => Param { name: "status", kind: Kind::Enum("WiFiRequestStatus") },
        (0x5701, 1) => Param { name: "ssid", kind: Kind::Str },
        (0x5701, 2) => Param { name: "passphrase", kind: Kind::Str },
        (0x5703, 1) => Param { name: "ssid", kind: Kind::Str },
        (0x5703, 2) => Param { name: "passphrase", kind: Kind::Str },
        (0x5703, 3) => Param { name: "security_type", kind: Kind::Enum("AccessoryWiFiSecurityType") },
        (0x5703, 4) => Param { name: "channel", kind: Kind::Num(1, false) },
        (0xa101, 3) => Param { name: "range", kind: Kind::Num(2, false) },
        (0xa101, 4) => Param { name: "outside_temperature", kind: Kind::Num(2, true) },
        (0xa101, 6) => Param { name: "range_warning", kind: Kind::Bool },
        (0xa101, 9) => Param { name: "range_gasoline", kind: Kind::Num(2, false) },
        (0xa101, 10) => Param { name: "range_diesel", kind: Kind::Num(2, false) },
        (0xa101, 11) => Param { name: "range_electric", kind: Kind::Num(2, false) },
        (0xa101, 12) => Param { name: "range_cng", kind: Kind::Num(2, false) },
        (0xa101, 13) => Param { name: "range_warning_gasoline", kind: Kind::Bool },
        (0xa101, 14) => Param { name: "range_warning_diesel", kind: Kind::Bool },
        (0xa101, 15) => Param { name: "range_warning_electric", kind: Kind::Bool },
        (0xa101, 16) => Param { name: "range_warning_cng", kind: Kind::Bool },
        (0xa101, 21) => Param { name: "min_battery_charge_wh", kind: Kind::Num(4, false) },
        (0xa101, 22) => Param { name: "current_battery_charge_wh", kind: Kind::Num(4, false) },
        (0xa101, 23) => Param { name: "max_battery_charge_wh", kind: Kind::Num(4, false) },
        (0xa101, 24) => Param { name: "displayed_battery_percentage_e3", kind: Kind::Num(4, false) },
        (0xa101, 25) => Param { name: "is_charging", kind: Kind::Bool },
        (0xa101, 28) => Param { name: "active_connector", kind: Kind::Enum("ChargingConnectorType") },
        (0xa101, 30) => Param { name: "max_range_electric", kind: Kind::Num(2, false) },
        (0xaa01, 0) => Param { name: "certificate", kind: Kind::Bytes },
        (0xaa02, 0) => Param { name: "challenge", kind: Kind::Bytes },
        (0xaa03, 0) => Param { name: "response", kind: Kind::Bytes },
        (0xae00, 0) => Param { name: "maximum_current_drawn_from_accessory", kind: Kind::Flag },
        (0xae00, 1) => Param { name: "device_battery_will_charge_if_power_is_present", kind: Kind::Flag },
        (0xae00, 2) => Param { name: "accessory_power_mode", kind: Kind::Flag },
        (0xae00, 4) => Param { name: "is_external_charger_connected", kind: Kind::Flag },
        (0xae00, 5) => Param { name: "battery_charging_state", kind: Kind::Flag },
        (0xae00, 6) => Param { name: "battery_charge_level", kind: Kind::Flag },
        (0xae01, 0) => Param { name: "maximum_current_drawn_from_accessory", kind: Kind::Num(2, false) },
        (0xae01, 1) => Param { name: "device_battery_will_charge_if_power_is_present", kind: Kind::Bool },
        (0xae01, 2) => Param { name: "accessory_power_mode", kind: Kind::Num(1, false) },
        (0xae01, 4) => Param { name: "is_external_charger_connected", kind: Kind::Bool },
        (0xae01, 5) => Param { name: "battery_charging_state", kind: Kind::Num(1, false) },
        (0xae01, 6) => Param { name: "battery_charge_level", kind: Kind::Num(2, false) },
        (0xae03, 0) => Param { name: "available_current_for_device", kind: Kind::Num(2, false) },
        (0xae03, 1) => Param { name: "device_battery_should_charge_if_power_is_present", kind: Kind::Bool },
        (0xea00, 0) => Param { name: "protocol_id", kind: Kind::Num(1, false) },
        (0xea00, 1) => Param { name: "session_id", kind: Kind::Num(2, false) },
        (0xea01, 0) => Param { name: "session_id", kind: Kind::Num(2, false) },
        (0xea03, 0) => Param { name: "session_id", kind: Kind::Num(2, false) },
        (0xea03, 1) => Param { name: "status", kind: Kind::Enum("SessionStatus") },
        (0xfffa, 1) => Param { name: "gps_fix_data", kind: Kind::Flag },
        (0xfffa, 2) => Param { name: "recommended_minimum", kind: Kind::Flag },
        (0xfffa, 3) => Param { name: "satellites_in_view", kind: Kind::Flag },
        (0xfffa, 4) => Param { name: "vehicle_speed", kind: Kind::Flag },
        (0xfffb, 0) => Param { name: "nmea_sentence", kind: Kind::Str },
        _ => UNKNOWN,
    }
}

/// The name and kind of a parameter nested inside a group.
pub fn group_param(group: Group, id: u16) -> Param {
    match (group, id) {
        (Group::BluetoothTransportComponent, 0) => Param { name: "id", kind: Kind::Num(2, false) },
        (Group::BluetoothTransportComponent, 1) => Param { name: "name", kind: Kind::Str },
        (Group::BluetoothTransportComponent, 2) => Param { name: "supports_iap2_connection", kind: Kind::Flag },
        (Group::BluetoothTransportComponent, 3) => Param { name: "bluetooth_transport_mac", kind: Kind::Bytes },
        (Group::CarPlayAvailabilityWiredAttributes, 0) => Param { name: "available", kind: Kind::Bool },
        (Group::CarPlayAvailabilityWiredAttributes, 1) => Param { name: "usb_transport_identifier", kind: Kind::Str },
        (Group::CarPlayAvailabilityWirelessAttributes, 0) => Param { name: "available", kind: Kind::Bool },
        (Group::CarPlayAvailabilityWirelessAttributes, 1) => Param { name: "bluetooth_transport_identifier", kind: Kind::Str },
        (Group::CarPlayStartSessionAsset, 0) => Param { name: "identifier", kind: Kind::Str },
        (Group::CarPlayStartSessionAsset, 1) => Param { name: "version", kind: Kind::Num(4, false) },
        (Group::CarPlayStartSessionWiredAttributes, 0) => Param { name: "ip_address", kind: Kind::Str },
        (Group::CarPlayStartSessionWirelessAttributes, 0) => Param { name: "wifi_ssid", kind: Kind::Str },
        (Group::CarPlayStartSessionWirelessAttributes, 1) => Param { name: "passphrase", kind: Kind::Str },
        (Group::CarPlayStartSessionWirelessAttributes, 2) => Param { name: "channel", kind: Kind::Num(1, false) },
        (Group::CarPlayStartSessionWirelessAttributes, 3) => Param { name: "ip_address", kind: Kind::Str },
        (Group::CarPlayStartSessionWirelessAttributes, 4) => Param { name: "security_type", kind: Kind::Enum("CarPlayWiFiSecurityType") },
        (Group::ExternalAccessoryProtocol, 0) => Param { name: "id", kind: Kind::Num(1, false) },
        (Group::ExternalAccessoryProtocol, 1) => Param { name: "name", kind: Kind::Str },
        (Group::ExternalAccessoryProtocol, 2) => Param { name: "match_action", kind: Kind::Enum("MatchAction") },
        (Group::ExternalAccessoryProtocol, 3) => Param { name: "native_transport_component_identifier", kind: Kind::Num(2, false) },
        (Group::ExternalAccessoryProtocol, 4) => Param { name: "car_play", kind: Kind::Flag },
        (Group::LocationInformationComponent, 0) => Param { name: "id", kind: Kind::Num(2, false) },
        (Group::LocationInformationComponent, 1) => Param { name: "name", kind: Kind::Str },
        (Group::LocationInformationComponent, 17) => Param { name: "global_positioning_system_fix_data", kind: Kind::Flag },
        (Group::LocationInformationComponent, 18) => Param { name: "recommended_minimum_specific_gps_transit_data", kind: Kind::Flag },
        (Group::MediaItemAttributes, 0) => Param { name: "persistent_id", kind: Kind::Num(8, false) },
        (Group::MediaItemAttributes, 1) => Param { name: "title", kind: Kind::Str },
        (Group::MediaItemAttributes, 4) => Param { name: "duration_ms", kind: Kind::Num(4, false) },
        (Group::MediaItemAttributes, 6) => Param { name: "album", kind: Kind::Str },
        (Group::MediaItemAttributes, 12) => Param { name: "artist", kind: Kind::Str },
        (Group::MediaItemAttributes, 14) => Param { name: "album_artist", kind: Kind::Str },
        (Group::MediaItemAttributes, 16) => Param { name: "genre", kind: Kind::Str },
        (Group::MediaItemAttributes, 26) => Param { name: "artwork_ftid", kind: Kind::Num(1, false) },
        (Group::PlaybackAttributes, 0) => Param { name: "status", kind: Kind::Enum("PlaybackStatus") },
        (Group::PlaybackAttributes, 1) => Param { name: "elapsed_ms", kind: Kind::Num(4, false) },
        (Group::PlaybackAttributes, 7) => Param { name: "app_name", kind: Kind::Str },
        (Group::PlaybackAttributes, 16) => Param { name: "app_bundle_id", kind: Kind::Str },
        (Group::RouteGuidanceDisplayComponent, 0) => Param { name: "id", kind: Kind::Num(2, false) },
        (Group::RouteGuidanceDisplayComponent, 1) => Param { name: "name", kind: Kind::Str },
        (Group::RouteGuidanceDisplayComponent, 2) => Param { name: "max_current_road_name_length", kind: Kind::Num(2, false) },
        (Group::RouteGuidanceDisplayComponent, 3) => Param { name: "max_destination_name_length", kind: Kind::Num(2, false) },
        (Group::RouteGuidanceDisplayComponent, 4) => Param { name: "max_after_maneuver_road_name_length", kind: Kind::Num(2, false) },
        (Group::RouteGuidanceDisplayComponent, 5) => Param { name: "max_maneuver_description_length", kind: Kind::Num(2, false) },
        (Group::RouteGuidanceDisplayComponent, 6) => Param { name: "max_guidance_maneuver_storage_capacity", kind: Kind::Num(2, false) },
        (Group::RouteGuidanceDisplayComponent, 7) => Param { name: "max_lane_guidance_description_length", kind: Kind::Num(2, false) },
        (Group::RouteGuidanceDisplayComponent, 8) => Param { name: "max_lane_guidance_storage_capacity", kind: Kind::Num(2, false) },
        (Group::SerialTransportComponent, 0) => Param { name: "id", kind: Kind::Num(2, false) },
        (Group::SerialTransportComponent, 1) => Param { name: "name", kind: Kind::Str },
        (Group::SerialTransportComponent, 2) => Param { name: "supports_iap2_connection", kind: Kind::Flag },
        (Group::StartMediaItemAttributes, 0) => Param { name: "persistent_id", kind: Kind::Flag },
        (Group::StartMediaItemAttributes, 1) => Param { name: "title", kind: Kind::Flag },
        (Group::StartMediaItemAttributes, 4) => Param { name: "duration_ms", kind: Kind::Flag },
        (Group::StartMediaItemAttributes, 6) => Param { name: "album", kind: Kind::Flag },
        (Group::StartMediaItemAttributes, 12) => Param { name: "artist", kind: Kind::Flag },
        (Group::StartMediaItemAttributes, 14) => Param { name: "album_artist", kind: Kind::Flag },
        (Group::StartMediaItemAttributes, 16) => Param { name: "genre", kind: Kind::Flag },
        (Group::StartMediaItemAttributes, 26) => Param { name: "artwork", kind: Kind::Flag },
        (Group::StartPlaybackAttributes, 0) => Param { name: "status", kind: Kind::Flag },
        (Group::StartPlaybackAttributes, 1) => Param { name: "elapsed_ms", kind: Kind::Flag },
        (Group::StartPlaybackAttributes, 7) => Param { name: "app_name", kind: Kind::Flag },
        (Group::StartPlaybackAttributes, 16) => Param { name: "app_bundle_id", kind: Kind::Flag },
        (Group::USBDeviceTransportComponent, 0) => Param { name: "id", kind: Kind::Num(2, false) },
        (Group::USBDeviceTransportComponent, 1) => Param { name: "name", kind: Kind::Str },
        (Group::USBDeviceTransportComponent, 2) => Param { name: "supports_iap2_connection", kind: Kind::Flag },
        (Group::USBDeviceTransportComponent, 3) => Param { name: "audio_sample_rate", kind: Kind::Num(1, false) },
        (Group::USBHostTransportComponent, 0) => Param { name: "id", kind: Kind::Num(2, false) },
        (Group::USBHostTransportComponent, 1) => Param { name: "name", kind: Kind::Str },
        (Group::USBHostTransportComponent, 2) => Param { name: "supports_iap2_connection", kind: Kind::Flag },
        (Group::USBHostTransportComponent, 3) => Param { name: "car_play_interface_number", kind: Kind::Num(1, false) },
        (Group::USBHostTransportComponent, 4) => Param { name: "supports_car_play", kind: Kind::Flag },
        (Group::USBHostTransportComponent, 6) => Param { name: "theme_asset_identifier", kind: Kind::Str },
        (Group::VehicleInformationComponent, 0) => Param { name: "id", kind: Kind::Num(2, false) },
        (Group::VehicleInformationComponent, 1) => Param { name: "name", kind: Kind::Str },
        (Group::VehicleInformationComponent, 2) => Param { name: "engine_types", kind: Kind::Enum("EngineType") },
        (Group::VehicleInformationComponent, 6) => Param { name: "display_name", kind: Kind::Str },
        (Group::VehicleInformationComponent, 8) => Param { name: "maps_display_name", kind: Kind::Str },
        (Group::VehicleInformationComponent, 11) => Param { name: "supported_charging_connectors", kind: Kind::Enum("ChargingConnectorType") },
        (Group::VehicleInformationComponent, 12) => Param { name: "power_ccs1_w", kind: Kind::Num(4, false) },
        (Group::VehicleInformationComponent, 13) => Param { name: "power_ccs2_w", kind: Kind::Num(4, false) },
        (Group::VehicleInformationComponent, 14) => Param { name: "power_j1772_w", kind: Kind::Num(4, false) },
        (Group::VehicleInformationComponent, 15) => Param { name: "power_chademo_w", kind: Kind::Num(4, false) },
        (Group::VehicleInformationComponent, 16) => Param { name: "power_mennekes_w", kind: Kind::Num(4, false) },
        (Group::VehicleInformationComponent, 17) => Param { name: "power_gbt_dc_w", kind: Kind::Num(4, false) },
        (Group::VehicleInformationComponent, 18) => Param { name: "power_gbt_ac_w", kind: Kind::Num(4, false) },
        (Group::VehicleInformationComponent, 19) => Param { name: "power_nacs_dc_w", kind: Kind::Num(4, false) },
        (Group::VehicleInformationComponent, 20) => Param { name: "power_nacs_ac_w", kind: Kind::Num(4, false) },
        (Group::VehicleStatusComponent, 0) => Param { name: "id", kind: Kind::Num(2, false) },
        (Group::VehicleStatusComponent, 1) => Param { name: "name", kind: Kind::Str },
        (Group::VehicleStatusComponent, 3) => Param { name: "range", kind: Kind::Flag },
        (Group::VehicleStatusComponent, 4) => Param { name: "outside_temperature", kind: Kind::Flag },
        (Group::VehicleStatusComponent, 6) => Param { name: "range_warning", kind: Kind::Flag },
        (Group::VehicleStatusComponent, 9) => Param { name: "range_gasoline", kind: Kind::Flag },
        (Group::VehicleStatusComponent, 10) => Param { name: "range_diesel", kind: Kind::Flag },
        (Group::VehicleStatusComponent, 11) => Param { name: "range_electric", kind: Kind::Flag },
        (Group::VehicleStatusComponent, 12) => Param { name: "range_cng", kind: Kind::Flag },
        (Group::VehicleStatusComponent, 13) => Param { name: "range_warning_gasoline", kind: Kind::Flag },
        (Group::VehicleStatusComponent, 14) => Param { name: "range_warning_diesel", kind: Kind::Flag },
        (Group::VehicleStatusComponent, 15) => Param { name: "range_warning_electric", kind: Kind::Flag },
        (Group::VehicleStatusComponent, 16) => Param { name: "range_warning_cng", kind: Kind::Flag },
        (Group::VehicleStatusComponent, 21) => Param { name: "electric_charge_info", kind: Kind::Flag },
        (Group::VehicleStatusComponent, 30) => Param { name: "max_range_info", kind: Kind::Flag },
        (Group::WirelessCarPlayTransportComponent, 0) => Param { name: "id", kind: Kind::Num(2, false) },
        (Group::WirelessCarPlayTransportComponent, 1) => Param { name: "name", kind: Kind::Str },
        (Group::WirelessCarPlayTransportComponent, 2) => Param { name: "supports_iap2_connection", kind: Kind::Flag },
        (Group::WirelessCarPlayTransportComponent, 4) => Param { name: "supports_car_play", kind: Kind::Flag },
        (Group::WirelessCarPlayTransportComponent, 6) => Param { name: "theme_asset_identifier", kind: Kind::Str },
        _ => UNKNOWN,
    }
}

/// The protocol's name for an enum value, `UNKNOWN` for one not listed.
pub fn enum_name(enumeration: &'static str, value: u8) -> &'static str {
    match (enumeration, value) {
        ("AccessoryWiFiSecurityType", 0) => "None",
        ("AccessoryWiFiSecurityType", 1) => "Wep",
        ("AccessoryWiFiSecurityType", 2) => "Wpa2OrWpa3Transition",
        ("CarPlayWiFiSecurityType", 3) => "Wpa3Transition",
        ("CarPlayWiFiSecurityType", 4) => "Wpa3Only",
        ("ChargingConnectorType", 0) => "Ccs1",
        ("ChargingConnectorType", 1) => "Ccs2",
        ("ChargingConnectorType", 2) => "J1772",
        ("ChargingConnectorType", 3) => "Chademo",
        ("ChargingConnectorType", 4) => "Mennekes",
        ("ChargingConnectorType", 5) => "GbtDc",
        ("ChargingConnectorType", 6) => "GbtAc",
        ("ChargingConnectorType", 7) => "NacsDc",
        ("ChargingConnectorType", 8) => "NacsAc",
        ("EngineType", 0) => "Gas",
        ("EngineType", 1) => "Diesel",
        ("EngineType", 2) => "Electric",
        ("EngineType", 3) => "Cng",
        ("MatchAction", 0) => "None",
        ("MatchAction", 1) => "SettingsAndPrompt",
        ("MatchAction", 2) => "SettingsOnly",
        ("MatchAction", 4) => "NoActionNoCommunication",
        ("PlaybackStatus", 0) => "Stopped",
        ("PlaybackStatus", 1) => "Playing",
        ("PlaybackStatus", 2) => "Paused",
        ("PlaybackStatus", 3) => "SeekForward",
        ("PlaybackStatus", 4) => "SeekBackward",
        ("PowerProvidingCapability", 0) => "None",
        ("PowerProvidingCapability", 1) => "Reserved",
        ("PowerProvidingCapability", 2) => "Advanced",
        ("SessionStatus", 0) => "Ok",
        ("SessionStatus", 1) => "Close",
        ("WiFiRequestStatus", 0) => "Success",
        ("WiFiRequestStatus", 1) => "UserDeclined",
        ("WiFiRequestStatus", 2) => "NetworkInformationUnavailable",
        ("WirelessCarPlayStatus", 0) => "Unavailable",
        ("WirelessCarPlayStatus", 1) => "Available",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::{enum_name, group_param, message, param, Group, Kind, Message};

    #[test]
    fn a_message_id_is_the_number_and_the_protocol_name() {
        assert_eq!(message(0x1d00), "StartIdentification");
        assert_eq!(message(0x5001), "NowPlayingUpdate");
        assert_eq!(message(0x9999), "UNKNOWN");
        assert_eq!(Message(0x4301).to_string(), "0x4301(CarPlayStartSession)");
    }

    #[test]
    fn parameters_and_groups_are_named_with_the_types_they_are_read_as() {
        assert_eq!(param(0x1d01, 0).name, "name");
        assert_eq!(param(0x1d01, 0).kind, Kind::Str);
        assert_eq!(param(0x4301, 2).kind, Kind::Num(4, false));
        assert_eq!(param(0x5001, 0).kind, Kind::Group(Group::MediaItemAttributes));
        assert_eq!(group_param(Group::MediaItemAttributes, 1).name, "title");
        assert_eq!(param(0x9999, 0), super::UNKNOWN);
    }

    #[test]
    fn an_enum_value_has_a_name_only_where_the_protocol_gives_one() {
        assert_eq!(enum_name("PlaybackStatus", 1), "Playing");
        assert_eq!(enum_name("PlaybackStatus", 99), "UNKNOWN");
    }
}
