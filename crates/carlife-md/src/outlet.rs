//! The queues towards the car, and their sending halves.
//!
//! Five queues: pictures, music, a navigation prompt, the voice channel, and
//! what the phone is playing. A session takes its receiving halves and gives
//! them back when it ends, so a car unplugged and plugged back in finds them
//! where it left them. Between sessions nothing drains them, and the senders
//! drop rather than wait.
//!
//! The pictures are the one queue that does not work that way: a picture that
//! arrives to find no room costs the queue an older one, not itself. See
//! `Video`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, watch, Notify};

use crate::media::{Media, Navi, Voice};
use crate::now_playing::NowPlaying;

/// How many items may wait for the car. Bounded and dropped past, so a session
/// that falls behind glitches instead of accumulating latency.
const BACKLOG: usize = 8;

/// Which drops are reported: every one while what the queue does is being
/// watched, and every hundredth once only the rate of them matters.
const REPORT_EVERY: u64 = 1;

/// One coded picture on its way to the car.
pub struct Frame {
    /// Annex-B access unit: the picture with the parameter sets a decoder needs
    /// to read it in front.
    pub data: Vec<u8>,
    /// Whether a decoder can start here. It is the one picture the queue keeps
    /// in preference to any number of the ones behind it, because they cannot
    /// be read without it.
    pub keyframe: bool,
}

/// The pictures waiting for the car.
///
/// Bounded by count, and holding the newest it can rather than the oldest: a
/// picture that is late is not worth showing, so one that arrives to find every
/// place taken costs the queue an older picture instead. Which one goes is the
/// oldest picture a decoder could not have started at — a keyframe is the last
/// thing given up — and since whatever followed that picture can no longer be
/// decoded, the drop also asks the phone for a fresh keyframe.
///
/// A session empties the queue when it takes it, so a car plugged out and back
/// in does not inherit the last one's pictures.
pub struct Video {
    waiting: Mutex<Waiting>,
    /// Whether a session holds the queue: one car, one owner.
    taken: AtomicBool,
    /// Wakes the reader when a picture arrives.
    ready: Notify,
    /// How many times the queue has asked for a keyframe. A counter rather than
    /// a message: two drops in one burst are one request, and the session
    /// watching it is the one that knows whether there is a stream to ask on.
    asks: watch::Sender<u64>,
}

struct Waiting {
    frames: VecDeque<Frame>,
    /// Pictures dropped for want of room since the program started.
    dropped: u64,
}

impl Video {
    fn new() -> Self {
        Self {
            waiting: Mutex::new(Waiting {
                frames: VecDeque::new(),
                dropped: 0,
            }),
            taken: AtomicBool::new(false),
            ready: Notify::new(),
            asks: watch::channel(0).0,
        }
    }

    /// Adds one picture, making room by dropping the oldest one a decoder could
    /// not have started at.
    fn push(&self, frame: Frame) {
        let dropped = {
            let Ok(mut waiting) = self.waiting.lock() else {
                return;
            };
            let dropped = if waiting.frames.len() >= BACKLOG {
                // The oldest picture that is not a keyframe. A queue holding
                // nothing else gives up its oldest keyframe, which no stream of
                // pictures does, but leaves the rule total.
                let oldest = waiting
                    .frames
                    .iter()
                    .position(|waiting| !waiting.keyframe)
                    .unwrap_or(0);
                let lost = waiting
                    .frames
                    .remove(oldest)
                    .expect("the index is one of the pictures");
                waiting.dropped += 1;
                Some((waiting.dropped, !lost.keyframe))
            } else {
                None
            };
            waiting.frames.push_back(frame);
            dropped
        };
        self.ready.notify_one();

        let Some((count, broke)) = dropped else {
            return;
        };
        if broke {
            self.asks.send_modify(|asks| *asks = asks.wrapping_add(1));
        }
        // Said on every `REPORT_EVERY`-th drop: one of them is what says the
        // queue is not keeping up, and the rate is what says how badly. A
        // stream that keeps arriving with nowhere to go would otherwise print
        // a line per frame.
        if count % REPORT_EVERY == 0 {
            tracing::warn!("carlife.outlet_backlog_dropped channel=video dropped={count}");
        }
    }

    /// The next picture, if one is waiting.
    pub fn try_recv(&self) -> Option<Frame> {
        self.waiting.lock().ok()?.frames.pop_front()
    }

    /// The next picture, waiting for one if the queue is empty.
    pub async fn recv(&self) -> Frame {
        loop {
            // Registered before the queue is looked at, so a picture that
            // arrives in between is a wakeup rather than a wait.
            let ready = self.ready.notified();
            if let Some(frame) = self.try_recv() {
                return frame;
            }
            ready.await;
        }
    }

    /// A request for a keyframe, for the session that can make one.
    fn requests(&self) -> watch::Receiver<u64> {
        self.asks.subscribe()
    }

    /// What the last car left belongs to a car that is no longer there.
    fn clear(&self) {
        if let Ok(mut waiting) = self.waiting.lock() {
            waiting.frames.clear();
        }
    }
}

pub struct Outlet {
    video: Arc<Video>,
    media_tx: mpsc::Sender<Media>,
    media_rx: Mutex<Option<mpsc::Receiver<Media>>>,
    navi_tx: mpsc::Sender<Navi>,
    navi_rx: Mutex<Option<mpsc::Receiver<Navi>>>,
    voice_tx: mpsc::Sender<Voice>,
    voice_rx: Mutex<Option<mpsc::Receiver<Voice>>>,
    playing_tx: mpsc::Sender<NowPlaying>,
    playing_rx: Mutex<Option<mpsc::Receiver<NowPlaying>>>,
    /// Whether the phone's screen stream is open, which decides what the car is
    /// shown and whether a keyframe can be asked for.
    stream_tx: watch::Sender<bool>,
    stream_rx: watch::Receiver<bool>,
}

impl Outlet {
    pub fn new() -> Self {
        let (media_tx, media_rx) = mpsc::channel(BACKLOG);
        let (navi_tx, navi_rx) = mpsc::channel(BACKLOG);
        let (voice_tx, voice_rx) = mpsc::channel(BACKLOG);
        let (playing_tx, playing_rx) = mpsc::channel(BACKLOG);
        let (stream_tx, stream_rx) = watch::channel(false);
        Self {
            video: Arc::new(Video::new()),
            media_tx,
            media_rx: Mutex::new(Some(media_rx)),
            navi_tx,
            navi_rx: Mutex::new(Some(navi_rx)),
            voice_tx,
            voice_rx: Mutex::new(Some(voice_rx)),
            playing_tx,
            playing_rx: Mutex::new(Some(playing_rx)),
            stream_tx,
            stream_rx,
        }
    }

    /// The sender side, for whoever produces what the car reads.
    pub fn sink(&self) -> Arc<Sink> {
        Arc::new(Sink {
            video: self.video.clone(),
            media: self.media_tx.clone(),
            navi: self.navi_tx.clone(),
            voice: self.voice_tx.clone(),
            playing: self.playing_tx.clone(),
            stream: self.stream_tx.clone(),
            no_media_reader: AtomicBool::new(false),
            media_behind: AtomicBool::new(false),
            no_navi_reader: AtomicBool::new(false),
            navi_behind: AtomicBool::new(false),
            no_voice_reader: AtomicBool::new(false),
            voice_behind: AtomicBool::new(false),
            playing_dropped: AtomicBool::new(false),
        })
    }

    /// Whether the phone's screen stream is open.
    pub fn streaming(&self) -> watch::Receiver<bool> {
        self.stream_rx.clone()
    }

    /// One car, one owner: a second call gets nothing. A session starts on an
    /// empty queue, for the reason `Video` states.
    pub fn take_video(&self) -> Option<Arc<Video>> {
        if self.video.taken.swap(true, Ordering::Relaxed) {
            return None;
        }
        self.video.clear();
        Some(self.video.clone())
    }

    pub fn return_video(&self) {
        self.video.taken.store(false, Ordering::Relaxed);
    }

    /// A request for a keyframe, for the session that can make one.
    pub fn keyframe_requests(&self) -> watch::Receiver<u64> {
        self.video.requests()
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

    pub fn take_voice(&self) -> Option<mpsc::Receiver<Voice>> {
        Some(emptied(self.voice_rx.lock().ok()?.take()?))
    }

    pub fn return_voice(&self, rx: mpsc::Receiver<Voice>) {
        if let Ok(mut guard) = self.voice_rx.lock() {
            *guard = Some(rx);
        }
    }

    pub fn take_playing(&self) -> Option<mpsc::Receiver<NowPlaying>> {
        Some(emptied(self.playing_rx.lock().ok()?.take()?))
    }

    pub fn return_playing(&self, rx: mpsc::Receiver<NowPlaying>) {
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
    video: Arc<Video>,
    media: mpsc::Sender<Media>,
    navi: mpsc::Sender<Navi>,
    voice: mpsc::Sender<Voice>,
    playing: mpsc::Sender<NowPlaying>,
    stream: watch::Sender<bool>,
    /// Each of these is said once. A stream that keeps arriving with nowhere to
    /// go would otherwise print a line per frame.
    no_media_reader: AtomicBool,
    media_behind: AtomicBool,
    no_navi_reader: AtomicBool,
    navi_behind: AtomicBool,
    no_voice_reader: AtomicBool,
    voice_behind: AtomicBool,
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

    /// One access unit, annex-B, and whether a decoder can start at it.
    pub fn video(&self, data: Vec<u8>, keyframe: bool) {
        self.video.push(Frame { data, keyframe });
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

    pub fn voice(&self, item: Voice) {
        Self::send(
            &self.voice,
            item,
            "voice",
            &self.no_voice_reader,
            &self.voice_behind,
        );
    }

    /// A record rather than a flow: what a car that is not reading did not hear
    /// is stale, so both ways of having nowhere to go are one line.
    pub fn playing(&self, item: NowPlaying) {
        if self.playing.try_send(item).is_err()
            && !self.playing_dropped.swap(true, Ordering::Relaxed)
        {
            tracing::warn!("carlife.media_dropped reason=no_reader");
        }
    }

    pub fn stream(&self, open: bool) {
        let _ = self.stream.send(open);
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

        let video = outlet.take_video().expect("the video half");
        assert!(outlet.take_video().is_none(), "one car, one owner");

        sink.video(vec![4], true);
        assert_eq!(video.recv().await.data, vec![4]);

        sink.video(vec![5], false);
        outlet.return_video();
        let video = outlet.take_video().expect("given back");
        assert!(video.try_recv().is_none(), "the stale frame is gone");
    }

    /// A full queue gives up the oldest picture that is not a keyframe, because
    /// what follows that picture can no longer be decoded — which is what the
    /// request that goes with the drop is for.
    #[tokio::test]
    async fn a_full_queue_gives_up_the_oldest_picture_a_decoder_could_not_start_at() {
        let outlet = Outlet::new();
        let sink = outlet.sink();
        let mut asks = outlet.keyframe_requests();
        let video = outlet.take_video().expect("the video half");

        // A keyframe and the seven pictures that fit beside it.
        sink.video(vec![0], true);
        for n in 1..BACKLOG as u8 {
            sink.video(vec![n], false);
        }
        // The one after them has no room: the oldest picture that is not a
        // keyframe goes, and the keyframe stays.
        sink.video(vec![BACKLOG as u8], false);

        let mut waiting = Vec::new();
        while let Some(frame) = video.try_recv() {
            waiting.push(frame.data[0]);
        }
        assert_eq!(waiting, vec![0, 2, 3, 4, 5, 6, 7, 8]);
        asks.changed().await.expect("a keyframe was asked for");
    }

    /// The keyframe is the last picture to go: a queue holding nothing else
    /// gives up its oldest one rather than the one behind it.
    #[tokio::test]
    async fn a_queue_of_nothing_but_keyframes_gives_up_the_oldest() {
        let outlet = Outlet::new();
        let sink = outlet.sink();
        let video = outlet.take_video().expect("the video half");

        for n in 0..=BACKLOG as u8 {
            sink.video(vec![n], true);
        }
        assert_eq!(video.recv().await.data, vec![1]);
    }

    #[tokio::test]
    async fn the_stream_watch_says_what_the_sink_was_told() {
        let outlet = Outlet::new();
        let mut streaming = outlet.streaming();
        assert!(!*streaming.borrow());
        outlet.sink().stream(true);
        streaming.changed().await.expect("a change");
        assert!(*streaming.borrow());
    }
}
