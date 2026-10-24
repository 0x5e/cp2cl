//! Everything the operator can tune, read once from the environment.
//!
//! Nothing here is required: the defaults build a working wired session and hand
//! it to whatever head unit turns up. See README for the full table.
//!
//! Two values are not settings at all, because they are properties of the
//! machine rather than choices: the name it goes by is the hostname
//! (`hostname`), and its Bluetooth address is read off the controller
//! (`bt_address`).

use std::path::{Path, PathBuf};

use tracing::level_filters::LevelFilter;

fn env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn env_or(key: &str, default: &str) -> String {
    env(key).unwrap_or_else(|| default.to_string())
}

/// `CP2CL_LOG_LEVEL`, or `info`.
///
/// Read on its own rather than with the rest, because it has to be known before
/// the config is: a setting that could not be used is reported by a log line,
/// and that line needs a subscriber to already exist. That is also why a value
/// that is not one of the four levels falls back on `info` without saying so —
/// there is nothing yet to say it to.
///
/// A level and nothing else. `tracing-subscriber`'s environment filter also
/// takes directives — `crate=debug`, `crate[span]=trace` — and parsing them is
/// what a regex engine was built into the binary for; there is one knob here and
/// it names one of four levels. A value that is not one of them is `info`.
pub fn log_filter() -> LevelFilter {
    match env("CP2CL_LOG_LEVEL") {
        Some(level) => level.parse().unwrap_or(LevelFilter::INFO),
        None => LevelFilter::INFO,
    }
}

/// What a machine with no controller to ask reports. Every caller wants
/// something stable and shaped like a MAC, and this is all that can be said.
const NO_ADDRESS: &str = "00:11:22:33:44:55";

/// What a machine that cannot say what it is called goes by. Something has to
/// go in the identification, and this is the program's own name.
const DEFAULT_NAME: &str = "cp2cl";

/// A Bluetooth controller's own address, which is the only place one is written
/// down — the phone checks it against the one it paired with, so a made-up value
/// is rejected where it matters.
pub(crate) fn bt_address(adapter: &str) -> String {
    std::fs::read_to_string(format!("/sys/class/bluetooth/{adapter}/address"))
        .map(|s| s.trim().to_uppercase())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| NO_ADDRESS.to_string())
}

/// What the machine goes by, which is what the accessory is called: the phone
/// shows this name in its CarPlay list, and it is the one the car is told the
/// model is.
fn hostname() -> String {
    read_hostname(Path::new("/proc/sys/kernel/hostname"))
}

/// Split from the fixed path so that a machine which cannot say what it is
/// called is something a test can reach.
fn read_hostname(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_NAME.to_string())
}

#[derive(Debug, Clone)]
pub struct Config {
    /// This machine's own Bluetooth address, which is also the identifier
    /// `/info` reports — see `bt_address`.
    pub bt_mac: String,
    /// What the accessory is called: the machine's hostname, which is what the
    /// phone lists it as — see `hostname`.
    pub name: String,

    /// Take CarPlay over Bluetooth and Wi-Fi instead of the cable.
    ///
    /// The phone is paired in the OS first — this program does not pair — and
    /// then finds us by the iAP service we publish. Everything after that is the
    /// same iAP2 conversation, except that `CarPlayStartSession` has to name a
    /// Wi-Fi network the phone can join: the configuration in `/etc/hostapd`
    /// that names `wifi_iface` says which one, which is read in `crate::wireless`.
    pub wireless: bool,
    /// The interface the access point runs on: whose link-local address the
    /// phone is told to connect back to, and which hostapd configuration in
    /// `/etc/hostapd` is the one being read.
    pub wifi_iface: String,
    /// The CarLife video channel takes its pictures from this file instead of
    /// from the CarPlay session, when the operator names one. Nothing to do with
    /// a phone: it is how the channel and the car's decoder are exercised
    /// without one.
    pub input_file: Option<PathBuf>,
    /// Linux only: the Bluetooth adapter, as BlueZ names it.
    pub bt_adapter: String,
}

impl Config {
    pub fn from_env() -> Self {
        let bt_adapter = env_or("CP2CL_BT_ADAPTER", "hci0");
        Self {
            bt_mac: bt_address(&bt_adapter),
            name: hostname(),
            // Wired stays the default: it needs nothing the operator has to
            // look up first.
            wireless: std::env::var("CP2CL_WIRELESS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            wifi_iface: env_or("CP2CL_WIFI_IFACE", "wlan0"),
            input_file: env("CP2CL_INPUT_FILE").map(PathBuf::from),
            bt_adapter,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_with_no_controller_still_has_a_mac_shaped_address_to_report() {
        assert_eq!(bt_address("no-such-adapter"), NO_ADDRESS);
    }

    #[test]
    fn a_machine_that_cannot_say_what_it_is_called_still_goes_by_a_name() {
        assert_eq!(read_hostname(Path::new("no-such-file")), DEFAULT_NAME);
    }
}
