//! The settings tab: the screen `config.json` holds, and the three buttons
//! under it that put the machine back to a known state.
//!
//! The file itself is read and written by [`crate::config`]; what is here is the
//! page's face on it — the choices its two lists are built from, the check a
//! body goes through before it becomes a file, and the actions that restart the
//! link, the board, or every pairing at once.
//!
//! The three actions differ in what they take away, and that is the order the
//! page shows them in: the service (one replug), the board (a reboot), every
//! pairing (a replug after the pairings are gone).

use axum::body::Bytes;
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::json_answer;
use crate::config::{self, Resolution, Screen};

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

/// How long the answer to a reboot has to get out before the machine goes down.
/// The reply is written first and the reboot follows it, because a page that is
/// answered after the fact is a page that never hears it.
const REBOOT_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// The settings as they stand, and the choices the page builds its lists from.
pub async fn get() -> Response {
    json_answer(StatusCode::OK, answer())
}

/// The same file, written. A body that is not a size or a rate is a `400` rather
/// than a file nobody can serve.
pub async fn save(body: Bytes) -> Response {
    let screen = match parse(&body) {
        Ok(screen) => screen,
        Err(e) => {
            tracing::warn!("web.settings_refused err={e}");
            return json_answer(StatusCode::BAD_REQUEST, json!({ "error": e }));
        }
    };
    match save_screen(&screen) {
        Ok(()) => {
            tracing::info!("web.settings_saved screen={screen} effect=restart");
            json_answer(StatusCode::OK, json!({ "ok": true }))
        }
        Err(e) => {
            tracing::warn!("web.settings_save_failed err={e}");
            json_answer(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("写入失败：{e}") }),
            )
        }
    }
}

/// One replug of the wired link, which is what the car sees as the accessory
/// coming back.
pub async fn restart(Extension(replug): Extension<mpsc::Sender<()>>) -> Response {
    tracing::info!("web.restart_requested");
    match replug.try_send(()) {
        Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {
            json_answer(StatusCode::OK, json!({ "ok": true }))
        }
        Err(mpsc::error::TrySendError::Closed(())) => {
            tracing::warn!("web.restart_refused err=carlife_half_not_running");
            json_answer(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "CarLife 未在运行，无法重启" }),
            )
        }
    }
}

/// The reset a held button does, asked for from the page.
///
/// Answers success or the reason it did not work, and nothing else: what the
/// page has to say about it is one line either way.
pub async fn factory_reset(
    Extension(replug): Extension<mpsc::Sender<()>>,
    Extension(adapter): Extension<String>,
) -> Response {
    tracing::info!("web.reset_requested");
    match reset(&adapter, &replug, "web").await {
        Ok(()) => json_answer(StatusCode::OK, json!({ "ok": true })),
        Err(Refusal::Unpair(e)) => {
            tracing::warn!("web.reset_failed err={e}");
            json_answer(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("未能解除配对：{e}") }),
            )
        }
        Err(Refusal::NotRunning) => {
            tracing::warn!("web.reset_failed err=carlife_half_not_running");
            json_answer(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "CarLife 未在运行，无法重连" }),
            )
        }
    }
}

/// The whole board, not this program: the answer is written first and the
/// reboot follows it, so the page hears an answer rather than a dropped
/// connection.
///
/// Nothing is put back on the way down — no `stopping`, no lights, no channel
/// closed on purpose. A reboot is not a stop, and the next start is the same as
/// a power cut; what it does have is the flush, which [`now`] does.
pub async fn reboot() -> Response {
    tracing::info!("web.reboot_requested");
    tokio::spawn(async {
        tokio::time::sleep(REBOOT_DELAY).await;
        // The last line this boot writes, and the reason it is a warn if it
        // fails: the page has already been told the machine is going.
        tracing::info!("main.reboot scope=device");
        if let Err(e) = now() {
            tracing::error!("main.reboot_failed err={e}");
        }
    });
    json_answer(StatusCode::OK, json!({ "ok": true }))
}

/// Why a reset did not do what was asked.
pub enum Refusal {
    /// BlueZ could not be asked to remove the pairings. The string is what it
    /// said, which is the only thing that tells a missing controller from a bus
    /// that is not answering.
    Unpair(String),
    /// The wired half is not running, so there is nothing to re-plug.
    NotRunning,
}

/// Unpairs every phone, then asks the wired half for one replug.
///
/// Both are tried even when the first fails: a replug is worth asking for
/// either way, and a phone that pairs after it is answered like any other. The
/// answer is the first step that did not work.
///
/// The page asks for it, and so does a hold on the board's button — the state
/// that keeps a phone from connecting is also the state that leaves the page as
/// the only way in. `reason` is the only difference between the two callers.
///
/// Not a restart of the program: a hold, or a click, taken while someone is
/// watching the panel should cost the link a second, not the session.
pub async fn reset(adapter: &str, replug: &mpsc::Sender<()>, reason: &str) -> Result<(), Refusal> {
    // A count of zero is a reset that found nothing to clear, which the
    // per-device line inside `unpair_all` is the detail of.
    let unpaired = match carplay_hu::unpair_all(adapter).await {
        Ok(count) => {
            tracing::info!("bt.unpaired count={count} adapter={adapter}");
            Ok(())
        }
        Err(e) => {
            tracing::warn!("bt.unpaired_unavailable adapter={adapter} err={e}");
            Err(Refusal::Unpair(e.to_string()))
        }
    };
    let replugged = match replug.try_send(()) {
        // A full channel is one replug already on its way, which is what this
        // would have asked for.
        Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {
            tracing::info!("main.replug reason={reason}");
            true
        }
        Err(mpsc::error::TrySendError::Closed(())) => {
            tracing::warn!("main.replug_refused err=carlife_half_not_running");
            false
        }
    };
    unpaired?;
    if !replugged {
        return Err(Refusal::NotRunning);
    }
    Ok(())
}

/// Reboots the board. On success this does not return.
fn now() -> std::io::Result<()> {
    // `RB_AUTOBOOT` is the ordinary restart. `sync` is what makes what was
    // written to the card part of the next boot rather than lost with this one.
    unsafe {
        libc::sync();
        if libc::reboot(libc::RB_AUTOBOOT) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The file with the page's screen in it. The page edits a size and a rate and
/// nothing else, so everything else in the file is kept as it stands.
fn save_screen(screen: &Screen) -> std::io::Result<()> {
    let mut config = config::load();
    config.screen = *screen;
    config::save(&config)
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
fn answer() -> Value {
    let mut body = config::json(&config::load());
    body["options"] = options();
    body
}

/// What a `POST` body asks for, or why it cannot be served: the same values the
/// file can hold, so a size put in by hand can be saved back.
fn parse(body: &[u8]) -> Result<Screen, String> {
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

    /// 一份只有屏幕设成 `screen` 的文档。
    fn written(screen: Screen) -> Value {
        config::json(&config::Config {
            screen,
            ..config::Config::default()
        })
    }

    /// 页面能写下来的就是文件能放下的：手改的值存得回去。
    #[test]
    fn the_page_writes_what_can_be_a_setting() {
        let body = |value: Value| serde_json::to_vec(&value).unwrap();

        let full = Screen {
            resolution: Some(Resolution {
                width: 1920,
                height: 720,
            }),
            fps: Some(60),
        };
        assert_eq!(parse(&body(written(full))), Ok(full));
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
            parse(&body(written(edited))),
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

    /// What the handler is answerable for is the replug: the unpairing goes to
    /// BlueZ, which no test has, so what these answer with here is the failure
    /// — see the handler's own arms for what a page is told.
    #[tokio::test]
    async fn the_page_reset_asks_for_one_replug() {
        let (replug, mut asked) = mpsc::channel(1);
        let _ = factory_reset(Extension(replug), Extension("hci0".to_string())).await;
        assert_eq!(asked.try_recv(), Ok(()), "one replug was asked for");
    }

    /// The restart is one replug, asked for once, and refused when there is
    /// nothing to ask.
    #[tokio::test]
    async fn the_page_restart_asks_for_one_replug() {
        let (replug, mut asked) = mpsc::channel(1);
        let answer = restart(Extension(replug)).await;
        assert_eq!(answer.status(), StatusCode::OK);
        assert_eq!(asked.try_recv(), Ok(()), "one replug was asked for");

        let (replug, mut asked) = mpsc::channel(1);
        replug.try_send(()).expect("room for one");
        let answer = restart(Extension(replug)).await;
        assert_eq!(answer.status(), StatusCode::OK);
        assert_eq!(asked.try_recv(), Ok(()), "and not a second one");

        let (replug, asked) = mpsc::channel(1);
        drop(asked);
        let answer = restart(Extension(replug)).await;
        assert_eq!(answer.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
