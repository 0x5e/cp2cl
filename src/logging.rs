//! What every line is made of: the stamp in front of it and the module column
//! beside the event name.
//!
//! `tracing_subscriber`'s own timer writes an ISO 8601 instant — date, time and
//! zone — which is more than a run that lasts minutes needs, and the board's
//! clock does not keep the date anyway: neither link it has — the phone's or
//! the car's — carries a wall clock it could be set from. What is wanted is a
//! stamp that only ever goes up, so lines can be put in order against each
//! other and spaced: seconds since boot, to the millisecond.

use std::fmt::{self, Write as _};
use std::mem::MaybeUninit;
use std::time::Duration;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::FmtContext;
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

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use super::{area_of, Line, GREY, RESET};

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
