//! The CarPlay half: this program plays the head unit an iPhone expects to be
//! plugged into.
//!
//! Two protocols on two connections: iAP2 over the cable, Bluetooth or the
//! tunnel says what to do, and the AirPlay session it names is where the picture
//! and the sound arrive. See `airplay` for the receiver, `session` for the wired
//! iAP2 link and `wireless` for the one that starts over Bluetooth.
//!
//! Nothing here knows what a CarLife message looks like. What the phone produces
//! leaves through `sink` as events, what it says about the song through `link`'s
//! `Panel`, and how far it got through `link`'s `Delegate`; the program turns all
//! three into whatever the other end understands.

pub mod assets;
pub mod channels;
pub mod link;
pub mod now_playing;
pub mod machine;
pub mod press;
pub mod report;
pub mod sink;

mod aac;
mod airplay;
mod session;
mod wireless;

use std::sync::Arc;

use anyhow::Result;
use livi_runtime::mfi_async::SharedCoprocessor;
use livi_runtime::vehicle::Vehicle;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch, Mutex};

use crate::airplay::clock::SyncedClock;
use crate::airplay::pairing::Pairings;
use crate::airplay::{AudioClock, MicSink};
use crate::link::{Delegate, Panel};
use crate::machine::Machine;
use crate::report::Report;
use crate::sink::{AudioCodec, StreamSink};

// What the program names without going through a module: the entry point, the
// event channel it fills in, the geometry the car named, and the one constant
// the page repeats.
pub use crate::airplay::Hu;
pub use crate::airplay::event::Events;
pub use crate::airplay::screen::CODEC;
pub use crate::airplay::Geometry;
pub use crate::wireless::unpair_all;

/// Everything a run needs from the program it runs in.
///
/// Three of these are the other end: `video_sink` and `audio_sink` carry what
/// the phone produces and `panel` carries what it says it is playing. The rest
/// is what the program knows and this half does not, plus the channels the two
/// ends keep in step with.
pub struct Ports {
    pub machine: Arc<Machine>,
    /// The binary's files: the channel table and the icons `/info` is built from.
    pub assets: &'static dyn assets::Assets,
    /// The MFi signer, for `/auth-setup` and for the iAP2 challenge alike.
    pub mfi: SharedCoprocessor,
    /// The socket the receiver accepts on. The port the phone is told is the one
    /// this is bound to.
    pub listener: TcpListener,
    /// Shut until the car has a session of its own. Nothing is accepted and
    /// nothing is published while it is.
    pub gate: watch::Receiver<bool>,
    /// Where the phone's pictures go.
    pub video_sink: Arc<dyn StreamSink>,
    /// Where the phone's sound goes.
    pub audio_sink: Arc<dyn StreamSink>,
    /// Where what the phone is playing goes.
    pub panel: Arc<dyn Panel>,
    /// Where a key pressed in the car arrives for the phone.
    pub events: Arc<Events>,
    /// Where this half reports, for the page.
    pub report: Arc<Report>,
    /// Where the car's position goes, for the phone to read.
    pub vehicle: Arc<Vehicle>,
    /// What the program is told as a phone arrives and gets further.
    pub delegate: Arc<dyn Delegate>,
    /// The geometry the car named, which is what `/info` answers with.
    pub screen: watch::Receiver<Option<Geometry>>,
    /// The channel number the car named, which picks the icon `/info` carries.
    pub channel_id: watch::Receiver<Option<String>>,
    /// Bumped when what the phone should be rendering has changed: `/info` is
    /// answered once per session, so a change only lands on the next one.
    pub restart: watch::Sender<u64>,
}

impl Hu {
    /// Builds the half and hands the binary's files over.
    ///
    /// One of these per run: the assets are installed here, the pairing record
    /// is read here, and the wire is composed here.
    pub fn new(ports: Ports) -> Result<Arc<Self>> {
        assets::install(ports.assets);
        // The identity the phone was paired with, kept in the data directory the
        // firmware installs and made on the first run.
        let loaded = Pairings::load_or_create(&ports.machine.data_dir)?;
        let accessory_key = hex::encode(loaded.identity.public());
        let pairings = Arc::new(Mutex::new(loaded));

        let airplay_port = ports.listener.local_addr()?.port() as u32;
        ports.report.with(|s| s.carplay.airplay_port = Some(airplay_port));

        // Fired when the car's session ends, which is the one thing a restart
        // cannot say: there is nothing to restart for.
        let (shutdown, _) = broadcast::channel(1);
        // Fired when a wireless phone moves its iAP2 control channel onto the
        // tunnel: nothing may page Bluetooth from then on.
        let handover = wireless::Handover::new();
        let tunnel = ports.machine.wireless.then(|| {
            Arc::new(airplay::tunnel::Setup {
                cfg: ports.machine.clone(),
                auth: ports.mfi.clone(),
                accessory_key: accessory_key.clone(),
                airplay_port,
                panel: ports.panel.clone(),
                vehicle: ports.vehicle.clone(),
                status: ports.report.clone(),
                events: ports.events.clone(),
                handover: handover.clone(),
                restart: ports.restart.clone(),
            })
        });
        Ok(Arc::new(Self {
            machine: ports.machine,
            pairings,
            accessory_key,
            airplay_port,
            video_sink: ports.video_sink,
            audio_sink: ports.audio_sink,
            panel: ports.panel,
            vehicle: ports.vehicle,
            handover,
            screen: ports.screen,
            channel_id: ports.channel_id,
            restart: ports.restart,
            shutdown,
            mfi: ports.mfi,
            audio_clock: Arc::new(Mutex::new(AudioClock::default())),
            timing: Arc::new(Mutex::new(SyncedClock::new())),
            audio_codec: Arc::new(Mutex::new(AudioCodec::default())),
            events: ports.events,
            mic: MicSink::new(),
            status: ports.report,
            delegate: ports.delegate,
            tunnel,
            listener: std::sync::Mutex::new(Some(ports.listener)),
            gate: ports.gate,
        }))
    }

    /// Runs the half until the program stops.
    ///
    /// The receiver accepts in a task of its own, because the link below is what
    /// the car's session waits on and neither is the other's servant: a wired
    /// run has a phone to find, a wireless one has two radios to bring up, and
    /// the phone that arrives on either comes back over the receiver.
    pub async fn run(self: Arc<Self>) {
        let receiver = self.clone();
        tokio::spawn(async move { receiver.serve().await });
        if self.machine.wireless {
            wireless::watch(&self).await;
        } else {
            session::watch(&self).await;
        }
    }

    /// The screen the car named, as `/info` reads it.
    pub fn geometry(&self) -> Option<Geometry> {
        *self.screen.borrow()
    }
}
