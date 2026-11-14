//! The status tab: what the page shows, and the two answers it is served.
//!
//! Neither half of the bridge is read for anything here. The CarLife crate keeps
//! what the car told it (`page`), the CarPlay crate what the phone did
//! (`report`), and this module holds only the part that belongs to neither: the
//! board's own CPU and memory, which is sampled here — nothing in the bridge
//! measures its own CPU time, and the kernel already counts both quantities in
//! `/proc`, so they are read from there rather than worked out a second way.
//!
//! A percentage takes two readings — the kernel keeps counters, not a rate — so
//! the sampling runs on its own instead of being asked for. A reader that polled
//! for it would be shown a share of whatever interval happened to sit between
//! its own two requests, which is a different measurement every time the page is
//! opened.
//!
//! The shape of the answer is the page's contract, and it is written down once —
//! in `Status::json` — rather than in the three places that produce it. The other
//! answer is the picture on the car's panel, which is bytes rather than JSON and
//! has a path of its own.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::extract::{RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use carlife_md::report::{Artwork, Report as CarReport};
use carplay_hu::report::Report;

/// Everything the page reads, in one place.
///
/// Three locks rather than one: each half writes its own and nothing here takes
/// two at once, so a page can catch one half a moment ahead of the other — which
/// is what a live view of two independent processes looks like.
pub struct Status {
    /// What the CarLife half reports. Written there, read here.
    carlife: Arc<CarReport>,
    /// What the CarPlay half reports. Written there, read here.
    report: Arc<Report>,
    /// What the board itself is doing, which is sampled rather than reported.
    sampled: Mutex<Sampled>,
}

impl Status {
    /// Around the CarPlay half's own report, which it writes for itself.
    pub fn new(report: Arc<Report>) -> Self {
        Self {
            carlife: Arc::new(CarReport::new()),
            report,
            sampled: Mutex::new(Sampled::default()),
        }
    }

    /// The car's half, for the side that writes it.
    pub fn carlife(&self) -> &Arc<CarReport> {
        &self.carlife
    }

    /// Reports what the board is doing. Nothing else writes through here.
    pub fn with(&self, report: impl FnOnce(&mut Sampled)) {
        // A poisoned lock means a sample panicked half way through. The data
        // behind it is still the best account anyone has, so it is taken rather
        // than thrown away: the page is not worth failing a bridge over.
        let mut sampled = self.sampled.lock().unwrap_or_else(|e| e.into_inner());
        report(&mut sampled);
    }

    /// The whole thing, as the page is served it.
    pub fn json(&self) -> Value {
        // Taken out of the lock before the other two are read, so no two locks
        // are ever held at once.
        let sampled = {
            let held = self.sampled.lock().unwrap_or_else(|e| e.into_inner());
            held.clone()
        };
        json!({
            "carlife": self.carlife.json(self.report.coding()),
            "carplay": self.report.json(),
            "now_playing": self.carlife.song_json(),
            "device": sampled.device.json(),
        })
    }

    /// The cover the car was last given, for the page to draw.
    pub fn artwork(&self) -> Option<Artwork> {
        self.carlife.artwork()
    }
}

/// What is sampled rather than reported: the board's own numbers, and nothing
/// else.
#[derive(Debug, Clone, Default)]
pub struct Sampled {
    /// The machine itself — see `Device`.
    pub device: Device,
}

/// The one answer the page reads, and the one it draws rather than reads.
///
/// The snapshot is never cached: the whole point of it is that the answer is
/// different a moment later, and a browser holding on to it would be showing the
/// state of a bridge that has since moved on.
pub async fn api(State(status): State<Arc<Status>>) -> Response {
    (
        [
            (header::CONTENT_TYPE, super::JSON),
            (header::CACHE_CONTROL, "no-store"),
        ],
        status.json().to_string(),
    )
        .into_response()
}

/// A cover is fetched by the name of the cover, so an answer is good for as
/// long as that name is — which is until the car is given another picture.
const COVER_CACHE: &str = "public, max-age=31536000, immutable";

const JPEG: &str = "image/jpeg";

/// The picture on the car's panel, which is the one answer in the page that is
/// not JSON.
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
pub async fn cover(State(status): State<Arc<Status>>, RawQuery(query): RawQuery) -> Response {
    let Some(cover) = status.artwork() else {
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

    use carlife_md::report::Geometry;

    use crate::web::embedded;

    /// A page with both halves empty and nothing sampled yet: what is served
    /// before a run has learned anything.
    fn empty_status() -> Status {
        Status::new(Arc::new(carplay_hu::report::Report::new()))
    }

    fn page_text() -> &'static str {
        embedded("web/index.html")
    }

    /// The page is one document and reads one key per part, so the snapshot has
    /// to keep answering under those names.
    #[test]
    fn the_endpoint_answers_with_every_part() {
        let status = empty_status();
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
        let body = empty_status().json();
        assert!(body["now_playing"]["duration_s"].is_null());
    }

    /// The bar and the play state on the page are the ones the car was given.
    /// A number worked out again for the page could differ from the one on the
    /// wire, and then the page would be describing a bar that is not there.
    #[test]
    fn the_page_shows_the_bar_the_car_was_given() {
        let status = empty_status();
        assert!(status.json()["now_playing"]["progress_ms"].is_null());
        assert!(status.json()["now_playing"]["playing"].is_null());

        status.carlife().with(|s| {
            s.now_playing.progress_ms = Some(20_000);
            s.now_playing.playing = Some(true);
        });
        assert_eq!(status.json()["now_playing"]["progress_ms"], 20_000);
        assert_eq!(status.json()["now_playing"]["playing"], true);

        // A new session starts having told the car nothing at all.
        status.carlife().song_cleared();
        assert!(status.json()["now_playing"]["progress_ms"].is_null());
        assert!(status.json()["now_playing"]["playing"].is_null());

        assert!(
            page_text().contains("playing.progress_ms"),
            "the bar is drawn from the position the car was given"
        );
        // Before a position is sent the row is still drawn, at the start of the
        // song, so the length is on the page from the moment there is a song.
        assert!(
            !page_text().contains("progressLine.style.display"),
            "the bar is never taken out of the page"
        );
        assert!(
            page_text().contains("播放中"),
            "and the state is shown as one"
        );
    }

    /// A cover is served until another replaces it, and it is named by what is
    /// in it: the same picture twice costs the page one fetch, a different one
    /// never reuses the name, and an empty one is no picture at all.
    #[test]
    fn a_cover_is_named_by_what_is_in_it() {
        let status = empty_status();
        assert!(status.artwork().is_none(), "nothing to draw yet");

        status.carlife().artwork_changed(vec![0xff, 0xd8, 0xff]);
        let first = status.json()["now_playing"]["cover"]["id"].clone();
        assert_eq!(status.artwork().map(|c| c.bytes.len()), Some(3));

        // The same bytes again are the same picture, so the page keeps it.
        status.carlife().artwork_changed(vec![0xff, 0xd8, 0xff]);
        assert_eq!(status.json()["now_playing"]["cover"]["id"], first);

        // Another picture is another name.
        status.carlife().artwork_changed(vec![0x42]);
        assert_ne!(status.json()["now_playing"]["cover"]["id"], first);

        // An empty one is the phone saying this song has no picture.
        status.carlife().artwork_changed(Vec::new());
        assert!(status.artwork().is_none());
        assert!(status.json()["now_playing"]["cover"].is_null());

        // And a new session starts with nothing, which lets the bytes go.
        status.carlife().artwork_changed(vec![1, 2, 3]);
        status.carlife().song_cleared();
        assert!(status.artwork().is_none());
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

        let status = Arc::new(empty_status());
        let ask = |query: Option<&str>| {
            cover(State(status.clone()), RawQuery(query.map(str::to_owned)))
        };

        // Nothing to draw yet, whatever is asked for.
        assert_eq!(ask(Some("id=0")).await.status(), StatusCode::NOT_FOUND);

        status.carlife().artwork_changed(vec![0xff, 0xd8, 0xff]);
        let name = format!("{:016x}", status.artwork().unwrap().id);

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
        let report = Arc::new(carplay_hu::report::Report::new());
        let status = Status::new(report.clone());
        let body = status.json();
        assert_eq!(
            body["carplay"]["video"]["codec"],
            carplay_hu::CODEC,
            "which is what the snapshot carries"
        );
        assert!(
            body["carplay"]["video"]["profile"].is_null(),
            "nothing described yet"
        );
        assert_eq!(body["carlife"]["video"]["codec"], carplay_hu::CODEC);

        report.with(|s| {
            s.video.profile = Some("Baseline".to_string());
            s.video.level = Some("3.1".to_string());
        });
        let body = status.json();
        for half in ["carplay", "carlife"] {
            assert_eq!(body[half]["video"]["profile"], "Baseline");
            assert_eq!(body[half]["video"]["level"], "3.1");
        }
    }

    /// A PCM stream is described the same way on both halves, so the answer
    /// carries the sample width and the page reads it rather than naming one.
    #[test]
    fn the_page_reads_the_sample_width_from_the_answer() {
        assert!(
            page_text().contains("bits(a.bits)"),
            "the phone's half reads the field"
        );
        assert!(
            page_text().contains("bits(d.media.bits)"),
            "the car's half reads the field"
        );
        assert!(!page_text().contains("16-bit"), "and names no width itself");

        let report = Arc::new(carplay_hu::report::Report::new());
        report.with(|s| {
            s.carplay.audio.push(carplay_hu::report::Audio {
                name: "media".into(),
                codec: "PCM".into(),
                bits: Some(16),
                rate: 48_000,
                channels: 2,
                ..Default::default()
            });
        });
        let body = Status::new(report).json();
        assert_eq!(body["carplay"]["audio"][0]["bits"], 16);
    }

    /// The panel is the car's, so the page shows its physical size on the car's
    /// side, next to the size the car named, and reads it from the answer.
    #[test]
    fn the_page_reads_the_panel_size_from_the_cars_half() {
        assert!(
            page_text().contains("d.physical"),
            "the page reads the field"
        );
        let status = empty_status();
        status.carlife().with(|s| s.carlife.physical = Some((407, 229)));
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
            embedded("carlife_channel_map.json").contains("\"video\""),
            "and the table is where the number comes from"
        );

        let status = empty_status();
        assert!(
            status.json()["carlife"]["agreed"].is_null(),
            "no screen has been named yet"
        );

        status.carlife().with(|s| {
            s.carlife.agreed = Some(Geometry {
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

        let status = empty_status();
        assert!(
            status.json()["carlife"]["gps"].is_null(),
            "a car that has not reported a position yet is not one at zero degrees"
        );

        status.carlife().with(|s| {
            s.carlife.gps = Some(carlife_md::CarGps {
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

        let status = empty_status();
        assert!(
            status.json()["carlife"]["speed"].is_null(),
            "a car that has not reported a speed yet is not one at zero"
        );

        // Zero is a speed the car has reported, and it is shown as one.
        status.carlife().with(|s| s.carlife.speed = Some(0));
        assert_eq!(status.json()["carlife"]["speed"], 0);
    }

    /// The board's own CPU and memory are their own part of the page, and the
    /// page writes both rows itself: a percent from the share the answer gives
    /// it, and memory from the two byte counts.
    #[test]
    fn the_page_reads_the_device_from_its_own_part() {
        assert!(page_text().contains("设备端"), "it has a heading of its own");
        assert!(
            page_text().contains("data.device"),
            "and the page reads the part"
        );
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
        let status = empty_status();
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
