//! Everything the operator can tune: what `config.json` holds, and the reading
//! and writing of that file.
//!
//! Nothing here is required: the defaults are enough for both links — the cable
//! and Bluetooth — to wait for a phone and hand what it sends to whatever head
//! unit turns up. See README for the full table, and `crate::web::settings` for
//! the page that edits the screen and nothing else.
//!
//! Every field may be absent, and an absent one is the default: a machine with
//! no file at all runs on those, and a hand-written file says only what it means
//! to change. Read as it is written: a size the page does not offer is served all
//! the same, and a value that cannot be used is reported and left at its default.
//! Only half a pair or a zero is dropped. The file is created at startup when it
//! is not there.
//!
//! Three values are not settings at all, because they are properties of the
//! machine rather than choices: the name it goes by is the hostname
//! (`hostname`), its Bluetooth address is read off the controller
//! (`bt_address`), and its maker out of the device tree (`dt_vendor`). All
//! three are read where they are used.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tracing::level_filters::LevelFilter;

/// What the program reads and writes: the credentials the firmware installs, and
/// the two files a board with a volume links onto it.
///
/// `/etc/cp2cl` rather than a home directory: this board has no user with one.
pub const DATA_DIR: &str = "/etc/cp2cl";

/// One of the files that live in `DATA_DIR`, by its bare name.
///
/// Not created, only named: what is read here is put there by the build or by an
/// earlier run, and a machine without the file is a machine that runs without
/// it rather than one that gets a directory it did not ask for.
pub fn data_file(name: &str) -> PathBuf {
    Path::new(DATA_DIR).join(name)
}

/// The file these settings live in, under [`DATA_DIR`].
const FILE: &str = "config.json";

/// Where it is.
fn path() -> PathBuf {
    data_file(FILE)
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

/// A size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

/// What the file says about the screen, every field optional: `None` is 默认.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Screen {
    /// Both halves or neither.
    pub resolution: Option<Resolution>,
    pub fps: Option<u32>,
}

/// `1920x720@60`, the shape the channel table writes a size in.
impl std::fmt::Display for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.resolution {
            Some(r) => write!(f, "{}x{}", r.width, r.height)?,
            None => f.write_str("default")?,
        }
        match self.fps {
            Some(fps) => write!(f, "@{fps}"),
            None => f.write_str("@default"),
        }
    }
}

impl Screen {
    /// Whether anything at all is set.
    pub fn is_empty(&self) -> bool {
        self.resolution.is_none() && self.fps.is_none()
    }
}

/// Everything the operator can tune, and nothing that is not a choice.
///
/// The field names are the file's: a field is `log_level`, `wifi_iface`,
/// `bt_adapter` or `addr` in `config.json`, and absent from it is what `Default`
/// says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The interface the access point runs on: whose link-local address the
    /// phone is told to connect back to, and which hostapd configuration in
    /// `/etc/hostapd` is the one being read.
    pub wifi_iface: String,
    /// Linux only: the Bluetooth adapter, as BlueZ names it.
    pub bt_adapter: String,
    /// How much goes to the terminal: `error`, `warn`, `info` or `debug`. The
    /// copy under `/var/log` is not held to it — see `crate::logging`.
    pub log_level: LevelFilter,
    /// Where the status page listens, `ip:port` as `bind` takes it. Port 80 is
    /// the one a browser assumes when none is given, and binding it takes root,
    /// which is how the program runs on the board.
    pub addr: String,
    /// The screen the settings page picked. `main` reads it at startup and
    /// `carlife::gadget` again for each session; empty is the car's own screen.
    pub screen: Screen,
}

impl Default for Config {
    /// Both links wait for a phone, and the first one to see it is the one the
    /// session runs on.
    fn default() -> Self {
        Self {
            wifi_iface: "wlan0".to_string(),
            bt_adapter: "hci0".to_string(),
            log_level: LevelFilter::INFO,
            addr: "0.0.0.0:80".to_string(),
            screen: Screen::default(),
        }
    }
}

/// The file as it stands; anything unreadable is the defaults.
pub fn load() -> Config {
    read(&path())
}

/// The same, creating the file when it is not there.
pub fn load_or_create() -> Config {
    let path = path();
    if !path.exists() {
        match save(&Config::default()) {
            Ok(()) => tracing::info!("settings.created path={}", path.display()),
            // Read-only `/etc` is a machine that runs on the defaults.
            Err(e) => tracing::warn!("settings.create_failed path={} err={e}", path.display()),
        }
    }
    read(&path)
}

/// How much the terminal shows, read on its own rather than with the rest.
///
/// It has to be known before the subscriber that filters the terminal is built,
/// and that is what makes it the one setting read before there is anywhere to
/// report a file that could not be read. The read that follows has a log to say
/// it in.
pub fn log_filter() -> LevelFilter {
    load().log_level
}

/// The file, written where it stands: in place rather than beside and renamed,
/// which is what lets it be a link onto the board's volume.
pub fn save(config: &Config) -> std::io::Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(&json(config))?;
    text.push('\n');
    std::fs::write(&path, text)
}

fn read(path: &Path) -> Config {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(e) => {
            tracing::warn!("settings.read_failed path={} err={e}", path.display());
            return Config::default();
        }
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        tracing::warn!("settings.read_failed path={} err=not_json", path.display());
        return Config::default();
    };
    from_value(&value)
}

/// The settings a JSON document holds. Nothing is checked against what the page
/// offers: only half a pair or a zero is dropped, because those serve nothing.
fn from_value(value: &Value) -> Config {
    let defaults = Config::default();
    let screen = &value["screen"];
    let width = number(screen, "width");
    let height = number(screen, "height");
    let resolution = match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => {
            Some(Resolution { width, height })
        }
        (None, None) => None,
        (width, height) => {
            tracing::warn!("settings.ignored field=resolution width={width:?} height={height:?}");
            None
        }
    };
    let fps = match number(screen, "fps") {
        Some(fps) if fps > 0 => Some(fps),
        Some(fps) => {
            tracing::warn!("settings.ignored field=fps value={fps}");
            None
        }
        None => None,
    };
    Config {
        screen: Screen { resolution, fps },
        log_level: level(value, defaults.log_level),
        wifi_iface: text(value, "wifi_iface", &defaults.wifi_iface),
        bt_adapter: text(value, "bt_adapter", &defaults.bt_adapter),
        addr: text(value, "addr", &defaults.addr),
    }
}

/// The terminal's filter, from `log_level`.
fn level(object: &Value, default: LevelFilter) -> LevelFilter {
    match &object["log_level"] {
        Value::Null => default,
        value => match value.as_str().and_then(level_of) {
            Some(level) => level,
            None => {
                tracing::warn!("settings.ignored field=log_level value={value}");
                default
            }
        },
    }
}

/// The four levels the terminal can be held to, by name. `trace` is not one of
/// them: nothing writes it, and it is below the copy under `/var/log` too.
fn level_of(text: &str) -> Option<LevelFilter> {
    Some(match text.trim().to_ascii_lowercase().as_str() {
        "error" => LevelFilter::ERROR,
        "warn" => LevelFilter::WARN,
        "info" => LevelFilter::INFO,
        "debug" => LevelFilter::DEBUG,
        _ => return None,
    })
}

/// The name a level is written by, in the file and in the log line that says
/// which one is in force. It is one of the four above.
pub(crate) fn level_name(level: LevelFilter) -> &'static str {
    match level {
        LevelFilter::ERROR => "error",
        LevelFilter::WARN => "warn",
        LevelFilter::DEBUG => "debug",
        // `level_of` is the only way a level gets in, so nothing else is here.
        _ => "info",
    }
}

/// A name, from a field that has to be a non-empty string to be one.
fn text(object: &Value, key: &str, default: &str) -> String {
    match &object[key] {
        Value::Null => default.to_string(),
        Value::String(text) if !text.trim().is_empty() => text.trim().to_string(),
        value => {
            tracing::warn!("settings.ignored field={key} value={value}");
            default.to_string()
        }
    }
}

fn number(object: &Value, key: &str) -> Option<u32> {
    object[key]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
}

/// The file's shape, as it is written.
pub(crate) fn json(config: &Config) -> Value {
    json!({
        "screen": {
            "width": config.screen.resolution.map(|r| r.width),
            "height": config.screen.resolution.map(|r| r.height),
            "fps": config.screen.fps,
        },
        "log_level": level_name(config.log_level),
        "wifi_iface": config.wifi_iface,
        "bt_adapter": config.bt_adapter,
        "addr": config.addr,
    })
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

    fn full() -> Screen {
        Screen {
            resolution: Some(Resolution {
                width: 1920,
                height: 720,
            }),
            fps: Some(60),
        }
    }

    /// 一份只有屏幕设成 `screen` 的文件。
    fn with(screen: Screen) -> Config {
        Config {
            screen,
            ..Config::default()
        }
    }

    /// 写下去的就是读回来的；留空的那一项不在文件里。
    #[test]
    fn what_is_written_is_what_is_read() {
        assert_eq!(from_value(&json(&with(full()))), with(full()));

        let rate_only = Screen {
            resolution: None,
            fps: Some(30),
        };
        assert_eq!(from_value(&json(&with(rate_only))).screen, rate_only);
        assert_eq!(json(&with(rate_only))["screen"]["width"], Value::Null);

        let nothing = Screen::default();
        assert_eq!(from_value(&json(&with(nothing))).screen, nothing);
        assert!(nothing.is_empty());

        // 日志里的一行，写法与渠道表一致。
        assert_eq!(full().to_string(), "1920x720@60");
        assert_eq!(nothing.to_string(), "default@default");
    }

    /// 四个字段按名字读，写什么是什么；没写的、写错的都是默认。
    #[test]
    fn the_four_settings_are_read_by_their_names() {
        let written = json!({
            "log_level": "debug",
            "wifi_iface": "wlan1",
            "bt_adapter": "hci1",
            "addr": "127.0.0.1:8080",
        });
        let read = from_value(&written);
        assert_eq!(read.log_level, LevelFilter::DEBUG);
        assert_eq!(read.wifi_iface, "wlan1");
        assert_eq!(read.bt_adapter, "hci1");
        assert_eq!(read.addr, "127.0.0.1:8080");
        assert_eq!(read.screen, Screen::default(), "没写的屏幕是默认");

        // 写错的字段留在默认上，其余照读。
        let wrong = json!({ "log_level": "loud", "wifi_iface": "" });
        let read = from_value(&wrong);
        assert_eq!(read.log_level, LevelFilter::INFO);
        assert_eq!(read.wifi_iface, "wlan0");
        assert_eq!(read.addr, "0.0.0.0:80", "没写的地址是默认");

        // 一个空文档就是整套默认，旧文件里多余的键不影响它。
        assert_eq!(from_value(&json!({})), Config::default());
        assert_eq!(from_value(&json!({ "wireless": false })), Config::default());
        assert_eq!(json(&Config::default())["log_level"], "info");
        assert!(json(&Config::default()).get("wireless").is_none());
    }

    /// 文件按写下来的读：页面不提供的尺寸与帧率照样生效。
    #[test]
    fn a_size_written_by_hand_is_served_as_it_is() {
        let edited = json!({
            "screen": { "width": 1024, "height": 768, "fps": 24 }
        });
        assert_eq!(
            from_value(&edited).screen,
            Screen {
                resolution: Some(Resolution {
                    width: 1024,
                    height: 768
                }),
                fps: Some(24)
            },
            "neither is one the page offers, and both are served"
        );

        let half = json!({ "screen": { "width": 1920, "fps": 30 } });
        assert_eq!(
            from_value(&half).screen,
            Screen {
                resolution: None,
                fps: Some(30)
            },
            "a width with no height is not a size"
        );

        let zero = json!({ "screen": { "width": 0, "height": 720, "fps": 0 } });
        assert_eq!(
            from_value(&zero).screen,
            Screen::default(),
            "a zero serves nothing"
        );

        // 空文档，与根本不是这个形状的。
        assert_eq!(from_value(&json!({})).screen, Screen::default());
        assert_eq!(from_value(&json!({ "screen": 3 })).screen, Screen::default());
    }

    /// 读不了的文件就是默认值，不是失败。
    #[test]
    fn a_file_that_cannot_be_read_is_the_defaults() {
        let dir = std::env::temp_dir().join(format!("cp2cl-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let missing = dir.join("not-here.json");
        assert_eq!(read(&missing), Config::default());

        let garbage = dir.join("garbage.json");
        std::fs::write(&garbage, "{ not json").unwrap();
        assert_eq!(read(&garbage), Config::default());

        let good = dir.join("good.json");
        std::fs::write(&good, json(&with(full())).to_string()).unwrap();
        assert_eq!(read(&good), with(full()));

        std::fs::remove_dir_all(&dir).ok();
    }
}
