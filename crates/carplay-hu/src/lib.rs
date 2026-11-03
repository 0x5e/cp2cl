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

pub mod aac;
pub mod airplay;
pub mod artwork;
pub mod assets;
pub mod link;
pub mod now_playing;
pub mod options;
pub mod press;
pub mod report;
pub mod session;
pub mod sink;
pub mod wireless;

use std::sync::Arc;

use livi_runtime::mfi_async::SharedCoprocessor;
use livi_runtime::vehicle::Vehicle;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch, Mutex};

use crate::airplay::clock::SyncedClock;
use crate::airplay::pairing::Pairings;
use crate::airplay::{AudioClock, EventSink, MicSink, ScreenParams, SharedChannel, SharedScreen};
use crate::link::{Delegate, Panel};
use crate::options::Options;
use crate::report::Report;
use crate::sink::{AudioCodec, MediaSink};

pub use crate::airplay::Hu;

/// Everything a run needs from the program it runs in.
///
/// Two of these are the other end: `video_sink` and `audio_sink` carry what the
/// phone produces, `panel` carries what it says it is playing, and `report` is
/// where the page reads this half from. The rest is what the program knows and
/// this half does not: the socket, the credentials, the gate the car's session
/// opens, and the two channels the car's own screen is written into.
pub struct Ports {
    pub options: Arc<Options>,
    /// The binary's files: the channel table and the icons `/info` is built from.
    pub assets: &'static dyn assets::Assets,
    /// The MFi signer, for `/auth-setup` and for the iAP2 challenge alike.
    pub mfi: SharedCoprocessor,
    /// The identity the phone was paired with, and where it is kept.
    pub pairings: Arc<Mutex<Pairings>>,
    /// The AirPlay identity's public key, hex, named in every start session.
    pub accessory_key: String,
    /// The port the receiver is bound on, which is what the phone is told.
    pub airplay_port: u32,
    /// The socket the receiver accepts on.
    pub listener: TcpListener,
    /// Shut until the car has a session of its own. Nothing is accepted and
    /// nothing is published while it is.
    pub gate: watch::Receiver<bool>,
    /// Where the phone's pictures go.
    pub video_sink: Arc<dyn MediaSink>,
    /// Where the phone's sound goes.
    pub audio_sink: Arc<dyn MediaSink>,
    /// Where what the phone is playing goes.
    pub panel: Arc<dyn Panel>,
    /// Where a key pressed in the car arrives for the phone.
    pub event_sink: Arc<EventSink>,
    /// Where this half reports, for the page.
    pub report: Arc<Report>,
    /// Where the car's position goes, for the phone to read.
    pub vehicle: Arc<Vehicle>,
    /// What the program is told as a phone arrives and gets further.
    pub delegate: Arc<dyn Delegate>,
    /// The screen the car named, read by `GET /info`.
    pub screen: SharedScreen,
    /// The channel number the car named, which `/info` picks the icon from.
    pub channel: SharedChannel,
    /// Fired when the phone's icon for us is tapped.
    pub desktop: broadcast::Sender<()>,
    /// Fired when what the phone should be rendering has changed.
    pub restart: broadcast::Sender<()>,
}

impl Hu {
    /// Builds the half and hands the binary's files over.
    ///
    /// One of these per run: the assets are installed here, the wire is composed
    /// here, and the rest is what the program brought.
    pub fn new(ports: Ports) -> Arc<Self> {
        assets::install(ports.assets);
        // Fired when the car's session ends, which is the one thing a restart
        // cannot say: there is nothing to restart for.
        let (shutdown, _) = broadcast::channel(1);
        // Fired when a wireless phone moves its iAP2 control channel onto the
        // tunnel: nothing may page Bluetooth from then on.
        let handover = wireless::Handover::new();
        let tunnel = ports.options.wireless.then(|| {
            Arc::new(airplay::tunnel::Setup {
                cfg: ports.options.clone(),
                auth: ports.mfi.clone(),
                accessory_key: ports.accessory_key.clone(),
                airplay_port: ports.airplay_port,
                panel: ports.panel.clone(),
                vehicle: ports.vehicle.clone(),
                status: ports.report.clone(),
                events: ports.event_sink.clone(),
                handover: handover.clone(),
                restart: ports.restart.clone(),
            })
        });
        Arc::new(Self {
            options: ports.options,
            pairings: ports.pairings,
            video_sink: ports.video_sink,
            audio_sink: ports.audio_sink,
            panel: ports.panel,
            vehicle: ports.vehicle,
            accessory_key: ports.accessory_key,
            airplay_port: ports.airplay_port,
            handover,
            screen: ports.screen,
            channel: ports.channel,
            desktop: ports.desktop,
            restart: ports.restart,
            shutdown,
            mfi: ports.mfi,
            audio_clock: Arc::new(Mutex::new(AudioClock::default())),
            timing: Arc::new(Mutex::new(SyncedClock::new())),
            audio_codec: Arc::new(Mutex::new(AudioCodec::default())),
            event_sink: ports.event_sink,
            mic: MicSink::new(),
            status: ports.report,
            delegate: ports.delegate,
            tunnel,
            listener: std::sync::Mutex::new(Some(ports.listener)),
            gate: ports.gate,
        })
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
        if self.options.wireless {
            wireless::watch(&self).await;
        } else {
            session::watch(&self).await;
        }
    }

    /// The screen the car named, as `/info` reads it.
    pub fn screen_params(&self) -> Option<ScreenParams> {
        self.screen.lock().ok().and_then(|held| *held)
    }
}
