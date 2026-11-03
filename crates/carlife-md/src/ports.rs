//! What the CarLife half is given: the other end, the host it reports to, and
//! the queues it drains.

use std::sync::Arc;

use livi_runtime::vehicle::Vehicle;
use tokio::sync::Notify;

use crate::link::{Assets, Delegate, Input};
use crate::outlet::Outlet;
use crate::report::Report;

/// Everything a session needs from the program it runs in.
pub struct Ports<'a> {
    /// Where the car's touch and panel keys go.
    pub input: Arc<dyn Input>,
    /// What the host is told as the session runs.
    pub delegate: &'a dyn Delegate,
    /// The guide's pictures and the cover a song without artwork gets.
    pub assets: &'a dyn Assets,
    /// Fires when the phone's icon for us is tapped: on the phone that icon
    /// stands for the car's own interface, so the car is asked to show it.
    pub home: Arc<Notify>,
    /// The queues towards the car: what the other half produces and a session
    /// drains for as long as it runs.
    pub outlet: Arc<Outlet>,
    /// Where the session reports, for the page.
    pub report: Arc<Report>,
    /// Where the car's position goes, for the phone to read.
    pub vehicle: Arc<Vehicle>,
}
