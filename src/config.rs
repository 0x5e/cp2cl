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

fn env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn env_or(key: &str, default: &str) -> String {
    env(key).unwrap_or_else(|| default.to_string())
}

/// `CP2CL_LOG`, or `info`.
///
/// Read on its own rather than with the rest, because it has to be known before
/// the config is: a setting that could not be used is reported by a log line,
/// and that line needs a subscriber to already exist.
pub fn log_filter() -> String {
    env_or("CP2CL_LOG", "info")
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    env(key).and_then(|s| s.parse().ok()).unwrap_or(default)
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
    /// Directory holding identity.pk8 + certificate.p7b (the MFi credentials).
    pub auth_dir: Option<PathBuf>,
    /// Screen size in millimetres, which is how CarPlay decides its dpi — and
    /// therefore how finely it draws. This is the whole difference between a
    /// sharp picture and a blurry one: MacPlay reports the window's real share
    /// of the panel and gets ~254 dpi, while inventing a large value drops it to
    /// ~162 and the phone quietly renders at 1×.
    ///
    /// No CarLife message carries a physical size, so this one is always the
    /// operator's.
    pub width_mm: u32,
    pub height_mm: u32,
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
            auth_dir: Self::resolve_auth_dir(),
            width_mm: env_num("CP2CL_WIDTH_MM", 0),
            // Left to `info.rs` to derive from the aspect ratio when zero.
            height_mm: env_num("CP2CL_HEIGHT_MM", 0),
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

    /// Look for credentials next to the binary, then in the working directory,
    /// then wherever CP2CL_AUTH_DIR points. Same idea as LIVI's resolveAuthDir,
    /// minus Electron: order is explicit override first, then a couple of
    /// sensible spots so `cargo run` works from a clone with credentials
    /// dropped in `authentication/`.
    fn resolve_auth_dir() -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(dir) = env("CP2CL_AUTH_DIR") {
            candidates.push(PathBuf::from(dir));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("authentication"));
            }
        }
        candidates.push(PathBuf::from("authentication"));
        candidates.push(PathBuf::from("assets/authentication"));
        candidates
            .into_iter()
            .find(|dir| Self::has_credentials(dir))
    }

    fn has_credentials(dir: &Path) -> bool {
        dir.join("identity.pk8").is_file() && dir.join("certificate.p7b").is_file()
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
