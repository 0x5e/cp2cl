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
//! what, and `device` for the one part of it that nobody reports: the board's own
//! CPU and memory, sampled on a task of its own.
//!
//! `/api/channels` is the channel table, asked for once when the page is opened.
//! It is the same file `/info` takes its labels from, handed over as it is
//! written: the page names a head unit's make from it, and a table compiled into
//! the binary is not one a second copy in the page could keep up with.
//!
//! `/api/cover` is the one answer that is not JSON: the song's picture, which
//! the page draws rather than reads. It is fetched by the name of the cover —
//! a hash of the picture — so a page that has one keeps it and asks again only
//! when the car has another, and an answer is only ever the picture that was
//! asked for.
//!
//! `/api/log` is the other one: the copy of the log the program keeps under
//! `/var/log`, asked for by how far the page has already read rather than in full.
//! It is a file that is always being added to, so a page that wants to keep up
//! with it has to say where it got to — see `log`. `/api/log/download` is the
//! same copy as one file, for taking off the board.
//!
//! `axum` is here for `axum::serve` and its router, which is less code than
//! parsing a request line by hand and less to get wrong. It used to be here for
//! a WebSocket too, and that half went with the browser client that needed it —
//! see the changelog — so the only feature asked for is HTTP/1.1 over tokio.

pub mod device;
mod status;

// The parts of a report that are built by name rather than by assignment. The
// rest of the snapshot is reached through `Status::with` and never named.
pub use status::{AccessPoint, Audio, Mic, Screen, Size, Statistic, Status};

use std::path::Path;
use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::TcpListener;

use crate::logging;

/// The page, and the stylesheet it names. Both live in `assets/web`, so they are
/// built into the binary with everything else: there is nothing to install
/// alongside it and nothing to fetch over a network the board may not have.
const PAGE_NAME: &str = "web/index.html";
const WEUI_NAME: &str = "web/vendor/weui.min.css";
/// The channel table, which the page reads the make of a head unit out of. The
/// same file `/info` takes its labels from: one table, so a make added by hand
/// is named on the page without a second copy to keep in step.
const CHANNELS_NAME: &str = "carlife_channel_map.json";

/// One of those two, which is in the binary because `build.rs` put every file
/// under `assets/` there. A name it does not have is a build that lost a file,
/// and that is worth saying out loud rather than answering with nothing.
fn embedded(name: &str) -> &'static str {
    crate::assets::text(name).unwrap_or_else(|| panic!("assets/{name} is not in the binary"))
}

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JSON: &str = "application/json";
const JPEG: &str = "image/jpeg";
const PLAIN: &str = "text/plain; charset=utf-8";

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
        .route("/api/channels", get(channels))
        .route("/api/cover", get(cover))
        .route("/api/log", get(log))
        .route("/api/log/download", get(download))
        .fallback(not_found)
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

/// The copy of the log, from the byte the page has read up to.
///
/// Asked for by offset rather than in full: the copy is written continuously
/// and the page asks every two seconds, so what goes over the wire is only
/// what has been written since the last answer.
///
/// `reset` is the one way an offset can turn out to be nowhere: the copy was
/// rolled over and is now shorter than where the page had got to, or the
/// program has run again. Either way the page is told to drop what it had, and
/// is given the copy from its beginning — which is what a page that asks for
/// nothing at all is given too.
async fn log(RawQuery(query): RawQuery) -> Response {
    let (reset, from, text) = copied(Path::new(logging::PATH), from(&query)).await;
    (
        [
            (header::CONTENT_TYPE, JSON),
            (header::CACHE_CONTROL, "no-store"),
        ],
        json!({
            "reset": reset,
            "from": from,
            "next": from + text.len() as u64,
            "text": text,
        })
        .to_string(),
    )
        .into_response()
}

/// What has been written to the copy since `from`, which is `None` when the
/// page has not asked for a place in it yet.
///
/// No offset means the whole of the copy, not the end of it. The copy is capped
/// at four megabytes, so handing all of it over is bounded, and a page that
/// opens at the tail would only ever be able to read forward from wherever it
/// was opened.
async fn copied(path: &Path, from: Option<u64>) -> (bool, u64, String) {
    // No copy yet, or one that cannot be read, is an empty one: the page shows
    // nothing and asks again.
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return (true, 0, String::new());
    };
    let written = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let (reset, from) = match from {
        Some(at) if at <= written => (false, at),
        // Nowhere in this copy: either it was rolled over and is shorter than
        // where the page had got to, or the program has run again.
        _ => (true, 0),
    };

    let mut bytes = Vec::new();
    if file.seek(std::io::SeekFrom::Start(from)).await.is_ok() {
        let _ = file.read_to_end(&mut bytes).await;
    }
    (reset, from, String::from_utf8_lossy(&bytes).into_owned())
}

/// Everything the board still has of the log, for taking away.
///
/// The page keeps up with the copy a chunk at a time and draws it with whatever
/// level is being looked at, which is what watching a run go by wants — and not
/// what is wanted by someone asking afterwards what happened. This answers with
/// both chunks as they were written: the one that was rolled over, since it is
/// the older of the two, then the one being added to.
async fn download() -> Response {
    let bytes = both(Path::new(logging::PREVIOUS), Path::new(logging::PATH)).await;
    if bytes.is_empty() {
        return (StatusCode::NOT_FOUND, "no log\n").into_response();
    }
    (
        [
            (header::CONTENT_TYPE, PLAIN),
            (header::CONTENT_DISPOSITION, "attachment; filename=cp2cl.log"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response()
}

/// The two chunks of the copy as one answer, the older one first.
///
/// A chunk that cannot be read is not a reason to answer with nothing: what is
/// there is still the log, and the one that could be read is better than none
/// of it.
async fn both(first: &Path, then: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    for path in [first, then] {
        if let Ok(mut file) = tokio::fs::File::open(path).await {
            let _ = file.read_to_end(&mut bytes).await;
        }
    }
    bytes
}

/// The offset a request asks for out of its query string.
///
/// One parameter, like `named`: `from=<bytes>`, which is where the last answer
/// left the page. Anything else is no offset, and no offset is the beginning.
fn from(query: &Option<String>) -> Option<u64> {
    let asked = query.as_deref()?;
    let at = asked.split('&').find_map(|pair| pair.strip_prefix("from="))?;
    at.parse().ok()
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

    /// The page is one document and reads one key per part, so the snapshot has
    /// to keep answering under those names.
    #[test]
    fn the_endpoint_answers_with_every_part() {
        let status = Status::new();
        let body = status.json();
        assert!(body.get("carlife").is_some());
        assert!(body.get("carplay").is_some());
        assert!(body.get("now_playing").is_some());
        assert!(body.get("device").is_some());
    }

    /// What is playing is its own part of the page, and its cover comes from a
    /// path of its own: a picture is drawn, not read out of the JSON.
    #[test]
    fn the_page_reads_the_song_from_its_own_part() {
        assert!(
            page_text().contains("data.now_playing"),
            "the page reads the part"
        );
        assert!(
            page_text().contains("/api/cover?id="),
            "and fetches the cover itself, by the name it was given"
        );
        assert!(
            page_text().contains("BOTH_UP"),
            "and shows the section only while both ends are up"
        );
        assert!(
            page_text().contains("duration_s"),
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
    /// carry it, and the page has to read it from there. The profile and the
    /// level are the stream's own, so both halves say the same ones — there is
    /// one video stream and the car is given it unchanged.
    #[test]
    fn the_page_reads_the_coding_from_the_answer() {
        assert!(page_text().contains("d.video"), "the page reads the field");
        assert!(!page_text().contains("H.264"), "and names no coding itself");
        let status = Status::new();
        let body = status.json();
        assert_eq!(
            body["carplay"]["video"]["codec"],
            crate::airplay::screen::CODEC,
            "which is what the snapshot carries"
        );
        assert!(body["carplay"]["video"]["profile"].is_null(), "nothing described yet");
        assert_eq!(body["carlife"]["video"]["codec"], crate::airplay::screen::CODEC);

        status.with(|s| {
            s.video.profile = Some("Baseline".to_string());
            s.video.level = Some("3.1".to_string());
        });
        let body = status.json();
        for half in ["carplay", "carlife"] {
            assert_eq!(body[half]["video"]["profile"], "Baseline");
            assert_eq!(body[half]["video"]["level"], "3.1");
        }
    }

    /// The panel is the car's, so the page shows its physical size on the car's
    /// side, next to the size the car named, and reads it from the answer.
    #[test]
    fn the_page_reads_the_panel_size_from_the_cars_half() {
        assert!(
            page_text().contains("d.physical"),
            "the page reads the field"
        );
        let status = Status::new();
        status.with(|s| s.carlife.physical = Some((407, 229)));
        let body = status.json();
        assert_eq!(body["carlife"]["physical"]["width_mm"], 407);
        assert_eq!(body["carlife"]["physical"]["height_mm"], 229);
    }

    /// The size in force is the `agreed` one, whoever named it: a number the
    /// channel table preconfigures has the table's size stored where the car's
    /// would be, so the page reads one field and the table is where the number
    /// comes from.
    #[test]
    fn the_page_shows_the_size_in_force() {
        assert!(
            page_text().contains("size(d.agreed)"),
            "the page reads the one field"
        );
        assert!(
            embedded(CHANNELS_NAME).contains("\"video\""),
            "and the table is where the number comes from"
        );

        let status = Status::new();
        assert!(
            status.json()["carlife"]["agreed"].is_null(),
            "no screen has been named yet"
        );

        status.with(|s| {
            s.carlife.agreed = Some(Size {
                width: 1920,
                height: 720,
                fps: 30,
            });
        });
        let body = status.json();
        assert_eq!(body["carlife"]["agreed"]["width"], 1920);
        assert_eq!(body["carlife"]["agreed"]["height"], 720);
        assert_eq!(body["carlife"]["agreed"]["fps"], 30);
    }

    /// The car's position is a section of its own, and it is the car's: the page
    /// shows it while the car reports one and reads it from the car's half.
    #[test]
    fn the_page_reads_the_cars_position_from_its_own_part() {
        assert!(
            page_text().contains("车载数据 · GPS"),
            "it has a heading of its own"
        );
        assert!(
            page_text().contains("data.carlife.gps"),
            "and the page reads the fix"
        );

        let status = Status::new();
        assert!(
            status.json()["carlife"]["gps"].is_null(),
            "a car that has not reported a position yet is not one at zero degrees"
        );

        status.with(|s| {
            s.carlife.gps = Some(crate::carlife::proto::CarGps {
                antenna_state: 1,
                latitude: 30_317_880,
                longitude: 120_128_094,
                ..Default::default()
            });
        });
        let body = status.json();
        assert_eq!(body["carlife"]["gps"]["latitude"], 30_317_880);
        assert_eq!(body["carlife"]["gps"]["longitude"], 120_128_094);
    }

    /// The speed is a section of its own as well, and it is the car's for the same
    /// reason: no speed reported yet is not a car standing still.
    #[test]
    fn the_page_reads_the_cars_speed_from_its_own_part() {
        assert!(
            page_text().contains("车载数据 · 车速"),
            "it has a heading of its own"
        );
        assert!(
            page_text().contains("data.carlife.speed"),
            "and the page reads the speed"
        );

        let status = Status::new();
        assert!(
            status.json()["carlife"]["speed"].is_null(),
            "a car that has not reported a speed yet is not one at zero"
        );

        // Zero is a speed the car has reported, and it is shown as one.
        status.with(|s| s.carlife.speed = Some(0));
        assert_eq!(status.json()["carlife"]["speed"], 0);
    }

    /// The log is a file that is always being added to, so the page asks for
    /// what it has not read rather than for all of it: an answer carries what
    /// was written since the offset, and the offset to ask from next.
    ///
    /// An offset that is nowhere in the copy — a copy that has been rolled over
    /// and is shorter than where the page had got to — is answered with the
    /// copy from the beginning, and with `reset` so the page knows to drop what
    /// it had rather than carry on from the end of another run.
    #[tokio::test]
    async fn the_log_is_read_from_where_the_page_left_off() {
        let path = std::env::temp_dir().join(format!("cp2cl-log-{}", std::process::id()));
        std::fs::write(&path, "one\ntwo\n").unwrap();

        let (reset, from, text) = copied(&path, None).await;
        assert!(reset, "no offset is the beginning");
        assert_eq!((from, text.as_str()), (0, "one\ntwo\n"));

        // The next answer starts where that one ended, and carries nothing new.
        let (reset, from, text) = copied(&path, Some(text.len() as u64)).await;
        assert!(!reset, "an offset in the copy is kept");
        assert_eq!((from, text.as_str()), (8, ""));

        // Half way in is the rest of it, from there.
        let (reset, from, text) = copied(&path, Some(4)).await;
        assert!(!reset);
        assert_eq!((from, text.as_str()), (4, "two\n"));

        // Past the end is a copy that moved under the page: rolled, or written
        // by another run.
        let (reset, from, text) = copied(&path, Some(4096)).await;
        assert!(reset);
        assert_eq!((from, text.as_str()), (0, "one\ntwo\n"));

        // A copy that is not there is an empty one, and is not an error.
        let gone = std::env::temp_dir().join("cp2cl-log-not-here");
        let (reset, from, text) = copied(&gone, Some(8)).await;
        assert!(reset);
        assert_eq!((from, text.as_str()), (0, ""));

        std::fs::remove_file(&path).ok();
    }

    /// A page that has just been opened is given the whole of the copy, not the
    /// tail of it: the copy is capped at four megabytes, and a page that opened
    /// at the tail could only ever read forward from there.
    #[tokio::test]
    async fn the_log_opens_at_its_beginning() {
        let path = std::env::temp_dir().join(format!("cp2cl-tail-{}", std::process::id()));
        let line = "x".repeat(99) + "\n";
        std::fs::write(&path, line.repeat(3000)).unwrap();

        let (reset, from, text) = copied(&path, None).await;
        assert!(reset, "a page with no offset starts over");
        assert_eq!(from, 0, "at the beginning of the copy");
        assert_eq!(text.len(), 300_000, "and carries all of it");

        std::fs::remove_file(&path).ok();
    }

    /// Taking the copy away is the whole of it rather than what the page is
    /// showing: both chunks, the older one first, and no level left out.
    #[tokio::test]
    async fn the_whole_copy_is_both_chunks_with_the_older_one_first() {
        let dir = std::env::temp_dir();
        let before = dir.join(format!("cp2cl-before-{}", std::process::id()));
        let now = dir.join(format!("cp2cl-now-{}", std::process::id()));
        std::fs::write(&before, "debug\nbefore\n").unwrap();
        std::fs::write(&now, "now\n").unwrap();

        assert_eq!(both(&before, &now).await, b"debug\nbefore\nnow\n".to_vec());

        // A chunk that is not there is only the other one, not nothing.
        std::fs::remove_file(&before).unwrap();
        assert_eq!(both(&before, &now).await, b"now\n".to_vec());

        // Neither of them is a copy at all.
        assert!(both(&before, &before).await.is_empty());

        std::fs::remove_file(&now).ok();
    }

    /// The board's own CPU and memory are their own part of the page, and the
    /// page writes both rows itself: a percent from the share the answer gives
    /// it, and memory from the two byte counts.
    #[test]
    fn the_page_reads_the_device_from_its_own_part() {
        assert!(page_text().contains("设备端"), "it has a heading of its own");
        assert!(page_text().contains("data.device"), "and the page reads the part");
        assert!(
            page_text().contains("d.cpu_percent"),
            "the page writes the percentage, from the share the answer gives it"
        );
        assert!(
            page_text().contains("d.software"),
            "and writes the version the binary was built as"
        );
        assert!(
            page_text().contains("d.firmware"),
            "and names the distribution the board is running on"
        );
        assert!(page_text().contains("d.kernel"), "and the kernel under it");
        let status = Status::new();
        assert!(
            status.json()["device"]["memory_total"].is_null(),
            "nothing sampled yet is not zero"
        );

        status.with(|s| {
            s.device.cpu_percent = Some(37);
            s.device.memory_used = Some(100 * 1024 * 1024);
            s.device.memory_total = Some(496 * 1024 * 1024);
        });
        let body = status.json();
        assert_eq!(body["device"]["cpu_percent"], 37);
        assert_eq!(body["device"]["memory_used"], 100 * 1024 * 1024);
        assert_eq!(body["device"]["memory_total"], 496 * 1024 * 1024);
    }
}
