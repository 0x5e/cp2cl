//! `cp2aa` — a headless CarPlay receiver that hands what the phone sends to
//! whoever is watching.
//!
//! Nothing here draws anything itself. Four pieces run concurrently:
//!
//! * a session watcher that carries iAP2 over USB — or over Bluetooth, when
//!   `CP2AA_WIRELESS` is set — and starts CarPlay,
//! * an airplay receiver that accepts the phone's return call and decrypts its
//!   media,
//! * the browser client's server, which fans those streams out over one
//!   WebSocket and takes input back — unless `CP2AA_SINK=aa`, in which case the
//!   bridge below is the consumer instead.

mod airplay;
mod bridge;
mod commands;
mod config;
mod session;
mod sink;
mod web;
mod wireless;

// On disk this is `src/aa-client/`, which is not a legal module name, so the
// path is spelled out. It is the Android Auto *phone* side: not wired to the
// CarPlay receiver yet, but it lives in the same binary on purpose — the
// bridge is the point of this program.
#[path = "aa-client/mod.rs"]
mod aa_client;

use std::sync::Arc;

use anyhow::{Context, Result};
use iap2_mfi::local::LocalCoprocessor;
use iap2_mfi::{AuthCoprocessor, NoCoprocessor};
use livi_runtime::mfi_async::SharedCoprocessor;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::aa_client::tls::PhoneIdentity;
use crate::airplay::clock::SyncedClock;
use crate::airplay::pairing::Pairings;
use crate::airplay::{AudioClock, EventSink, MicSink, SharedScreen};
use crate::bridge::Bridge;
use crate::config::{Config, Sink};
use crate::sink::{AaOutlet, AudioCodec, MediaSink};

#[tokio::main]
async fn main() -> Result<()> {
    // rustls will not guess between two crypto providers, and more than one can
    // be in the tree. There is only one here — `ring`, which the Android Auto
    // half needs for its TLS 1.2 server — so name it before anything builds a
    // TLS config. `axum-server` is built without a provider of its own for
    // exactly this reason.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cfg = Arc::new(Config::from_env());
    tracing_subscriber::fmt()
        .with_env_filter(&cfg.log_filter)
        .with_target(false)
        .init();

    println!("cp2aa — headless CarPlay receiver");

    // Ask for a free port rather than picking one: whatever we get is what gets
    // advertised to the phone, so there is no way for the two to disagree.
    let listener = TcpListener::bind(("::", 0))
        .await
        .context("bind airplay listener")?;
    let airplay_port = listener.local_addr()?.port() as u32;
    println!("[cp2aa] airplay receiver listening on {airplay_port}");

    let codec = AudioCodec::default();

    // What the phone is asked to render. Written by the browser's display panel
    // — or, later, by a head unit saying what it can show — and read by
    // `GET /info`, which is why one slot serves both.
    let screen: SharedScreen = Arc::new(std::sync::Mutex::new(None));

    // Input goes out through here whatever produced it. Built now and filled in
    // later: the keys only exist once pair-verify has run.
    let event_sink = EventSink::new();

    // Fired when what the phone should render changes, which is the only way a
    // display change lands — see `airplay::server::Shared::restart`.
    let (restart, _) = tokio::sync::broadcast::channel(1);

    // Two ways out of here: the browser client on this machine, or a head unit
    // that thinks we are an Android phone. The bytes are identical either way,
    // so nothing downstream knows which one it got.
    let outlet: Option<Arc<AaOutlet>> = (cfg.sink == Sink::Aa).then(|| Arc::new(AaOutlet::new()));
    let mic = MicSink::new();
    let hub: Option<Arc<web::Hub>> = (cfg.sink == Sink::Web).then(|| {
        web::Hub::new(
            &cfg,
            screen.clone(),
            event_sink.clone(),
            mic.clone(),
            restart.clone(),
        )
    });

    let (video_sink, audio_sink): (Arc<dyn MediaSink>, Arc<dyn MediaSink>) = match &outlet {
        Some(outlet) => {
            println!(
                "[cp2aa] CP2AA_SINK=aa: streams go to Android Auto instead of the browser"
            );
            (outlet.video(), outlet.audio())
        }
        // One object, not two: a video event and an audio event differ only in
        // what the hub converts them to.
        None => {
            let hub = hub.clone().expect("no sink was built");
            (hub.clone(), hub)
        }
    };

    if let Some(hub) = &hub {
        let hub = hub.clone();
        let port = cfg.web_port;
        tokio::spawn(async move {
            if let Err(e) = web::serve(hub, port).await {
                eprintln!("[web] {e}");
            }
        });
    }

    let pairings = Arc::new(Mutex::new(Pairings::load_or_create(None)?));

    // Started before the iPhone is expected, because it is what tells the iPhone
    // what to render: a head unit has to be heard from first for its resolution
    // to reach `/info`.
    if let Some(outlet) = &outlet {
        let identity = Arc::new(PhoneIdentity::load_default().with_context(|| {
            "no Android Auto phone certificate; put android_auto.crt + \
             android_auto.key in ./aa, or set CP2AA_AA_CERT_DIR"
        })?);
        let bridge = Arc::new(Bridge::new(
            cfg.clone(),
            identity,
            outlet.clone(),
            screen.clone(),
            event_sink.clone(),
        ));
        tokio::spawn(async move {
            if let Err(e) = bridge.run().await {
                eprintln!("[aa] transport stopped: {e}");
            }
        });
    }

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
            screen: screen.clone(),
            restart: restart.clone(),
            event_sink: event_sink.clone(),
            mic,
            mfi: auth.clone(),
            audio_clock: Arc::new(Mutex::new(AudioClock::default())),
            timing: Arc::new(Mutex::new(SyncedClock::new())),
        },
    ));

    if cfg.wireless {
        println!(
            "[cp2aa] wireless: the phone will be told to join {:?} and reach us on \
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
                println!(
                    "[cp2aa] local MFi credentials loaded from {}",
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
            eprintln!(
                "[cp2aa] no MFi credentials found; the phone will reject us at \
                 authentication. Put identity.pk8 + certificate.p7b in ./authentication \
                 or set CP2AA_AUTH_DIR."
            );
            Ok(Box::new(NoCoprocessor))
        }
    }
}
