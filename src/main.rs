//! `cp2cl` — a headless CarPlay receiver that hands what the phone sends to the
//! car.
//!
//! Nothing here draws anything itself. Three pieces run concurrently:
//!
//! * a session watcher that carries iAP2 over USB — or over Bluetooth, when
//!   `CP2CL_WIRELESS` is set — and starts CarPlay,
//! * an airplay receiver that accepts the phone's return call and decrypts its
//!   media,
//! * the CarLife session, which presents this device to a head unit as a phone
//!   and feeds it those streams.

mod aac;
mod airplay;
mod artwork;
mod assets;
mod carlife;
mod commands;
mod config;
mod logging;
mod navigation;
mod playing;
mod session;
mod sink;
mod web;
mod wireless;

use std::sync::Arc;

use anyhow::{Context, Result};
use iap2_mfi::local::LocalCoprocessor;
use iap2_mfi::{AuthCoprocessor, NoCoprocessor};
use livi_runtime::mfi_async::SharedCoprocessor;
use livi_runtime::vehicle::Vehicle;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing_subscriber::prelude::*;

use crate::airplay::clock::SyncedClock;
use crate::airplay::pairing::Pairings;
use crate::airplay::{AudioClock, EventSink, MicSink, ScreenParams, SharedChannel, SharedScreen};
use crate::config::Config;
use crate::sink::{AudioCodec, ClOutlet, ClSink, MediaSink};

#[tokio::main]
async fn main() -> Result<()> {
    // Everything the program prints goes through this, so `CP2CL_LOG_LEVEL` is the
    // only knob for how much of it is shown, and the levels are the usual ones:
    // `error` is something we set out to do and could not, `warn` is something
    // dropped or unexpected that we carried on from, `info` is the life of a
    // session — connecting, pairing, streams opening and closing — and `debug`
    // is the per-event detail that would otherwise drown all of it.
    // It is installed before the config is read on purpose: a setting that
    // cannot be used is reported by a log line, and that line needs somewhere
    // to go.
    //
    // It goes to two places, and it is the same lines in both: the terminal,
    // coloured, and a copy under `/var/log` that is not, which is what the status
    // page reads and what is still there of a run that has ended. See
    // `logging::Journal` for how the copy is kept to size.
    //
    // The terminal is held to `CP2CL_LOG_LEVEL`, the copy is not: a run cannot
    // be dialled up to `debug` after the fact, so what is left on disk is what
    // a question about that run is answered from, and it is asked once the run
    // is over. `trace` is below both of them — nothing writes it, and what did
    // would be the internals of other people's crates.
    let journal = logging::Journal::open();
    let unnoticed = journal.reason().map(str::to_owned);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(logging::Line)
                .with_filter(config::log_filter()),
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
        tracing::warn!("logging.copy_unavailable path={} err={reason}", logging::PATH);
    }

    let cfg = Arc::new(Config::from_env());

    tracing::info!("process.started name=cp2cl");
    // Which build this run is, said once and at the top: every question about a
    // log begins with "which build was that", and the answer is not in the lines
    // that follow. It is the same string the page shows, from the same place.
    tracing::info!("process.version software={:?}", web::device::SOFTWARE);

    // The settings this run was handed, because "why is nothing happening" is
    // nearly always one of them. The two halves of the program log different
    // things — a wired run never says `wireless.*`, and a wireless run waits for
    // the car before it says anything at all — so a log that does not name its
    // own mode is one that cannot be read.
    tracing::info!(
        "process.config wireless={} wifi_iface={} bt_adapter={} ppi={}",
        cfg.wireless,
        cfg.wifi_iface,
        cfg.bt_adapter,
        cfg.ppi
    );

    // Everything the page shows is reported from wherever it is learned, and
    // nothing reads it back: see `web::status`. Started before anything else so
    // that the first thing a page says is never "nothing yet" for want of a
    // place to put it.
    let status = Arc::new(web::Status::new());
    status.with(|s| {
        s.carplay.wireless = cfg.wireless;
        s.carplay.name = cfg.name.clone();
        s.carplay.bt_mac = cfg.bt_mac.clone();
    });

    // Bound before the streams are: a page that cannot be reached is worth
    // saying so about at startup rather than when someone goes looking for it.
    // Not fatal, though — the page is the one part of this that is not the
    // bridge, and 80 is a port a machine not running as root cannot have.
    match TcpListener::bind(web::ADDR).await {
        Ok(listener) => {
            tracing::info!("web.listening url=http://{}/", web::ADDR);
            tokio::spawn(web::serve(listener, status.clone()));
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
    let airplay_port = listener.local_addr()?.port() as u32;
    status.with(|s| s.carplay.airplay_port = Some(airplay_port));

    let codec = AudioCodec::default();

    // What the phone is asked to render: the CarLife session writes it once a
    // head unit has said what its screen is, and `GET /info` is answered from it.
    // Until then nothing has, and the default stands.
    let screen: SharedScreen = Arc::new(std::sync::Mutex::new(None));

    // Which channel the head unit on the other end is, once it has said. `GET
    // /info` is answered from it, so the phone is shown the icon and the label
    // that go with the car it is plugged into.
    let channel: SharedChannel = Arc::new(std::sync::Mutex::new(None));

    // Fired when the phone's icon for us is tapped. A broadcast rather than a
    // queue: the car can be plugged out and back in, and whoever is listening
    // at the time is the one that matters.
    let (desktop, _) = tokio::sync::broadcast::channel(1);

    // Input goes out through here whatever produced it. Built now and filled in
    // later: the keys only exist once pair-verify has run.
    let event_sink = EventSink::new();

    // What a running session is told when what to render changes. `/info` is
    // answered once per session and never asked again, so a new size only lands
    // on the next one: the session is dropped and built again. One slot, because
    // only the newest signal is worth anything — a session acting on a change
    // that has already been superseded is worse than not acting at all.
    let (restart, _) = tokio::sync::broadcast::channel(1);

    // And what a running session is told when the CarLife session it feeds has
    // ended, which is the one thing a restart cannot say: there is nothing to
    // restart for. See `airplay::serve`.
    let (shutdown, _) = tokio::sync::broadcast::channel(1);

    let mic = MicSink::new();

    // The car's position goes to the phone through this, and only through this:
    // the CarLife session is where a fix arrives and the iAP2 session is what
    // hands it on, so both halves hold the same sending half.
    let vehicle = Arc::new(Vehicle::default());

    // The car is the consumer, and this is the queue between the two halves:
    // access units and the phone's sound arrive from CarPlay and wait here for a
    // CarLife session to take them out, on the video channel and the media one.
    let outlet = Arc::new(ClOutlet::new());

    // One object per stream, not two: a video event and an audio event differ
    // only in which channel the consumer converts them to. There are two of them
    // because there are two streams that can be told apart this early — the
    // screen and the sound — and the CarLife sink is one object because both of
    // them are the same car on the same connection. What the phone is playing,
    // and where it is going, go through the same object, which is why it is kept
    // whole rather than erased into the two traits above.
    let (video_sink, audio_sink, panel): (Arc<dyn MediaSink>, Arc<dyn MediaSink>, Arc<ClSink>) = {
        let sink = outlet.sink();
        (sink.clone(), sink.clone(), sink)
    };

    // Worth saying out loud: nothing here draws a picture, and what the phone is
    // asked to render is the car's own screen — so nothing is rendered until a
    // car has said what that screen is.
    tracing::info!("bridge.ready sink=carlife screen_source=car");

    let pairings = Arc::new(Mutex::new(Pairings::load_or_create(None)?));

    // Built before the receiver starts because both halves need it: iAP2 to
    // answer the phone's challenge, and airplay to answer /auth-setup.
    let auth = SharedCoprocessor::new(mfi_signer()?);

    // A wireless phone moves its iAP2 control channel onto the AirPlay session
    // once CarPlay is running, and these are the two pieces that takes: the gate
    // that stops Bluetooth being paged while it has — see `wireless::Handover` —
    // and where the session on the tunnel is described from. A wired run has
    // neither: its control channel is on the cable and there is nothing to move
    // it to, so a type 130 SETUP is answered as it always was.
    let handover = wireless::Handover::new();
    let tunnel = cfg.wireless.then(|| {
        Arc::new(airplay::tunnel::Setup {
            cfg: cfg.clone(),
            auth: auth.clone(),
            bt_mac: wireless::parse_mac(&cfg.bt_mac),
            airplay_port,
            panel: panel.clone(),
            vehicle: vehicle.clone(),
            status: status.clone(),
            events: event_sink.clone(),
            handover: handover.clone(),
            restart: restart.clone(),
        })
    });

    // Whether the CarPlay half is open for the phone at all. It starts shut and
    // the car's session is the only thing that opens it: nothing is published
    // and nothing is accepted until there is a head unit on the other end.
    let (carplay, gate) = tokio::sync::watch::channel(false);
    tracing::debug!(
        "bridge.carplay_side state={}",
        if *carplay.borrow() { "open" } else { "shut" }
    );

    // Bound either way, and bound now: this port is what the phone is told, so
    // it has to be known before the car turns up and before the watcher below
    // has anything to advertise. Whether anything is accepted on it is the
    // gate's business.
    tracing::info!(
        "airplay.receiver port={airplay_port} state={}",
        if *carplay.borrow() {
            "listening"
        } else {
            "held"
        }
    );
    tokio::spawn(airplay::serve(
        listener,
        airplay::Shared {
            cfg: cfg.clone(),
            pairings,
            video_sink,
            audio_sink,
            audio_codec: Arc::new(Mutex::new(codec)),
            screen: screen.clone(),
            channel: channel.clone(),
            desktop: desktop.clone(),
            restart: restart.clone(),
            shutdown,
            event_sink: event_sink.clone(),
            mic,
            mfi: auth.clone(),
            audio_clock: Arc::new(Mutex::new(AudioClock::default())),
            timing: Arc::new(Mutex::new(SyncedClock::new())),
            status: status.clone(),
            tunnel,
        },
        gate.clone(),
    ));

    // The car comes first and CarPlay follows it: nothing is published and
    // nothing is accepted until there is a head unit on the other end of the
    // bridge, and both go away again when it does.
    let opening = carplay.clone();
    let nudge = restart.clone();
    let mut report = move |up: bool| {
        let _ = opening.send(up);
        if up {
            // The phone will not start CarPlay on its own — it has to be told
            // again — and a fresh iAP2 session is how it is told.
            let _ = nudge.send(());
        }
    };
    // A file the operator names is the whole source then, and nothing the phone
    // sends reaches the car's decoder — which is how the channel and that
    // decoder are exercised without one.
    let frames = match &cfg.input_file {
        Some(path) => carlife::session::Frames::from_file(path),
        None => carlife::session::Frames::Live(outlet),
    };

    // The car names its screen over the CarLife session, and the name is what the
    // phone is asked to render at, so this is where the two halves meet. A size
    // that lands after the phone has already read `/info` is worth a session of
    // its own — that is what `restart` is for — and a size that changes nothing
    // is not worth saying twice.
    let mut named = {
        let slot = screen.clone();
        let nudge = restart.clone();
        move |width: u32, height: u32, fps: u32| {
            let wanted = ScreenParams {
                width,
                height,
                fps,
                ..ScreenParams::default()
            };
            let Ok(mut slot) = slot.lock() else {
                return;
            };
            if (*slot).unwrap_or_default() == wanted {
                return;
            }
            tracing::info!("screen.named width={width} height={height} fps={fps}");
            *slot = Some(wanted);
            drop(slot);
            let _ = nudge.send(());
        }
    };
    tracing::info!("bridge.carlife_up carplay_side=waiting");
    // In the background, because the two sides are each other's condition: this
    // task is what opens the gate, and the watcher below is the only thing that
    // acts on it. Waiting for `serve` to return here would leave nobody to
    // answer the gate it opens.
    let carried = cfg.clone();
    let alone = carplay.clone();
    // The car's own input goes the way everything else does, so the session gets
    // the same sink the CarPlay side fills in.
    let input = event_sink;
    let reported = status.clone();
    let handed = vehicle.clone();
    tokio::spawn(async move {
        if let Err(e) = carlife::serve(
            carried,
            &frames,
            &mut named,
            channel,
            input,
            &mut report,
            &desktop,
            reported,
            handed,
        )
        .await
        {
            // `{e:#}` and not `{e}`: the outer message says what we were doing
            // and the causes under it hold the errno, which is the part that
            // says why.
            tracing::error!("bridge.carlife_stopped err={e:#}");
            tracing::warn!("bridge.degraded mode=carplay_only");
            let _ = alone.send(true);
        }
    });

    if cfg.wireless {
        wireless::watch(
            cfg, auth, airplay_port, restart, gate, panel, status, vehicle, handover,
        )
        .await;
    } else {
        session::watch(cfg, auth, airplay_port, restart, gate, panel, status, vehicle).await;
    }
    Ok(())
}

/// Local credentials when they are built in, a dead end with a clear message otherwise.
///
/// Wired CarPlay still runs the full MFi challenge, so without credentials the
/// phone identifies us and then refuses to authenticate. Saying so up front
/// beats the cryptic 0xAA04 that comes back later.
fn mfi_signer() -> Result<Box<dyn AuthCoprocessor + Send>> {
    let (Some(key), Some(cert)) = (
        assets::get("authentication/identity.pk8"),
        assets::get("authentication/certificate.p7b"),
    ) else {
        tracing::warn!(
            "mfi.credentials_missing consequence=phone_rejects \
             want=\"identity.pk8 + certificate.p7b in assets/authentication\""
        );
        return Ok(Box::new(NoCoprocessor));
    };
    let signer = LocalCoprocessor::load(key, cert)
        .context("the built-in MFi credentials could not be used")?;
    tracing::info!(
        "mfi.credentials_loaded key_bytes={} cert_bytes={}",
        key.len(),
        cert.len()
    );
    Ok(Box::new(signer))
}
