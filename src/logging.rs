//! The stamp in front of every line.
//!
//! `tracing_subscriber`'s own timer writes an ISO 8601 instant — date, time and
//! zone — which is more than a run that lasts minutes needs, and the board's
//! clock does not keep the date anyway. What is wanted is the time of day and
//! its millisecond: the same shape the CarLife side's logs are read in.

use std::fmt;
use std::mem::MaybeUninit;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

/// Writes `HH:MM:SS.mmm`. The space after it is the layer's, not this one's.
pub struct Clock;

impl FormatTime for Clock {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let (hour, minute, second) = time_of_day(now.as_secs());
        write!(
            w,
            "{hour:02}:{minute:02}:{second:02}.{:03}",
            now.subsec_millis()
        )
    }
}

/// The wall clock's hour, minute and second at a Unix time.
///
/// In the zone the process was started in: `localtime_r` reads `TZ`, so a board
/// set to a zone prints that zone and one with no `TZ` prints UTC.
///
/// `libc::time_t` is a deprecated alias — libc has not followed musl 1.2 to a
/// 64-bit one — but it is still what the call takes on this target, so it is
/// written and the deprecation is allowed rather than worked around.
#[allow(deprecated)]
fn time_of_day(seconds: u64) -> (i32, i32, i32) {
    let seconds = seconds as libc::time_t;
    let mut tm = MaybeUninit::<libc::tm>::uninit();
    // SAFETY: `tm` points at writable storage and the second argument is a value,
    // not a pointer. A null return means nothing was written, which is why the
    // struct is only read after a non-null one.
    let filled = unsafe { libc::localtime_r(&seconds, tm.as_mut_ptr()) };
    if filled.is_null() {
        return (0, 0, 0);
    }
    // SAFETY: `localtime_r` returned non-null, so it filled every field of the
    // struct it was handed.
    let tm = unsafe { tm.assume_init() };
    (tm.tm_hour, tm.tm_min, tm.tm_sec)
}
