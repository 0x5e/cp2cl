//! One CarPlay session at a time, whichever link the phone arrives on first.
//!
//! The cable and Bluetooth both wait for a phone. The first one to see it takes
//! the one slot and keeps it for the whole session; the other stops offering
//! itself until the slot is free again. Nothing here knows what a session is —
//! the caller holds the [`Claim`] for as long as its session lasts.
//!
//! The slot also remembers what the phone on it calls its own cable: a phone
//! served over Bluetooth names the serial its cable would carry, and that name
//! is how the cable recognises it as the same phone and takes the session over
//! rather than waiting for the slot to come free.

use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::report::Report;

/// The link a phone arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Usb,
    Wireless,
}

impl Side {
    /// How the link is named in a log line and on the page.
    pub fn name(self) -> &'static str {
        match self {
            Side::Usb => "usb",
            Side::Wireless => "wireless",
        }
    }
}

/// The slot both links compete for.
///
/// A `watch` channel rather than a plain lock so a link that lost the slot can
/// wait for it to come back instead of polling. The sender sits behind a mutex
/// so that checking the slot and taking it is one step: two links seeing the
/// same free slot at the same time must not both get it.
pub struct Admission {
    state: Mutex<watch::Sender<Option<Side>>>,
    /// What the phone on the slot calls its own cable. See `learn_serial`.
    serial: Mutex<watch::Sender<Option<String>>>,
    status: Arc<Report>,
}

impl Admission {
    pub fn new(status: Arc<Report>) -> Self {
        let (state, _) = watch::channel(None);
        let (serial, _) = watch::channel(None);
        Self {
            state: Mutex::new(state),
            serial: Mutex::new(serial),
            status,
        }
    }

    /// Which link owns the slot right now.
    pub fn owner(&self) -> Option<Side> {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()).borrow()
    }

    /// A receiver that is told whenever the slot changes hands.
    pub fn subscribe(&self) -> watch::Receiver<Option<Side>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subscribe()
    }

    /// Records the serial the phone on `side` gave for its own cable.
    ///
    /// A phone names its other links once it has identified itself — see
    /// `crate::bringup` — and the name is worth exactly one thing here: a phone
    /// on the cable with the same serial is the same phone, so the cable may
    /// take the session over instead of waiting for the slot. Anything a link
    /// that does not own the slot says is dropped, because it is not the phone
    /// being served.
    pub fn learn_serial(&self, side: Side, serial: &str) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if *state.borrow() != Some(side) {
            return;
        }
        let known = self.serial.lock().unwrap_or_else(|e| e.into_inner());
        known.send_modify(|current| *current = Some(serial.to_string()));
    }

    /// The serial the phone on the slot gave for its cable, if it has said.
    pub fn serial(&self) -> Option<String> {
        self.serial
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .borrow()
            .clone()
    }

    /// A receiver that is told when the serial above is learned or forgotten.
    pub fn subscribe_serial(&self) -> watch::Receiver<Option<String>> {
        self.serial
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subscribe()
    }

    /// Takes the free slot. `None` when the other link already owns it.
    pub fn claim(&self, side: Side) -> Option<Claim<'_>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let taken = state.borrow().is_some();
        if taken {
            return None;
        }
        state.send_modify(|owner| *owner = Some(side));
        self.status
            .with(|s| s.carplay.transport = Some(side.name()));
        Some(Claim {
            admission: self,
            side,
        })
    }

    fn release(&self, side: Side) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mine = *state.borrow() == Some(side);
        if !mine {
            return;
        }
        state.send_modify(|owner| *owner = None);
        // The name goes with the slot: the next phone to take it is not this
        // one until it says so.
        self.serial
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send_modify(|serial| *serial = None);
        self.status.with(|s| s.carplay.transport = None);
    }
}

/// Kept for the whole session: dropping it is what re-arms both links.
pub struct Claim<'a> {
    admission: &'a Admission,
    side: Side,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.admission.release(self.side);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission() -> Arc<Admission> {
        Arc::new(Admission::new(Arc::new(Report::new())))
    }

    #[test]
    fn the_first_link_takes_the_slot_and_the_second_is_refused() {
        let slot = admission();
        let mut seen = slot.subscribe();

        let held = slot.claim(Side::Usb).expect("a free slot is taken");
        assert_eq!(slot.owner(), Some(Side::Usb));
        assert_eq!(*seen.borrow_and_update(), Some(Side::Usb));
        assert!(slot.claim(Side::Wireless).is_none());
        assert_eq!(slot.status.json()["transport"], serde_json::json!("usb"));

        drop(held);
        assert_eq!(slot.owner(), None);
        assert_eq!(*seen.borrow_and_update(), None);
        assert_eq!(slot.status.json()["transport"], serde_json::Value::Null);

        let other = slot.claim(Side::Wireless).expect("the slot is free again");
        assert_eq!(slot.owner(), Some(Side::Wireless));
        drop(other);
    }

    #[test]
    fn a_stale_claim_cannot_release_a_slot_owned_by_the_other_link() {
        let slot = admission();
        // Two `Admission` values are not involved: this is the same slot, so a
        // claim can only be stale if the owner was changed behind its back.
        // `release` checks the side for exactly that case.
        let held = slot.claim(Side::Usb).expect("a free slot is taken");
        slot.release(Side::Wireless);
        assert_eq!(slot.owner(), Some(Side::Usb));
        drop(held);
        assert_eq!(slot.owner(), None);
    }

    /// Only the link serving the phone may say what its cable is called, and
    /// the name is dropped with the session it belongs to: a serial left behind
    /// would match a phone that has nothing to do with the one now on the slot.
    #[test]
    fn the_phone_on_the_slot_names_its_cable_and_no_one_else_does() {
        let slot = admission();
        let mut seen = slot.subscribe_serial();

        let held = slot.claim(Side::Wireless).expect("a free slot is taken");
        slot.learn_serial(Side::Usb, "CP2CL0001");
        assert_eq!(slot.serial(), None, "the cable does not own the slot");

        slot.learn_serial(Side::Wireless, "CP2CL0001");
        assert_eq!(slot.serial().as_deref(), Some("CP2CL0001"));
        assert_eq!(
            *seen.borrow_and_update(),
            Some("CP2CL0001".to_string()),
            "the cable is told what to look for"
        );

        drop(held);
        assert_eq!(slot.serial(), None, "the name goes with the slot");
    }
}
