//! The stamp in front of every line.
//!
//! `tracing_subscriber`'s own timer writes an ISO 8601 instant — date, time and
//! zone — which is more than a run that lasts minutes needs, and the board's
//! clock does not keep the date anyway: neither link it has — the phone's or
//! the car's — carries a wall clock it could be set from. What is wanted is a
//! stamp that only ever goes up, so lines can be put in order against each
//! other and spaced: seconds since boot, to the millisecond.

use std::fmt;
use std::mem::MaybeUninit;
use std::time::Duration;

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

/// Writes the seconds since boot, to the millisecond (`123.456`). The space
/// after it is the layer's, not this one's.
pub struct Clock;

impl FormatTime for Clock {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        write!(w, "{:.3}", uptime().as_secs_f64())
    }
}

/// Seconds since the machine booted, suspend time included.
///
/// `CLOCK_BOOTTIME` rather than `CLOCK_MONOTONIC` so that a board which sleeps
/// still counts the sleep: the stamp is read as time passed, not time awake.
fn uptime() -> Duration {
    let mut ts = MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: `ts` points at writable storage. A non-zero return means nothing
    // was written, which is why the struct is only read after a zero one.
    let filled = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, ts.as_mut_ptr()) };
    if filled != 0 {
        return Duration::ZERO;
    }
    // SAFETY: `clock_gettime` returned zero, so it filled every field of the
    // struct it was handed.
    let ts = unsafe { ts.assume_init() };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}
