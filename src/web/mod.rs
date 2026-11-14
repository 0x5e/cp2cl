//! The status page: what the bridge is doing, read from a browser.
//!
//! The page is one file with no build step, and it has three tabs — 状态, 日志
//! and 设置 — so there is one module per tab and this one is the shell they hang
//! off: the router, the page itself, its stylesheet, and the channel table.
//!
//! - [`status`] is the snapshot the page polls every two seconds, and the cover
//!   picture it draws rather than reads.
//! - [`log`] is the copy under `/var/log`, read forwards from where the page got
//!   to.
//! - [`settings`] is `config.json` seen from the page, and the buttons under
//!   it that put the machine back to a known state.
//!
//! Everything is compiled into the binary rather than fetched: this is a page
//! for looking at a bridge that is already running, and it has to work on a
//! board that cannot reach the internet.
//!
//! `axum` is here for `axum::serve` and its router, which is less code than
//! parsing a request line by hand and less to get wrong. It used to be here for
//! a WebSocket too, and that half went with the browser client that needed it —
//! see the changelog — so the only feature asked for is HTTP/1.1 over tokio.

pub mod log;
pub mod settings;
pub mod status;

// The snapshot the page reads, built by name rather than by assignment.
pub use status::Status;

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// The page, and the stylesheet it names. Both live in `assets/web`, so they are
/// built into the binary with everything else: there is nothing to install
/// alongside it and nothing to fetch over a network the board may not have.
const PAGE_NAME: &str = "web/index.html";
const WEUI_NAME: &str = "web/vendor/weui.min.css";
/// The channel table, which the page reads the make of a head unit out of. The
/// same file `/info` takes its labels from: one table, so a make added by hand
/// is named on the page without a second copy to keep in step.
const CHANNELS_NAME: &str = "carlife_channel_map.json";

/// One of those, which is in the binary because `build.rs` put every file under
/// `assets/` there. A name it does not have is a build that lost a file, and
/// that is worth saying out loud rather than answering with nothing.
pub(crate) fn embedded(name: &str) -> &'static str {
    crate::assets::text(name).unwrap_or_else(|| panic!("assets/{name} is not in the binary"))
}

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
pub(crate) const JSON: &str = "application/json";
pub(crate) const PLAIN: &str = "text/plain; charset=utf-8";

/// Serves the page and the paths behind its three tabs, for as long as the
/// program runs.
///
/// The listener is bound by the caller, from the `addr` setting: a page is read
/// by opening the board's address in a browser, and that address is a choice
/// rather than something this side can know.
///
/// `replug` is the channel the wired half is asked for one through, and
/// `adapter` is the controller the pairings belong to — the two things a reset
/// from the page needs. Both are the button's own: see [`settings::reset`].
///
/// Never returns while the bridge is up: `axum::serve` runs until the listener
/// fails, and a failure there is worth a line rather than the end of the
/// program — the streams are what matter, and a page that has gone quiet is not
/// a reason to take them down.
pub async fn serve(
    listener: TcpListener,
    status: Arc<Status>,
    replug: mpsc::Sender<()>,
    adapter: String,
) {
    let app = Router::new()
        .route("/", get(page))
        .route("/index.html", get(page))
        .route("/vendor/weui.min.css", get(stylesheet))
        .route("/api/status", get(status::api))
        .route("/api/cover", get(status::cover))
        .route("/api/channels", get(channels))
        .route("/api/log", get(log::log))
        .route("/api/log/download", get(log::download))
        .route("/api/settings", get(settings::get).post(settings::save))
        .route("/api/restart", post(settings::restart))
        .route("/api/reset", post(settings::factory_reset))
        .route("/api/reboot", post(settings::reboot))
        .fallback(not_found)
        .layer(Extension(replug))
        .layer(Extension(adapter))
        .with_state(status);
    if let Err(e) = axum::serve(listener, app).await {
        tracing::warn!("web.serve_failed err={e}");
    }
}

async fn page() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, HTML)], embedded(PAGE_NAME))
}

async fn stylesheet() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, CSS)], embedded(WEUI_NAME))
}

/// The channel table as the binary carries it: one entry per make, each with the
/// channel numbers it answers on and, where there is one, the label the phone
/// draws. Answered as it is written rather than as a table built here, so what a
/// browser is given is what `/info` is given.
///
/// The page asks for it once, when it is opened: the table is built in and does
/// not change while the program runs.
async fn channels() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, JSON)], embedded(CHANNELS_NAME))
}

/// A JSON answer, never cached: all of these are about something that changes.
pub(crate) fn json_answer(status: StatusCode, body: serde_json::Value) -> Response {
    (
        [
            (header::CONTENT_TYPE, JSON),
            (header::CACHE_CONTROL, "no-store"),
        ],
        (status, body.to_string()),
    )
        .into_response()
}

/// Anything else, including the `/favicon.ico` every browser asks for.
async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "not found\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The page and its stylesheet, as the binary carries them.
    fn page_text() -> &'static str {
        embedded(PAGE_NAME)
    }

    fn weui_text() -> &'static str {
        embedded(WEUI_NAME)
    }

    /// The page and its stylesheet both have to be in the binary: a board with
    /// no route to the internet has nothing else to fall back on.
    #[test]
    fn the_page_and_its_stylesheet_are_both_compiled_in() {
        assert!(
            page_text().contains("/api/status"),
            "the page reads the endpoint"
        );
        assert!(
            page_text().contains("/vendor/weui.min.css"),
            "and names the stylesheet"
        );
        assert!(
            weui_text().contains(".weui-cells"),
            "which is the one that was vendored"
        );
    }

    /// The channel table is the binary's, and the page keeps no copy: a make
    /// added to the file by hand is named on the page, and on the phone, from
    /// the one table — there is nothing else to remember to edit.
    #[test]
    fn the_page_reads_the_channel_table_from_the_binary() {
        assert!(
            page_text().contains("/api/channels"),
            "the page asks for the table"
        );
        assert!(
            !page_text().contains("20062100"),
            "and carries no numbers of its own"
        );
        assert!(
            embedded(CHANNELS_NAME).contains("20062100"),
            "which the binary has"
        );
    }

    /// 设置页读的写的都是同一个文件，两个下拉的选项跟着答案一起来：页面里不列
    /// 一份自己的，所以能选什么和后端收什么是同一份。
    #[test]
    fn the_page_reads_and_writes_the_settings_file() {
        for said in ["分辨率", "帧率", "重置", "保存", "保存成功，重启生效"] {
            assert!(page_text().contains(said), "the page says {said}");
        }
        assert!(
            page_text().contains("/api/settings"),
            "one path, read and written"
        );
        assert!(
            page_text().contains("method: \"POST\""),
            "and it is written rather than only read"
        );
        assert!(
            !page_text().contains("1920x1080"),
            "the choices come with the answer, not out of the page"
        );
        assert!(
            page_text().contains("data-manual"),
            "a value the choices do not list is shown as it is, not as 默认"
        );
    }

    #[test]
    fn the_page_restarts_the_service() {
        assert!(
            page_text().contains("重启服务"),
            "the page names the button"
        );
        assert!(
            page_text().contains("/api/restart"),
            "and asks for the restart"
        );
        let save = page_text().find("id=\"set-save\"").expect("the save button");
        let restart = page_text().find("id=\"set-restart\"").expect("the restart button");
        assert!(
            restart > save,
            "the restart sits under the two buttons it does not save"
        );
    }

    /// The two buttons that cannot be taken back ask through one dialog, which
    /// is part of the markup rather than `confirm` — a browser can be set not to
    /// show that one at all. The order on the page is the order of how much each
    /// takes away: the service, the board, every pairing.
    #[test]
    fn the_page_asks_before_a_reboot_or_a_reset() {
        for (label, path) in [
            ("重启服务", "/api/restart"),
            ("重启设备", "/api/reboot"),
            ("恢复出厂设置", "/api/reset"),
        ] {
            assert!(page_text().contains(label), "the page names {label}");
            assert!(page_text().contains(path), "and asks for it through {path}");
        }
        let restart = page_text().find("id=\"set-restart\"").expect("the restart button");
        let reboot = page_text().find("id=\"set-reboot\"").expect("the reboot button");
        let reset = page_text().find("id=\"set-reset-all\"").expect("the reset button");
        assert!(restart < reboot && reboot < reset, "restart, reboot, reset");
        for id in ["confirm-mask", "confirm-box", "confirm-ok", "confirm-cancel"] {
            assert!(
                page_text().contains(&format!("id=\"{id}\"")),
                "the dialog has {id}"
            );
        }
    }

    /// The page is three tabs, and the log one is the copy on disk read
    /// forwards: it asks from where it left off, and starts over when the
    /// copy it was reading has been rolled away from under it.
    #[test]
    fn the_page_has_a_tab_for_the_log_and_reads_it_forward() {
        for label in ["状态", "日志", "设置"] {
            assert!(page_text().contains(label), "the tabbar names {label}");
        }
        assert!(
            page_text().contains("/api/log"),
            "the log tab reads the copy"
        );
        assert!(
            page_text().contains("?from="),
            "from where it left off, not from the beginning"
        );
        assert!(
            page_text().contains("data.reset"),
            "and starts over when the copy has been rolled"
        );
        assert!(
            page_text().contains("stamp.className = \"tm\""),
            "the stamp of a line is greyed, as on the terminal"
        );
        assert!(
            page_text().contains("level.className = \"lv \" + LEVELS[found[2]]"),
            "and its level is coloured by which level it is"
        );
        assert!(
            page_text().contains("id=\"log-level\""),
            "and the lowest level it shows can be picked"
        );
        assert!(
            page_text().contains("id=\"log-wrap\""),
            "and a line kept on one row instead of wrapped"
        );
        assert!(
            page_text().contains("/api/log/download"),
            "and the whole of it can be taken away"
        );
    }
}
