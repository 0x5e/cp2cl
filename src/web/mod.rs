//! The status page: what the bridge is doing, read from a browser.
//!
//! The page is one file with no build step, and the server is three handlers.
//! That is deliberate: this is a page for looking at a bridge that is already
//! running, and everything it needs has to fit on a board that cannot reach the
//! internet — so both the page and its stylesheet are compiled into the binary
//! rather than fetched.
//!
//! Two paths do the work. `/` is the page, and `/api/status` is the one thing it
//! reads: a single JSON document holding both halves of the bridge, which the
//! page polls. There is no push and no state on this side — the page asks, and
//! the answer is whatever the bridge last reported. See `status` for who reports
//! what.
//!
//! `axum` is here for `axum::serve` and its router, which is less code than
//! parsing a request line by hand and less to get wrong. It used to be here for
//! a WebSocket too, and that half went with the browser client that needed it —
//! see the changelog — so the only feature asked for is HTTP/1.1 over tokio.

mod status;

// The parts of a report that are built by name rather than by assignment. The
// rest of the snapshot is reached through `Status::with` and never named.
pub use status::{AccessPoint, Audio, Mic, Screen, Size, Statistic, Status};

use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;

/// The page, and the stylesheet it names. Both are read at compile time, so
/// there is nothing to install alongside the binary and nothing to fetch over a
/// network the board may not have.
const PAGE: &str = include_str!("index.html");
const WEUI: &str = include_str!("vendor/weui.min.css");

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JSON: &str = "application/json";

/// Where the page is served.
///
/// Not a setting. A page is read by opening the board's address in a browser,
/// and a port to remember is one more thing to get wrong — so it is the port a
/// browser assumes when none is given, on every interface, because which one
/// the reader is on is not something this side can know.
///
/// 80 is below 1024, so binding it takes root, which is how the program runs on
/// the board. Where it is refused the page is what is lost, and the caller says
/// so rather than stopping over it — the streams are the bridge.
pub const ADDR: &str = "0.0.0.0:80";

/// Serves the page and its one endpoint, for as long as the program runs.
///
/// Never returns while the bridge is up: `axum::serve` runs until the listener
/// fails, and a failure there is worth a line rather than the end of the
/// program — the streams are what matter, and a page that has gone quiet is not
/// a reason to take them down.
pub async fn serve(listener: TcpListener, status: Arc<Status>) {
    let app = Router::new()
        .route("/", get(page))
        .route("/index.html", get(page))
        .route("/vendor/weui.min.css", get(stylesheet))
        .route("/api/status", get(api))
        .fallback(not_found)
        .with_state(status);
    if let Err(e) = axum::serve(listener, app).await {
        tracing::warn!("[web] the status page stopped: {e}");
    }
}

async fn page() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, HTML)], PAGE)
}

async fn stylesheet() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, CSS)], WEUI)
}

/// The one endpoint the page reads.
///
/// Never cached: the whole point of it is that the answer is different a moment
/// later, and a browser holding on to it would be showing the state of a bridge
/// that has since moved on.
async fn api(State(status): State<Arc<Status>>) -> Response {
    (
        [
            (header::CONTENT_TYPE, JSON),
            (header::CACHE_CONTROL, "no-store"),
        ],
        status.json().to_string(),
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

    /// The page and its stylesheet both have to be in the binary: a board with
    /// no route to the internet has nothing else to fall back on.
    #[test]
    fn the_page_and_its_stylesheet_are_both_compiled_in() {
        assert!(PAGE.contains("/api/status"), "the page reads the endpoint");
        assert!(
            PAGE.contains("/vendor/weui.min.css"),
            "and names the stylesheet"
        );
        assert!(
            WEUI.contains(".weui-cells"),
            "which is the one that was vendored"
        );
    }

    /// The page is one document and reads one key per half, so the snapshot has
    /// to keep answering under those two names.
    #[test]
    fn the_endpoint_answers_with_both_halves() {
        let status = Status::new();
        let body = status.json();
        assert!(body.get("carlife").is_some());
        assert!(body.get("carplay").is_some());
    }
}
