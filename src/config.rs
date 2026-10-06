//! Everything the operator can tune, read once from the environment.
//!
//! Nothing here is required: the defaults build a working wired session and serve
//! it to the browser client. See README for the full table.

use std::net::SocketAddr;
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

/// `CP2AA_LOG`, or `info`.
///
/// Read on its own rather than with the rest, because it has to be known before
/// the config is: a setting that could not be used is reported by a log line,
/// and that line needs a subscriber to already exist.
pub fn log_filter() -> String {
    env_or("CP2AA_LOG", "info")
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    env(key).and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// Where a decoded stream goes, once CarPlay has handed it over.
///
/// Both ends of this are the same media, so the choice is only about who
/// consumes it: the browser client on this machine, or a head unit that thinks
/// we are an Android phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// Serve PlayPort's browser client over HTTPS and push the streams to it.
    /// The default.
    Web,
    /// Send to a connected Android Auto head unit over `aa_client`.
    Aa,
}

impl Sink {
    fn parse(text: &str) -> Option<Self> {
        if text.eq_ignore_ascii_case("web") {
            Some(Self::Web)
        } else if text.eq_ignore_ascii_case("aa") {
            Some(Self::Aa)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Directory holding identity.pk8 + certificate.p7b (the MFi credentials).
    pub auth_dir: Option<PathBuf>,
    /// What the phone is asked to render. 1280x720 is the safe CarPlay default.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Screen size in millimetres, which is how CarPlay decides its dpi — and
    /// therefore how finely it draws. This is the whole difference between a
    /// sharp picture and a blurry one: MacPlay reports the window's real share
    /// of the panel and gets ~254 dpi, while inventing a large value drops it to
    /// ~162 and the phone quietly renders at 1×.
    pub width_mm: u32,
    pub height_mm: u32,
    /// Advertise HEVC. Video then arrives as hvcC / H.265 NALs.
    pub hevc: bool,
    /// Advertised as deviceID / bluetoothIDs. The phone only wants something
    /// stable and shaped like a MAC; it never resolves these.
    pub device_id: String,
    pub bt_mac: String,
    /// Accessory name shown on the phone.
    pub name: String,
    /// Linux only: which USB network gadget carries the airplay stream.
    /// Auto-detected when unset.
    pub av_iface: Option<String>,
    /// What consumes the streams: the browser client, or an Android Auto head unit.
    pub sink: Sink,
    /// Where the browser client is served, read from `CP2AA_WEB_ADDR` as
    /// `address:port`. The scheme is not part of it because there is only one:
    /// the page decodes with WebCodecs, which a browser only offers in a secure
    /// context, so this is HTTPS whether or not anyone says so.
    pub web_addr: SocketAddr,
    /// The access token the page has to carry — on the socket, and when changing
    /// the display. Fixed rather than minted per run, so the URL to open, and
    /// any bookmark of it, survives a restart. The default is sixteen zero bytes
    /// in hex, which is what the debugging URL used to look like anyway.
    pub web_token: String,
    /// Android Auto over TCP. The wireless protocol is TCP on this port, so a
    /// head unit that connects over the network works without a gadget — which
    /// is also the only way to try any of this from a Mac.
    pub aa_tcp_port: u16,
    /// Android Auto over a USB gadget instead: the board plays an Android phone,
    /// the head unit is the host. Linux only, and it needs configfs — which is
    /// why the field only exists there.
    #[cfg(target_os = "linux")]
    pub aa_usb: bool,

    /// Take CarPlay over Bluetooth and Wi-Fi instead of the cable.
    ///
    /// The phone is paired in the OS first — this program does not pair — and
    /// then finds us by the iAP service we publish. Everything after that is the
    /// same iAP2 conversation, except that `CarPlayStartSession` has to name a
    /// Wi-Fi network the phone can join, which is why the fields below exist.
    pub wireless: bool,
    /// The network both ends are on. Wireless CarPlay does not make one: the
    /// phone is told to join this, so it has to be a network it can reach.
    pub ssid: String,
    pub passphrase: String,
    /// 1..196. Reported to the phone so it can find the network faster; a wrong
    /// value is not fatal, the phone scans anyway.
    pub wifi_channel: u8,
    /// The interface facing that network. Its IPv6 link-local address is what
    /// the phone is told to connect back to.
    pub wifi_iface: String,
    /// The router's BSSID. Sent as the accessory's device identifier; without
    /// it the phone falls back to this machine's Wi-Fi MAC, which on recent
    /// macOS SDKs comes back as a placeholder.
    pub ap_mac: Option<String>,
    /// Only talk to this Bluetooth MAC. Several iPhones paired with the same
    /// machine otherwise race for the session.
    pub target_bt: Option<String>,
    /// Linux only: the Bluetooth adapter, as BlueZ names it.
    pub bt_adapter: String,
}

impl Config {
    pub fn from_env() -> Self {
        // Named first so the window and the physical size can default to them.
        let width = env_num("CP2AA_WIDTH", 1280);
        let height = env_num("CP2AA_HEIGHT", 720);
        let width_mm = env_num("CP2AA_WIDTH_MM", 0);
        Self {
            auth_dir: Self::resolve_auth_dir(),
            width,
            height,
            fps: env_num("CP2AA_FPS", 60),
            width_mm,
            // Left to `info.rs` to derive from the aspect ratio when zero.
            height_mm: env_num("CP2AA_HEIGHT_MM", 0),
            // HEVC by default: at the bitrate the phone sends, H.265 is visibly
            // sharper than H.264. MacPlay reports `codec=h265` for the same
            // session, which is where the difference in clarity comes from.
            hevc: std::env::var("CP2AA_HEVC")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            device_id: env_or("CP2AA_DEVICE_ID", "00:11:22:33:44:55"),
            bt_mac: env_or("CP2AA_BT_MAC", "00:11:22:33:44:55"),
            name: env_or("CP2AA_NAME", "cp2aa"),
            av_iface: env("CP2AA_AV_IFACE"),
            sink: match env("CP2AA_SINK") {
                Some(text) => match Sink::parse(&text) {
                    Some(sink) => sink,
                    None => {
                        tracing::warn!("[cp2aa] CP2AA_SINK={text} is neither `web` nor `aa`; using web");
                        Sink::Web
                    }
                },
                None => Sink::Web,
            },
            web_addr: web_addr_from_env(),
            web_token: env_or("CP2AA_WEB_TOKEN", "00000000000000000000000000000000"),
            // 5288 is the port Android Auto uses for wireless.
            aa_tcp_port: env_num("CP2AA_AA_TCP_PORT", 5288),
            #[cfg(target_os = "linux")]
            aa_usb: std::env::var("CP2AA_AA_USB")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            // Wired stays the default: it needs nothing the operator has to
            // look up first.
            wireless: std::env::var("CP2AA_WIRELESS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            ssid: env_or("CP2AA_WIFI_SSID", ""),
            passphrase: env_or("CP2AA_WIFI_PASS", ""),
            wifi_channel: env_num("CP2AA_WIFI_CHANNEL", 36),
            wifi_iface: env_or("CP2AA_WIFI_IFACE", default_wifi_iface()),
            ap_mac: env("CP2AA_AP_MAC"),
            target_bt: env("CP2AA_TARGET_BT"),
            bt_adapter: env_or("CP2AA_BT_ADAPTER", "hci0"),
        }
    }

    /// Look for credentials next to the binary, then in the working directory,
    /// then wherever CP2AA_AUTH_DIR points. Same idea as LIVI's resolveAuthDir,
    /// minus Electron: order is explicit override first, then a couple of
    /// sensible spots so `cargo run` works from a clone with credentials
    /// dropped in `authentication/`.
    fn resolve_auth_dir() -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(dir) = env("CP2AA_AUTH_DIR") {
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

/// Where the browser client listens unless told otherwise. `0.0.0.0` is every
/// interface, which is what makes the page reachable from another device.
const DEFAULT_WEB_ADDR: &str = "0.0.0.0:8080";

/// `CP2AA_WEB_ADDR`, or the default when it is unset or not an `address:port`.
fn web_addr_from_env() -> SocketAddr {
    let Some(text) = env("CP2AA_WEB_ADDR") else {
        return default_web_addr();
    };
    text.parse().unwrap_or_else(|_| {
        tracing::warn!("[cp2aa] CP2AA_WEB_ADDR={text} is not `address:port`; using {DEFAULT_WEB_ADDR}");
        default_web_addr()
    })
}

fn default_web_addr() -> SocketAddr {
    // A constant, so this cannot fail.
    DEFAULT_WEB_ADDR.parse().expect("the default web address parses")
}

/// The interface the phone will be told to reach us on, per platform.
fn default_wifi_iface() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "en0"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "wlan0"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_web_address_is_every_interface_on_8080() {
        assert_eq!(default_web_addr().to_string(), "0.0.0.0:8080");
    }
}
