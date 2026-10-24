//! The phone's clock, learned on the timing port.
//!
//! An iPhone stamps its timing packets from its own monotonic base, which is
//! not the wall clock NTP counts from: the first offset that comes back is
//! therefore an arbitrary number of seconds, not an error. MacPlay says as much
//! and simply steps its clock by it. Everything we stamp afterwards — our
//! timing requests, our replies to the phone's, and the `/feedback` timestamp —
//! has to come from this clock, because the phone places our media position in
//! *its* domain. Send wall-clock NTP there and the playout position it works
//! out is off by the same arbitrary amount.
//!
//! The shape of this is MacPlay's `TimingSync`: step on the first sample and on
//! any large residual, slew towards it otherwise, and only ever accept the
//! least delayed sample of a group so a queued packet cannot pull the clock.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Seconds from 1900, which NTP counts from, to 1970, which Unix counts from.
const SECONDS_1900_TO_1970: i128 = 2_208_988_800;
const TWO32: f64 = 4_294_967_296.0;
const NS_PER_SEC: i128 = 1_000_000_000;
/// A phase error above this steps the clock instead of slewing it.
const STEP_THRESHOLD_SEC: f64 = 0.128;
/// Fraction of the residual offset applied per accepted sample once locked.
const SLEW_GAIN: f64 = 1.0 / 8.0;
/// How many round-trip times a sample has to beat.
const DELAY_WINDOW: usize = 8;
/// Responses collected before the least delayed one of the group is used.
const PICK_COUNT: usize = 2;

/// Our monotonic clock, steered onto the phone's.
pub struct SyncedClock {
    start: Instant,
    /// Nanoseconds added to the monotonic clock to land in the phone's domain.
    offset_ns: i128,
    synced: bool,
    /// The last `DELAY_WINDOW` round-trip times, in seconds.
    delays: [f64; DELAY_WINDOW],
    delay_index: usize,
    /// The least delayed sample so far in the current group.
    pick: Option<(f64, f64)>,
    pick_left: usize,
}

impl SyncedClock {
    /// Starts on the wall clock, which is where every timestamp goes until the
    /// first sample lands and moves it.
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            offset_ns: wall_ns(),
            synced: false,
            delays: [f64::INFINITY; DELAY_WINDOW],
            delay_index: 0,
            pick: None,
            pick_left: PICK_COUNT,
        }
    }

    /// Now, in the phone's domain, as a 64-bit NTP stamp.
    pub fn now(&self) -> [u8; 8] {
        ntp_from_ns(self.start.elapsed().as_nanos() as i128 + self.offset_ns)
    }

    /// The same thing as the integer the wire carries.
    pub fn now_u64(&self) -> u64 {
        u64::from_be_bytes(self.now())
    }

    /// One round trip, `offset` and `rtt` in seconds.
    ///
    /// Both are nonsense until the clock has been stepped once, which is the
    /// point: only the least delayed sample of a group is used, so a packet
    /// that queued behind others cannot pull the clock towards it.
    pub fn sample(&mut self, offset: f64, rtt: f64) {
        if !offset.is_finite() || !rtt.is_finite() || rtt < 0.0 {
            return;
        }
        if self.pick.map(|(best, _)| rtt < best).unwrap_or(true) {
            self.pick = Some((rtt, offset));
        }
        self.pick_left -= 1;
        if self.pick_left > 0 {
            return;
        }
        self.pick_left = PICK_COUNT;
        let Some((picked_rtt, picked_offset)) = self.pick.take() else {
            return;
        };

        let least_delayed = self.delays.iter().all(|d| picked_rtt <= *d);
        self.delays[self.delay_index] = picked_rtt;
        self.delay_index = (self.delay_index + 1) % DELAY_WINDOW;
        if !least_delayed {
            return;
        }
        self.apply(picked_offset, picked_rtt);
    }

    /// Steps on a large phase error, slews towards it otherwise.
    fn apply(&mut self, offset: f64, rtt: f64) {
        let stepping = !self.synced || offset.abs() > STEP_THRESHOLD_SEC;
        let applied = if stepping { offset } else { offset * SLEW_GAIN };
        self.offset_ns += (applied * NS_PER_SEC as f64) as i128;
        if !stepping {
            return;
        }
        // A step throws away what the window knew about the delay, so the next
        // group is judged against a clean one.
        self.delays = [f64::INFINITY; DELAY_WINDOW];
        self.delay_index = 0;
        self.pick = None;
        let what = if self.synced { "stepped" } else { "synced" };
        self.synced = true;
        // The first offset is the distance between two arbitrary clock bases,
        // not a measurement of anything — say so, because -1.8e9 s looks like
        // a bug and is not one.
        let note = if offset.abs() > 3600.0 {
            "phone's own clock base"
        } else {
            "residual"
        };
        tracing::info!(
            "clock.synced action={what} offset_s={offset:.3} note={note:?} rtt_ms={:.1}",
            rtt * 1000.0
        );
    }
}

impl Default for SyncedClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Nanoseconds since the Unix epoch, which is what the offset is carried in.
fn wall_ns() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i128
}

/// NTP fixed point: 32 bits of seconds since 1900, 32 bits of fraction.
///
/// The seconds wrap in 2036 and the fraction is written as an unsigned field,
/// so both go out through `as u32` rather than being range checked.
fn ntp_from_ns(ns: i128) -> [u8; 8] {
    let secs = ns.div_euclid(NS_PER_SEC) + SECONDS_1900_TO_1970;
    let frac = ns.rem_euclid(NS_PER_SEC) * (1i128 << 32) / NS_PER_SEC;
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&(secs as u32).to_be_bytes());
    out[4..].copy_from_slice(&(frac as u32).to_be_bytes());
    out
}

/// Seconds between two 64-bit NTP stamps, as a signed difference.
///
/// The stamps themselves overflow an `i64` (seconds since 1900 shifted up by 32
/// is about 1.7e19), but their difference never does — so wrap first, then sign.
pub fn ntp_diff(a: u64, b: u64) -> f64 {
    let d = a.wrapping_sub(b);
    let d = if d >= 1u64 << 63 {
        d as i128 - (1i128 << 64)
    } else {
        d as i128
    };
    d as f64 / TWO32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stamp one second later than another differs by one second, whatever
    /// the stamps themselves are — which is the whole reason the wrap has to
    /// happen before the sign does.
    #[test]
    fn diff_is_signed_and_small() {
        let t = 3_999_000_000u64 << 32;
        let later = t + (1u64 << 32) + (1u64 << 31);
        assert!((ntp_diff(later, t) - 1.5).abs() < 1e-6);
        assert!((ntp_diff(t, later) + 1.5).abs() < 1e-6);
    }

    /// The first sample moves the clock onto the phone's base; a small
    /// residual afterwards is only slewed towards, a fraction at a time.
    #[test]
    fn steps_then_slews() {
        let mut clock = SyncedClock::new();
        let before = clock.now_u64();
        // Two samples, since a group is picked from `PICK_COUNT` of them, and
        // the first of them has to be the least delayed of the empty window.
        clock.sample(-1_791_038_693.5, 0.002);
        clock.sample(-1_791_038_693.5, 0.004);
        let stepped = clock.now_u64();
        assert!(ntp_diff(stepped, before) < -1.7e9);

        let base = clock.now_u64();
        // Well under the step threshold, so only an eighth of it is applied.
        clock.sample(0.08, 0.001);
        clock.sample(0.08, 0.001);
        let moved = ntp_diff(clock.now_u64(), base);
        assert!((moved - 0.01).abs() < 1e-3, "slewed by {moved}");
    }

    /// A sample more delayed than the recent window is dropped: a packet that
    /// queued is a worse measurement than the ones that did not.
    ///
    /// A step clears the window, so the delayed sample has to come after one
    /// that was only slewed — that is the one that leaves a baseline behind.
    #[test]
    fn drops_the_more_delayed_sample() {
        let mut clock = SyncedClock::new();
        // Steps, and with it clears the window.
        clock.sample(0.0, 0.001);
        clock.sample(0.0, 0.001);
        // Under the step threshold, so it only slews — and records 1 ms.
        clock.sample(0.001, 0.001);
        clock.sample(0.001, 0.001);
        // The offset rather than two stamps: a stamp is read off a running
        // clock, so two of them differ by however long the machine took to get
        // from one read to the other — which is what a tolerance would have to
        // hide. What this is about is the offset, and that does not drift.
        let settled = clock.offset_ns;
        // Slower than everything in the window, so it is thrown away.
        clock.sample(5.0, 0.5);
        clock.sample(5.0, 0.5);
        assert_eq!(clock.offset_ns, settled, "the delayed sample moved the clock");
    }
}
