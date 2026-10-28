//! `/etc/cp2cl/config.json`: the screen the settings page picks, read once at
//! startup. Either field may be absent, and an absent one is 默认 — the channel
//! table's value, or the car's where the table has none. See
//! `carlife::session::screen_in_place_of`.
//!
//! Read as it is written: a size the page does not offer is served all the same.
//! Only half a pair or a zero is dropped. The file is created at startup when it
//! is not there.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// The file, under [`crate::config::DATA_DIR`].
const FILE: &str = "config.json";

/// The sizes the page offers: width, height, and the shape the pair is. They are
/// the four `assets/guide` is built at.
const RESOLUTIONS: [(u32, u32, &str); 4] = [
    (1280, 720, "16:9"),
    (1920, 1080, "16:9"),
    (1280, 480, "24:9"),
    (1920, 720, "24:9"),
];

/// The rates the page offers.
const RATES: [u32; 2] = [30, 60];

/// A size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

/// What the file says, with every field optional: `None` is 默认.
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

/// Where the file is.
fn path() -> PathBuf {
    crate::config::data_file(FILE)
}

/// The file as it stands; anything unreadable is the defaults.
fn load() -> Screen {
    read(&path())
}

/// The same, creating the file when it is not there.
pub fn load_or_create() -> Screen {
    let path = path();
    if !path.exists() {
        match save(&Screen::default()) {
            Ok(()) => tracing::info!("settings.created path={}", path.display()),
            // Read-only `/etc` is a machine that runs on the defaults.
            Err(e) => tracing::warn!("settings.create_failed path={} err={e}", path.display()),
        }
    }
    read(&path)
}

/// The file, replaced in one step.
pub fn save(screen: &Screen) -> std::io::Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(&json(screen))?;
    text.push('\n');
    // Beside it rather than in `/tmp`: a rename across filesystems is a copy.
    let staged = path.with_extension("json.tmp");
    std::fs::write(&staged, text)?;
    std::fs::rename(&staged, &path)
}

fn read(path: &Path) -> Screen {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Screen::default(),
        Err(e) => {
            tracing::warn!("settings.read_failed path={} err={e}", path.display());
            return Screen::default();
        }
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        tracing::warn!("settings.read_failed path={} err=not_json", path.display());
        return Screen::default();
    };
    from_value(&value)
}

/// The settings a JSON document holds. Nothing is checked against what the page
/// offers: only half a pair or a zero is dropped, because those serve nothing.
fn from_value(value: &Value) -> Screen {
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
    Screen { resolution, fps }
}

fn number(object: &Value, key: &str) -> Option<u32> {
    object[key]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
}

/// The file's shape, as it is written.
fn json(screen: &Screen) -> Value {
    json!({
        "screen": {
            "width": screen.resolution.map(|r| r.width),
            "height": screen.resolution.map(|r| r.height),
            "fps": screen.fps,
        }
    })
}

/// The choices the page builds its two lists from, sent with the settings so
/// there is one list rather than two.
fn options() -> Value {
    json!({
        "resolutions": RESOLUTIONS
            .iter()
            .map(|(width, height, shape)| json!({
                "width": width,
                "height": height,
                "label": format!("{width}x{height} ({shape})"),
            }))
            .collect::<Vec<_>>(),
        "rates": RATES,
    })
}

/// What `GET /api/settings` answers: the file as it stands, and the choices.
pub fn answer() -> Value {
    let mut body = json(&load());
    body["options"] = options();
    body
}

/// What a `POST` body asks for, or why it cannot be served: the same values the
/// file can hold, so a size put in by hand can be saved back.
pub fn parse(body: &[u8]) -> Result<Screen, String> {
    let value: Value = serde_json::from_slice(body).map_err(|_| "请求体不是 JSON".to_string())?;
    if !value.is_object() {
        return Err("请求体必须是一个对象".to_string());
    }
    let screen = &value["screen"];
    if !screen.is_null() && !screen.is_object() {
        return Err("screen 必须是一个对象".to_string());
    }

    let resolution = match (present(screen, "width")?, present(screen, "height")?) {
        (None, None) => None,
        (Some(width), Some(height)) if width > 0 && height > 0 => {
            Some(Resolution { width, height })
        }
        (Some(_), Some(_)) => return Err("分辨率不能是 0".to_string()),
        _ => return Err("分辨率的宽和高要一起给".to_string()),
    };

    let fps = match present(screen, "fps")? {
        Some(fps) if fps > 0 => Some(fps),
        Some(_) => return Err("帧率不能是 0".to_string()),
        None => None,
    };

    Ok(Screen { resolution, fps })
}

/// One field, which may be absent or `null` and must be a number otherwise.
fn present(screen: &Value, key: &str) -> Result<Option<u32>, String> {
    match &screen[key] {
        Value::Null => Ok(None),
        value => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("{key} 必须是整数")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Screen {
        Screen {
            resolution: Some(Resolution {
                width: 1920,
                height: 720,
            }),
            fps: Some(60),
        }
    }

    /// 写下去的就是读回来的；留空的那一项不在文件里。
    #[test]
    fn what_is_written_is_what_is_read() {
        assert_eq!(from_value(&json(&full())), full());

        let rate_only = Screen {
            resolution: None,
            fps: Some(30),
        };
        assert_eq!(from_value(&json(&rate_only)), rate_only);
        assert_eq!(json(&rate_only)["screen"]["width"], Value::Null);

        let nothing = Screen::default();
        assert_eq!(from_value(&json(&nothing)), nothing);
        assert!(nothing.is_empty());

        // 日志里的一行，写法与渠道表一致。
        assert_eq!(full().to_string(), "1920x720@60");
        assert_eq!(nothing.to_string(), "default@default");
    }

    /// 文件按写下来的读：页面不提供的尺寸与帧率照样生效。
    #[test]
    fn a_size_written_by_hand_is_served_as_it_is() {
        let edited = json!({
            "screen": { "width": 1024, "height": 768, "fps": 24 }
        });
        assert_eq!(
            from_value(&edited),
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
            from_value(&half),
            Screen {
                resolution: None,
                fps: Some(30)
            },
            "a width with no height is not a size"
        );

        let zero = json!({ "screen": { "width": 0, "height": 720, "fps": 0 } });
        assert_eq!(
            from_value(&zero),
            Screen::default(),
            "a zero serves nothing"
        );

        // 空文档，与根本不是这个形状的。
        assert_eq!(from_value(&json!({})), Screen::default());
        assert_eq!(from_value(&json!({ "screen": 3 })), Screen::default());
    }

    /// 页面能写下来的就是文件能放下的：手改的值存得回去。
    #[test]
    fn the_page_writes_what_can_be_a_setting() {
        let body = |value: Value| serde_json::to_vec(&value).unwrap();

        assert_eq!(parse(&body(json(&full()))), Ok(full()));
        assert_eq!(parse(&body(json!({ "screen": {} }))), Ok(Screen::default()));
        assert_eq!(
            parse(&body(json!({ "screen": { "fps": 30 } }))),
            Ok(Screen {
                resolution: None,
                fps: Some(30)
            }),
            "an absent size is 默认"
        );

        let edited = Screen {
            resolution: Some(Resolution {
                width: 1024,
                height: 768,
            }),
            fps: Some(24),
        };
        assert_eq!(
            parse(&body(json(&edited))),
            Ok(edited),
            "a size and a rate the page does not list are the file's own"
        );

        for bad in [
            json!({ "screen": { "width": 1920 } }),
            json!({ "screen": { "height": 720 } }),
            json!({ "screen": { "width": 0, "height": 720 } }),
            json!({ "screen": { "width": 1920, "height": 720, "fps": 0 } }),
            json!({ "screen": { "width": "1920", "height": "720" } }),
            json!({ "screen": 3 }),
            json!([1, 2]),
        ] {
            assert!(parse(&body(bad.clone())).is_err(), "{bad} is refused");
        }
        assert!(parse(b"not json").is_err());
    }

    /// 可选项就是引导页的那四个尺寸，少一个就要被拉长。
    #[test]
    fn the_choices_are_the_ones_the_guide_is_built_at() {
        let options = options();
        let resolutions = options["resolutions"].as_array().expect("a list");
        assert_eq!(resolutions.len(), RESOLUTIONS.len());
        assert_eq!(resolutions[0]["label"], "1280x720 (16:9)");
        assert_eq!(resolutions[3]["label"], "1920x720 (24:9)");
        assert_eq!(options["rates"], json!([30, 60]));

        for (width, height, _) in RESOLUTIONS {
            let name = format!("guide/{width}x{height}.h264");
            assert!(
                crate::assets::get(&name).is_some(),
                "{name} is in the binary"
            );
        }
    }

    /// 读不了的文件就是默认值，不是失败。
    #[test]
    fn a_file_that_cannot_be_read_is_the_defaults() {
        let dir = std::env::temp_dir().join(format!("cp2cl-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let missing = dir.join("not-here.json");
        assert_eq!(read(&missing), Screen::default());

        let garbage = dir.join("garbage.json");
        std::fs::write(&garbage, "{ not json").unwrap();
        assert_eq!(read(&garbage), Screen::default());

        let good = dir.join("good.json");
        std::fs::write(&good, json(&full()).to_string()).unwrap();
        assert_eq!(read(&good), full());

        std::fs::remove_dir_all(&dir).ok();
    }
}
