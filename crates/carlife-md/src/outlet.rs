//! The queues towards the car, and their sending halves.
//!
//! Four queues: pictures, music, a navigation prompt, and what the phone is
//! playing. A session takes its receiving halves and gives them back when it
//! ends, so a car unplugged and plugged back in finds them where it left them.
//! Between sessions nothing drains them, and the senders drop rather than wait.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, watch};

use crate::media::{Media, Navi};
use crate::playing::Playing;

/// How many items may wait for the car. Bounded and dropped past, so a session
/// that falls behind glitches instead of accumulating latency.
const BACKLOG: usize = 8;

pub struct Outlet {
    video_tx: mpsc::Sender<Vec<u8>>,
    video_rx: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    media_tx: mpsc::Sender<Media>,
    media_rx: Mutex<Option<mpsc::Receiver<Media>>>,
    navi_tx: mpsc::Sender<Navi>,
    navi_rx: Mutex<Option<mpsc::Receiver<Navi>>>,
    playing_tx: mpsc::Sender<Playing>,
    playing_rx: Mutex<Option<mpsc::Receiver<Playing>>>,
    /// Whether the phone's screen stream is open, which decides what the car is
    /// shown and whether a keyframe can be asked for.
    screen_tx: watch::Sender<bool>,
    screen_rx: watch::Receiver<bool>,
}

impl Outlet {
    pub fn new() -> Self {
        let (video_tx, video_rx) = mpsc::channel(BACKLOG);
        let (media_tx, media_rx) = mpsc::channel(BACKLOG);
        let (navi_tx, navi_rx) = mpsc::channel(BACKLOG);
        let (playing_tx, playing_rx) = mpsc::channel(BACKLOG);
        let (screen_tx, screen_rx) = watch::channel(false);
        Self {
            video_tx,
            video_rx: Mutex::new(Some(video_rx)),
            media_tx,
            media_rx: Mutex::new(Some(media_rx)),
            navi_tx,
            navi_rx: Mutex::new(Some(navi_rx)),
            playing_tx,
            playing_rx: Mutex::new(Some(playing_rx)),
            screen_tx,
            screen_rx,
        }
    }

    /// The sender side, for whoever produces what the car reads.
    pub fn sink(&self) -> Arc<Sink> {
        Arc::new(Sink {
            video: self.video_tx.clone(),
            media: self.media_tx.clone(),
            navi: self.navi_tx.clone(),
            playing: self.playing_tx.clone(),
            screen: self.screen_tx.clone(),
            no_video_reader: AtomicBool::new(false),
            video_behind: AtomicBool::new(false),
            no_media_reader: AtomicBool::new(false),
            media_behind: AtomicBool::new(false),
            no_navi_reader: AtomicBool::new(false),
            navi_behind: AtomicBool::new(false),
            playing_dropped: AtomicBool::new(false),
        })
    }

    pub fn screen(&self) -> watch::Receiver<bool> {
        self.screen_rx.clone()
    }

    /// One car, one owner: a second call gets nothing.
    pub fn take_video(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        Some(emptied(self.video_rx.lock().ok()?.take()?))
    }

    pub fn return_video(&self, rx: mpsc::Receiver<Vec<u8>>) {
        if let Ok(mut guard) = self.video_rx.lock() {
            *guard = Some(rx);
        }
    }

    pub fn take_media(&self) -> Option<mpsc::Receiver<Media>> {
        Some(emptied(self.media_rx.lock().ok()?.take()?))
    }

    pub fn return_media(&self, rx: mpsc::Receiver<Media>) {
        if let Ok(mut guard) = self.media_rx.lock() {
            *guard = Some(rx);
        }
    }

    pub fn take_navi(&self) -> Option<mpsc::Receiver<Navi>> {
        Some(emptied(self.navi_rx.lock().ok()?.take()?))
    }

    pub fn return_navi(&self, rx: mpsc::Receiver<Navi>) {
        if let Ok(mut guard) = self.navi_rx.lock() {
            *guard = Some(rx);
        }
    }

    pub fn take_playing(&self) -> Option<mpsc::Receiver<Playing>> {
        Some(emptied(self.playing_rx.lock().ok()?.take()?))
    }

    pub fn return_playing(&self, rx: mpsc::Receiver<Playing>) {
        if let Ok(mut guard) = self.playing_rx.lock() {
            *guard = Some(rx);
        }
    }
}

impl Default for Outlet {
    fn default() -> Self {
        Self::new()
    }
}

fn emptied<T>(mut rx: mpsc::Receiver<T>) -> mpsc::Receiver<T> {
    while rx.try_recv().is_ok() {}
    rx
}

/// Where the streams go when the car is the one reading them.
pub struct Sink {
    video: mpsc::Sender<Vec<u8>>,
    media: mpsc::Sender<Media>,
    navi: mpsc::Sender<Navi>,
    playing: mpsc::Sender<Playing>,
    screen: watch::Sender<bool>,
    /// Each of these is said once. A stream that keeps arriving with nowhere to
    /// go would otherwise print a line per frame.
    no_video_reader: AtomicBool,
    video_behind: AtomicBool,
    no_media_reader: AtomicBool,
    media_behind: AtomicBool,
    no_navi_reader: AtomicBool,
    navi_behind: AtomicBool,
    playing_dropped: AtomicBool,
}

impl Sink {
    fn send<T>(
        queue: &mpsc::Sender<T>,
        item: T,
        channel: &str,
        no_reader: &AtomicBool,
        behind: &AtomicBool,
    ) {
        match queue.try_send(item) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                if !no_reader.swap(true, Ordering::Relaxed) {
                    tracing::warn!("carlife.outlet_dropped channel={channel} reason=no_reader");
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if !behind.swap(true, Ordering::Relaxed) {
                    tracing::warn!("carlife.outlet_backlog_dropped channel={channel}");
                }
            }
        }
    }

    /// One access unit, annex-B.
    pub fn video(&self, access_unit: Vec<u8>) {
        Self::send(
            &self.video,
            access_unit,
            "video",
            &self.no_video_reader,
            &self.video_behind,
        );
    }

    pub fn media(&self, item: Media) {
        Self::send(
            &self.media,
            item,
            "media",
            &self.no_media_reader,
            &self.media_behind,
        );
    }

    pub fn navi(&self, item: Navi) {
        Self::send(
            &self.navi,
            item,
            "navi",
            &self.no_navi_reader,
            &self.navi_behind,
        );
    }

    /// A record rather than a flow: what a car that is not reading did not hear
    /// is stale, so both ways of having nowhere to go are one line.
    pub fn playing(&self, item: Playing) {
        if self.playing.try_send(item).is_err()
            && !self.playing_dropped.swap(true, Ordering::Relaxed)
        {
            tracing::warn!("carlife.media_dropped reason=no_reader");
        }
    }

    pub fn screen(&self, open: bool) {
        let _ = self.screen.send(open);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Queues are handed over one at a time and a session starts on an empty
    /// one: what the last car left belongs to a car that is no longer there.
    #[tokio::test]
    async fn a_session_takes_its_queues_empty() {
        let outlet = Outlet::new();
        let sink = outlet.sink();

        let mut video = outlet.take_video().expect("the video half");
        assert!(outlet.take_video().is_none(), "one car, one owner");

        sink.video(vec![4]);
        assert_eq!(video.recv().await, Some(vec![4]));

        sink.video(vec![5]);
        outlet.return_video(video);
        let mut video = outlet.take_video().expect("given back");
        assert!(video.try_recv().is_err(), "the stale frame is gone");
    }

    /// A reader that is gone is said once, not once per frame.
    #[tokio::test]
    async fn a_queue_with_no_reader_drops_without_blocking() {
        let outlet = Outlet::new();
        let sink = outlet.sink();
        drop(outlet.take_video());
        for _ in 0..64 {
            sink.video(vec![0; 8]);
        }
    }

    #[tokio::test]
    async fn the_screen_watch_says_what_the_sink_was_told() {
        let outlet = Outlet::new();
        let mut screen = outlet.screen();
        assert!(!*screen.borrow());
        outlet.sink().screen(true);
        screen.changed().await.expect("a change");
        assert!(*screen.borrow());
    }
}
