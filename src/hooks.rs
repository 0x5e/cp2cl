//! What the two halves ask of the program while it runs.
//!
//! The car's session reports three things out of it — that it is up or down,
//! what size the car asked to be rendered at, and which channel number the car
//! names itself by — and asks two back: the size the page picked, and what the
//! channel table says about that number's panel. The presses the car makes go
//! the other way, through the same sink the phone reads them from.
//!
//! The phone's half reports how far a phone got, which is the third fact the
//! lights are drawn from, and nothing else.
//!
//! Nothing here is policy either half could have had. A session is the only
//! thing that knows when the CarPlay half should be open, which is why that
//! switch is thrown here rather than watched there.

use std::sync::Arc;

use carlife_md::link::{Assets, Delegate, Input};
use carlife_md::panel::Panel;
use carlife_md::press::Command;
use carlife_md::settings::Screen;
use carplay_hu::airplay::{EventSink, ScreenParams, SharedChannel, SharedScreen};
use carplay_hu::link::Link;
use tokio::sync::{broadcast, watch};

use crate::led;

/// The one object both halves are given: it answers for each of them as the
/// host that is told what happened, as the asset table, and as where a key goes.
pub struct Hooks {
    /// Whether the CarPlay half accepts anything at all. Shut until a head unit
    /// has been told who this device is, and shut again when it goes.
    carplay: watch::Sender<bool>,
    /// What a running CarPlay session is told when what it renders changes.
    restart: broadcast::Sender<()>,
    /// The size the car named, which is what `/info` answers with.
    screen: SharedScreen,
    /// The channel number the car named, which picks the icon `/info` carries.
    channel: SharedChannel,
    /// The bridge's own light.
    lights: Arc<led::Signals>,
    /// Where a key pressed in the car arrives for the phone.
    events: Arc<EventSink>,
}

impl Hooks {
    pub fn new(
        carplay: watch::Sender<bool>,
        restart: broadcast::Sender<()>,
        screen: SharedScreen,
        channel: SharedChannel,
        lights: Arc<led::Signals>,
        events: Arc<EventSink>,
    ) -> Arc<Self> {
        Arc::new(Self {
            carplay,
            restart,
            screen,
            channel,
            lights,
            events,
        })
    }

    /// The size the operator picked, out of the settings file.
    ///
    /// Read each time rather than kept: the page writes it while the program
    /// runs, and a copy taken at startup is a copy of what was there before.
    pub fn screen_chosen(&self) -> Screen {
        let picked = crate::settings::load().screen;
        Screen {
            resolution: picked
                .resolution
                .map(|size| carlife_md::settings::Resolution {
                    width: size.width,
                    height: size.height,
                }),
            fps: picked.fps,
        }
    }
}

impl Delegate for Hooks {
    fn session(&self, up: bool) {
        let _ = self.carplay.send(up);
        self.lights.carlife(up);
        if up {
            // The phone will not start CarPlay on its own — it has to be told
            // again — and a fresh iAP2 session is how it is told.
            let _ = self.restart.send(());
        }
    }

    /// A size that lands after the phone has already read `/info` is worth a
    /// session of its own — that is what `restart` is for — and a size that
    /// changes nothing is not worth saying twice.
    fn screen(&self, width: u32, height: u32, fps: u32) {
        let wanted = ScreenParams {
            width,
            height,
            fps,
            ..ScreenParams::default()
        };
        let Ok(mut slot) = self.screen.lock() else {
            return;
        };
        if (*slot).unwrap_or_default() == wanted {
            return;
        }
        tracing::info!("carplay.video.screen_named width={width} height={height} fps={fps}");
        *slot = Some(wanted);
        drop(slot);
        let _ = self.restart.send(());
    }

    fn channel(&self, channel: Option<&str>) {
        if let Ok(mut reported) = self.channel.lock() {
            *reported = channel.map(str::to_string);
        }
    }

    /// Read again for each session, because the page can change it while the
    /// program runs.
    fn screen_override(&self) -> Screen {
        self.screen_chosen()
    }

    /// The table belongs to this program — `airplay::info` reads it for `/info`
    /// too — so the entry is looked up here and handed over as the CarLife half
    /// reads it.
    fn panel(&self, channel: Option<&str>) -> Panel {
        let entry = carplay_hu::airplay::info::panel(channel);
        Panel {
            preset: entry.preset.map(|preset| carlife_md::panel::Preset {
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
