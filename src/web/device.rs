//! What the board itself is doing: how much CPU it is using, how much memory
//! is left, and which build it is running.
//!
//! This is the one part of the page that is about neither end of the bridge, and
//! the one part that is observed rather than reported: nothing in the bridge
//! measures its own CPU time, and the kernel already counts both quantities in
//! `/proc`, so they are read from there rather than worked out a second way.
//!
//! A percentage takes two readings — the kernel keeps counters, not a rate — so
//! this samples on its own instead of being asked. A reader that polled for it
//! would be shown a share of whatever interval happened to sit between its own
//! two requests, which is a different measurement every time the page is opened.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::status::Status;

/// Between samples. A second is long enough that one tick of the kernel's clock
/// no longer shows up as a whole percent, and short enough that the page — which
/// polls every two seconds — never shows a number that has already been overtaken.
const SAMPLE: Duration = Duration::from_secs(5);

/// Which build this is: the tag on the commit it was built from, and the commit.
///
/// Both are read by `build.rs` from git rather than from a file that says so —
/// a version is a name given to a commit, so the commit is what knows it. A
/// commit carrying no tag is `dev`, which is the name of a build nobody has
/// named: `v0.8.0 (f3c999a)` is a release, `dev (993e2b9)` is not.
pub const SOFTWARE: &str = concat!(env!("CP2CL_VERSION"), " (", env!("CP2CL_COMMIT"), ")");

/// What the board itself is doing.
///
/// Empty until the first sample: `cpu_percent` needs two readings to compare,
/// and the memory figures arrive with them.
#[derive(Debug, Clone, Default)]
pub struct Device {
    /// Share of the CPU used since the last sample, over every core at once:
    /// 0-100, not 0-(100 × cores).
    pub cpu_percent: Option<u8>,
    /// Memory in use and installed, in bytes.
    ///
    /// In use is what the kernel could not hand out again — total less
    /// available, which is what `free` calls used — so what is only cached
    /// counts as free, as it should.
    pub memory_used: Option<u64>,
    pub memory_total: Option<u64>,
}

impl Device {
    pub fn json(&self) -> Value {
        json!({
            "software": SOFTWARE,
            "cpu_percent": self.cpu_percent,
            "memory_used": self.memory_used,
            "memory_total": self.memory_total,
        })
    }
}

/// Samples the device, for as long as the program runs.
///
/// Never returns: the page can be opened and closed as often as anyone likes,
/// and the counters have to have been read all along for the first answer to
/// mean anything.
pub async fn watch(status: Arc<Status>) {
    let mut before = cpu_ticks();
    loop {
        tokio::time::sleep(SAMPLE).await;
        let after = cpu_ticks();
        let cpu = before
            .zip(after)
            .and_then(|(before, after)| busy_share(before, after));
        before = after;

        // Read outside the lock: two small files, and the snapshot is only
        // ever held for assignments.
        let memory = memory_in_use();
        status.with(|s| {
            s.device.cpu_percent = cpu;
            s.device.memory_used = memory.map(|(used, _)| used);
            s.device.memory_total = memory.map(|(_, total)| total);
        });
    }
}

/// The two readings a share is worked out from, as (total, idle) ticks.
///
/// `None` for the second reading is not a failure worth reporting: it is a
/// single sample lost, and the next one is a second away.
fn busy_share((total, idle): (u64, u64), (total_now, idle_now): (u64, u64)) -> Option<u8> {
    let spent = total_now.checked_sub(total)?;
    let waited = idle_now.saturating_sub(idle).min(spent);
    (spent > 0).then(|| (100 * (spent - waited) / spent) as u8)
}

/// The CPU time the kernel has accounted for, as (total, idle) in ticks.
fn cpu_ticks() -> Option<(u64, u64)> {
    std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|stat| parse_cpu(&stat))
}

/// The first line of `/proc/stat`, which is every core added together.
///
/// Eight fields and no more: the two after them are guest time, which the
/// kernel has already counted in user and nice. `cpu0` and its siblings are
/// what follows, and are not read — the page shows the device, not its cores.
fn parse_cpu(stat: &str) -> Option<(u64, u64)> {
    let fields = stat.lines().next()?.strip_prefix("cpu ")?.split_whitespace();
    let mut total = 0u64;
    let mut idle = 0u64;
    for (i, field) in fields.take(8).enumerate() {
        let ticks = field.parse::<u64>().ok()?;
        total += ticks;
        // idle and iowait: a core waiting for work it has not been given and a
        // core waiting for storage are both time the CPU did not spend working.
        if i == 3 || i == 4 {
            idle += ticks;
        }
    }
    (total > 0).then_some((total, idle))
}

/// Memory in use and installed, in bytes, from one reading of `/proc/meminfo`.
///
/// One reading for both: they are subtracted from each other, so they have to
/// describe the same moment.
fn memory_in_use() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let total = meminfo(&text, "MemTotal")?;
    let available = meminfo(&text, "MemAvailable")?;
    Some((total.saturating_sub(available), total))
}

/// One field of `/proc/meminfo`, in bytes. The kernel writes kilobytes.
fn meminfo(text: &str, field: &str) -> Option<u64> {
    let line = text.lines().find(|line| line.starts_with(field))?;
    let kb = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The board's own first line, followed by a core's: only the first is the
    /// device.
    const STAT: &str = "\
cpu  155666 1358 36582 850123 1234 0 890 0 0 0
cpu0 38876 402 9100 212531 300 0 220 0 0 0
intr 1234
";

    #[test]
    fn the_aggregate_line_is_the_whole_device() {
        let (total, idle) = parse_cpu(STAT).unwrap();
        assert_eq!(total, 155666 + 1358 + 36582 + 850123 + 1234 + 0 + 890 + 0);
        assert_eq!(idle, 850123 + 1234, "idle and iowait are both waiting");
    }

    /// Guest time is already in user and nice, so a device running one would be
    /// counted twice if the two fields after `steal` were read as well.
    #[test]
    fn guest_time_is_not_counted_twice() {
        let busy = parse_cpu("cpu 10 0 0 0 0 0 0 0 40 0\n").unwrap();
        assert_eq!(busy, (10, 0), "40 ticks of guest are inside the 10 of user");
    }

    #[test]
    fn a_share_is_the_time_not_spent_waiting() {
        // 300 of 400 ticks working.
        assert_eq!(busy_share((0, 0), (400, 100)), Some(75));
        // All of it waiting.
        assert_eq!(busy_share((100, 50), (200, 150)), Some(0));
        // None of it waiting.
        assert_eq!(busy_share((100, 50), (200, 50)), Some(100));
        // Nothing has happened, so there is no share to report.
        assert_eq!(busy_share((100, 50), (100, 50)), None);
    }

    #[test]
    fn memory_comes_back_in_bytes() {
        let text = "\
MemTotal:         507904 kB
MemFree:           12000 kB
MemAvailable:     200000 kB
SwapTotal:             0 kB
";
        assert_eq!(meminfo(text, "MemTotal"), Some(507904 * 1024));
        assert_eq!(meminfo(text, "MemAvailable"), Some(200000 * 1024));
        assert_eq!(meminfo(text, "SwapCached"), None, "a field that is not there");
    }

    /// Nothing sampled yet is what a page served before the first second gets,
    /// and it has to be an honest "not known" rather than a zero.
    #[test]
    fn an_unsampled_device_says_nothing() {
        let body = Device::default().json();
        assert!(body["cpu_percent"].is_null());
        assert!(body["memory_total"].is_null());
    }

    /// The build it is running is not a sample and is never "not known": it is
    /// a version and the commit, and a commit with no tag on it is `dev`.
    #[test]
    fn the_build_says_which_build_it_is() {
        let body = Device::default().json();
        let software = body["software"].as_str().expect("the page reads it");
        let (version, commit) = software
            .split_once(" (")
            .unwrap_or_else(|| panic!("`v0.8.0 (f3c999a)`, not {software}"));
        assert!(version == "dev" || version.starts_with('v'), "{version}");
        assert!(commit.ends_with(')') && commit.len() >= 8, "{commit}");
    }
}
