//! What the page shows: the two halves' own reports, put side by side.
//!
//! Neither half is read for anything here. The CarLife crate keeps what the car
//! told it (`page`), the CarPlay crate what the phone did (`report`), and this
//! module holds only the part that belongs to neither: the board's own CPU and
//! memory, which `web::device` samples.
//!
//! The shape of the answer is the page's contract, and it is written down once —
//! in `Status::json` — rather than in the three places that produce it.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use carlife_md::page::{self, Cover};
use carplay_hu::report::Report;

use super::device::Device;

/// Everything the page reads, in one place.
///
/// Three locks rather than one: each half writes its own and nothing here takes
/// two at once, so a page can catch one half a moment ahead of the other — which
/// is what a live view of two independent processes looks like.
pub struct Status {
    /// What the CarLife half reports. Written there, read here.
    carlife: Arc<page::Page>,
    /// What the CarPlay half reports. Written there, read here.
    report: Arc<Report>,
    /// What the board itself is doing, which is sampled rather than reported.
    sampled: Mutex<Sampled>,
}

impl Status {
    /// Around the CarPlay half's own report, which it writes for itself.
    pub fn new(report: Arc<Report>) -> Self {
        Self {
            carlife: Arc::new(page::Page::new()),
            report,
            sampled: Mutex::new(Sampled::default()),
        }
    }

    /// The car's half, for the side that writes it.
    pub fn carlife(&self) -> &Arc<page::Page> {
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
            "carlife": self.carlife.carlife_json(self.report.coding()),
            "carplay": self.report.json(),
            "now_playing": self.carlife.now_playing_json(),
            "device": sampled.device.json(),
        })
    }

    /// The cover the car was last given, for the page to draw.
    pub fn cover(&self) -> Option<Cover> {
        self.carlife.cover()
    }
}

/// What is sampled rather than reported: the board's own numbers, and nothing
/// else.
#[derive(Debug, Clone, Default)]
pub struct Sampled {
    /// The machine itself — see `web::device`.
    pub device: Device,
}
