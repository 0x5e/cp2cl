//! The picture the car is shown while the phone's screen stream is not open.
//!
//! The video channel carries H.264 and nothing else, so the guide is a coded
//! stream rather than a picture. The streams under `assets/guide/` are built
//! from `guide/index.html` by `guide/build.sh`, one file per screen size; none
//! is encoded at run time. A board carries no encoder, and a guide that failed
//! to encode would be a black screen at the one moment it is wanted.
//!
//! None of this belongs to either half: which picture a car is shown while its
//! phone has nothing to show is the program's decision, and the CarLife half
//! only writes out whatever reaches the video queue. So the guide is frames
//! pushed onto that queue, and the session is what carries them — the same road
//! the phone's own pictures take.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use carlife_md::outlet::Outlet;
use carplay_hu::Geometry;

/// Where the coded streams are, and what their names state:
/// `guide/1920x720.h264`.
const DIR: &str = "guide/";
const SUFFIX: &str = ".h264";

/// The rate the guide goes out at when the car has not named one.
const DEFAULT_FPS: u32 = 30;

/// Keeps the video queue fed while there is a car to show the guide to and the
/// phone has no picture of its own.
///
/// Three things decide whether a frame goes out, and any of them can change
/// while the wait for the next one is running: whether a car session is up,
/// whether the phone's screen stream is open, and which size the car named —
/// the last being what the picture is chosen by and how often it is sent.
pub async fn run(
    outlet: Arc<Outlet>,
    mut session: watch::Receiver<bool>,
    mut screen: watch::Receiver<Option<Geometry>>,
) {
    let sink = outlet.sink();
    let mut open = outlet.streaming();
    let mut guide: Option<Guide> = None;
    let mut shape = (0, 0);
    let mut period = Duration::from_millis(1000 / u64::from(DEFAULT_FPS));

    loop {
        // What the guide is, for the size the car named and at the rate it
        // named. Both come out of the same message, so they change together.
        match *screen.borrow() {
            Some(geometry) => {
                let size = (geometry.width, geometry.height);
                if guide.is_none() || shape != size {
                    shape = size;
                    guide = stream(size.0, size.1);
                    tracing::info!(
                        "guide.loaded width={} height={} frames={}",
                        size.0,
                        size.1,
                        guide.as_ref().map_or(0, |guide| guide.units.len())
                    );
                }
                if geometry.fps > 0 {
                    period = Duration::from_millis(1000 / u64::from(geometry.fps));
                }
            }
            // Nothing has said what to draw for yet. A guide is a picture of a
            // panel's shape, and there is no shape.
            None => guide = None,
        }

        if guide.is_none() || !*session.borrow() || *open.borrow() {
            if !waited(&mut session, &mut screen, &mut open).await {
                return;
            }
            continue;
        }

        let showing = guide.as_mut().expect("checked just above");
        tokio::select! {
            _ = tokio::time::sleep(period) => sink.video(showing.next()),
            _ = waited(&mut session, &mut screen, &mut open) => {}
        }
    }
}

/// Waits for whichever of the three says something new, and whether the program
/// is still there to be told.
async fn waited(
    session: &mut watch::Receiver<bool>,
    screen: &mut watch::Receiver<Option<Geometry>>,
    open: &mut watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        changed = session.changed() => changed.is_ok(),
        changed = screen.changed() => changed.is_ok(),
        changed = open.changed() => changed.is_ok(),
    }
}

/// The guide's frames, in the order the file holds them, started over at the
/// end.
///
/// The file holds a second of pictures — one keyframe and the skipped frames
/// after it — because a stream of nothing but keyframes costs a keyframe a
/// frame. Looping it is what keeps the picture up: a guide that ran out would
/// leave the car with nothing to show.
struct Guide {
    units: Vec<Vec<u8>>,
    next: usize,
}

impl Guide {
    /// A guide that cycles through `units`. Nothing to show is no guide.
    fn cycle(units: Vec<Vec<u8>>) -> Option<Self> {
        (!units.is_empty()).then_some(Self { units, next: 0 })
    }

    /// The next frame, which is the first again once the file has run out.
    fn next(&mut self) -> Vec<u8> {
        let unit = self.units[self.next].clone();
        self.next = (self.next + 1) % self.units.len();
        unit
    }
}

/// The guide for a screen `width` x `height`, as frames ready to send.
///
/// `None` is a build with no guide in it, which is a state the session already
/// knows how to run in: it carries heartbeats and nothing else.
fn stream(width: u32, height: u32) -> Option<Guide> {
    Guide::cycle(access_units(picture(width, height)?))
}

/// The coded stream built for a screen `width` x `height`.
fn picture(width: u32, height: u32) -> Option<&'static [u8]> {
    let exact = format!("{DIR}{width}x{height}{SUFFIX}");
    if let Some(bytes) = crate::assets::get(&exact) {
        return Some(bytes);
    }
    // Nothing was built at this size. The file whose shape is closest is the one
    // the car has to stretch least: two shapes ship, and a screen between them
    // is a few percent out whichever way it is given. A picture a little out of
    // shape beats a black screen.
    let wanted = width.max(1) as f64 / height.max(1) as f64;
    let mut best: Option<(f64, &'static [u8])> = None;
    for name in crate::assets::names() {
        let Some((w, h)) = stated(name) else { continue };
        let Some(bytes) = crate::assets::get(name) else {
            continue;
        };
        let off = (w as f64 / h as f64 / wanted).ln().abs();
        if best.is_none_or(|(known, _)| off < known) {
            best = Some((off, bytes));
        }
    }
    let (off, bytes) = best?;
    tracing::info!("guide.stretched width={width} height={height} off={off:.3}");
    Some(bytes)
}

/// The size a guide's name states, for `guide/1920x720.h264`. Any other file in
/// the binary is not a guide and states nothing.
fn stated(name: &str) -> Option<(u32, u32)> {
    let rest = name.strip_prefix(DIR)?.strip_suffix(SUFFIX)?;
    let (width, height) = rest.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

/// Splits an annex-B stream into access units, one per coded picture.
///
/// What a decoder is handed is a picture together with the parameter sets that
/// describe it, so the boundaries are where a slice arrives after another slice
/// and where a parameter set or an access unit delimiter arrives after a
/// picture. Splitting on the slices alone would leave each picture's parameter
/// sets at the end of the picture before it.
///
/// Anything ahead of the first start code is dropped, and a stream with no start
/// code in it has no access units.
fn access_units(bytes: &[u8]) -> Vec<Vec<u8>> {
    // Where each NAL begins and what kind it is; the type is the low five bits
    // of the byte after the start code, which is the whole of a NAL header.
    let mut nals: Vec<(usize, u8)> = Vec::new();
    let mut at = 0;
    while at + 3 <= bytes.len() {
        let header = if bytes[at..at + 3] == [0, 0, 1] {
            at + 3
        } else if at + 4 <= bytes.len() && bytes[at..at + 4] == [0, 0, 0, 1] {
            at + 4
        } else {
            at += 1;
            continue;
        };
        if header >= bytes.len() {
            break;
        }
        nals.push((at, bytes[header] & 0x1f));
        at = header;
    }

    // A stream with no start code in it has no access units to give.
    if nals.is_empty() {
        return Vec::new();
    }

    // Where the units begin, starting at the first NAL. A unit opens at every
    // slice that follows one, and at every parameter set or delimiter that
    // follows a picture.
    let mut starts = vec![0usize];
    let mut in_picture = false;
    for (index, &(_, kind)) in nals.iter().enumerate() {
        if let 1..=5 = kind {
            if in_picture {
                starts.push(index);
            }
            in_picture = true;
        } else if in_picture && matches!(kind, 7 | 8 | 9) {
            starts.push(index);
            in_picture = false;
        }
    }

    let mut bounds: Vec<usize> = starts.iter().map(|&index| nals[index].0).collect();
    bounds.push(bytes.len());
    bounds
        .windows(2)
        .map(|unit| bytes[unit[0]..unit[1]].to_vec())
        .collect()
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

    #[test]
    fn a_stream_is_split_into_one_access_unit_per_picture() {
        // Parameter sets, an IDR, a picture that follows it, and then a picture
        // with its parameter sets written out again, the way an encoder does at
        // a keyframe.
        let stream: Vec<u8> = [
            &[0u8, 0, 0, 1, 0x67][..],
            &[0, 0, 0, 1, 0x68][..],
            &[0, 0, 0, 1, 0x65][..],
            &[0, 0, 1, 0x41][..],
            &[0, 0, 0, 1, 0x67][..],
            &[0, 0, 0, 1, 0x68][..],
            &[0, 0, 0, 1, 0x41][..],
        ]
        .concat();

        let units = access_units(&stream);
        assert_eq!(units.len(), 3, "one unit per picture");
        assert_eq!(
            units[0],
            [0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65],
            "the first picture keeps the parameter sets in front of it"
        );
        assert_eq!(units[1], [0, 0, 1, 0x41], "a picture on its own");
        assert_eq!(
            units[2],
            [0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x41],
            "a repeat of the parameter sets opens the unit, not closes it"
        );

        // One picture in a file is one access unit; nothing in a file is none.
        assert_eq!(access_units(&[0, 0, 0, 1, 0x65, 0xaa]).len(), 1);
        assert!(access_units(&[1, 2, 3, 4]).is_empty());
    }

    /// The guide the binary carries is a stream of pictures rather than one, and
    /// a shape no file was built for is served the nearest one.
    #[test]
    fn the_binary_carries_a_guide_and_a_shape_nearest_to_it() {
        let bytes = picture(1920, 720).expect("1920x720 is built");
        assert!(access_units(bytes).len() > 1, "more than a picture");

        // A shape no file was built for is served the one nearest to it.
        let nearest = picture(1900, 700).expect("a shape between the ones built");
        assert!(access_units(nearest).len() > 1, "still a stream of pictures");
    }

    /// The size the car named decides the picture, and the frames go out while
    /// there is a car to show them to and no stream of the phone's own.
    #[tokio::test]
    async fn the_queue_is_fed_until_the_phone_has_a_picture_of_its_own() {
        let outlet = Arc::new(Outlet::new());
        let mut video = outlet.take_video().expect("nothing has taken it");
        let (session, session_rx) = watch::channel(true);
        let (screen, screen_rx) = watch::channel(Some(Geometry {
            width: 1280,
            height: 720,
            fps: 30,
            ..Geometry::default()
        }));
        let task = tokio::spawn(run(outlet.clone(), session_rx, screen_rx));

        // A keyframe first, then the pictures after it at the rate the car
        // named: one per frame's worth of time, not one per second.
        let first = frame(&mut video).await;
        assert_eq!(opening(&first), 7, "a picture opens with its SPS");

        let asking = tokio::time::Instant::now();
        let second = frame(&mut video).await;
        assert!(asking.elapsed() < Duration::from_millis(500));
        assert_eq!(opening(&second), 1, "a picture after the keyframe, not another");

        // The phone's own pictures are what the channel carries from now on.
        outlet.sink().stream(true);
        while video.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(video.try_recv().is_err(), "the guide has stood down");

        // And it is back once the stream closes again.
        outlet.sink().stream(false);
        let again = frame(&mut video).await;
        assert!(matches!(opening(&again), 1 | 5 | 7), "a picture of the guide");

        task.abort();
        drop((session, screen));
    }

    /// The next frame on the queue, which arrives within a frame's worth of
    /// time given the rate the test names.
    async fn frame(video: &mut tokio::sync::mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(1), video.recv())
            .await
            .expect("a frame in time")
            .expect("a frame")
    }

    /// The kind of the first NAL in a picture, whatever length its start code is.
    fn opening(picture: &[u8]) -> u8 {
        let mut at = 0;
        while at + 3 <= picture.len() {
            let header = if picture[at..at + 3] == [0, 0, 1] {
                at + 3
            } else if at + 4 <= picture.len() && picture[at..at + 4] == [0, 0, 0, 1] {
                at + 4
            } else {
                at += 1;
                continue;
            };
            if header < picture.len() {
                return picture[header] & 0x1f;
            }
            at = header;
        }
        panic!("a picture opens with a start code");
    }
}
