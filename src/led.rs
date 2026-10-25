//! The board's two lights: what this program makes them say.
//!
//! Both are kernel LED-class devices — `gpio-leds` on PA10 (green, status) and
//! PL10 (red, power) — so each is two sysfs files rather than a GPIO held open.
//! While a light's trigger is `timer` the kernel blinks it at
//! `delay_on`/`delay_off`, which is why the patterns are rates here rather than
//! a loop: nothing in this process wakes up to turn a light on or off.
//!
//! The red one means something about *this program* rather than about the
//! board: the device tree lights it with `default-state = "on"`, so it is put
//! out while the program runs and comes back when there is none.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::watch;

use crate::config::Config;

/// The status LED, by the label the device tree gives it.
const STATUS: &str = "/sys/class/leds/nanopi:green:status";

/// The power LED, by the label the device tree gives it. Its node carries
/// `default-state = "on"`, which is the state this module hands back on exit.
const POWER: &str = "/sys/class/leds/nanopi:red:pwr";

/// What the green light is saying: four states, one look each, so what a person
/// sees can be read without a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Put out: no car, or nothing here for a phone to come back to.
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
}

enum Look {
    Steady(bool),
    Blink { on: u64, off: u64 },
}

/// Drives the status light for as long as the program runs.
///
/// Every part of the bridge that learns something the light shows sends a
/// phase; this is the only place that writes it to the filesystem, because a
/// sysfs write blocks and none of those places is one to wait in.
pub async fn run(mut phase: watch::Receiver<Phase>) {
    let Some(led) = Led::open(STATUS, "status") else {
        return;
    };
    loop {
        let wanted = *phase.borrow_and_update();
        match led.show(wanted) {
            Ok(()) => tracing::info!("led.phase light=status phase={}", wanted.name()),
            Err(e) => tracing::warn!(
                "led.write_failed light=status phase={} err={e}",
                wanted.name()
            ),
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
    /// Takes the lights off the boot's hands: the red one out, which is what a
    /// running program looks like.
    pub fn take() -> Option<Self> {
        let power = Led::open(POWER, "power")?;
        let status = Led::open(STATUS, "status");
        let lights = Self { power, status };
        if let Err(e) = lights.power.level(false) {
            tracing::warn!("led.write_failed light=power err={e}");
        }
        tracing::info!("led.power state=off reason=program_running");
        Some(lights)
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

/// Whether there is anything for a phone to come back to: an access point on
/// air and a Bluetooth controller up.
///
/// Both are started by the init scripts before this program, so a missing one is
/// a boot that failed rather than a race — and a light that beats for a phone
/// which cannot arrive is worse than no light at all.
pub fn ready(cfg: &Config) -> bool {
    if !cfg.wireless {
        return true;
    }
    let ap = Path::new("/sys/class/net").join(&cfg.wifi_iface).exists();
    let bt = Path::new("/sys/class/bluetooth")
        .join(&cfg.bt_adapter)
        .exists();
    if !(ap && bt) {
        tracing::warn!("led.not_ready wifi={ap} bt={bt} consequence=no_heartbeat");
    }
    ap && bt
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

    fn show(&self, phase: Phase) -> io::Result<()> {
        match phase.look() {
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

    /// The two rates are the requirement, and they are the one thing here that
    /// a typo would change without anything failing.
    #[test]
    fn the_two_blink_rates_are_the_ones_asked_for() {
        let blink = |phase: Phase| match phase.look() {
            Look::Blink { on, off } => (on, off),
            Look::Steady(_) => (0, 0),
        };
        assert_eq!(blink(Phase::Waiting), (500, 500));
        assert_eq!(blink(Phase::Connecting), (200, 200));
        assert!(matches!(Phase::Off.look(), Look::Steady(false)));
        assert!(matches!(Phase::Linked.look(), Look::Steady(true)));
    }
}
