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

mod ap;
mod artwork;
mod assets;
mod button;
mod config;
mod guide;
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
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::{mpsc, watch, Notify};
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
    // over. `trace` is below both of them, and a dependency that writes one
    // DEBUG line per USB packet is held to its warnings — see `logging::filter`.
    let journal = logging::Journal::open();
    let unnoticed = journal.reason().map(str::to_owned);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(logging::Line)
                .with_filter(logging::filter(settings::log_filter())),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(logging::Line)
                .with_ansi(false)
                .with_writer(journal)
                .with_filter(logging::filter(tracing::level_filters::LevelFilter::DEBUG)),
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

    // The settings this run was handed. Which link a phone arrives on is not
    // one of them: both wait at the same time, and `carplay_hu::admission` says
    // which of them won.
    tracing::info!(
        "main.config wifi_iface={} bt_adapter={} log_level={}",
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
    // (`panic = "abort"`) and a signal; the first is the hook below, and the
    // second is handled where the signals are taken, at the end of `main`.
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
    // Kept so that a stop can end it before the lights are put back: a state
    // painted after that would be the one left on the board, and a stop is not
    // the moment to find out which of the two wrote last.
    let painting = tokio::spawn(led::run(facts, lights.clone()));
    tokio::spawn(button::watch(cfg.bt_adapter.clone(), replug.clone()));

    // Everything the page shows is reported from wherever it is learned, and
    // nothing reads it back: see `web::status`. Started before anything else so
    // that the first thing a page says is never "nothing yet" for want of a
    // place to put it. The phone's half writes its own report, which is why the
    // page is built around it rather than the other way round.
    let report = Arc::new(Report::new());
    report.with(|s| {
        s.carplay.name = hostname();
        s.carplay.bt_mac = config::bt_address(&cfg.bt_adapter);
    });
    let status = Arc::new(web::Status::new(report.clone()));

    // Bound before the streams are: a page that cannot be reached is worth
    // saying so about at startup rather than when someone goes looking for it.
    // Not fatal, though — the page is the one part of this that is not the
    // bridge, so an address it cannot have is the page's loss and not the
    // streams'.
    match TcpListener::bind(cfg.addr.as_str()).await {
        Ok(listener) => {
            tracing::info!("web.listening url=http://{}/", cfg.addr);
            tokio::spawn(web::serve(listener, status.clone(), replug));
        }
        Err(e) => tracing::warn!("web.listen_failed addr={} err={e:#}", cfg.addr),
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

    // The cover takes a detour: it is the one thing the phone sends that the
    // car's message may not have room for, and making it fit is tens of
    // milliseconds of decoding and encoding. The task that does it is here
    // rather than in either half, because the size it aims at is one end's and
    // the picture is the other's.
    let (covers, arriving) = mpsc::unbounded_channel();
    tokio::spawn(artwork::carry(arriving, outlet.sink()));

    // One object per stream, not two: a video event and an audio event differ
    // only in which channel the consumer converts them to. There are two of them
    // because there are two streams that can be told apart this early — the
    // screen and the sound — and the CarLife sink is one object because both of
    // them are the same car on the same connection. What the phone is playing,
    // and where it is going, go through the same object, which is why it is kept
    // whole rather than erased into the two traits below.
    let sink = Arc::new(ClSink::new(outlet.sink(), covers));
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

    // What the CarPlay half is told about the machine: nothing here is a choice,
    // it is what the board says about itself.
    let machine = Arc::new(carplay_hu::machine::Machine {
        wifi_iface: cfg.wifi_iface.clone(),
        bt_adapter: cfg.bt_adapter.clone(),
        name: hostname(),
        vendor: config::dt_vendor(),
        bt_address: config::bt_address(&cfg.bt_adapter),
        data_dir: data_dir.clone(),
    });

    // What the access point says about the phones on it: associated, handshake
    // done, gone. Nothing acts on it — it is what puts the phone's arrival on
    // Wi-Fi in the same timeline as everything else, and that stretch is the
    // longest one in a session's first seconds with no other line in it.
    tokio::spawn(ap::run(cfg.wifi_iface.clone()));

    // Whether the CarPlay half is open for the phone at all. It starts shut and
    // the car's session is the only thing that opens it: nothing is published
    // and nothing is accepted until there is a head unit on the other end.
    let (gateway, gate) = watch::channel(false);

    // What the car is shown while the phone has nothing to show it: frames built
    // into the binary, pushed onto the video queue for as long as there is a car
    // and no stream. The video queue is the whole of the interface — the CarLife
    // half writes what reaches it and knows nothing about a guide.
    tokio::spawn(guide::run(
        outlet.clone(),
        gateway.subscribe(),
        screen.subscribe(),
    ));
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
    // Read once, here rather than in the task below: there is one key, and every
    // session the car gets is answered with it.
    let carlife_auth = carlife_signer();
    // In the background, because the two sides are each other's condition: this
    // task is what opens the gate, and the watcher below is the only thing that
    // acts on it. Waiting for it to return here would leave nobody to answer the
    // gate it opens.
    let car = hooks.clone();
    let alone = gateway.clone();
    // The car's microphone is the one thing that travels the other way, and the
    // CarPlay half owns what to do with it: what coding the phone asked for was
    // decided at the stream's SETUP, which is on that side of the bridge.
    let microphone = Arc::new(Microphone(phone.mic.clone()));
    // The car's own input goes the way everything else does, so the session gets
    // the same sink the CarPlay side fills in.
    let reported = status.carlife().clone();
    // The handle is kept rather than dropped: a stop ends this half where it
    // stands, and a task nobody holds is one that can only be waited for.
    let carlife = tokio::spawn(async move {
        // Held in this task rather than in `main`: the ports are borrows of it,
        // and both have to outlive the loop that runs one session after another.
        let ports = carlife_md::Ports {
            input: car.clone(),
            delegate: &*car,
            home: home.clone(),
            outlet,
            report: reported,
            location: car.clone(),
            voice: microphone,
        };
        let outcome = carlife_md::Md::new(&settings, &ports, replug_requested, carlife_auth)
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

    // Every signal the program stops on is taken here, by the program, and none
    // by either half or by the lights: what a signal means is that two halves
    // have to end, so this is where they are ended rather than asked to notice.
    //
    // Nothing here waits for an answer: what a stop promises is that the process
    // is gone, and a half that will not let go is a board that has to be
    // unplugged. Both are told, they get one turn, the lights are put back, and
    // the program leaves. The two channels are taken before `phone` is moved into
    // its run, and are the whole of what the stop reaches them through.
    let (stopping, shutdown) = (phone.stopping.clone(), phone.shutdown.clone());
    let mut term = arm(SignalKind::terminate(), "term");
    let mut int = arm(SignalKind::interrupt(), "int");
    let mut hup = arm(SignalKind::hangup(), "hup");
    let mut quit = arm(SignalKind::quit(), "quit");
    let asked = async {
        tokio::select! {
            () = raised(&mut term) => "term",
            () = raised(&mut int) => "int",
            () = raised(&mut hup) => "hup",
            () = raised(&mut quit) => "quit",
        }
    };

    // How long the halves have to put back what they put back on their way out:
    // the wireless one's controllers went back in seventy milliseconds on the
    // board, and whatever is not done here is done again at the next start.
    const TURN: std::time::Duration = std::time::Duration::from_millis(500);
    // The phone's half runs last and does not return on its own: it accepts on
    // the socket above, and both links wait for a phone — over the cable or over
    // Bluetooth — until one of them has one, or until a signal ends everything.
    tokio::select! {
        () = phone.run() => tracing::info!("main.stopped reason=run_returned"),
        signal = asked => {
            tracing::info!("main.stopping signal={signal}");
            // Both halves, each in the way it has: `stopping` ends the links'
            // waits, `shutdown` a session already in progress — the same thing a
            // takeover sends to end one — and the car's side is a task with
            // nothing to unwind that is dropped where it stands.
            let _ = stopping.send(true);
            let _ = shutdown.send(());
            carlife.abort();
            tokio::time::sleep(TURN).await;
            // Nothing paints after this: the task that holds the phases is
            // ended first, so what `restore` writes is what stays.
            painting.abort();
            let _ = painting.await;
            if let Some(lights) = &lights {
                lights.restore();
            }
            tracing::info!("main.stopped reason=stop_requested");
            std::process::exit(0);
        }
    }
    Ok(())
}

/// One signal the program stops on, or why there is none.
///
/// Not being able to ask for one is a reason to carry on with the others rather
/// than to stop listening: the remaining ones still end the run.
fn arm(kind: SignalKind, name: &str) -> Option<Signal> {
    signal(kind)
        .inspect_err(|e| tracing::warn!("main.signal_handler_missing signal={name} err={e}"))
        .ok()
}

/// Waits for a signal that was asked for. One that was not is a future that is
/// never ready, which is what keeps its arm of the `select!` quiet — and what
/// keeps a signal that could not be taken from reading as one that arrived.
async fn raised(signal: &mut Option<Signal>) {
    match signal {
        Some(armed) => {
            let _ = armed.recv().await;
        }
        None => std::future::pending().await,
    }
}

/// The car's microphone, as the CarLife half asks for it.
///
/// Nothing is converted here: the CarPlay half knows what the phone asked for
/// and owns the socket it goes out on, and this is the one object both halves
/// name it by.
struct Microphone(Arc<carplay_hu::MicSink>);

impl carlife_md::link::Voice for Microphone {
    fn mic(&self, pcm: &[u8]) {
        self.0.send_pcm(pcm);
    }
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

/// The CarLife key the car's challenge is answered with.
///
/// Not a credential of ours: it is the CarLife app's own private key, extracted
/// from `com.baidu.carlife`, and the car accepts answers made with it because
/// Baidu made the matching public half. The build hands it over in
/// `CARLIFE_PRIVATE_KEY` — a PEM block, or the bare Base64 the app's own constant
/// is — and what the program keeps is the parsed key rather than the text.
///
/// `None` is a build that was given none, which leaves the car unauthenticated
/// rather than failed: the challenge is skipped, the head unit asks again in half
/// a minute, and the streams run either way. Said once, here.
fn carlife_signer() -> Option<carlife_md::Signer> {
    let Some(pem) = option_env!("CARLIFE_PRIVATE_KEY")
        .map(str::trim)
        .filter(|pem| !pem.is_empty())
    else {
        tracing::warn!(
            "carlife.auth.key_absent env=CARLIFE_PRIVATE_KEY consequence=carlife_auth_skipped"
        );
        return None;
    };
    carlife_md::Signer::from_pem(pem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    use rsa::pkcs1v15::Pkcs1v15Sign;
    use rsa::pkcs8::DecodePublicKey;

    /// The public half of the CarLife key, as the extraction report printed it.
    ///
    /// Not a secret: the car holds the same one, and it is what an answer is
    /// checked with. Kept here rather than read from `authentication/`, so the
    /// pair is verified in a checkout that carries no extracted files — the
    /// private half comes from the build, the public half from this constant.
    const CARLIFE_PUBLIC_PEM: &str = "\
-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQC86AUdyKOcMUB/iDMtHpFBOdLP
WThDziP8t20sF0mRKlNBIAxlUZxQWIa/DEhJL9kx05+RyrFDN4B3d66UIURtia3G
hWPoiBCbNornl73ynfFKk1BUDma3O8Qs9Z6YOCkXrGOFQBDfltfXs/w7CogkuS2s
ZsBPOOMVom04I7KWPQIDAQAB
-----END PUBLIC KEY-----
";

    /// The two halves the car will be answered with and checks with, when this
    /// build has one: the key it was given, and the public half the extraction
    /// report printed, including whether the two are really one pair.
    ///
    /// A build without the key asserts nothing, because running without one is a
    /// supported way to run.
    #[test]
    fn the_key_the_build_was_given_is_one_the_car_accepts() {
        let Some(signer) = carlife_signer() else {
            return;
        };

        // The challenge the extraction report verified its own round trip with.
        let challenge = "1234567890_1.0.0";
        let answer = signer.answer(challenge).expect("the challenge is answered");
        let sealed = BASE64.decode(answer).expect("the answer is Base64");

        let key = rsa::RsaPublicKey::from_public_key_pem(CARLIFE_PUBLIC_PEM)
            .expect("a public key this reads");
        key.verify(
            Pkcs1v15Sign::new_unprefixed(),
            challenge.as_bytes(),
            &sealed,
        )
        .expect("the half the car holds accepts the answer");
    }
}
