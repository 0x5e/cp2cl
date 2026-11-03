//! What the two halves ask of the program while it runs.
//!
//! The car's session reports three things out of it — that it is up or down,
//! what size the car asked to be rendered at, and which channel number the car
//! names itself by — and asks two back: the size the page picked, and what the
//! channel table says about that number's panel. The presses the car makes go
//! the other way, through the same sink the phone reads them from.
//!
//! The phone's half reports how far a phone got, which is the third fact the
//! lights are drawn from, and says when its icon was tapped.
//!
//! Nothing here is policy either half could have had. A session is the only
//! thing that knows when the CarPlay half should be open, which is why that
//! switch is thrown here rather than watched there.

use std::sync::Arc;

use carlife_md::channels::ChannelEntry;
use carlife_md::link::{Assets, Delegate, Input};
use carlife_md::machine::{ScreenOverride, Size};
use carlife_md::press::Command;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use carplay_hu::link::Link;
use carplay_hu::{Events, Geometry};
use livi_runtime::vehicle::Vehicle;
use tokio::sync::{watch, Notify};

use crate::led;

/// The one object both halves are given: it answers for each of them as the
/// host that is told what happened, as the asset table, and as where a key goes.
pub struct Hooks {
    /// Whether the CarPlay half accepts anything at all. Shut until a head unit
    /// has been told who this device is, and shut again when it goes.
    carplay: watch::Sender<bool>,
    /// Bumped when a running CarPlay session should read `/info` again.
    restart: watch::Sender<u64>,
    /// The size the car named, which is what `/info` answers with.
    screen: watch::Sender<Option<Geometry>>,
    /// The channel number the car named, which picks the icon `/info` carries.
    channel_id: watch::Sender<Option<String>>,
    /// Fired by the phone's half and waited on by the car's: on the phone our
    /// icon stands for the car's own interface.
    home: Arc<Notify>,
    /// The bridge's own light.
    lights: Arc<led::Signals>,
    /// Where a key pressed in the car arrives for the phone.
    events: Arc<Events>,
    /// Where the car's position goes. The phone's half reads the same object,
    /// which is why it is handed in rather than made here.
    vehicle: Arc<Vehicle>,
}

impl Hooks {
    pub fn new(
        carplay: watch::Sender<bool>,
        restart: watch::Sender<u64>,
        screen: watch::Sender<Option<Geometry>>,
        channel_id: watch::Sender<Option<String>>,
        home: Arc<Notify>,
        lights: Arc<led::Signals>,
        events: Arc<Events>,
        vehicle: Arc<Vehicle>,
    ) -> Arc<Self> {
        Arc::new(Self {
            carplay,
            restart,
            screen,
            channel_id,
            home,
            lights,
            events,
            vehicle,
        })
    }

    /// The size the operator picked, out of the settings file.
    ///
    /// Read each time rather than kept: the page writes it while the program
    /// runs, and a copy taken at startup is a copy of what was there before.
    pub fn screen_chosen(&self) -> ScreenOverride {
        let picked = crate::settings::load().screen;
        ScreenOverride {
            size: picked.resolution.map(|size| Size {
                width: size.width,
                height: size.height,
            }),
            fps: picked.fps,
        }
    }

    /// Asks a running session to read `/info` again, which costs the phone a
    /// session: it reads that answer once and never asks again.
    fn changed(&self) {
        self.restart.send_modify(|seen| *seen += 1);
    }
}

impl Delegate for Hooks {
    fn session(&self, up: bool) {
        let _ = self.carplay.send(up);
        self.lights.carlife(up);
        if up {
            // The phone will not start CarPlay on its own — it has to be told
            // again — and a fresh iAP2 session is how it is told.
            self.changed();
        }
    }

    /// A size that lands after the phone has already read `/info` is worth a
    /// session of its own — that is what `restart` is for — and a size that
    /// changes nothing is not worth saying twice.
    fn screen_named(&self, width: u32, height: u32, fps: u32) {
        let wanted = Geometry {
            width,
            height,
            fps,
            ..Geometry::default()
        };
        if *self.screen.borrow() == Some(wanted) {
            return;
        }
        tracing::info!("carplay.video.screen_named width={width} height={height} fps={fps}");
        let _ = self.screen.send(Some(wanted));
        self.changed();
    }

    fn channel_id(&self, channel: Option<&str>) {
        let _ = self.channel_id.send(channel.map(str::to_string));
    }

    /// Read again for each session, because the page can change it while the
    /// program runs.
    fn screen_chosen(&self) -> ScreenOverride {
        Hooks::screen_chosen(self)
    }

    /// The table belongs to this program — `airplay::info` reads it for `/info`
    /// too — so the entry is looked up here and handed over as the CarLife half
    /// reads it.
    fn channel_entry(&self, channel: Option<&str>) -> ChannelEntry {
        let entry = carplay_hu::channels::of(channel);
        ChannelEntry {
            preset: entry.preset.map(|preset| carlife_md::channels::Preset {
                width: preset.width,
                height: preset.height,
                fps: preset.fps,
            }),
            ppi: entry.ppi,
        }
    }
}

impl carplay_hu::link::Delegate for Hooks {
    /// Whether both radios are up, which only the wireless watcher can say.
    fn radios(&self, up: bool) {
        self.lights.radios(up);
    }

    /// How far a phone got. The light is the only thing that acts on it.
    fn carplay(&self, link: Link) {
        self.lights.carplay(link);
    }

    /// Read back from the light, which is where the fact is kept: the one
    /// caller that asks is telling a phone that has gone from one that never
    /// arrived.
    fn link(&self) -> Link {
        self.lights.phone()
    }

    /// A tap on our icon on the phone's screen, which is the car's own interface
    /// being asked for. Kept if the car is not waiting yet.
    fn tapped(&self) {
        self.home.notify_one();
    }
}

impl carlife_md::link::Location for Hooks {
    /// The car's position goes to the phone through the object the iAP2 session
    /// reads, which takes the block base64'd: the encoding is that object's
    /// convention rather than CarLife's, so it is done here.
    fn fix(&self, nmea: &str) {
        let _ = self.vehicle.push_location(&BASE64.encode(nmea));
    }
}

impl Assets for Hooks {
    fn get(&self, name: &str) -> Option<&'static [u8]> {
        crate::assets::get(name)
    }

    fn names(&self) -> Vec<&'static str> {
        crate::assets::names().collect()
    }
}

impl Input for Hooks {
    fn press(
        &self,
        command: Command,
    ) -> futures_util::future::BoxFuture<'_, std::io::Result<()>> {
        Box::pin(self.events.dispatch(as_carplay(command)))
    }
}

/// The car's keys, said in the phone's vocabulary.
///
/// The two halves name the same commands and neither may grow one the other has
/// not, which is what makes this a `match` rather than a table: a command added
/// to one side and not carried over here does not compile.
fn as_carplay(command: Command) -> carplay_hu::press::Command {
    use carplay_hu::press::Command as Phone;
    match command {
        Command::Play => Phone::Play,
        Command::Pause => Phone::Pause,
        Command::PlayPause => Phone::PlayPause,
        Command::Next => Phone::Next,
        Command::Prev => Phone::Prev,
        Command::Home => Phone::Home,
        Command::Back => Phone::Back,
        Command::Select => Phone::Select,
        Command::Left => Phone::Left,
        Command::Right => Phone::Right,
        Command::Up => Phone::Up,
        Command::Down => Phone::Down,
        Command::Wheel(ticks) => Phone::Wheel(ticks),
        Command::Touch(slots) => Phone::Touch(
            slots.map(|slot| carplay_hu::press::TouchContact {
                down: slot.down,
                x: slot.x,
                y: slot.y,
            }),
        ),
        Command::Telephony(index) => Phone::Telephony(index),
        Command::KeyFrame => Phone::KeyFrame,
        Command::Siri => Phone::Siri,
        Command::Night(on) => Phone::Night(on),
    }
}
