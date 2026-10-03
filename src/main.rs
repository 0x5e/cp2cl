//! `cp2aa` — a headless CarPlay receiver that plays what the phone sends.
//!
//! Nothing here draws anything itself. Three pieces run concurrently:
//!
//! * a wired session watcher that carries iAP2 over USB and starts CarPlay,
//! * an airplay receiver that accepts the phone's return call and decrypts its
//!   media into two ffplay processes,
//! * a stdin reader turning keyboard lines into HID reports.

mod airplay;
mod commands;
mod config;
mod session;
mod sink;

use std::sync::Arc;

use anyhow::{Context, Result};
use iap2_mfi::{AuthCoprocessor, local::LocalCoprocessor, NoCoprocessor};
use livi_runtime::mfi_async::SharedCoprocessor;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::airplay::pairing::Pairings;
use crate::airplay::{AudioClock, EventSink};
use crate::config::Config;
use crate::sink::{AudioCodec, Pipelines, PlayerSink};

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
    let listener = TcpListener::bind(("::", 0)).await.context("bind airplay listener")?;
    let airplay_port = listener.local_addr()?.port() as u32;
    println!("[cp2aa] airplay receiver listening on {airplay_port}");

    let codec = AudioCodec::default();
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

    // Independent on purpose: the phone may open the screen stream long before
    // (or without) the audio one, and neither may hold the other back.
    let video_sink = Arc::new(
        PlayerSink::spawn(&cfg.player.program, &pipelines.video, None, "video")
            .with_context(|| format!("start video player ({:?})", pipelines.video))?,
    );
    let audio_sink = Arc::new(
        PlayerSink::spawn(
            &cfg.player.program,
            &pipelines.audio,
            pipelines.audio_header.clone(),
            "audio",
        )
        .with_context(|| format!("start audio player ({:?})", pipelines.audio))?,
    );

    let pairings = Arc::new(Mutex::new(Pairings::load_or_create(None)?));
    let event_sink = EventSink::new();

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
            event_sink: event_sink.clone(),
            mfi: auth.clone(),
            audio_clock: Arc::new(Mutex::new(AudioClock::default())),
        },
    ));

    tokio::spawn(stdin_loop(event_sink.clone()));

    session::watch(cfg, auth, airplay_port).await;
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
                println!("[cp2aa] local MFi credentials loaded from {}", dir.display());
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
