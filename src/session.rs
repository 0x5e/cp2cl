//! The wired half: find the phone, carry iAP2 over USB, start the session.
//!
//! This is the only place cp2cl touches LIVI. Everything up to and including
//! `CarPlayStartSession` is LIVI's `bringup::run_accessory`; we supply a control
//! channel (the USB carkit stream wrapped in the iAP2 link) and an auth source
//! (local MFi credentials), then watch the event stream until the phone leaves.
//! After that the media arrives over a completely separate connection handled by
//! `crate::airplay`.
//!
//! It also owns the two pieces both halves share rather than duplicate: the
//! bringup loop, and the latch that lets a session be ended from the outside.
//!
//! A board has no usbmuxd to ask — Alpine does not even package one — so the phone is
//! claimed here instead: `iap2_usbmux` puts it into its CarPlay USB configuration and
//! drives the mux off it, and `iap2_wired::open_carkit` runs lockdown over that to open
//! `com.apple.carkit.service`. The claim is held for as long as the session lasts,
//! because letting go hands the phone back to its default configuration.
//!
//! The session also has to bring the phone's USB network function up itself: nothing
//! else on a board gets there before the phone is answered. See `bridge`, and the second
//! handle the session holds onto.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use iap2_link::LinkConfig;
use livi_runtime::bringup::{run_accessory, BringupEvent, CpConfig};
use livi_runtime::driver::{spawn_link_stream, ArtworkRx};
use livi_runtime::ident::Identity;
// `CpConfig::transport` is a `Transport`, but `bringup` only imports it
// privately, so it has to be named through the module that defines it.
use livi_runtime::ident::Transport;
use livi_runtime::mfi_async::SharedCoprocessor;
use livi_runtime::vehicle::Vehicle;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{broadcast, mpsc};

use crate::artwork::{self, Fitted};
use crate::carlife::proto;
use crate::config::Config;
use crate::navigation::Guidance;
use crate::playing::{Playing, Track};
use crate::sink::ClSink;
use crate::wireless::{parse_mac, short};

/// How often we ask who is attached. Cheap enough that a second is not rude, and
/// fast enough that plugging in feels immediate.
const SCAN_INTERVAL: Duration = Duration::from_secs(1);

/// How long the phone is left alone between the old iAP2 session ending and a
/// new one being offered. It has to notice the first before it will act on the
/// second, and the reference waits the same 750ms between tearing its own
/// bootstrap down and starting another.
const RESTART_PAUSE: Duration = Duration::from_millis(750);

/// iAP2's `NowPlayingUpdate`, which is what the phone describes a song with. It
/// is subscribed to with everything else at the start of a session — see LIVI's
/// `bringup::subscriptions` — and arrives as one more incoming message.
const NOW_PLAYING: u16 = 0x5001;

/// iAP2's two route-guidance messages, which the car's turn-by-turn strip is
/// drawn from: the route as a whole — its state, the distance to the next turn,
/// and which of its turns is the current one — and one turn of it at a time, by
/// index. Subscribed to with everything else at the start of a session, like
/// `NOW_PLAYING` above, and reconciled on the other side; see `crate::navigation`.
const ROUTE_GUIDANCE: u16 = 0x5201;
const ROUTE_GUIDANCE_MANEUVER: u16 = 0x5202;

/// Why a bringup ended, which is what says whether to start another.
pub(crate) enum Ended {
    /// The phone left, or the session failed. There is nothing to restart.
    Finished,
    /// What we render changed, and the phone can only be told by starting
    /// CarPlay over — it reads `/info` once per session and never asks again.
    Restart,
}

/// One phone on the cable.
struct Phone {
    /// Names it in the log and in the "is it still plugged in" check.
    id: String,
}

/// Who is on the cable right now.
///
/// Read out of `/sys/bus/usb/devices` rather than asked of a daemon: a board has no
/// usbmuxd to ask, so the phone is claimed in-process instead — see `claim` and
/// `carkit` below.
async fn attached() -> Vec<Phone> {
    iap2_usbmux::find_iphones()
        .into_iter()
        .map(|phone| Phone { id: phone.serial })
        .collect()
}

/// Claims the phone's CarPlay USB configuration and its mux, and keeps both.
///
/// Only the CarPlay configuration carries the mux and the network function, so this is
/// what makes either reachable at all. The handle has to outlive every session it
/// serves: dropping it hands the phone back to its default configuration.
async fn claim(serial: String) -> anyhow::Result<Arc<iap2_usbmux::MuxDevice>> {
    // The phone is given up to five seconds to re-enumerate with the CarPlay
    // configurations, so this sleeps — keep it off the runtime's threads.
    let device = tokio::task::spawn_blocking(move || iap2_usbmux::MuxDevice::start(&serial))
        .await
        .map_err(|e| anyhow::anyhow!("mux task: {e}"))?
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Arc::new(device))
}

/// The phone's USB network function, carried by us rather than by the kernel.
///
/// `cdc_ncm` does make an interface for it, but every interface is born down and only
/// gets a link-local once something brings it up — and on a board the only thing that
/// would is NetworkManager, which gets there long after the phone has been answered. So
/// the function is claimed here instead: `NcmBridge` takes the NCM control and data
/// interfaces off `cdc_ncm`, lays a TAP over them, gives it the host MAC the phone
/// expects and brings it up, which is the address `CarPlayStartSession` then hands back.
///
/// The handle has to outlive every session it serves, because letting go returns the
/// interfaces to `cdc_ncm` and takes the interface — and so the address — with it.
async fn bridge(serial: String) -> anyhow::Result<iap2_usbmux::NcmBridge> {
    // Claiming interfaces and bringing a link up are all blocking work.
    tokio::task::spawn_blocking(move || iap2_usbmux::NcmBridge::start(&serial))
        .await
        .map_err(|e| anyhow::anyhow!("ncm task: {e}"))?
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// The carkit stream for a phone we claimed ourselves.
async fn carkit(mux: &Arc<iap2_usbmux::MuxDevice>) -> anyhow::Result<Box<dyn idevice::ReadWrite>> {
    iap2_wired::open_carkit(mux)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .into_stream()
        .ok_or_else(|| anyhow::anyhow!("carkit stream unavailable after opening the service"))
}

/// Keeps asking who is plugged in until we stop being asked to stop.
///
/// `gate` is the CarLife session. While it is shut nothing is looked for at all:
/// the phone has no CarPlay to start, and starting one would only produce an
/// AirPlay connection the receiver is not accepting yet.
///
/// `panel` is where what the phone has to say about itself goes — the song it is
/// playing, the route it is guiding along. It is the CarLife side's own sink, so
/// all of it reaches the car by the road every other thing the phone produces
/// does.
pub async fn watch(
    cfg: Arc<Config>,
    auth: SharedCoprocessor,
    airplay_port: u32,
    restart: broadcast::Sender<()>,
    mut gate: tokio::sync::watch::Receiver<bool>,
    panel: Arc<ClSink>,
) {
    let mut busy: Option<String> = None;
    loop {
        while !*gate.borrow() {
            if gate.changed().await.is_err() {
                return;
            }
        }
        let phones = attached().await;
        let current: Vec<String> = phones.iter().map(|p| p.id.clone()).collect();
        match &busy {
            Some(id) if !current.contains(id) => {
                tracing::info!("[cp] {} unplugged", short(id));
                busy = None;
            }
            _ => {}
        }
        if busy.is_none() {
            if let Some(phone) = phones.into_iter().next() {
                tracing::info!("[cp] {} found over usb", short(&phone.id));
                match attach(&cfg, auth.clone(), &phone, airplay_port, &restart, &panel).await {
                    Ok(_) => busy = Some(phone.id.clone()),
                    Err(e) => tracing::warn!("[cp] {} could not attach: {e}", short(&phone.id)),
                }
            }
        }
        tokio::time::sleep(SCAN_INTERVAL).await;
    }
}

/// One phone: open carkit, negotiate iAP2, run the session — and open another
/// one whenever what we render changes.
///
/// A display change cannot be delivered to a running session: the phone reads
/// `/info` once, at the start of one, and never asks again. So it is dropped and
/// the phone is given a fresh iAP2 session to start CarPlay from — which is what
/// unplugging and replugging does, minus the cable. Re-sending
/// `CarPlayStartSession` over the live session does *not* work: that message is
/// the accessory's answer to the phone's own `CarPlayAvailability`, and a phone
/// that has already started CarPlay has no question outstanding.
async fn attach(
    cfg: &Config,
    auth: SharedCoprocessor,
    phone: &Phone,
    airplay_port: u32,
    restart: &broadcast::Sender<()>,
    panel: &Arc<ClSink>,
) -> anyhow::Result<Ended> {
    // Subscribed before anything opens, so a display change made while the
    // carkit is coming up is still waiting for us rather than lost.
    let mut changed = restart.subscribe();

    // The phone's USB configuration is ours from here on, so it is claimed once and
    // held across every session this attach produces.
    let mux = claim(phone.id.clone()).await?;

    // The stream the phone sends its media over: the interface laid over the phone's USB
    // network function ourselves, because asking the kernel for it means asking nobody to
    // bring it up in time. See `bridge` — and note the handle is kept, not just read:
    // dropping it hands the function back to `cdc_ncm` and takes the interface with it.
    let ncm = bridge(phone.id.clone()).await;
    let av_iface = match &ncm {
        Ok(ncm) => Some(ncm.ifname.clone()),
        Err(e) => {
            tracing::warn!("[cp] no NCM bridge ({e}); falling back to the kernel's interface");
            av_interface().await
        }
    };
    tracing::info!("[cp] av interface: {:?}", av_iface);

    loop {
        let tunnel = carkit(&mux).await?;
        tracing::info!("[cp] {} carkit up, starting iAP2", short(&phone.id));

        let identity = Identity {
            name: cfg.name.clone(),
            ssid: cfg.name.clone(),
            bt_mac: parse_mac(&cfg.bt_mac),
        };
        // The session task owns both of these for as long as it runs, and a
        // restart means running it again — so neither may be moved out of the
        // loop, and neither type is `Copy`.
        let signer = auth.clone();

        let (stream, shut) = Latched::new(tunnel);
        let link = LinkConfig {
            max_outgoing: 4,
            control_version: 2,
            zero_ack: true,
            ..LinkConfig::default()
        };
        let (channel, artwork) = spawn_link_stream(stream, link, true);

        // The cover art is a file transfer of its own on the same link, so it is
        // carried across as it lands rather than waited for: the update that
        // names the song says only that art is coming.
        let carrying = tokio::spawn(carry_artwork(artwork, panel.clone()));

        let cp = CpConfig {
            wifi_iface: String::new(),
            ssid: cfg.name.clone(),
            passphrase: "00000000".into(),
            channel: 36,
            security_type: iap2_csm::messages::wifi::SecurityType::WpaWpa2,
            airplay_port,
            source_version: "320.17".into(),
            public_key: String::new(),
            transport: Transport::Wired,
            av_iface: av_iface.clone(),
            // The USB network function is already up by the time we get here —
            // this is a wired session on a cable that has been plugged in for a
            // while — so there is nothing to wait for.
            av_iface_late: None,
            available_current_ma: 500,
            ap_mac: None,
            ap_on_air: None,
            // Bluetooth-only: whether a phone we can see is also on the cable.
            on_cable: None,
            // Wired sends the start once, when the phone offers CarPlay. Every
            // start after that comes from a new session, below.
            start_again: None,
        };

        let (tx, rx) = mpsc::channel(64);
        // `Vehicle` owns the two watch senders and `feed` only hands out
        // receivers, so the `Vehicle` has to outlive the session it feeds. As a
        // temporary it would be dropped at the end of this statement, `changed()`
        // would then report `Err` at once, and `run_accessory` spins on that
        // until the session ends.
        let vehicle = Vehicle::default();
        let feed = vehicle.feed();
        let accessory = tokio::spawn(async move {
            run_accessory(channel, signer, identity, cp, tx, feed).await
        });

        let ended =
            run_bringup("[cp]", &short(&phone.id), airplay_port, None, rx, &mut changed, panel)
                .await;

        // Neither half of the link may outlive this round. Aborting the session
        // drops its handle to the link, and the latch is what makes the driver's
        // parked read return so the tunnel itself closes — see `Latched`. The
        // art has nowhere to go once the link that carried it is gone.
        accessory.abort();
        let _ = accessory.await;
        carrying.abort();
        shut.shutdown();

        match ended {
            Ended::Restart => {
                tracing::info!(
                    "[cp] {} is being told to start over; reopening carkit",
                    short(&phone.id)
                );
                tokio::time::sleep(RESTART_PAUSE).await;
            }
            // Nothing to reopen. The caller drops the phone claim along with us,
            // and picking it up again is what starts a fresh session.
            other => return Ok(other),
        }
    }
}

/// Prints the progress of one iAP2 bringup, and says how it ended.
///
/// Both transports run the same conversation, and differ only in their log
/// prefix and in which events they can produce, so the loop lives here once.
/// `ssid` is the network a wireless session tells the phone to join; the wired
/// one never has that step and passes `None`.
///
/// `changed` fires when what the phone should render has changed, which the
/// caller turns into a new session.
///
/// `panel` is where what the phone says about itself goes: the bringup hands on
/// every incoming message, and these are the ones this side acts on rather than
/// leaves alone.
pub(crate) async fn run_bringup(
    prefix: &str,
    peer: &str,
    airplay_port: u32,
    ssid: Option<&str>,
    mut rx: mpsc::Receiver<BringupEvent>,
    changed: &mut broadcast::Receiver<()>,
    panel: &ClSink,
) -> Ended {
    loop {
        let event = tokio::select! {
            event = rx.recv() => event,
            signal = changed.recv() => {
                return match signal {
                    // `Lagged` means more than one change arrived before this
                    // one was read; either way one new session covers them all.
                    Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => Ended::Restart,
                    // Every sender is gone, so nothing can ask for a restart
                    // again; ending is both correct and the only way to stop
                    // this branch from firing forever.
                    Err(broadcast::error::RecvError::Closed) => Ended::Finished,
                };
            }
        };
        match event {
            Some(BringupEvent::Identified) => tracing::info!("{prefix} {peer} identified"),
            Some(BringupEvent::Authenticated) => {
                tracing::info!("{prefix} {peer} MFi authentication passed")
            }
            Some(BringupEvent::Subscribed) => tracing::info!("{prefix} {peer} subscriptions sent"),
            Some(BringupEvent::WifiConfigSent) => {
                if let Some(ssid) = ssid {
                    tracing::info!("{prefix} {peer} told to join {ssid:?}");
                }
            }
            Some(BringupEvent::CarPlayStartSent { ip }) => tracing::info!(
                "{prefix} {peer} CarPlayStartSession sent — the phone should knock on \
                 {ip}:{airplay_port}"
            ),
            Some(BringupEvent::Failed(reason)) => {
                tracing::error!("{prefix} {peer} session failed: {reason}");
                return Ended::Finished;
            }
            Some(BringupEvent::Closed) => {
                tracing::info!("{prefix} {peer} session closed");
                return Ended::Finished;
            }
            // What the phone is playing, which the car's panel is owed. It is
            // one of the messages the bringup hands on rather than acts on, and
            // the only one of them that goes any further than the log.
            Some(BringupEvent::Incoming {
                msg_id: NOW_PLAYING,
                frame,
            }) => {
                if let Some(track) = Track::from_iap2(&frame) {
                    panel.playing(Playing::Track(track));
                }
            }
            // And where it is going. The two messages are sent one at a time and
            // each describes part of the route, so neither is worth anything on
            // its own: they are handed on as they arrive and put back together
            // by whoever draws the strip. See `crate::navigation`.
            Some(BringupEvent::Incoming {
                msg_id: ROUTE_GUIDANCE | ROUTE_GUIDANCE_MANEUVER,
                frame,
            }) => {
                if let Some(update) = Guidance::from_iap2(&frame) {
                    panel.guidance(update);
                }
            }
            Some(_) => {}
            // The bringup task ended without saying so, which only happens when
            // it was torn down; there is nothing left to watch.
            None => return Ended::Finished,
        }
    }
}

/// The phone's cover art, as it lands, on its way to the car's panel.
///
/// It is its own file transfer on the link rather than part of the message that
/// named the song, so this is where the two are put back together — as far as
/// they can be: the file says nothing about which song it belongs to, so the
/// CarLife side takes it as belonging to the newest one, and holds it until a
/// song turns up to attach it to.
///
/// A cover the car's message cannot carry is re-encoded first — see
/// `crate::artwork` — because the alternative is a song shown with no picture at
/// all. It is done here rather than on the CarLife side so that the decoding and
/// the encoding happen off the task that writes the streams: tens of
/// milliseconds is long enough to be felt in the video.
pub(crate) async fn carry_artwork(mut artwork: ArtworkRx, panel: Arc<ClSink>) {
    // An app that re-announces the song it is playing sends its cover again, and
    // the same picture is not worth decoding twice. The picture is still handed
    // over — the CarLife side drops one it is already holding — but the work is
    // not done again.
    let mut last: Option<((usize, u64), Fitted)> = None;

    while let Some(image) = artwork.recv().await {
        let seen = (image.len(), fingerprint(&image));
        let fitted = match &last {
            Some((before, fitted)) if *before == seen => fitted.clone(),
            _ => {
                let started = Instant::now();
                let budget = proto::MAX_ART;
                // A join error means the task panicked, or the runtime is going
                // away with the session. Either way there is no picture to hand
                // over, and the next cover is still worth carrying.
                let Ok(fitted) =
                    tokio::task::spawn_blocking(move || artwork::fit(&image, budget)).await
                else {
                    tracing::warn!("[cp] cover dropped: the compression task did not finish");
                    continue;
                };
                report_cover(&fitted, seen.0, started.elapsed());
                last = Some((seen, fitted.clone()));
                fitted
            }
        };

        match fitted {
            Fitted::AsIs(bytes) | Fitted::Shrunk { bytes, .. } => {
                panel.playing(Playing::Artwork(bytes))
            }
            // The panel is better left with the song it has than with a picture
            // the car would refuse the message over.
            Fitted::GaveUp { .. } => {}
        }
    }
}

/// What became of a cover, said once and at the level it deserves.
///
/// A cover that fitted is the ordinary case and worth a line only when someone
/// is looking for it, so it is `debug`. One that had to be re-encoded is worth
/// `info`: it is the one place where how large the phone's artwork was, and how
/// much of it survived, can be seen.
fn report_cover(fitted: &Fitted, bytes: usize, took: Duration) {
    let ms = took.as_millis();
    match fitted {
        Fitted::AsIs(_) => tracing::debug!("[cp] cover: in={bytes} passthrough"),
        Fitted::Shrunk {
            bytes: out,
            kind,
            size,
            quality,
            tries,
            orientation,
        } => {
            let turned = if *orientation == 1 {
                String::new()
            } else {
                format!(" orientation={orientation}")
            };
            tracing::info!(
                "[cp] cover compressed: src={kind} in={bytes} out={} size={}x{} q={quality} \
                 tries={tries}{turned} ms={ms}",
                out.len(),
                size.0,
                size.1
            );
        }
        Fitted::GaveUp { why, best, tries } => match best {
            Some(best) => tracing::warn!(
                "[cp] cover dropped: in={bytes} reason={why} best={} size={}x{} q={} \
                 tries={tries} ms={ms}",
                best.bytes,
                best.size.0,
                best.size.1,
                best.quality
            ),
            None => tracing::warn!("[cp] cover dropped: in={bytes} reason={why} ms={ms}"),
        },
    }
}

/// A cheap fingerprint of a file, so the same cover arriving twice is recognised
/// as the one that was just dealt with.
///
/// FNV-1a, which is not a hash to trust an attacker with and does not need to be:
/// it only ever compares two files the phone itself sent.
fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// A byte stream that can be made to look like the far end went away.
///
/// LIVI's link driver splits whatever stream it is handed and parks the read
/// half in a task of its own, so dropping our handle to the link leaves the
/// socket open underneath it: that read is still waiting, and the half holding
/// it is still alive. Ending a session therefore means making that read return,
/// and this is the smallest thing that can: `poll_read` reports end-of-file once
/// the latch is set, which unwinds the driver and drops the socket with it.
pub(crate) struct Latched<S> {
    inner: S,
    shut: Arc<Shut>,
}

impl<S> Latched<S> {
    pub(crate) fn new(inner: S) -> (Self, Arc<Shut>) {
        let shut = Arc::new(Shut::default());
        let latch = Self {
            inner,
            shut: shut.clone(),
        };
        (latch, shut)
    }
}

/// The switch end of a `Latched`: hand it to whoever needs to end the session.
#[derive(Default)]
pub(crate) struct Shut {
    closed: AtomicBool,
    /// The waker of a read that is waiting right now, so setting the latch can
    /// interrupt it. Only ever one: a stream has a single reader.
    parked: Mutex<Option<Waker>>,
}

impl Shut {
    pub(crate) fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(waker) = self.parked.lock().unwrap().take() {
            waker.wake();
        }
    }
}

fn ended_of_file() -> Poll<std::io::Result<()>> {
    // `ReadBuf` left untouched, which is how a read reports end-of-file.
    Poll::Ready(Ok(()))
}

impl<S: AsyncRead + Unpin> AsyncRead for Latched<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.shut.closed.load(Ordering::SeqCst) {
            return ended_of_file();
        }
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Pending => {
                *this.shut.parked.lock().unwrap() = Some(cx.waker().clone());
                Poll::Pending
            }
            other => other,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Latched<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.shut.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.shut.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Which interface carries CarPlay's own media, when the NCM bridge could not be
/// started: the bridge normally names the interface it builds and brings up
/// itself, so this is only the fallback.
async fn av_interface() -> Option<String> {
    // A board usually names it usb0; take the first one that looks like it.
    let entries = std::fs::read_dir("/sys/class/net").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("usb") || name.starts_with("enp") {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::ClOutlet;

    /// A cover is carried or it is not: one the car's message cannot take never
    /// reaches the CarLife side, and the ones either side of it still do.
    #[tokio::test]
    async fn a_cover_the_car_cannot_take_never_reaches_it() {
        let outlet = Arc::new(ClOutlet::new());
        let mut told = outlet.take_playing().expect("the queue");
        let (tx, rx) = mpsc::unbounded_channel();
        let carrying = tokio::spawn(carry_artwork(rx, outlet.sink()));

        // Three bytes of a JPEG: small enough to go as it is.
        let fits = vec![0xff, 0xd8, 0xff];
        tx.send(fits.clone()).expect("a cover");
        // Neither a picture nor something the message could carry.
        tx.send(vec![0x42; 40 * 1024]).expect("a cover");
        tx.send(fits.clone()).expect("a cover");

        for _ in 0..2 {
            let item = told.recv().await.expect("a cover");
            assert_eq!(item, Playing::Artwork(fits.clone()));
        }
        assert!(told.try_recv().is_err(), "nothing else was carried");

        drop(tx);
        carrying.await.expect("the task ends with the link");
    }
}
