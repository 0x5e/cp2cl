//! The picture the car is shown while the phone's screen stream is not open.
//!
//! The video channel carries H.264 and nothing else, so the guide is a coded
//! stream rather than a picture. The streams under `assets/guide/` are built
//! from `guide/index.html` by `scripts/guide.sh`, one file per screen size; none
//! is encoded at run time. A board carries no encoder, and a guide that failed
//! to encode would be a black screen at the one moment it is wanted.
//!
//! A screen no file was built for is given the file whose shape is closest. A
//! build carrying no guide at all leaves the channel to its heartbeats, which is
//! what it had before there was a guide.

use crate::assets;

/// Where the coded streams are, and what their names state:
/// `guide/1920x720.h264`.
const DIR: &str = "guide/";
const SUFFIX: &str = ".h264";

/// The guide's frames, in the order the file holds them, started over at the
/// end.
///
/// The file holds a second of pictures — one keyframe and the skipped frames
/// after it — because a stream of nothing but keyframes costs a keyframe a
/// frame. Looping it is what keeps the picture up: a guide that ran out would
/// leave the car with nothing to show.
pub struct Guide {
    units: Vec<Vec<u8>>,
    next: usize,
}

impl Guide {
    /// A guide that cycles through `units`. Nothing to show is no guide.
    pub fn cycle(units: Vec<Vec<u8>>) -> Option<Self> {
        (!units.is_empty()).then_some(Self { units, next: 0 })
    }

    /// The next frame, which is the first again once the file has run out.
    pub fn next(&mut self) -> Vec<u8> {
        let unit = self.units[self.next].clone();
        self.next = (self.next + 1) % self.units.len();
        unit
    }
}

/// The coded stream built for a screen `width` x `height`.
///
/// `None` is a build with no guide in it, which is a state the session already
/// knows how to run in.
pub fn picture(width: u32, height: u32) -> Option<&'static [u8]> {
    let exact = format!("{DIR}{width}x{height}{SUFFIX}");
    if let Some(bytes) = assets::get(&exact) {
        return Some(bytes);
    }
    // Nothing was built at this size. The file whose shape is closest is the one
    // the car has to stretch least: two shapes ship, and a screen between them
    // is a few percent out whichever way it is given. A picture a little out of
    // shape beats a black screen.
    let wanted = width.max(1) as f64 / height.max(1) as f64;
    let mut best: Option<(f64, &'static [u8])> = None;
    for name in assets::names() {
        let Some((w, h)) = stated(name) else { continue };
        let Some(bytes) = assets::get(name) else { continue };
        let off = (w as f64 / h as f64 / wanted).ln().abs();
        if best.map_or(true, |(known, _)| off < known) {
            best = Some((off, bytes));
        }
    }
    let (off, bytes) = best?;
    tracing::info!("carlife.guide_stretched width={width} height={height} off={off:.3}");
    Some(bytes)
}

/// The size a guide's name states, for `guide/1920x720.h264`. Any other file in
/// the binary is not a guide and states nothing.
fn stated(name: &str) -> Option<(u32, u32)> {
    let rest = name.strip_prefix(DIR)?.strip_suffix(SUFFIX)?;
    let (width, height) = rest.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guides_name_states_its_size() {
        assert_eq!(stated("guide/1920x720.h264"), Some((1920, 720)));
        assert_eq!(stated("guide/1280x480.h264"), Some((1280, 480)));
    }

    #[test]
    fn nothing_else_states_a_size() {
        for name in [
            "web/index.html",
            "icons/Default-256.png",
            "guide/manifest.txt",
            "guide/1920x720.png",
            "guide/1920by720.h264",
        ] {
            assert_eq!(stated(name), None, "{name} is not a guide");
        }
    }

    #[test]
    fn an_empty_file_is_no_guide() {
        assert!(Guide::cycle(Vec::new()).is_none());
    }

    #[test]
    fn the_file_starts_over_at_the_end() {
        let mut guide = Guide::cycle(vec![b"one".to_vec(), b"two".to_vec()]).expect("two frames");
        for expected in [&b"one"[..], b"two", b"one", b"two"] {
            assert_eq!(guide.next(), expected);
        }
    }
}
