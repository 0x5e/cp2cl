//! The board's one button: a press held for three seconds.
//!
//! The device tree already declares it as a `gpio-keys` key, so nothing here
//! reads a GPIO: `/dev/input/eventN` reports the press and the release, and the
//! three seconds are counted here. That is the only way to get a hold out of a
//! driver that reports edges — none of the events it sends means "still down".
//!
//! What a long press does is one log line, for now.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{sleep_until, Instant};

/// How long the button has to stay down to count as held.
const HELD: Duration = Duration::from_secs(3);

/// The name the driver gives its device, which is how the node is found: this
/// image has no udev, so there is no by-path link to ask.
const NAME: &str = "gpio-keys";

/// `EV_KEY`, and the value that says the key went down. Zero is the release and
/// two is autorepeat, which this driver does not send.
const KEY: u16 = 0x01;
const DOWN: i32 = 1;

/// One event record: the kernel's own word-sized time fields, then type, code
/// and value. Counted from the pointer width rather than from
/// `struct input_event`, whose userspace layout is not the kernel's on a 32-bit
/// target with a 64-bit `time_t`.
const EVENT_LEN: usize = if std::mem::size_of::<usize>() == 4 {
    16
} else {
    24
};

pub async fn watch() {
    let Some(path) = device() else {
        tracing::warn!("button.unavailable name={NAME} consequence=long_press_unused");
        return;
    };
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!("button.open_failed path={} err={e}", path.display());
            return;
        }
    };
    tracing::info!("button.ready path={}", path.display());

    let (tx, mut presses) = mpsc::channel(8);
    // Read where it blocks: an event node has nothing to read most of the time,
    // and a thread parked in `read` costs less than a task woken to find out.
    std::thread::spawn(move || read_presses(file, tx));

    let mut held_until: Option<Instant> = None;
    loop {
        tokio::select! {
            press = presses.recv() => match press {
                // The descriptor is gone; there is nothing left to watch.
                None => return,
                Some(true) => held_until = Some(Instant::now() + HELD),
                Some(false) => held_until = None,
            },
            // Idle unless something is being held: the guard decides that, and
            // the deadline is only read when it does.
            _ = sleep_until(held_until.unwrap_or_else(Instant::now)), if held_until.is_some() => {
                tracing::info!("button.long_press held_ms={}", HELD.as_millis());
                // One report per press: the release is what arms the next one.
                held_until = None;
            }
        }
    }
}

/// Reads the node until it goes away, handing each press and release over.
fn read_presses(mut file: File, out: mpsc::Sender<bool>) {
    let mut buf = [0u8; 8 * EVENT_LEN];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!("button.read_failed err={e}");
                return;
            }
        };
        for (kind, _, value) in events(&buf[..n]) {
            if kind != KEY {
                continue;
            }
            let pressed = match value {
                DOWN => true,
                0 => false,
                _ => continue,
            };
            if out.blocking_send(pressed).is_err() {
                return;
            }
        }
    }
}

/// The `(type, code, value)` of every complete record in `buf`.
fn events(buf: &[u8]) -> impl Iterator<Item = (u16, u16, i32)> + '_ {
    buf.chunks_exact(EVENT_LEN).map(|event| {
        let tail = &event[EVENT_LEN - 8..];
        (
            u16::from_ne_bytes([tail[0], tail[1]]),
            u16::from_ne_bytes([tail[2], tail[3]]),
            i32::from_ne_bytes([tail[4], tail[5], tail[6], tail[7]]),
        )
    })
}

/// Where the button's events are. Found through the class directory rather than
/// by number: which `eventN` it is depends on what probed before it.
fn device() -> Option<PathBuf> {
    for entry in std::fs::read_dir("/sys/class/input").ok()?.flatten() {
        let node = entry.file_name();
        let Some(node) = node.to_str() else { continue };
        if !node.starts_with("event") {
            continue;
        }
        let Ok(name) = std::fs::read_to_string(entry.path().join("device/name")) else {
            continue;
        };
        if name.trim() == NAME {
            return Some(Path::new("/dev/input").join(node));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_press_is_read_out_of_the_record_the_kernel_writes() {
        let mut record = vec![0u8; EVENT_LEN];
        record[EVENT_LEN - 8..EVENT_LEN - 6].copy_from_slice(&KEY.to_ne_bytes());
        // BTN_0, which is the code the device tree gives `k1`.
        record[EVENT_LEN - 6..EVENT_LEN - 4].copy_from_slice(&0x100u16.to_ne_bytes());
        record[EVENT_LEN - 4..].copy_from_slice(&DOWN.to_ne_bytes());
        assert_eq!(events(&record).collect::<Vec<_>>(), [(KEY, 0x100, DOWN)]);
    }
}
