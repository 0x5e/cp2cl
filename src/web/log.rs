//! The log tab: the copy of the log the program keeps under `/var/log`, asked
//! for by how far the page has already read rather than in full.
//!
//! A copy is always being added to, so a page that wants to keep up with it has
//! to say where it got to — that is `from`, and the answer says where the next
//! one starts. `/api/log/download` is the same copy as one file, for taking off
//! the board.

use std::path::Path;

use axum::extract::RawQuery;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::{JSON, PLAIN};
use crate::logging;

/// The copy, from the byte the page has read up to.
///
/// `reset` is the one way an offset can turn out to be nowhere: the copy was
/// rolled over and is now shorter than where the page had got to, or the
/// program has run again. Either way the page is told to drop what it had, and
/// is given the copy from its beginning — which is what a page that asks for
/// nothing at all is given too.
pub async fn log(RawQuery(query): RawQuery) -> Response {
    let (reset, from, text, read) = copied(Path::new(logging::PATH), from(&query)).await;
    (
        [
            (header::CONTENT_TYPE, JSON),
            (header::CACHE_CONTROL, "no-store"),
        ],
        json!({
            "reset": reset,
            "from": from,
            "next": from + read,
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
///
/// The last part is how many bytes were handed over, which is the offset the
/// next answer starts at. It is counted from the bytes rather than taken from
/// the text: a copy is not promised to be valid UTF-8, and a lossy reading of
/// what is not spells one or more bytes as a three-byte `U+FFFD`, so the text
/// is longer than what it was read from and its length is no offset at all.
async fn copied(path: &Path, from: Option<u64>) -> (bool, u64, String, u64) {
    // No copy yet, or one that cannot be read, is an empty one: the page shows
    // nothing and asks again.
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return (true, 0, String::new(), 0);
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
    let read = bytes.len() as u64;
    (reset, from, String::from_utf8_lossy(&bytes).into_owned(), read)
}

/// Everything the board still has of the log, for taking away.
///
/// The page keeps up with the copy a chunk at a time and draws it with whatever
/// level is being looked at, which is what watching a run go by wants — and not
/// what is wanted by someone asking afterwards what happened. This answers with
/// both chunks as they were written: the one that was rolled over, since it is
/// the older of the two, then the one being added to.
pub async fn download() -> Response {
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
/// One parameter: `from=<bytes>`, which is where the last answer left the page.
/// Anything else is no offset, and no offset is the beginning.
fn from(query: &Option<String>) -> Option<u64> {
    let asked = query.as_deref()?;
    let at = asked.split('&').find_map(|pair| pair.strip_prefix("from="))?;
    at.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let (reset, from, text, read) = copied(&path, None).await;
        assert!(reset, "no offset is the beginning");
        assert_eq!((from, text.as_str(), read), (0, "one\ntwo\n", 8));

        // The next answer starts where that one ended, and carries nothing new.
        let (reset, from, text, read) = copied(&path, Some(read)).await;
        assert!(!reset, "an offset in the copy is kept");
        assert_eq!((from, text.as_str(), read), (8, "", 0));

        // Half way in is the rest of it, from there.
        let (reset, from, text, read) = copied(&path, Some(4)).await;
        assert!(!reset);
        assert_eq!((from, text.as_str(), read), (4, "two\n", 4));

        // Past the end is a copy that moved under the page: rolled, or written
        // by another run.
        let (reset, from, text, read) = copied(&path, Some(4096)).await;
        assert!(reset);
        assert_eq!((from, text.as_str(), read), (0, "one\ntwo\n", 8));

        // A copy that is not there is an empty one, and is not an error.
        let gone = std::env::temp_dir().join("cp2cl-log-not-here");
        let (reset, from, text, read) = copied(&gone, Some(8)).await;
        assert!(reset);
        assert_eq!((from, text.as_str(), read), (0, "", 0));

        std::fs::remove_file(&path).ok();
    }

    /// How far the page has read is a count of bytes, and a text is not one: a
    /// byte the copy has that UTF-8 does not is spelled as a three-byte
    /// `U+FFFD`, so an offset taken from the text would run ahead of the copy
    /// and the lines after it would be read from the wrong place.
    #[tokio::test]
    async fn the_offset_counts_the_bytes_and_not_the_text() {
        let path = std::env::temp_dir().join(format!("cp2cl-log-odd-{}", std::process::id()));
        std::fs::write(&path, b"a\xffb\n").unwrap();

        let (reset, from, text, read) = copied(&path, None).await;
        assert!(reset);
        assert_eq!(from, 0);
        assert_eq!(read, 4, "four bytes in the copy");
        assert_eq!(text.len(), 6, "and six in the text made of them");
        assert!(text.ends_with("b\n"));

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

        let (reset, from, text, read) = copied(&path, None).await;
        assert!(reset, "a page with no offset starts over");
        assert_eq!(from, 0, "at the beginning of the copy");
        assert_eq!(text.len(), 300_000, "and carries all of it");
        assert_eq!(read, 300_000, "which is where the next one starts");

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
}
