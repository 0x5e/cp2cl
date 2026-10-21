//! What every line is made of: the stamp in front of it and the module column
//! beside the event name, and where a copy of them is kept.
//!
//! `tracing_subscriber`'s own timer writes an ISO 8601 instant — date, time and
//! zone — which is more than a run that lasts minutes needs, and the board's
//! clock does not keep the date anyway: neither link it has — the phone's or
//! the car's — carries a wall clock it could be set from. What is wanted is a
//! stamp that only ever goes up, so lines can be put in order against each
//! other and spaced: seconds since boot, to the millisecond.
//!
//! Two layers write them, and it is the same formatter in both: one to the
//! terminal, coloured, and one to a copy on disk that is not, because the copy
//! is read by a page and by a shell rather than by an eye at a terminal. The
//! copy is capped and rolled — see `Journal` — since `/tmp` on the board is
//! memory, and it is what the status page reads.

use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::{FmtContext, MakeWriter};
use tracing_subscriber::registry::LookupSpan;

/// Writes the seconds since boot, to the millisecond (`123.456`). The space
/// after it is the line's, not this one's.
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

/// A line is `<seconds since boot> <LEVEL> [<area>]\t<event name> <fields>`.
///
/// Where the writer takes them, the stamp is written grey and the level in its
/// own colour: both are glanced at rather than read, unlike the name and the
/// fields that follow them.
///
/// The bracketed area is the module column, and it is read off the event name
/// rather than passed in: a call site cannot then say one thing in a column and
/// another in the name, and the name is left whole for `grep`.
pub struct Line;

/// The column is as wide as the longest area name, `wireless`, so that the tab
/// after it always reaches the same stop. A tab on its own does not do that: a
/// name whose bracket ends exactly on one of the eight-column stops jumps a
/// whole stop, which is what `[airplay]` does once the stamp has grown to four
/// digits — eight columns to the right of everything else.
const AREA_WIDTH: usize = 8;

impl<S, N> FormatEvent<S, N> for Line
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut w: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let level = *event.metadata().level();
        let mut said = Said::default();
        event.record(&mut said);

        let colours = w.has_ansi_escapes();
        if colours {
            w.write_str(GREY)?;
        }
        Clock.format_time(&mut w)?;
        if colours {
            w.write_str(RESET)?;
            write!(w, " {}{level:>5}{RESET} ", level_colour(level))?;
        } else {
            write!(w, " {level:>5} ")?;
        }
        if let Some(area) = area_of(&said.message) {
            let pad = AREA_WIDTH.saturating_sub(area.len());
            write!(w, "[{area}]{:pad$}\t", "")?;
        }
        w.write_str(&said.message)?;
        w.write_str(&said.rest)?;
        writeln!(w)
    }
}

/// The area a line belongs to: the first segment of its event name.
///
/// `None` for a message that does not begin with one, which is printed as it
/// was written — the column is worth having, not worth failing over.
fn area_of(message: &str) -> Option<&str> {
    let (area, rest) = message.split_once('.')?;
    let mut chars = area.chars();
    let named = matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    (named && !rest.is_empty()).then_some(area)
}

/// The grey the stamp is written in, and the escape that closes it.
///
/// Greyed because it is what is read past: how long ago something happened is
/// looked up, while what happened is read.
const GREY: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

fn level_colour(level: Level) -> &'static str {
    if level == Level::ERROR {
        "\x1b[31m"
    } else if level == Level::WARN {
        "\x1b[33m"
    } else if level == Level::INFO {
        "\x1b[32m"
    } else if level == Level::DEBUG {
        "\x1b[34m"
    } else {
        "\x1b[35m"
    }
}

/// Everything an event carries: the message, and whatever else was recorded
/// beside it, which is ` key=value` the way the message itself is written.
#[derive(Default)]
struct Said {
    message: String,
    rest: String,
}

impl Visit for Said {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.rest, " {}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.rest, " {}={value:?}", field.name());
        }
    }
}

// --------------------------------------------------------------- the copy ---

/// Where the plain copy of the log is written, and where the chunk before it is
/// kept. Both are in `/tmp`, which is memory on the board, which is why neither
/// is left to grow: see `CAP`.
pub const PATH: &str = "/tmp/cp2cl.log";
const PREVIOUS: &str = "/tmp/cp2cl.log.1";

/// How big a chunk is allowed to get before it is rolled over, in bytes.
///
/// At the level a session is watched at — a line for every link that comes up,
/// every stream that opens and every second of audio — four megabytes is the
/// better part of an hour; at `debug` it is minutes. What the copy is for is
/// the run that is happening and the one before it, and no more.
const CAP: u64 = 4 * 1024 * 1024;

/// The copy of the log that goes to disk: the same lines, uncoloured.
///
/// A `MakeWriter` rather than a writer, because the layer asks for one per line
/// and that is the moment to see whether the chunk has reached `CAP` and roll
/// it. A line is written whole — the layer formats it into a buffer and hands
/// it over in one `write` — so a copy that is read while it is being written
/// never has half a line at the end of it.
pub struct Journal {
    path: PathBuf,
    previous: PathBuf,
    file: Mutex<Option<File>>,
    /// Why there is no copy, if there is not one. Said by the caller, once the
    /// subscriber is up: from in here there is nowhere to say it, since saying
    /// it is what would have to be written.
    reason: Option<String>,
}

impl Journal {
    /// Opens the copy, keeping the one the run before this one left behind.
    ///
    /// The run that is looked at is usually the one that just ended, so it is
    /// moved aside rather than appended to: a copy with two runs in it has to
    /// be read around, and the moment they meet is the moment that matters.
    pub fn open() -> Self {
        Self::at(Path::new(PATH), Path::new(PREVIOUS))
    }

    /// Why there was no copy to start, which is `None` when there was one.
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    fn at(path: &Path, previous: &Path) -> Self {
        match start(path, previous) {
            Ok(file) => Self {
                path: path.to_owned(),
                previous: previous.to_owned(),
                file: Mutex::new(Some(file)),
                reason: None,
            },
            Err(e) => Self {
                path: path.to_owned(),
                previous: previous.to_owned(),
                file: Mutex::new(None),
                reason: Some(format!("{e:#}")),
            },
        }
    }
}

fn start(path: &Path, previous: &Path) -> io::Result<File> {
    // Nothing to keep on a first run, and a rename that finds nothing is not
    // worth an answer of its own.
    let _ = fs::rename(path, previous);
    chunk(path)
}

/// Opens the chunk for writing at its end.
///
/// Not with `append`, because a file opened that way cannot be truncated and a
/// chunk that could not be moved aside has to be cut back to nothing instead.
/// The position is the end all the same: the layer writes a whole line at a
/// time, one writer holds the file while it does, and a rolled chunk is opened
/// again.
fn chunk(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
}

impl<'a> MakeWriter<'a> for Journal {
    type Writer = Sink<'a>;

    fn make_writer(&'a self) -> Sink<'a> {
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        if file.as_ref().is_some_and(|open| over(open, CAP)) {
            *file = roll(&self.path, &self.previous);
        }
        Sink(file)
    }
}

/// Whether the chunk has reached the cap, which is when it is rolled.
fn over(file: &File, cap: u64) -> bool {
    file.metadata().map(|m| m.len() >= cap).unwrap_or(false)
}

/// Moves the chunk aside and starts another, giving back the open file.
///
/// `None` when there is no copy to be had any more, which ends it: a board
/// cannot be filled up by its own log, and the terminal still has the lines.
fn roll(path: &Path, previous: &Path) -> Option<File> {
    // The chunk is moved aside whole, so what is opened in its place is a new
    // file; where the move fails, the chunk is cut back to nothing instead of
    // being left to grow.
    let _ = fs::rename(path, previous);
    chunk(path).ok()
}

/// What the layer writes a copy through: the open chunk, or nowhere at all
/// when there is not one.
pub struct Sink<'a>(MutexGuard<'a, Option<File>>);

impl io::Write for Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.as_mut() {
            Some(file) => io::Write::write(file, buf),
            // Nowhere to put it is not a reason to stop printing it: the layer
            // is told it was written so it carries on to the next line.
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0.as_mut() {
            Some(file) => io::Write::flush(file),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter as _;
    use tracing_subscriber::prelude::*;

    use super::{area_of, roll, Journal, Line, GREY, RESET};

    /// A chunk of the copy in the temporary directory, under a name of its own
    /// so that a test never writes where the program would.
    fn scratch(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("cp2cl-{}-{name}", std::process::id()));
        path
    }

    #[test]
    fn a_chunk_that_reaches_the_cap_is_rolled_over() {
        let (path, previous) = (scratch("roll.log"), scratch("roll.log.1"));
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        io::Write::write_all(&mut file, b"12345").unwrap();

        assert!(super::over(&file, 5), "five bytes against a cap of five");
        assert!(!super::over(&file, 6), "and not against one of six");

        let rolled = roll(&path, &previous).expect("a chunk can be rolled");
        assert_eq!(fs::read(&previous).unwrap(), b"12345", "the old one is kept");
        assert!(!super::over(&rolled, 1), "and the new one starts empty");

        fs::remove_file(&path).ok();
        fs::remove_file(&previous).ok();
    }

    #[test]
    fn a_copy_that_cannot_be_started_says_why_and_loses_the_lines() {
        let journal = Journal::at(
            Path::new("/nonexistent/cp2cl.log"),
            Path::new("/nonexistent/cp2cl.log.1"),
        );
        assert!(
            journal.reason().is_some(),
            "a copy in a directory that is not there has a reason"
        );

        // What is written to it goes nowhere, and writing to it is not an
        // error: the terminal still has the lines.
        let mut sink = journal.make_writer();
        io::Write::write_all(&mut sink, b"dropped\n").unwrap();
    }

    #[test]
    fn the_copy_is_the_same_line_without_the_colour() {
        let (path, previous) = (scratch("plain.log"), scratch("plain.log.1"));
        let journal = Journal::at(&path, &previous);
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .event_format(Line)
                .with_ansi(false)
                .with_writer(journal),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("cl.encoder state=started");
        });

        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(
            text.ends_with(" INFO [cl]      \tcl.encoder state=started\n"),
            "{text:?}"
        );

        fs::remove_file(&path).ok();
        fs::remove_file(&previous).ok();
    }

    #[test]
    fn the_area_is_the_first_segment_of_the_event_name() {
        assert_eq!(area_of("cl.encoder state=started"), Some("cl"));
        assert_eq!(area_of("usb.control_request request=51"), Some("usb"));
        assert_eq!(area_of("process.started name=cp2cl"), Some("process"));
        assert_eq!(area_of("cl.video_channel source=first"), Some("cl"));
    }

    #[test]
    fn a_message_that_does_not_begin_with_an_event_name_has_no_area() {
        assert_eq!(area_of("192.168.4.1 went away"), None);
        assert_eq!(area_of("C.encoder state=started"), None);
        assert_eq!(area_of("cl."), None);
        assert_eq!(area_of(""), None);
    }

    #[test]
    fn a_line_is_the_stamp_the_level_the_column_and_the_event_name() {
        let text = plain(&render("cl.encoder state=started"));
        // `[cl]` padded out to the ten columns `[wireless]` needs, then the tab.
        assert!(
            text.ends_with(" INFO [cl]      \tcl.encoder state=started\n"),
            "{text:?}"
        );
    }

    #[test]
    fn the_event_name_starts_in_the_same_column_whatever_the_area() {
        let short = expand(&plain(&render("cl.encoder state=started")));
        let long = expand(&plain(&render("airplay.held reason=car_session_not_up")));
        // Measured from the bracket, since the stamp is as wide as the uptime
        // makes it and both lines need not have been written in the same second.
        let column = |line: &str, name: &str| {
            line.find(name).unwrap() - line.find('[').unwrap()
        };
        assert_eq!(column(&short, "cl.encoder"), column(&long, "airplay.held"));
    }

    #[test]
    fn the_stamp_is_the_only_grey_part_of_the_line() {
        let text = render("cl.encoder state=started");
        let rest = text.strip_prefix(GREY).expect("the stamp is grey");
        let (stamp, rest) = rest.split_once(RESET).expect("the grey is shut");
        assert!(
            stamp.chars().all(|c| c.is_ascii_digit() || c == '.'),
            "{stamp:?}"
        );
        assert!(rest.starts_with(" \x1b[32m"), "{rest:?}");
        assert_eq!(text.matches(RESET).count(), 2, "{text:?}");
    }

    /// One line as the layer would have written it.
    fn render(message: &str) -> String {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let lines = tracing_subscriber::fmt()
            .event_format(Line)
            .with_writer(move || Sink(sink.clone()))
            .finish();
        tracing::subscriber::with_default(lines, || tracing::info!("{message}"));
        let written = seen.lock().unwrap().clone();
        String::from_utf8(written).unwrap()
    }

    /// The line with its colouring taken out, which is what a file or a pipe
    /// would have seen.
    fn plain(text: &str) -> String {
        let (mut out, mut escape) = (String::new(), false);
        for c in text.chars() {
            if escape {
                escape = c != 'm';
            } else if c == '\x1b' {
                escape = true;
            } else {
                out.push(c);
            }
        }
        out
    }

    /// The line with its tabs dealt with the way a terminal deals with them:
    /// the next stop to the right, every eight columns.
    fn expand(text: &str) -> String {
        let mut out = String::new();
        for c in text.chars() {
            if c == '\t' {
                out.push_str(&" ".repeat(8 - out.len() % 8));
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Collects what the layer writes, so a line can be read back as it would
    /// appear on a terminal.
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
