//! `cp2cl` — a headless CarPlay receiver that hands what the phone sends to
//! whoever is watching.
//!
//! Nothing here draws anything itself. Three pieces run concurrently:
//!
//! * a session watcher that carries iAP2 over USB — or over Bluetooth, when
//!   `CP2CL_WIRELESS` is set — and starts CarPlay,
//! * an airplay receiver that accepts the phone's return call and decrypts its
//!   media,
//! * the browser client's server, which fans those streams out over one
//!   WebSocket and takes input back.

mod airplay;
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
use crate::config::Config;
use crate::sink::{AudioCodec, MediaSink};

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
    tracing::info!("[cp2cl] airplay receiver listening on {airplay_port}");

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

    let mic = MicSink::new();
    let hub = web::Hub::new(
        &cfg,
        screen.clone(),
        event_sink.clone(),
        mic.clone(),
        restart.clone(),
    );

    // One object, not two: a video event and an audio event differ only in what
    // the hub converts them to.
    let video_sink: Arc<dyn MediaSink> = hub.clone();
    let audio_sink: Arc<dyn MediaSink> = hub.clone();

    let serving = hub.clone();
    let address = cfg.web_addr;
    let token = cfg.web_token.clone();
    tokio::spawn(async move {
        if let Err(e) = web::serve(serving, address, token).await {
            tracing::error!("[web] {e}");
        }
    });

    let pairings = Arc::new(Mutex::new(Pairings::load_or_create(None)?));

    // Built before the receiver starts because both halves need it: iAP2 to
    // answer the phone's challenge, and airplay to answer /auth-setup.
    let auth = SharedCoprocessor::new(mfi_signer(&cfg)?);

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
            event_sink,
            mic,
            mfi: auth.clone(),
            audio_clock: Arc::new(Mutex::new(AudioClock::default())),
            timing: Arc::new(Mutex::new(SyncedClock::new())),
        },
    ));

    if cfg.wireless {
        tracing::info!(
            "[cp2cl] wireless: the phone will be told to join {:?} and reach us on \
             {} port {airplay_port}",
            cfg.ssid, cfg.wifi_iface
        );
        wireless::watch(cfg, auth, airplay_port, restart).await;
    } else {
        session::watch(cfg, auth, airplay_port, restart).await;
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
