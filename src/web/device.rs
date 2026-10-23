//! What the board itself is doing: how much CPU it is using, how much memory
//! is left, which system it is running on, and which build it is running.
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

use std::sync::{Arc, OnceLock};
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
/// named: `v0.9.1 (a12935f)` is a release, `dev (993e2b9)` is not. A build from
/// a tree that does not match the commit says so: `v0.9.1 (a12935f-dirty)`.
pub const SOFTWARE: &str = concat!(env!("CP2CL_VERSION"), " (", env!("CP2CL_COMMIT"), ")");

/// The two answers below, kept after the first one: neither changes under a
/// running program, and both cost a file read.
static FIRMWARE: OnceLock<Option<String>> = OnceLock::new();
static KERNEL: OnceLock<Option<String>> = OnceLock::new();

/// The distribution this program is running on, as `Ubuntu 24.04` or
/// `ImmortalWrt 23.05.3`.
///
/// `PRETTY_NAME` is nearly this and not quite — OpenWrt and its derivatives put
/// the revision in it (`ImmortalWrt 23.05.3 r27840-6b0f2b1`) — so the name and
/// the version are read as two fields instead of taking whichever string the
/// builder thought to write.
pub fn firmware() -> Option<&'static str> {
    FIRMWARE
        .get_or_init(|| os_release().as_deref().and_then(os_firmware))
        .as_deref()
}

/// The kernel's own release string, as `uname -r` prints it:
/// `6.6.31-rockchip64`.
pub fn kernel() -> Option<&'static str> {
    KERNEL
        .get_or_init(|| {
            let text = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
            let release = text.trim();
            (!release.is_empty()).then(|| release.to_string())
        })
        .as_deref()
}

/// `/etc/os-release`, or the copy systemd keeps in `/usr/lib` for the images
/// that leave `/etc` to the administrator.
fn os_release() -> Option<String> {
    ["/etc/os-release", "/usr/lib/os-release"]
        .into_iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
}

/// `<NAME> <VERSION_ID>` out of an `os-release` file.
///
/// The version falls back to `VERSION` because not every builder writes an id —
/// Buildroot writes both, and Ubuntu's `VERSION` carries the release name and
/// the id does not.
fn os_firmware(text: &str) -> Option<String> {
    let name = os_field(text, "NAME")?;
    match os_field(text, "VERSION_ID").or_else(|| os_field(text, "VERSION")) {
        Some(version) => Some(format!("{name} {version}")),
        None => Some(name),
    }
}

/// One field of an `os-release` file, without the quotes it may be written in.
///
/// The key is matched whole: `ID` is a line of its own and not the beginning of
/// `ID_LIKE`.
fn os_field(text: &str, key: &str) -> Option<String> {
    let value = text.lines().find_map(|line| {
        let (found, value) = line.split_once('=')?;
        (found.trim() == key).then_some(value.trim())
    })?;
    let value = value.trim_matches('"').trim_matches('\'');
    (!value.is_empty()).then(|| value.to_string())
}

/// What the board itself is doing.
///
/// Only what is sampled is here, and it is empty until the first sample:
/// `cpu_percent` needs two readings to compare, and the memory figures arrive
/// with them. What the machine *is* needs no sampling and is not a field — see
/// `SOFTWARE`, `firmware` and `kernel`.
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
            "firmware": firmware(),
            "kernel": kernel(),
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

    /// The three systems this is built for, in the two words the page shows:
    /// the distribution's own name, and the version without anything the
    /// builder added around it.
    #[test]
    fn the_firmware_is_the_distribution_and_its_version() {
        let ubuntu = "\
PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"
NAME=\"Ubuntu\"
VERSION_ID=\"24.04\"
VERSION=\"24.04.1 LTS (Noble Numbat)\"
";
        assert_eq!(os_firmware(ubuntu).as_deref(), Some("Ubuntu 24.04"));

        // Buildroot old enough to write no id: the version is the one there is.
        let buildroot = "NAME=Buildroot\nVERSION=2024.02.1\nID=buildroot\n";
        assert_eq!(os_firmware(buildroot).as_deref(), Some("Buildroot 2024.02.1"));

        // The revision is in the pretty name and is not part of the version.
        let immortal = "\
NAME=\"ImmortalWrt\"
VERSION=\"23.05.3\"
ID=\"immortalwrt\"
VERSION_ID=\"23.05.3\"
PRETTY_NAME=\"ImmortalWrt 23.05.3 r27840-6b0f2b1\"
";
        assert_eq!(
            os_firmware(immortal).as_deref(),
            Some("ImmortalWrt 23.05.3")
        );
    }

    /// Quotes are the file's business, and a key is matched whole: `ID` is a
    /// line of its own, not the first half of `ID_LIKE`.
    #[test]
    fn a_field_is_read_without_its_quotes() {
        let text = "# a comment\nNAME='Alpine Linux'\nVERSION_ID=\"3.20\"\nID=alpine\n";
        assert_eq!(os_field(text, "NAME").as_deref(), Some("Alpine Linux"));
        assert_eq!(os_field(text, "VERSION_ID").as_deref(), Some("3.20"));
        assert_eq!(os_field(text, "ID_LIKE"), None, "a key that starts the same");
        assert_eq!(os_field(text, "MISSING"), None, "a key that is not there");
    }

    /// A page is shown which system it is talking to on its first answer, not
    /// after the first sample: neither version is a reading of a counter.
    #[test]
    fn the_system_is_named_on_every_answer() {
        let body = Device::default().json();
        assert!(body.get("firmware").is_some(), "the key is there");
        assert!(body.get("kernel").is_some(), "and so is the other");
    }
}
