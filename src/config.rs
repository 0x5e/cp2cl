//! Everything the operator can tune, read once from the environment.
//!
//! Nothing here is required: the defaults build a working wired session and hand
//! it to whatever head unit turns up. See README for the full table.

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

/// Where the CarLife video channel's pictures come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// The CarPlay session. The default.
    Cp,
    /// One access unit, read from the file `CP2CL_INPUT_FILE` names and sent
    /// over and over. Nothing to do with a phone: it is how the channel and the
    /// car's decoder are exercised without one.
    File,
}

impl Input {
    fn parse(text: &str) -> Option<Self> {
        if text.eq_ignore_ascii_case("cp") {
            Some(Self::Cp)
        } else if text.eq_ignore_ascii_case("file") {
            Some(Self::File)
        } else {
            None
        }
    }
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
    /// Advertised as deviceID / bluetoothIDs. The phone only wants something
    /// stable and shaped like a MAC; it never resolves these.
    pub device_id: String,
    pub bt_mac: String,
    /// Accessory name shown on the phone.
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
    /// The router's BSSID. Sent as the accessory's device identifier; without
    /// it the phone falls back to this machine's Wi-Fi MAC.
    pub ap_mac: Option<String>,
    /// Where the CarLife video channel's pictures come from: `Input::Cp` unless
    /// `CP2CL_INPUT` says otherwise.
    pub input: Input,
    /// Only talk to this Bluetooth MAC. Several iPhones paired with the same
    /// machine otherwise race for the session.
    pub target_bt: Option<String>,
    /// Linux only: the Bluetooth adapter, as BlueZ names it.
    pub bt_adapter: String,
}

impl Config {
    pub fn from_env() -> Self {
        let input = match env("CP2CL_INPUT") {
            Some(text) => Input::parse(&text).unwrap_or_else(|| {
                tracing::warn!("[cp2cl] CP2CL_INPUT={text} is neither `cp` nor `file`; using cp");
                Input::Cp
            }),
            None => Input::Cp,
        };
        Self {
            auth_dir: Self::resolve_auth_dir(),
            width_mm: env_num("CP2CL_WIDTH_MM", 0),
            // Left to `info.rs` to derive from the aspect ratio when zero.
            height_mm: env_num("CP2CL_HEIGHT_MM", 0),
            device_id: env_or("CP2CL_DEVICE_ID", "00:11:22:33:44:55"),
            bt_mac: env_or("CP2CL_BT_MAC", "00:11:22:33:44:55"),
            name: env_or("CP2CL_NAME", "cp2cl"),
            // Wired stays the default: it needs nothing the operator has to
            // look up first.
            wireless: std::env::var("CP2CL_WIRELESS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            wifi_iface: env_or("CP2CL_WIFI_IFACE", "wlan0"),
            ap_mac: env("CP2CL_AP_MAC"),
            input,
            target_bt: env("CP2CL_TARGET_BT"),
            bt_adapter: env_or("CP2CL_BT_ADAPTER", "hci0"),
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
    fn the_input_source_is_named_as_one_of_the_two_that_exist() {
        assert_eq!(Input::parse("cp"), Some(Input::Cp));
        assert_eq!(Input::parse("File"), Some(Input::File));
        assert_eq!(Input::parse("still"), None);
    }
}
