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
mod carlife;
mod commands;
mod config;
mod session;
mod sink;
mod web;
mod wireless;

use std::sync::Arc;

use anyhow::{Context, Result};
use iap2_mfi::local::LocalCoprocessor;
use iap2_mfi::{AuthCoprocessor, NoCoprocessor};
use livi_runtime::mfi_async::SharedCoprocessor;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::airplay::clock::SyncedClock;
use crate::airplay::pairing::Pairings;
use crate::airplay::{AudioClock, EventSink, MicSink, ScreenParams, SharedScreen};
use crate::config::Config;
use crate::sink::{AudioCodec, ClOutlet, MediaSink};

#[tokio::main]
async fn main() -> Result<()> {
    // Everything the program prints goes through this, so `CP2CL_LOG` is the
    // only knob for how much of it is shown, and the levels are the usual ones:
    // `error` is something we set out to do and could not, `warn` is something
    // dropped or unexpected that we carried on from, `info` is the life of a
    // session — connecting, pairing, streams opening and closing — and `debug`
    // is the per-event detail that would otherwise drown all of it.
    // It is installed before the config is read on purpose: a setting that
    // cannot be used is reported by a log line, and that line needs somewhere
    // to go.
    tracing_subscriber::fmt()
        .with_env_filter(config::log_filter())
        .with_target(false)
        .init();

    let cfg = Arc::new(Config::from_env());

    tracing::info!("cp2cl — headless CarPlay receiver");

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
            tracing::info!("[web] the status page is on http://{}/", web::ADDR);
            tokio::spawn(web::serve(listener, status.clone()));
        }
        Err(e) => tracing::warn!("[web] no status page on {}: {e:#}", web::ADDR),
    }

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

    // The car is the consumer, and this is the queue between the two halves:
    // access units and the phone's sound arrive from CarPlay and wait here for a
    // CarLife session to take them out, on the video channel and the media one.
    let outlet = Arc::new(ClOutlet::new());

    // One object per stream, not two: a video event and an audio event differ
    // only in which channel the consumer converts them to. There are two of them
    // because there are two streams that can be told apart this early — the
    // screen and the sound — and the CarLife sink is one object because both of
    // them are the same car on the same connection.
    let (video_sink, audio_sink): (Arc<dyn MediaSink>, Arc<dyn MediaSink>) = {
        let sink = outlet.sink();
        (sink.clone(), sink)
    };

    // Worth saying out loud: nothing here draws a picture, and what the phone is
    // asked to render is the car's own screen — so nothing is rendered until a
    // car has said what that screen is.
    tracing::info!(
        "[cp2cl] the streams go to the car; the phone is asked to render at whatever \
         screen the car says it has"
    );

    let pairings = Arc::new(Mutex::new(Pairings::load_or_create(None)?));

    // Built before the receiver starts because both halves need it: iAP2 to
    // answer the phone's challenge, and airplay to answer /auth-setup.
    let auth = SharedCoprocessor::new(mfi_signer(&cfg)?);

    // Whether the CarPlay half is open for the phone at all. It starts shut and
    // the car's session is the only thing that opens it: nothing is published
    // and nothing is accepted until there is a head unit on the other end.
    let (carplay, gate) = tokio::sync::watch::channel(false);
    tracing::debug!(
        "[cp2cl] the CarPlay side starts {}",
        if *carplay.borrow() { "open" } else { "shut" }
    );

    // Bound either way, and bound now: this port is what the phone is told, so
    // it has to be known before the car turns up and before the watcher below
    // has anything to advertise. Whether anything is accepted on it is the
    // gate's business.
    tracing::info!(
        "[cp2cl] airplay receiver on {airplay_port}, {}",
        if *carplay.borrow() {
            "listening"
        } else {
            "held until the car's session is up"
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
            restart: restart.clone(),
            shutdown,
            event_sink: event_sink.clone(),
            mic,
            mfi: auth.clone(),
            audio_clock: Arc::new(Mutex::new(AudioClock::default())),
            timing: Arc::new(Mutex::new(SyncedClock::new())),
            status: status.clone(),
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
            tracing::info!(
                "[cp2cl] the car's screen is {width}x{height} at {fps} fps; \
                 the phone is asked for the same"
            );
            *slot = Some(wanted);
            drop(slot);
            let _ = nudge.send(());
        }
    };
    tracing::info!("[cp2cl] CarLife is on; the CarPlay side waits for the car");
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
    tokio::spawn(async move {
        if let Err(e) =
            carlife::serve(carried, &frames, &mut named, input, &mut report, reported).await
        {
            // `{e:#}` and not `{e}`: the outer message says what we were doing
            // and the causes under it hold the errno, which is the part that
            // says why.
            tracing::error!("[cp2cl] CarLife stopped: {e:#}");
            tracing::warn!("[cp2cl] carrying on with the CarPlay side on its own");
            let _ = alone.send(true);
        }
    });

    if cfg.wireless {
        wireless::watch(cfg, auth, airplay_port, restart, gate, status).await;
    } else {
        session::watch(cfg, auth, airplay_port, restart, gate, status).await;
    }
    Ok(())
}

/// Local credentials when they exist, a dead end with a clear message otherwise.
///
/// Wired CarPlay still runs the full MFi challenge, so without credentials the
/// phone identifies us and then refuses to authenticate. Saying so up front
/// beats the cryptic 0xAA04 that comes back later.
fn mfi_signer(cfg: &Config) -> Result<Box<dyn AuthCoprocessor + Send>> {
    match &cfg.auth_dir {
        Some(dir) => match LocalCoprocessor::load(dir) {
            Ok(signer) => {
                tracing::info!(
                    "[cp2cl] local MFi credentials loaded from {}",
                    dir.display()
                );
                Ok(Box::new(signer))
            }
            Err(e) => Err(anyhow::anyhow!(
                "credentials in {} could not be used: {e}",
                dir.display()
            )),
        },
        None => {
            tracing::warn!(
                "[cp2cl] no MFi credentials found; the phone will reject us at \
                 authentication. Put identity.pk8 + certificate.p7b in ./authentication \
                 or set CP2CL_AUTH_DIR."
            );
            Ok(Box::new(NoCoprocessor))
        }
    }
}
