//! `cp2cl` — a headless CarPlay receiver that hands what the phone sends to the
//! car.
//!
//! Nothing here draws anything itself. Three pieces run concurrently:
//!
//! * the CarPlay half (`carplay_hu`), which plays the head unit an iPhone
//!   expects to be plugged into and accepts the phone's streams,
//! * the CarLife half (`carlife_md`), which presents this device to a head unit
//!   as a phone and feeds it those streams,
//! * this program, which owns the board — its settings, its lights, its page —
//!   and is what the two halves know of each other.

mod assets;
mod button;
mod config;
mod hooks;
mod led;
mod logging;
mod settings;
mod sink;
mod web;

// The input vocabulary and the playing record belong to the CarLife half and
// live in that crate. Re-exported under their old names so the paths inside the
// program still read the same.
pub use carlife_md::now_playing;
pub use carlife_md::press as commands;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use iap2_mfi::{AuthCoprocessor, FileCoprocessor, NoCoprocessor};
use livi_runtime::mfi_async::SharedCoprocessor;
use livi_runtime::vehicle::Vehicle;
use tokio::net::TcpListener;
use tokio::sync::{watch, Notify};
use tracing_subscriber::prelude::*;

use carlife_md::outlet::Outlet;
use carplay_hu::{Events, Geometry};
use carplay_hu::report::Report;
use carplay_hu::sink::StreamSink;

use crate::config::hostname;
use crate::sink::ClSink;

#[tokio::main]
async fn main() -> Result<()> {
    // Everything the program prints goes through this, so `log_level` in
    // `/etc/cp2cl/config.json` is the only knob for how much of it is shown, and
    // the levels are the usual ones: `error` is something we set out to do and
    // could not, `warn` is something dropped or unexpected that we carried on
    // from, `info` is the life of a session — connecting, pairing, streams
    // opening and closing — and `debug` is the per-event detail that would
    // otherwise drown all of it.
    // That one setting is read here, before the subscriber exists, which is why
    // it is the only one read before there is anywhere to report a file that
    // could not be read; the read that follows says so.
    //
    // It goes to two places, and it is the same lines in both: the terminal,
    // coloured, and a copy under `/var/log` that is not, which is what the status
    // page reads and what is still there of a run that has ended. See
    // `logging::Journal` for how the copy is kept to size.
    //
    // The terminal is held to `log_level`, the copy is not: a run cannot be
    // dialled up to `debug` after the fact, so what is left on disk is what a
    // question about that run is answered from, and it is asked once the run is
    // over. `trace` is below both of them — nothing writes it, and what did
    // would be the internals of other people's crates.
    let journal = logging::Journal::open();
    let unnoticed = journal.reason().map(str::to_owned);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(logging::Line)
                .with_filter(settings::log_filter()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(logging::Line)
                .with_ansi(false)
                .with_writer(journal)
                .with_filter(tracing::level_filters::LevelFilter::DEBUG),
        )
        .init();
    if let Some(reason) = unnoticed {
        tracing::warn!("main.copy_unavailable path={} err={reason}", logging::PATH);
    }

    // Read once: what is saved while this run is up reaches the phone on the
    // next one.
    let cfg = Arc::new(settings::load_or_create());

    tracing::info!("main.started name=cp2cl");
    // Which build this run is, said once and at the top: every question about a
    // log begins with "which build was that", and the answer is not in the lines
    // that follow. It is the same string the page shows, from the same place.
    tracing::info!("main.version software={:?}", web::device::SOFTWARE);
    tracing::info!("main.screen {}", cfg.screen);

    // The settings this run was handed, because "why is nothing happening" is
    // nearly always one of them. The two halves of the program log different
    // things — a wired run never says `bt.*`, and a wireless run waits for
    // the car before it says anything at all — so a log that does not name its
    // own mode is one that cannot be read.
    tracing::info!(
        "main.config wireless={} wifi_iface={} bt_adapter={} log_level={}",
        cfg.wireless,
        cfg.wifi_iface,
        cfg.bt_adapter,
        settings::level_name(cfg.log_level)
    );

    // One channel for the two things that ask the wired link to come back: the
    // button and the settings page. One replug is a gadget unbind and bind,
    // which is what makes a head unit enumerate the accessory again.
    let (replug, replug_requested) = tokio::sync::mpsc::channel(1);

    // The board's lights and its one button. All three are best-effort: an
    // image without them runs exactly as before and says so once.
    //
    // The red light is the program's own state: out while the green one shows a
    // phase, back when the bridge has nothing to show and when there is no
    // program at all. Two ways out skip the drop that would put it back, a panic
    // (`panic = "abort"`) and a signal; both are handled here.
    let lights = led::Lights::take().map(Arc::new);
    if let Some(lights) = &lights {
        let lights = lights.clone();
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            lights.restore();
            default(info);
        }));
    }
    let (led, facts) = led::Signals::new();
    tokio::spawn(led::run(facts, lights.clone()));
    // A wired run needs neither radio, so there is nothing to wait for: the
    // first state it can be in is the one after both of them are up.
    if !cfg.wireless {
        led.radios(true);
    }
    tokio::spawn(button::watch(cfg.bt_adapter.clone(), replug.clone()));
    if let Some(lights) = &lights {
        // A wireless run already catches SIGTERM to put the Bluetooth
        // controllers back, and that path ends by returning through here, where
        // the lights go back anyway. Taking the same signal here as well would
        // cut it short, so only a wired run takes it — neither run has a
        // handler for SIGINT, SIGHUP or SIGQUIT.
        //
        // Cloned rather than handed over: this `lights` stays in `main` to the
        // end, so the drop that puts them back is the one on the way out of
        // `main` and not whatever the runtime does with a task it is shutting
        // down.
        let catch_term = !cfg.wireless;
        tokio::spawn(led::stopper(lights.clone(), catch_term));
    }

    // Everything the page shows is reported from wherever it is learned, and
    // nothing reads it back: see `web::status`. Started before anything else so
    // that the first thing a page says is never "nothing yet" for want of a
    // place to put it. The phone's half writes its own report, which is why the
    // page is built around it rather than the other way round.
    let report = Arc::new(Report::new());
    report.with(|s| {
        s.carplay.wireless = cfg.wireless;
        s.carplay.name = hostname();
        s.carplay.bt_mac = config::bt_address(&cfg.bt_adapter);
    });
    let status = Arc::new(web::Status::new(report.clone()));

    // Bound before the streams are: a page that cannot be reached is worth
    // saying so about at startup rather than when someone goes looking for it.
    // Not fatal, though — the page is the one part of this that is not the
    // bridge, and 80 is a port a machine not running as root cannot have.
    match TcpListener::bind(web::ADDR).await {
        Ok(listener) => {
            tracing::info!("web.listening url=http://{}/", web::ADDR);
            tokio::spawn(web::serve(listener, status.clone(), replug));
        }
        Err(e) => tracing::warn!("web.listen_failed addr={} err={e:#}", web::ADDR),
    }

    // The board's own CPU and memory, which nothing in the bridge measures: the
    // page's last section, and the only one sampled rather than reported.
    tokio::spawn(web::device::watch(status.clone()));

    // Ask for a free port rather than picking one: whatever we get is what gets
    // advertised to the phone, so there is no way for the two to disagree.
    let listener = TcpListener::bind(("::", 0))
        .await
        .context("bind airplay listener")?;

    // What the phone is asked to render: the CarLife session writes it once a
    // head unit has said what its screen is, and `GET /info` is answered from it.
    // Until then nothing has, and the default stands.
    let (screen, screen_in) = watch::channel::<Option<Geometry>>(None);

    // Which channel the head unit on the other end is, once it has said. `GET
    // /info` is answered from it, so the phone is shown the icon and the label
    // that go with the car it is plugged into.
    let (channel_id, channel_id_in) = watch::channel::<Option<String>>(None);

    // Fired when the phone's icon for us is tapped. The car can be plugged out
    // and back in, and whoever is waiting at the time is the one that matters:
    // a tap with nobody waiting is kept until there is. The CarLife half waits
    // on it; the hook that fires it holds the other handle.
    let home = Arc::new(Notify::new());

    // Input goes out through here whatever produced it. Built now and filled in
    // later: the keys only exist once pair-verify has run.
    let press = Events::new();

    // What a running session is told when what to render changes. `/info` is
    // answered once per session and never asked again, so a new size only lands
    // on the next one: the session is dropped and built again. Only that
    // something changed is kept, because a session acting on a change that has
    // already been superseded is worse than not acting at all.
    let (restart, _) = watch::channel(0u64);

    // The car's position goes to the phone through this, and only through this:
    // the CarLife session is where a fix arrives and the iAP2 session is what
    // hands it on, so both halves hold the same sending half.
    let vehicle = Arc::new(Vehicle::default());

    // The car is the consumer, and this is the queue between the two halves:
    // access units and the phone's sound arrive from CarPlay and wait here for a
    // CarLife session to take them out, on the video channel and the media one.
    let outlet = Arc::new(Outlet::new());

    // One object per stream, not two: a video event and an audio event differ
    // only in which channel the consumer converts them to. There are two of them
    // because there are two streams that can be told apart this early — the
    // screen and the sound — and the CarLife sink is one object because both of
    // them are the same car on the same connection. What the phone is playing,
    // and where it is going, go through the same object, which is why it is kept
    // whole rather than erased into the two traits below.
    let sink = Arc::new(ClSink::new(outlet.sink()));
    let (video_sink, audio_sink): (Arc<dyn StreamSink>, Arc<dyn StreamSink>) =
        (sink.clone(), sink.clone());

    // Worth saying out loud: nothing here draws a picture, and what the phone is
    // asked to render is the car's own screen — so nothing is rendered until a
    // car has said what that screen is. A size picked on the page is the one
    // exception.
    tracing::info!(
        "main.bridge.ready sink=carlife screen_source={}",
        if cfg.screen.is_empty() { "car" } else { "config" }
    );

    let data_dir = PathBuf::from(config::DATA_DIR);

    // Built before the receiver starts because both halves need it: iAP2 to
    // answer the phone's challenge, and airplay to answer /auth-setup.
    let auth = SharedCoprocessor::new(mfi_signer()?);

    // What the CarPlay half is told about the machine, and the two of these that
    // are not properties of it: how much room the car's media message leaves for
    // a cover, which is the consumer's number rather than the phone's.
    let machine = Arc::new(carplay_hu::machine::Machine {
        wireless: cfg.wireless,
        wifi_iface: cfg.wifi_iface.clone(),
        bt_adapter: cfg.bt_adapter.clone(),
        name: hostname(),
        vendor: config::dt_vendor(),
        bt_address: config::bt_address(&cfg.bt_adapter),
        data_dir: data_dir.clone(),
        artwork_limit: carlife_md::MAX_ARTWORK,
    });

    // Whether the CarPlay half is open for the phone at all. It starts shut and
    // the car's session is the only thing that opens it: nothing is published
    // and nothing is accepted until there is a head unit on the other end.
    let (gateway, gate) = watch::channel(false);
    tracing::debug!(
        "main.bridge.carplay_side state={}",
        if *gateway.borrow() { "open" } else { "shut" }
    );

    // The one object both halves are given: it answers as the host that is told
    // what happened, as the asset table, and as where a key goes.
    let hooks = hooks::Hooks::new(
        gateway.clone(),
        restart.clone(),
        screen,
        channel_id,
        home.clone(),
        led.clone(),
        press.clone(),
        vehicle.clone(),
    );

    // Bound either way, and bound now: this port is what the phone is told, so
    // it has to be known before the car turns up and before the watcher has
    // anything to advertise. Whether anything is accepted on it is the gate's
    // business. The number comes from the socket rather than from a constant:
    // there is then no way for the two to disagree.
    let airplay_port = listener.local_addr().map(|addr| addr.port()).unwrap_or(0);
    tracing::info!(
        "carplay.airplay.receiver port={airplay_port} state={}",
        if *gateway.borrow() {
            "listening"
        } else {
            "held"
        }
    );

    let phone = carplay_hu::Hu::new(carplay_hu::Ports {
        machine,
        assets: &assets::TABLE,
        mfi: auth,
        listener,
        gate,
        video_sink,
        audio_sink,
        panel: sink,
        events: press,
        report: report.clone(),
        vehicle: vehicle.clone(),
        delegate: hooks.clone(),
        screen: screen_in,
        channel_id: channel_id_in,
        restart: restart.clone(),
    })?;

    // The car comes first and CarPlay follows it: nothing is published and
    // nothing is accepted until there is a head unit on the other end of the
    // bridge, and both go away again when it does. Which of the two switched
    // sides is open is the CarLife half's to say, and this is where it says it.

    // What the CarLife half is told about the machine. None of it is a choice:
    // the hostname is the name the device goes by, the device tree is its maker,
    // and the controller holds its own address. The screen is the one exception,
    // and it is read out of the settings file — for each session, because the
    // page can change it while the program runs.
    let settings = carlife_md::machine::Machine {
        screen: hooks.screen_chosen(),
        name: hostname(),
        vendor: config::dt_vendor(),
        bt_address: config::bt_address(&cfg.bt_adapter),
        data_dir,
    };

    tracing::info!("main.bridge.carlife_up carplay_side=waiting");
    // In the background, because the two sides are each other's condition: this
    // task is what opens the gate, and the watcher below is the only thing that
    // acts on it. Waiting for it to return here would leave nobody to answer the
    // gate it opens.
    let car = hooks.clone();
    let alone = gateway.clone();
    // The car's own input goes the way everything else does, so the session gets
    // the same sink the CarPlay side fills in.
    let reported = status.carlife().clone();
    tokio::spawn(async move {
        // Held in this task rather than in `main`: the ports are borrows of it,
        // and both have to outlive the loop that runs one session after another.
        let ports = carlife_md::Ports {
            input: car.clone(),
            delegate: &*car,
            assets: &*car,
            home: home.clone(),
            outlet,
            report: reported,
            location: car.clone(),
        };
        let outcome = carlife_md::Md::new(&settings, &ports, replug_requested)
            .serve()
            .await;
        if let Err(e) = outcome {
            // `{e:#}` and not `{e}`: the outer message says what we were doing
            // and the causes under it hold the errno, which is the part that
            // says why.
            tracing::error!("main.bridge.carlife_stopped err={e:#}");
            tracing::warn!("main.bridge.degraded mode=carplay_only");
            let _ = alone.send(true);
        }
    });

    // The phone's half runs last and does not return: it accepts on the socket
    // above, and it is what carries iAP2 — over the cable or over Bluetooth,
    // whichever this run was configured for.
    phone.run().await;
    Ok(())
}

/// The two halves of the MFi identity, by the names the firmware installs them
/// under in `config::DATA_DIR`.
const MFI_KEY: &str = "identity.pk8";
const MFI_CERT: &str = "certificate.p7b";

/// Local credentials when the data directory holds them, a dead end with a clear
/// message otherwise.
///
/// Wired CarPlay still runs the full MFi challenge, so without credentials the
/// phone identifies us and then refuses to authenticate. Saying so up front
/// beats the cryptic 0xAA04 that comes back later.
fn mfi_signer() -> Result<Box<dyn AuthCoprocessor + Send>> {
    let read = |name: &str| {
        let path = config::data_file(name);
        std::fs::read(&path).map_err(|e| {
            // A file that is not there is a build without credentials, which the
            // line below says once for both halves. One that is there and cannot
            // be read is a different thing, and this is the only place it is said.
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("carplay.mfi.credential_unreadable file={} err={e:#}", path.display());
            }
            e
        })
    };
    let (Ok(key), Ok(cert)) = (read(MFI_KEY), read(MFI_CERT)) else {
        tracing::warn!(
            "carplay.mfi.credentials_missing dir={} consequence=phone_rejects",
            config::DATA_DIR
        );
        return Ok(Box::new(NoCoprocessor));
    };
    // The certificate first, the key second: that is the order the coprocessor
    // takes them in, and the two are not interchangeable.
    let signer = FileCoprocessor::from_bytes(&cert, &key)
        .context("the MFi credentials in the data directory could not be used")?;
    tracing::info!(
        "carplay.mfi.credentials_loaded key_bytes={} cert_bytes={}",
        key.len(),
        cert.len()
    );
    Ok(Box::new(signer))
}
