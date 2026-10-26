//! The board's two lights: what this program makes them say.
//!
//! Both are kernel LED-class devices — `gpio-leds` on PA10 (green, status) and
//! PL10 (red, power) — so each is two sysfs files rather than a GPIO held open.
//! While a light's trigger is `timer` the kernel blinks it at
//! `delay_on`/`delay_off`, which is why the patterns are rates here rather than
//! a loop: nothing in this process wakes up to turn a light on or off.
//!
//! The red one means something about *this program* rather than about the
//! board: the device tree lights it with `default-state = "on"`. It beats while
//! the green one is out (no car, or nothing here for a phone to come back to),
//! is put out while the green one is showing a phase — waiting, connecting,
//! linked — and is lit when there is no program at all.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::watch;

use crate::config::Config;
use crate::wireless;

/// The status LED, by the label the device tree gives it.
const STATUS: &str = "/sys/class/leds/nanopi:green:status";

/// The power LED, by the label the device tree gives it. Its node carries
/// `default-state = "on"`, which is the state this module hands back on exit.
const POWER: &str = "/sys/class/leds/nanopi:red:pwr";

/// What the green light is saying: four states, one look each, so what a person
/// sees can be read without a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Put out, and the red one beating: no car, or nothing here for a phone to
    /// come back to.
    Off,
    /// The car is up and no phone is connected: a slow beat.
    Waiting,
    /// A phone is on its way: a fast one.
    Connecting,
    /// CarPlay is up: steady.
    Linked,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Off => "off",
            Phase::Waiting => "waiting",
            Phase::Connecting => "connecting",
            Phase::Linked => "linked",
        }
    }

    /// The two rates, in milliseconds on and off: a slow even beat to say
    /// "waiting", a fast one to say "connecting". Both are half on and half
    /// off, so the tempo is the whole of the difference.
    fn look(self) -> Look {
        match self {
            Phase::Off => Look::Steady(false),
            Phase::Waiting => Look::Blink { on: 500, off: 500 },
            Phase::Connecting => Look::Blink { on: 200, off: 200 },
            Phase::Linked => Look::Steady(true),
        }
    }

    /// What the red one says while this phase holds: a beat while the green one
    /// is out, meaning nothing here is ready for a phone, and out while the
    /// green one is saying something.
    fn power(self) -> Look {
        match self {
            Phase::Off => Look::Blink { on: 500, off: 500 },
            Phase::Waiting | Phase::Connecting | Phase::Linked => Look::Steady(false),
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

/// Drives both lights for as long as the program runs.
///
/// Every part of the bridge that learns something the lights show sends a
/// phase; this is the only place that writes it to the filesystem, because a
/// sysfs write blocks and none of those places is one to wait in.
///
/// The red one follows the same phase: beating while the green one is out, and
/// out while the green one is saying something.
pub async fn run(mut phase: watch::Receiver<Phase>, lights: Option<Arc<Lights>>) {
    let status = Led::open(STATUS, "status");
    // Written on the first pass rather than in `take`: the phase is what says
    // what the red one does, and it is the same answer here as on every change
    // after.
    let mut shown = None;
    loop {
        let wanted = *phase.borrow_and_update();
        if let Some(status) = &status {
            match status.show(wanted.look()) {
                Ok(()) => tracing::info!("led.phase light=status phase={}", wanted.name()),
                Err(e) => tracing::warn!(
                    "led.write_failed light=status phase={} err={e}",
                    wanted.name()
                ),
            }
        }
        if shown != Some(wanted) {
            if let Some(lights) = &lights {
                lights.show_power(wanted);
            }
            shown = Some(wanted);
        }
        if phase.changed().await.is_err() {
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
    /// Takes the lights: nothing is written until `run` has a phase to show,
    /// because that phase is what decides the red one.
    pub fn take() -> Option<Self> {
        let power = Led::open(POWER, "power")?;
        let status = Led::open(STATUS, "status");
        Some(Self { power, status })
    }

    /// Beating or put out, which is the whole of what the red light says.
    fn show_power(&self, phase: Phase) {
        let look = phase.power();
        match self.power.show(look) {
            Ok(()) => tracing::info!("led.power state={}", look.state()),
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

/// Whether there is anything for a phone to come back to: the interface the
/// access point runs on, and the controller it is paged on.
///
/// A light that beats for a phone which cannot arrive is worse than no light at
/// all, so a missing radio is a phase this program never leaves.
pub fn ready(cfg: &Config) -> bool {
    if !cfg.wireless {
        return true;
    }
    let (wifi, bt) = wireless::radios_present(&cfg.wifi_iface, &cfg.bt_adapter);
    if !(wifi && bt) {
        tracing::warn!("led.not_ready wifi={wifi} bt={bt} consequence=no_heartbeat");
    }
    wifi && bt
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

    /// The rates are the requirement, and they are the one thing here that a
    /// typo would change without anything failing.
    #[test]
    fn the_blink_rates_are_the_ones_asked_for() {
        let blink = |look: Look| match look {
            Look::Blink { on, off } => (on, off),
            Look::Steady(_) => (0, 0),
        };
        assert_eq!(blink(Phase::Waiting.look()), (500, 500));
        assert_eq!(blink(Phase::Connecting.look()), (200, 200));
        assert_eq!(blink(Phase::Off.power()), (500, 500));
        assert!(matches!(Phase::Off.look(), Look::Steady(false)));
        assert!(matches!(Phase::Linked.look(), Look::Steady(true)));
        for phase in [Phase::Waiting, Phase::Connecting, Phase::Linked] {
            assert!(matches!(phase.power(), Look::Steady(false)));
        }
    }
}
