//! The status page: what the bridge is doing, read from a browser.
//!
//! The page is one file with no build step, and the server is three handlers.
//! That is deliberate: this is a page for looking at a bridge that is already
//! running, and everything it needs has to fit on a board that cannot reach the
//! internet — so both the page and its stylesheet are compiled into the binary
//! rather than fetched.
//!
//! Two paths do the work. `/` is the page, and `/api/status` is the one thing it
//! reads: a single JSON document holding every part of the bridge, which the
//! page polls. There is no push and no state on this side — the page asks, and
//! the answer is whatever the bridge last reported. See `status` for who reports
//! what.
//!
//! `/api/cover` is the one answer that is not JSON: the song's picture, which
//! the page draws rather than reads. It is fetched by the name of the cover —
//! a hash of the picture — so a page that has one keeps it and asks again only
//! when the car has another, and an answer is only ever the picture that was
//! asked for.
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

use axum::extract::{RawQuery, State};
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
const JPEG: &str = "image/jpeg";

/// A cover is fetched by the name of the cover, so an answer is good for as
/// long as that name is — which is until the car is given another picture.
const COVER_CACHE: &str = "public, max-age=31536000, immutable";

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
        .route("/api/cover", get(cover))
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

/// The picture on the car's panel, which is the one answer here that is not
/// JSON.
///
/// Its own path rather than a field in the snapshot: the page already has the
/// picture it drew, and a status poll every two seconds is no place for thirty
/// kilobytes of JPEG. The bytes are copied out from under the lock so the page
/// can be answered without holding it.
///
/// Asked for by name, and answered only under the name of the picture that is
/// held. A name that is not the current one is a page asking for a cover it
/// drew before the song changed, and those bytes are gone — so it is answered
/// with nothing rather than with whatever is here now, which would put one
/// song's picture under another song's name and leave it in a cache that never
/// expires.
async fn cover(State(status): State<Arc<Status>>, RawQuery(query): RawQuery) -> Response {
    let Some(cover) = status.cover() else {
        return (StatusCode::NOT_FOUND, "no cover\n").into_response();
    };
    if named(&query) != Some(cover.id) {
        return (StatusCode::NOT_FOUND, "no cover\n").into_response();
    }
    (
        [
            (header::CONTENT_TYPE, JPEG),
            (header::CACHE_CONTROL, COVER_CACHE),
        ],
        (*cover.bytes).clone(),
    )
        .into_response()
}

/// The name a request asks for, out of its query string.
///
/// One parameter, so one split rather than a parser: `id=<name>`, sixteen hex
/// digits as the snapshot writes them. Anything else — another parameter, a
/// page's own `undefined`, nothing at all — is no name, and no name is no
/// cover.
fn named(query: &Option<String>) -> Option<u64> {
    let asked = query.as_deref()?;
    let name = asked.split('&').find_map(|pair| pair.strip_prefix("id="))?;
    u64::from_str_radix(name, 16).ok()
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

    /// The page is one document and reads one key per part, so the snapshot has
    /// to keep answering under those names.
    #[test]
    fn the_endpoint_answers_with_every_part() {
        let status = Status::new();
        let body = status.json();
        assert!(body.get("carlife").is_some());
        assert!(body.get("carplay").is_some());
        assert!(body.get("now_playing").is_some());
    }

    /// What is playing is its own part of the page, and its cover comes from a
    /// path of its own: a picture is drawn, not read out of the JSON.
    #[test]
    fn the_page_reads_the_song_from_its_own_part() {
        assert!(PAGE.contains("data.now_playing"), "the page reads the part");
        assert!(
            PAGE.contains("/api/cover?id="),
            "and fetches the cover itself, by the name it was given"
        );
        assert!(
            PAGE.contains("BOTH_UP"),
            "and shows the section only while both ends are up"
        );
        assert!(
            PAGE.contains("duration_s"),
            "the page writes the length itself, from seconds"
        );
        let body = Status::new().json();
        assert!(body["now_playing"]["duration_s"].is_null());
    }

    /// A cover is served until another replaces it, and it is named by what is
    /// in it: the same picture twice costs the page one fetch, a different one
    /// never reuses the name, and an empty one is no picture at all.
    #[test]
    fn a_cover_is_named_by_what_is_in_it() {
        let status = Status::new();
        assert!(status.cover().is_none(), "nothing to draw yet");

        status.cover_changed(vec![0xff, 0xd8, 0xff]);
        let first = status.json()["now_playing"]["cover"]["id"].clone();
        assert_eq!(status.cover().map(|c| c.bytes.len()), Some(3));

        // The same bytes again are the same picture, so the page keeps it.
        status.cover_changed(vec![0xff, 0xd8, 0xff]);
        assert_eq!(status.json()["now_playing"]["cover"]["id"], first);

        // Another picture is another name.
        status.cover_changed(vec![0x42]);
        assert_ne!(status.json()["now_playing"]["cover"]["id"], first);

        // An empty one is the phone saying this song has no picture.
        status.cover_changed(Vec::new());
        assert!(status.cover().is_none());
        assert!(status.json()["now_playing"]["cover"].is_null());

        // And a new session starts with nothing, which lets the bytes go.
        status.cover_changed(vec![1, 2, 3]);
        status.now_playing_cleared();
        assert!(status.cover().is_none());
        assert!(status.json()["now_playing"]["song"].is_null());
    }

    /// A cover is answered only under the name of the picture that is held.
    ///
    /// The name is the picture, so an answer under another name would be one
    /// song's cover drawn as another's — and kept under that name for a year by
    /// the browser. A page whose cover had no name asks for `id=undefined`; one
    /// that kept a name from a song that has finished asks for that. Neither is
    /// this picture, so neither is given it.
    #[tokio::test]
    async fn a_cover_is_only_served_under_its_own_name() {
        use axum::body::to_bytes;

        let status = Arc::new(Status::new());
        let ask = |query: Option<&str>| {
            cover(State(status.clone()), RawQuery(query.map(str::to_owned)))
        };

        // Nothing to draw yet, whatever is asked for.
        assert_eq!(ask(Some("id=0")).await.status(), StatusCode::NOT_FOUND);

        status.cover_changed(vec![0xff, 0xd8, 0xff]);
        let name = format!("{:016x}", status.cover().unwrap().id);

        let answer = ask(Some(&format!("id={name}"))).await;
        assert_eq!(answer.status(), StatusCode::OK);
        let bytes = to_bytes(answer.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&bytes[..], &[0xff, 0xd8, 0xff]);

        for query in [Some("id=undefined"), Some("n=0"), Some("id=0"), None] {
            assert_eq!(
                ask(query).await.status(),
                StatusCode::NOT_FOUND,
                "{query:?} is not the name of the cover"
            );
        }
    }

    /// A coding is a constant of the receiver, not of the page: the answer has to
    /// carry it, and the page has to read it from there.
    #[test]
    fn the_page_reads_the_coding_from_the_answer() {
        assert!(PAGE.contains("d.video.codec"), "the page reads the field");
        assert!(!PAGE.contains("H.264"), "and names no coding itself");
        let body = Status::new().json();
        assert_eq!(
            body["carplay"]["video"]["codec"],
            crate::airplay::screen::CODEC,
            "which is what the snapshot carries"
        );
    }

    /// The panel is the car's, so the page shows its physical size on the car's
    /// side, next to the size the car named, and reads it from the answer.
    #[test]
    fn the_page_reads_the_panel_size_from_the_cars_half() {
        assert!(PAGE.contains("d.physical"), "the page reads the field");
        let status = Status::new();
        status.with(|s| s.carlife.physical = Some((407, 229)));
        let body = status.json();
        assert_eq!(body["carlife"]["physical"]["width_mm"], 407);
        assert_eq!(body["carlife"]["physical"]["height_mm"], 229);
    }
}
