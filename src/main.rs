//! `cp2cl` — a headless CarPlay receiver that hands what the phone sends to
//! whoever is watching.
//!
//! Nothing here draws anything itself. Three pieces run concurrently:
//!
//! * a session watcher that carries iAP2 over USB — or over Bluetooth, when
//!   `CP2CL_WIRELESS` is set — and starts CarPlay,
//! * an airplay receiver that accepts the phone's return call and decrypts its
//!   media,
//! * one consumer of those streams, which is either the browser client's server,
//!   fanning them out over one WebSocket and taking input back, or the CarLife
//!   session feeding a head unit — never both.

mod airplay;
// The protocol half of CarLife is plain code and its tests run anywhere, but
// presenting a USB device is something only Linux does, so off Linux nothing
// reaches into the module and most of it would be reported as unused.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
use crate::airplay::{AudioClock, EventSink, MicSink, SharedScreen};
use crate::config::{Config, Sink};
// Only the CarLife half reads this, and only Linux has one.
#[cfg(target_os = "linux")]
use crate::config::Input;
use crate::sink::{AudioCodec, ClOutlet, MediaSink};

#[tokio::main]
async fn main() -> Result<()> {
    // rustls will not guess which crypto provider to use, and `axum-server` is
    // built without one of its own on purpose — see Cargo.toml — so the process
    // has to say. Do it before anything builds a TLS config.
    let _ = rustls::crypto::ring::default_provider().install_default();

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

    // Ask for a free port rather than picking one: whatever we get is what gets
    // advertised to the phone, so there is no way for the two to disagree.
    let listener = TcpListener::bind(("::", 0))
        .await
        .context("bind airplay listener")?;
    let airplay_port = listener.local_addr()?.port() as u32;

    let codec = AudioCodec::default();

    // What the phone is asked to render. Written by the browser's display panel
    // and read by `GET /info`, which is why one slot serves both.
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

    // Who reads the streams. One or the other, never both: the media is the
    // same either way and only the destination differs, so the browser client
    // and the head unit are not built together.
    let outlet: Option<Arc<ClOutlet>> = (cfg.sink == Sink::Cl).then(|| Arc::new(ClOutlet::new()));
    let hub = (cfg.sink == Sink::Web).then(|| {
        web::Hub::new(
            &cfg,
            screen.clone(),
            event_sink.clone(),
            mic.clone(),
            restart.clone(),
        )
    });

    // One object per stream, not two: a video event and an audio event differ
    // only in what the consumer converts them to. The CarLife sink is one object
    // for the same reason — it takes both and has a channel for the first only.
    let (video_sink, audio_sink): (Arc<dyn MediaSink>, Arc<dyn MediaSink>) =
        match (&outlet, &hub) {
            (Some(outlet), _) => {
                let sink = outlet.video();
                (sink.clone(), sink)
            }
            (None, Some(hub)) => (hub.clone(), hub.clone()),
            (None, None) => unreachable!("the sink is one of the two"),
        };

    match &hub {
        Some(hub) => {
            let serving = hub.clone();
            let address = cfg.web_addr;
            let token = cfg.web_token.clone();
            tokio::spawn(async move {
                if let Err(e) = web::serve(serving, address, token).await {
                    tracing::error!("[web] {e}");
                }
            });
        }
        // Worth saying out loud: with no browser client there is no page to open
        // and no display panel, so what the phone is asked to render comes from
        // the environment alone.
        None => tracing::info!(
            "[cp2cl] no browser client; the streams go to the car, and the screen \
             comes from CP2CL_WIDTH/HEIGHT/FPS"
        ),
    }

    let pairings = Arc::new(Mutex::new(Pairings::load_or_create(None)?));

    // Built before the receiver starts because both halves need it: iAP2 to
    // answer the phone's challenge, and airplay to answer /auth-setup.
    let auth = SharedCoprocessor::new(mfi_signer(&cfg)?);

    // Whether the CarPlay half is open for the phone at all. With the head unit
    // reading the streams it starts shut, and the car's session is the only thing
    // that opens it; with the browser client there is nothing to wait for, so it
    // starts open and nothing touches it again.
    let (carplay, gate) = tokio::sync::watch::channel(cfg.sink == Sink::Web);
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
            screen,
            restart: restart.clone(),
            shutdown,
            event_sink,
            mic,
            mfi: auth.clone(),
            audio_clock: Arc::new(Mutex::new(AudioClock::default())),
            timing: Arc::new(Mutex::new(SyncedClock::new())),
        },
        gate.clone(),
    ));

    // With the car reading the streams it comes first and CarPlay follows it:
    // nothing is published and nothing is accepted until there is a head unit on
    // the other end of the bridge, and both go away again when it does. The
    // outlet is what says the car is the reader, so it is also what says to start
    // the CarLife half.
    #[cfg(target_os = "linux")]
    if let Some(outlet) = outlet.clone() {
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
        let frames = match cfg.input {
            Input::Cp => carlife::session::Frames::Live(outlet),
            Input::File => carlife::session::Frames::from_file(),
        };
        tracing::info!("[cp2cl] CarLife is on; the CarPlay side waits for the car");
        // In the background, because the two sides are each other's condition:
        // this task is what opens the gate, and the watcher below is the only
        // thing that acts on it. Waiting for `serve` to return here would leave
        // nobody to answer the gate it opens.
        let carried = cfg.clone();
        let alone = carplay.clone();
        tokio::spawn(async move {
            if let Err(e) = carlife::serve(carried, &frames, &mut report).await {
                tracing::error!("[cp2cl] CarLife stopped: {e}");
                tracing::warn!("[cp2cl] carrying on with the CarPlay side on its own");
                let _ = alone.send(true);
            }
        });
    }

    if cfg.wireless {
        tracing::info!(
            "[cp2cl] wireless: the phone will be told to join {:?} and reach us on \
             {} port {airplay_port}",
            cfg.ssid, cfg.wifi_iface
        );
        wireless::watch(cfg, auth, airplay_port, restart, gate).await;
    } else {
        session::watch(cfg, auth, airplay_port, restart, gate).await;
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
