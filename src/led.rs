//! The board's two lights: what this program makes them say.
//!
//! Both are kernel LED-class devices — `gpio-leds` on PA10 (green, status) and
//! PL10 (red, power) — so each is two sysfs files rather than a GPIO held open.
//! While a light's trigger is `timer` the kernel blinks it at
//! `delay_on`/`delay_off`, which is why the patterns are rates here rather than
//! a loop: nothing in this process wakes up to turn a light on or off.
//!
//! Nothing here decides *when* anything happens. The parts of the bridge that
//! learn something send it as one of three facts — whether the radios are up,
//! whether the car is talking to us, and how far a phone got — and the five
//! states are read off those facts, highest first. That is what keeps a caller
//! from having to know what the one above it says: a phone that connects while
//! the car is still missing does not have to be careful about it.
//!
//! The red one means something about *this program* rather than about the
//! board: the device tree lights it with `default-state = "on"`. It beats while
//! there is nothing for a phone to come back to — no radios, or no car — and is
//! put out once there is. It is lit when there is no program at all.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::watch;

/// The status LED, by the label the device tree gives it.
const STATUS: &str = "/sys/class/leds/nanopi:green:status";

/// The power LED, by the label the device tree gives it. Its node carries
/// `default-state = "on"`, which is the state this module hands back on exit.
const POWER: &str = "/sys/class/leds/nanopi:red:pwr";

/// What the green light is saying: five states, one look each, so what a person
/// sees can be read without a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Nothing for a phone to come back to: the interface, the controller or
    /// the access point is missing. Green out, red beating.
    Init,
    /// Both radios are up and no car is: a slow beat, and the red one with it.
    Waiting,
    /// The car is up and no phone is connected: a slow beat, red out.
    Standby,
    /// A phone is on its way: a fast one.
    Connecting,
    /// CarPlay is up: steady.
    Linked,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Init => "init",
            Phase::Waiting => "waiting",
            Phase::Standby => "standby",
            Phase::Connecting => "connecting",
            Phase::Linked => "linked",
        }
    }

    /// The two rates, in milliseconds on and off: a slow even beat to say
    /// "waiting", a fast one to say "connecting". Both are half on and half
    /// off, so the tempo is the whole of the difference. The two states that
    /// share the slow beat are told apart by the red one.
    fn look(self) -> Look {
        match self {
            Phase::Init => Look::Steady(false),
            Phase::Waiting | Phase::Standby => Look::Blink { on: 500, off: 500 },
            Phase::Connecting => Look::Blink { on: 200, off: 200 },
            Phase::Linked => Look::Steady(true),
        }
    }

    /// What the red one says while this phase holds: a beat until there is a
    /// car to hand a phone's media to, and out once there is.
    fn power(self) -> Look {
        match self {
            Phase::Init | Phase::Waiting => Look::Blink { on: 500, off: 500 },
            Phase::Standby | Phase::Connecting | Phase::Linked => Look::Steady(false),
        }
    }
}

/// How far a phone has got, which is the last of the three facts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Carplay {
    /// No phone, or none that has reached the receiver.
    #[default]
    Idle,
    /// A phone has opened the receiver. Whether it will get anywhere is not
    /// known yet.
    Connecting,
    /// The phone's screen is arriving.
    Linked,
}

/// What the lights are told, and the whole of what decides what they show.
///
/// Three facts rather than one state, because a state is an answer to a
/// question none of the reporting places can ask: the car's session says
/// nothing about the radios, and the receiver says nothing about the car.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Facts {
    /// Both radios are up: the interface, the controller, the daemon behind
    /// them and the access point the phone is told to join.
    pub radios: bool,
    /// The car's session is up — the head unit has been given our device
    /// information, which is past enumeration and past the point where the
    /// cable alone would be enough.
    pub carlife: bool,
    pub carplay: Carplay,
}

impl Facts {
    /// The state, read off the facts from the top down: the first one whose
    /// conditions all hold is the one the lights show.
    ///
    /// Read rather than announced, so a fact that stops being true drops the
    /// lights to the state it still satisfies without anyone working out which
    /// one that is. A car unplugged while a phone is mirrored answers
    /// `Waiting` because that is what is left, not because anybody said so.
    fn phase(self) -> Phase {
        match (self.radios, self.carlife, self.carplay) {
            (true, true, Carplay::Linked) => Phase::Linked,
            (true, true, Carplay::Connecting) => Phase::Connecting,
            (true, true, Carplay::Idle) => Phase::Standby,
            (true, false, _) => Phase::Waiting,
            (false, _, _) => Phase::Init,
        }
    }
}

#[derive(Clone, Copy)]
enum Look {
    Steady(bool),
    Blink { on: u64, off: u64 },
}

impl Look {
    /// How the log names it.
    fn state(self) -> &'static str {
        match self {
            Look::Steady(true) => "on",
            Look::Steady(false) => "off",
            Look::Blink { .. } => "blink",
        }
    }
}

/// Where the three facts are reported, and the only handle the rest of the
/// bridge has on them.
///
/// Held by whoever learns something rather than queried by the lights: a report
/// is a few assignments, and every place that learns a fact is one that cannot
/// wait for a sysfs write.
pub struct Signals {
    facts: watch::Sender<Facts>,
}

impl Signals {
    /// The sender, and the receiver the light task reads.
    pub fn new() -> (Arc<Self>, watch::Receiver<Facts>) {
        let (facts, receiver) = watch::channel(Facts::default());
        (Arc::new(Self { facts }), receiver)
    }

    /// Whether the radios are up. Sent from the place that publishes the
    /// service, once for each car session, so a later answer can differ from an
    /// earlier one.
    pub fn radios(&self, up: bool) {
        self.facts.send_if_modified(|facts| {
            let changed = facts.radios != up;
            facts.radios = up;
            changed
        });
    }

    /// Whether the car's session is up.
    pub fn carlife(&self, up: bool) {
        self.facts.send_if_modified(|facts| {
            let changed = facts.carlife != up;
            facts.carlife = up;
            changed
        });
    }

    /// How far the phone has got.
    pub fn carplay(&self, state: Carplay) {
        self.facts.send_if_modified(|facts| {
            let changed = facts.carplay != state;
            facts.carplay = state;
            changed
        });
    }

    /// How far the phone has got, read back by the one caller that has to tell
    /// a phone that has gone from one that never arrived.
    pub fn phone(&self) -> Carplay {
        self.facts.borrow().carplay
    }
}

/// Drives both lights for as long as the program runs.
///
/// The state is computed from the facts on arrival and written only when it
/// differs from what the lights are already showing: a fact that changes
/// without moving the state — a car going while no phone is here — is worth no
/// sysfs write at all.
///
/// This is the only place that writes them, because a sysfs write blocks and
/// none of the places that report a fact is one to wait in.
pub async fn run(mut facts: watch::Receiver<Facts>, lights: Option<Arc<Lights>>) {
    let status = Led::open(STATUS, "status");
    let mut shown = None;
    loop {
        let wanted = facts.borrow_and_update().phase();
        if shown != Some(wanted) {
            if let Some(status) = &status {
                match status.show(wanted.look()) {
                    Ok(()) => tracing::info!("led.phase light=status phase={}", wanted.name()),
                    Err(e) => tracing::warn!(
                        "led.write_failed light=status phase={} err={e}",
                        wanted.name()
                    ),
                }
            }
            if let Some(lights) = &lights {
                lights.show_power(wanted);
            }
            shown = Some(wanted);
        }
        if facts.changed().await.is_err() {
            return;
        }
    }
}

/// Both lights, while the program runs, so that stopping puts the board back
/// the way it came up: green out — the kernel's timer keeps blinking without
/// anyone asking it to — and red on.
///
/// Held by `main` rather than spawned: the drop is what puts them back, and a
/// drop at the end of `main` is the one moment every ordinary way out passes
/// through. Panics abort straight past it (`panic = "abort"`), and a signal
/// skips it too, which is what `stopper` and the hook in `main` are for.
pub struct Lights {
    power: Led,
    status: Option<Led>,
}

impl Lights {
    /// Takes the lights. Nothing is written until `run` has a state to show,
    /// because the state is what decides both of them.
    pub fn take() -> Option<Self> {
        let power = Led::open(POWER, "power")?;
        let status = Led::open(STATUS, "status");
        Some(Self { power, status })
    }

    /// Beating or put out, which is the whole of what the red light says.
    fn show_power(&self, phase: Phase) {
        let look = phase.power();
        match self.power.show(look) {
            Ok(()) => tracing::info!("led.power state={} phase={}", look.state(), phase.name()),
            Err(e) => tracing::warn!("led.write_failed light=power err={e}"),
        }
    }

    /// Puts them back, which is what "the program is gone" looks like. There is
    /// nobody left to tell if this fails: it is the last thing that happens.
    pub fn restore(&self) {
        if let Some(status) = &self.status {
            let _ = status.level(false);
        }
        let _ = self.power.level(true);
    }
}

impl Drop for Lights {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Puts the power light back when a signal stops the program, which is the way
/// out that skips `Drop`. The four signals a shell, an init script or a terminal
/// sends: what a process does when one of them arrives without a handler is end,
/// and the light would stay out.
///
/// `catch_term` names whether nothing else already catches SIGTERM: the
/// wireless watcher does, to put the Bluetooth controllers back, and that path
/// ends by returning through `main` — where the light goes back on anyway.
/// Handling the signal here as well would cut that short, so a wireless run
/// leaves SIGTERM to it and takes the other three, which have no handler.
pub async fn stopper(lights: Arc<Lights>, catch_term: bool) {
    let mut term = if catch_term {
        arm(SignalKind::terminate(), "term")
    } else {
        None
    };
    let mut int = arm(SignalKind::interrupt(), "int");
    let mut hup = arm(SignalKind::hangup(), "hup");
    let mut quit = arm(SignalKind::quit(), "quit");

    tokio::select! {
        () = raised(&mut term) => tracing::info!("process.stopped signal=term"),
        () = raised(&mut int) => tracing::info!("process.stopped signal=int"),
        () = raised(&mut hup) => tracing::info!("process.stopped signal=hup"),
        () = raised(&mut quit) => tracing::info!("process.stopped signal=quit"),
    }
    lights.restore();
    std::process::exit(0);
}

/// One signal the stopper listens for. Not being able to ask for it is a reason
/// to carry on with the others rather than to stop listening.
fn arm(kind: SignalKind, name: &str) -> Option<Signal> {
    signal(kind)
        .inspect_err(|e| tracing::warn!("led.signal_unavailable kind={name} err={e}"))
        .ok()
}

/// Waits for a signal that was asked for. One that was not is a future that is
/// never ready, which is what keeps its arm of the `select!` quiet.
async fn raised(signal: &mut Option<Signal>) {
    match signal {
        Some(armed) => {
            let _ = armed.recv().await;
        }
        None => std::future::pending().await,
    }
}

struct Led {
    dir: PathBuf,
}

impl Led {
    /// The LED-class device a label names, if this image has one.
    fn open(path: &str, light: &str) -> Option<Self> {
        let dir = PathBuf::from(path);
        if !dir.exists() {
            tracing::warn!("led.unavailable light={light} path={path} consequence=unused");
            return None;
        }
        Some(Self { dir })
    }

    fn show(&self, look: Look) -> io::Result<()> {
        match look {
            Look::Steady(on) => self.level(on),
            Look::Blink { on, off } => {
                // Selecting the trigger starts the blink; the delays come after
                // it, and writing them is what makes the new pattern take over.
                self.set("trigger", "timer")?;
                self.set("delay_on", &on.to_string())?;
                self.set("delay_off", &off.to_string())
            }
        }
    }

    /// Lit or put out, whatever was driving it before.
    fn level(&self, on: bool) -> io::Result<()> {
        // A trigger owns `brightness`: the write fails with EBUSY while one is
        // in place, so it has to let go first.
        self.set("trigger", "none")?;
        self.set("brightness", if on { "1" } else { "0" })
    }

    fn set(&self, name: &str, value: &str) -> io::Result<()> {
        std::fs::write(self.dir.join(name), value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ladder, from the top down: each state is the one above it plus one
    /// more fact, so anything that stops being true drops to the state it still
    /// satisfies. A typo in the order would show a phone as connected while the
    /// car it is feeding is unplugged, which is the one reading a person would
    /// act on.
    #[test]
    fn the_state_is_the_highest_one_whose_facts_hold() {
        let facts = |radios, carlife, carplay| Facts {
            radios,
            carlife,
            carplay,
        };
        assert_eq!(facts(true, true, Carplay::Linked).phase(), Phase::Linked);
        assert_eq!(
            facts(true, true, Carplay::Connecting).phase(),
            Phase::Connecting
        );
        assert_eq!(facts(true, true, Carplay::Idle).phase(), Phase::Standby);
        // The car gone takes the phone's state with it: nothing is being handed
        // to a head unit that is not there.
        for carplay in [Carplay::Idle, Carplay::Connecting, Carplay::Linked] {
            assert_eq!(facts(true, false, carplay).phase(), Phase::Waiting);
        }
        for carplay in [Carplay::Idle, Carplay::Linked] {
            assert_eq!(facts(false, false, carplay).phase(), Phase::Init);
        }
        assert_eq!(facts(false, true, Carplay::Linked).phase(), Phase::Init);
    }

    /// The rates are the requirement, and they are the one thing here that a
    /// typo would change without anything failing.
    #[test]
    fn the_blink_rates_are_the_ones_asked_for() {
        let blink = |look: Look| match look {
            Look::Blink { on, off } => (on, off),
            Look::Steady(_) => (0, 0),
        };
        assert_eq!(blink(Phase::Waiting.look()), (500, 500));
        assert_eq!(blink(Phase::Standby.look()), (500, 500));
        assert_eq!(blink(Phase::Connecting.look()), (200, 200));
        assert_eq!(blink(Phase::Init.power()), (500, 500));
        assert_eq!(blink(Phase::Waiting.power()), (500, 500));
        assert!(matches!(Phase::Init.look(), Look::Steady(false)));
        assert!(matches!(Phase::Linked.look(), Look::Steady(true)));
        for phase in [Phase::Standby, Phase::Connecting, Phase::Linked] {
            assert!(matches!(phase.power(), Look::Steady(false)));
        }
    }
}
