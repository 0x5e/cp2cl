//! What the CarLife half is given: the other end, the host it reports to, and
//! the queues it drains.

use std::sync::Arc;

use tokio::sync::Notify;

use crate::link::{Delegate, Input, Location, Voice};
use crate::outlet::Outlet;
use crate::report::Report;

/// Everything a session needs from the program it runs in.
pub struct Ports<'a> {
    /// Where the car's touch and panel keys go.
    pub input: Arc<dyn Input>,
    /// What the host is told as the session runs.
    pub delegate: &'a dyn Delegate,
    /// The picture the panel is shown for a song whose app sent none.
    ///
    /// The car's media message carries a cover whether or not there is one to
    /// carry, and a body that draws a blank is not what a phone sends — so the
    /// program picks one and this is it.
    pub placeholder: &'static [u8],
    /// Fires when the phone's icon for us is tapped: on the phone that icon
    /// stands for the car's own interface, so the car is asked to show it.
    pub home: Arc<Notify>,
    /// The queues towards the car: what the other half produces and a session
    /// drains for as long as it runs.
    pub outlet: Arc<Outlet>,
    /// Where the session reports, for the page.
    pub report: Arc<Report>,
    /// Where the car's position goes, for the phone to read.
    pub location: Arc<dyn Location>,
    /// Where the car's microphone goes. The VR channel is the only one that runs
    /// both ways, so this is the only audio the car produces.
    pub voice: Arc<dyn Voice>,
}
