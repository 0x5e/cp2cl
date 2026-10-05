//! `cp2aa` — a headless CarPlay receiver that plays what the phone sends.
//!
//! Nothing here draws anything itself. Three pieces run concurrently:
//!
//! * a session watcher that carries iAP2 over USB — or over Bluetooth, when
//!   `CP2AA_WIRELESS` is set — and starts CarPlay,
//! * an airplay receiver that accepts the phone's return call and decrypts its
//!   media into two ffplay processes,
//! * a stdin reader turning keyboard lines into HID reports.

mod airplay;
mod bridge;
mod commands;
mod config;
mod session;
mod sink;
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
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::aa_client::tls::PhoneIdentity;
use crate::airplay::clock::SyncedClock;
use crate::airplay::pairing::Pairings;
use crate::airplay::{AudioClock, EventSink};
use crate::bridge::Bridge;
use crate::config::{Config, Sink};
use crate::sink::{AaOutlet, AudioCodec, MediaSink, Pipelines, PlayerSink};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Arc::new(Config::from_env());
    tracing_subscriber::fmt()
        .with_env_filter(&cfg.log_filter)
        .with_target(false)
        .init();

    println!("cp2aa — headless CarPlay to ffplay");

    // Ask for a free port rather than picking one: whatever we get is what gets
    // advertised to the phone, so there is no way for the two to disagree.
    let listener = TcpListener::bind(("::", 0))
        .await
        .context("bind airplay listener")?;
    let airplay_port = listener.local_addr()?.port() as u32;
    println!("[cp2aa] airplay receiver listening on {airplay_port}");

    let codec = AudioCodec::default();

    // Two ways out of here: played on this machine, or sent to a head unit
    // that thinks we are an Android phone. The bytes are identical either way,
    // so nothing downstream knows which one it got.
    let outlet: Option<Arc<AaOutlet>> = (cfg.sink == Sink::Aa).then(|| Arc::new(AaOutlet::new()));
    let (video_sink, audio_sink): (Arc<dyn MediaSink>, Arc<dyn MediaSink>) = match &outlet {
        None => {
            let pipelines = Pipelines::build(codec, cfg.fps, cfg.window, cfg.hevc);
            println!("[cp2aa] playing with {}", cfg.player.program.display());
            println!(
                "[cp2aa] {}x{} as {} ({}) in a {}x{} window",
                cfg.width,
                cfg.height,
                if cfg.hevc { "HEVC" } else { "H.264" },
                if cfg.width_mm > 0 {
                    format!("{}mm wide", cfg.width_mm)
                } else {
                    "default 200mm wide".into()
                },
                cfg.window.0,
                cfg.window.1
            );
            // Independent on purpose: the phone may open the screen stream long
            // before (or without) the audio one, and neither may hold the other
            // back.
            (
                Arc::new(
                    PlayerSink::spawn(&cfg.player.program, &pipelines.video, None, "video")
                        .with_context(|| format!("start video player ({:?})", pipelines.video))?,
                ),
                Arc::new(
                    PlayerSink::spawn(
                        &cfg.player.program,
                        &pipelines.audio,
                        pipelines.audio_header.clone(),
                        "audio",
                    )
                    .with_context(|| format!("start audio player ({:?})", pipelines.audio))?,
                ),
            )
        }
        Some(outlet) => {
            println!(
                "[cp2aa] CP2AA_SINK=aa: streams go to Android Auto instead of being played here"
            );
            (outlet.video(), outlet.audio())
        }
    };

    let pairings = Arc::new(Mutex::new(Pairings::load_or_create(None)?));
    let event_sink = EventSink::new();
    // What the head unit asks for, once it asks. Read by `GET /info`, written by
    // the bridge.
    let screen = Arc::new(std::sync::Mutex::new(None));

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
            event_sink: event_sink.clone(),
            mfi: auth.clone(),
            audio_clock: Arc::new(Mutex::new(AudioClock::default())),
            timing: Arc::new(Mutex::new(SyncedClock::new())),
        },
    ));

    tokio::spawn(stdin_loop(event_sink.clone()));

    if cfg.wireless {
        println!(
            "[cp2aa] wireless: the phone will be told to join {:?} and reach us on \
             {} port {airplay_port}",
            cfg.ssid, cfg.wifi_iface
        );
        wireless::watch(cfg, auth, airplay_port).await;
    } else {
        session::watch(cfg, auth, airplay_port).await;
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

/// Turns typed lines into HID reports. Available even before the phone connects,
/// with the presses going nowhere until then.
async fn stdin_loop(sink: Arc<EventSink>) {
    println!("{}", commands::HELP);
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let Ok(line) = lines.next_line().await else {
            break;
        };
        let Some(line) = line else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match commands::parse(line) {
            Some(commands::Command::Quit) => {
                println!("bye");
                std::process::exit(0);
            }
            Some(cmd) => {
                if let Err(e) = sink.dispatch(cmd).await {
                    eprintln!("[input] {e}");
                }
            }
            None => println!("unknown command: {line}\n{}", commands::HELP),
        }
    }
}
