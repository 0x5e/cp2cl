//! Everything the operator can tune, read once from the environment.
//!
//! Nothing here is required: the defaults build a working wired session and hand
//! it to whatever head unit turns up. See README for the full table.
//!
//! `screen` is not from the environment: it is `/etc/cp2cl/config.json`, read at
//! the start of each session. See `crate::settings`.
//!
//! Three values are not settings at all, because they are properties of the
//! machine rather than choices: the name it goes by is the hostname
//! (`hostname`), its Bluetooth address is read off the controller
//! (`bt_address`), and its maker out of the device tree (`dt_vendor`). All
//! three are read where they are used.

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

/// Where the program keeps what it has to remember — the identity it presents to
/// the phone now, its settings once there are any — and where it reads the
/// credentials the firmware installs beside them.
///
/// `/etc/cp2cl` rather than a home directory: this board has no user with one,
/// and `/etc` is on the writable root, so what lands here outlives the boot — a
/// new identity every ignition reads as a new accessory to the phone.
///
/// The credentials are files here rather than bytes in the binary, and this is
/// the only place the path is written down: everything that reads them asks for
/// one by name with `data_file`, and the firmware installs them into this same
/// directory — see `buildroot/package/cp2cl`.
pub const DATA_DIR: &str = "/etc/cp2cl";

/// `DATA_DIR`, created if the first run has not made it yet.
pub fn data_dir() -> std::io::Result<PathBuf> {
    let dir = PathBuf::from(DATA_DIR);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// One of the files that live in `DATA_DIR`, by its bare name.
///
/// Not created, only named: what is read here is put there by the build or by an
/// earlier run, and a machine without the file is a machine that runs without
/// it rather than one that gets a directory it did not ask for.
pub fn data_file(name: &str) -> PathBuf {
    Path::new(DATA_DIR).join(name)
}

/// What a machine with no controller to ask reports. Every caller wants
/// something stable and shaped like a MAC, and this is all that can be said.
const NO_ADDRESS: &str = "00:11:22:33:44:55";

/// What a machine that cannot say what it is called goes by. Something has to
/// go in the identification, and this is the program's own name.
const DEFAULT_NAME: &str = "cp2cl";

/// What a machine with no device tree to ask reports as its maker.
const DEFAULT_VENDOR: &str = "Unknown";

/// A Bluetooth controller's own address, which is the only place one is written
/// down — the phone checks it against the one it paired with, so a made-up value
/// is rejected where it matters.
///
/// Read where it is used rather than carried in the configuration: the file
/// appears when the controller is brought up, and a copy taken at startup is a
/// copy of whatever there was before that — the fallback below.
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
pub(crate) fn hostname() -> String {
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

/// The board's maker: the vendor half of the first `compatible` entry. Read
/// where it is used, like the two above, and returned as the board writes it.
pub(crate) fn dt_vendor() -> String {
    read_dt_vendor(Path::new("/sys/firmware/devicetree/base/compatible"))
}

/// Split from the fixed path so that a machine with no device tree is something
/// a test can reach.
fn read_dt_vendor(path: &Path) -> String {
    let raw = std::fs::read(path).unwrap_or_default();
    vendor_from_compatible(&raw).unwrap_or_else(|| DEFAULT_VENDOR.to_string())
}

/// `compatible` is a NUL-separated list of `<vendor>,<board>` entries, most
/// specific first, so the vendor is the prefix of the first one.
fn vendor_from_compatible(raw: &[u8]) -> Option<String> {
    let first = raw.split(|b| *b == 0).next()?;
    let entry = std::str::from_utf8(first).ok()?;
    let (vendor, _board) = entry.split_once(',')?;
    let vendor = vendor.trim();
    (!vendor.is_empty()).then(|| vendor.to_string())
}

#[derive(Debug, Clone)]
pub struct Config {
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
    /// The screen the settings page picked, out of `/etc/cp2cl/config.json`.
    /// `main` fills it in at startup and `carlife::gadget` again for each
    /// session; empty is the car's own screen.
    pub screen: crate::settings::Screen,
}

impl Config {
    pub fn from_env() -> Self {
        let bt_adapter = env_or("CP2CL_BT_ADAPTER", "hci0");
        Self {
            // Wireless is what this is for: the board is reached as an
            // accessory over Bluetooth and Wi-Fi. `CP2CL_WIRELESS=0` is the way
            // back to the cable, for a machine with no radio to use.
            wireless: std::env::var("CP2CL_WIRELESS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(true),
            wifi_iface: env_or("CP2CL_WIFI_IFACE", "wlan0"),
            input_file: env("CP2CL_INPUT_FILE").map(PathBuf::from),
            bt_adapter,
            screen: crate::settings::Screen::default(),
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

    #[test]
    fn a_machine_with_no_device_tree_still_reports_a_maker() {
        assert_eq!(read_dt_vendor(Path::new("no-such-file")), DEFAULT_VENDOR);
    }

    #[test]
    fn the_maker_is_the_prefix_of_the_first_compatible_entry() {
        let raw = b"rockchip,rk3566\0rockchip,rk356x\0";
        assert_eq!(vendor_from_compatible(raw).as_deref(), Some("rockchip"));
        assert_eq!(vendor_from_compatible(b"\0"), None, "an empty first entry");
        assert_eq!(vendor_from_compatible(b"no-comma"), None);
    }
}
