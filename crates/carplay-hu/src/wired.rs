//! The wired half: find the phone, carry iAP2 over USB, start the session.
//!
//! This is the only place cp2cl touches LIVI. Everything up to and including
//! `CarPlayStartSession` is LIVI's `bringup::run_accessory`; we supply a control
//! channel (the USB carkit stream wrapped in the iAP2 link) and an auth source
//! (local MFi credentials), then watch the event stream until the phone leaves.
//! After that the media arrives over a completely separate connection handled by
//! `crate::airplay`.
//!
//! It also owns the latch that lets a cable session be ended from the outside.
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

use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use iap2_link::LinkConfig;
use livi_runtime::bringup::{run_accessory, CpConfig};
use livi_runtime::driver::spawn_link_stream;
use livi_runtime::ident::Identity;
// `CpConfig::transport` is a `Transport`, but `bringup` only imports it
// privately, so it has to be named through the module that defines it.
use livi_runtime::ident::Transport;
use livi_runtime::mfi_async::SharedCoprocessor;
use livi_runtime::vehicle::{Fuels, Plugs, Vehicle};
use livi_wifi::Channel;
use rustix::io::Errno;
use rustix::net::{
    bind,
    netlink::{self, SocketAddrNetlink},
    recvfrom, socket_with, AddressFamily, RecvFlags, SocketFlags, SocketType,
};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{broadcast, mpsc, watch};

use crate::admission::{Admission, Side};
use crate::bringup::{carry_artwork, run_bringup, Ended};
use crate::frames::Trace;
use crate::link::Panel;
use crate::machine::Machine;
use crate::report::Report;
use crate::Hu;
use crate::wireless::{access_point, parse_mac, short};

/// How long a phone that is still on the cable waits before it is asked again
/// after a session ended. Only a restart and a link that would not come up get
/// here, and neither is a reason to spin.
const RETRY_PAUSE: Duration = Duration::from_secs(1);

/// How long a phone that has dropped out of the cable's list is given to come
/// back before it is taken as gone. Changing its USB configuration — which is
/// what `claim` does at the start of every session — can hide it for a moment,
/// and that is not an unplug.
const UNPLUG_CONFIRM: Duration = Duration::from_millis(500);

/// How long to wait before watching the cable again. The events are the only
/// way anything here learns about a cable, so a watch that could not be made is
/// retried rather than given up on.
const HOTPLUG_RETRY: Duration = Duration::from_secs(1);

/// Where the kernel broadcasts its own hotplug events: `NETLINK_KOBJECT_UEVENT`,
/// multicast group 1. `udevd` reads this group and re-broadcasts what it has
/// processed to group 2, which is the one `nusb` watches.
const KERNEL_MULTICAST_GROUP: u32 = 1;

/// What one uevent is read into. A packet longer than this is truncated, which
/// costs nothing here: all a packet says is that the cable changed.
const UEVENT_BUFFER: usize = 8192;

/// How long the wireless link is given to let the slot go once it has been
/// asked to. It ends in a fraction of this — the wait is only here so a link
/// that has wedged cannot hold the cable out of its own session for ever.
const TAKE_OVER_WAIT: Duration = Duration::from_secs(10);

/// How long the phone is left alone between the old iAP2 session ending and a
/// new one being offered. It has to notice the first before it will act on the
/// second, and the reference waits the same 750ms between tearing its own
/// bootstrap down and starting another.
const RESTART_PAUSE: Duration = Duration::from_millis(750);

/// One phone on the cable.
#[derive(Clone, PartialEq, Eq)]
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

/// What is on the cable, kept current by the kernel's own hotplug events.
///
/// The events say a device came or went, not what it was, so each one is a
/// reason to read the cable again rather than an answer: what goes out on the
/// channel is the list, published only when it changed. Nothing here polls.
///
/// The events are read off the kernel's own multicast group rather than the one
/// `udevd` re-broadcasts on, so no daemon has to be running for them to arrive.
/// See `hotplug`.
fn present() -> watch::Receiver<Vec<Phone>> {
    let (tx, rx) = watch::channel(Vec::new());
    tokio::spawn(async move {
        loop {
            let socket = match hotplug() {
                Ok(socket) => socket,
                Err(e) => {
                    tracing::warn!("usb.hotplug_unavailable err={e:#} action=retry");
                    tokio::time::sleep(HOTPLUG_RETRY).await;
                    continue;
                }
            };
            tracing::info!("usb.hotplug_ready action=listen");
            // Read once the socket is in place, not before: a phone that arrived
            // while it was being made is in this first list rather than in an
            // event that has already been missed.
            refresh(&tx).await;
            // The socket only stops reporting events if it breaks, and watching
            // is the only way this link hears about a cable — so a broken one is
            // replaced, and the read above runs again with it.
            if let Err(e) = listen(&socket, &tx).await {
                tracing::warn!("usb.hotplug_lost err={e:#} action=rewatch");
            }
            tokio::time::sleep(HOTPLUG_RETRY).await;
        }
    });
    rx
}

/// A socket on the group the kernel broadcasts uevents to.
///
/// Group 1 is the kernel's own and needs nothing running to hear it; group 2 is
/// `udevd`'s relay, and with no daemon under it the socket opens, binds and then
/// stays silent for ever — which is what a watch on it looks like when the
/// daemon is not there.
///
/// Only root may broadcast on this group, and what a forged packet would buy is
/// one more read of sysfs, so the sender is not checked.
fn hotplug() -> anyhow::Result<AsyncFd<OwnedFd>> {
    let fd: OwnedFd = socket_with(
        AddressFamily::NETLINK,
        SocketType::RAW,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        Some(netlink::KOBJECT_UEVENT),
    )?;
    bind(&fd, &SocketAddrNetlink::new(0, KERNEL_MULTICAST_GROUP))?;
    // SAFETY: the descriptor is owned by the `AsyncFd` that registers it and is
    // closed only when that is dropped, which is what `register` asks of it.
    let socket = unsafe { AsyncFd::register(fd) }
        .map_err(|e| anyhow::anyhow!("netlink socket: {e}"))?;
    Ok(socket)
}

/// Reads the cable on every event until the socket stops reporting them.
async fn listen(socket: &AsyncFd<OwnedFd>, tx: &watch::Sender<Vec<Phone>>) -> std::io::Result<()> {
    loop {
        let mut ready = socket.readable().await?;
        // One read of sysfs answers every event waiting on the socket, so the
        // socket is emptied first and the cable is read once after it.
        match ready.try_io(|fd| drain(fd.get_ref().as_fd())) {
            Ok(Ok(_)) => refresh(tx).await,
            Ok(Err(e)) => return Err(e),
            // Readiness for something already taken: `try_io` has cleared it and
            // the next wait answers again.
            Err(_nothing_there) => {}
        }
    }
}

/// Empties the socket, and says how much was in it.
///
/// Nothing a uevent carries is used — it is only a reason to look at the cable
/// again — so each packet goes into one buffer and is dropped, and a truncated
/// one counts the same as a whole one.
///
/// An empty socket is `WouldBlock` rather than an empty answer: that error is
/// what `try_io` clears the readiness it was handed for, and returning `Ok`
/// instead would leave it set and the next wait would answer at once every time.
fn drain(fd: BorrowedFd<'_>) -> std::io::Result<usize> {
    let mut buf = [MaybeUninit::uninit(); UEVENT_BUFFER];
    let mut bytes = 0;
    loop {
        match recvfrom(fd, &mut buf, RecvFlags::DONTWAIT) {
            Ok(((packet, _), _, _)) => bytes += packet.len(),
            Err(Errno::AGAIN) if bytes == 0 => {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            // Nothing left: the socket has been emptied.
            Err(Errno::AGAIN) => return Ok(bytes),
            Err(Errno::INTR) => continue,
            Err(e) => return Err(std::io::Error::from_raw_os_error(e.raw_os_error())),
        }
    }
}

/// Reads the cable and says so when the answer changed.
async fn refresh(tx: &watch::Sender<Vec<Phone>>) {
    let now = attached().await;
    tx.send_if_modified(|known| {
        if *known == now {
            false
        } else {
            *known = now;
            true
        }
    });
}

/// Ends the session when the phone leaves the cable.
///
/// The cable is the one link that says nothing when it goes: what is noticed is
/// the AirPlay stream falling silent five seconds later, then a carkit that
/// takes five more to fail against a phone that is no longer there. The kernel
/// says so at once, and the session is ended with the same signal the car's
/// going away sends, which is what lets the slot go for the wireless link.
fn watch_unplug(
    mut present: watch::Receiver<Vec<Phone>>,
    id: String,
    shutdown: broadcast::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while present.changed().await.is_ok() {
            if present.borrow().iter().any(|phone| phone.id == id) {
                continue;
            }
            // Gone from the list is not yet gone from the cable: the phone is
            // hidden for a moment while its USB configuration changes, which is
            // what `claim` does to it as the session starts. Only a phone still
            // missing on a fresh read has left.
            tokio::time::sleep(UNPLUG_CONFIRM).await;
            if attached().await.iter().any(|phone| phone.id == id) {
                continue;
            }
            // A shutdown nobody is listening for is one this session has
            // already ended on.
            let _ = shutdown.send(());
            return;
        }
    })
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

/// Keeps an eye on who is plugged in until we stop being asked to stop.
///
/// `gate` is the CarLife session. While it is shut nothing is looked for at all:
/// the phone has no CarPlay to start, and starting one would only produce an
/// AirPlay connection the receiver is not accepting yet. The cable also waits
/// while the wireless link owns the one slot: a phone found on USB then is not
/// one a new iAP2 session may be started for.
///
/// Nothing here is on a clock: the cable is a [`present`] channel fed by the
/// kernel's hotplug events, and the rest are the watches that decide whether
/// there is anything to do with what is on it.
///
/// `panel` is where what the phone has to say about itself goes — the song it is
/// playing, the route it is guiding along. It is the CarLife side's own sink, so
/// all of it reaches the car by the road every other thing the phone produces
/// does.
///
/// `accessory_key` is the AirPlay identity's public key in hex: the start
/// session names it so the phone recognises the accessory on the cable as the
/// one it paired with.
pub(crate) async fn watch(hu: &Arc<Hu>) {
    let cfg = hu.machine.clone();
    let auth = hu.mfi.clone();
    let airplay_port = hu.airplay_port;
    let accessory_key = hu.accessory_key.clone();
    let restart = hu.restart.clone();
    let panel = hu.panel.clone();
    let status = hu.status.clone();
    let vehicle = hu.vehicle.clone();
    let mut gate = hu.gate.clone();
    let mut owner = hu.admission.subscribe();
    let mut named = hu.admission.subscribe_serial();
    let mut stopping = hu.stopping.subscribe();
    let mut present = present();
    // The phone reported last time, so its disappearance is said once. The
    // claim, not this, is what decides whether a session may be started.
    let mut seen: Option<String> = None;
    loop {
        // Everything that has to be true before there is anything to do, and
        // `borrow_and_update` on each so `changed()` waits for the next change
        // rather than answering at once for one this loop has already read.
        loop {
            let car = *gate.borrow_and_update();
            let held = *owner.borrow_and_update();
            let declared = named.borrow_and_update().is_some();
            let asked_to_stop = *stopping.borrow_and_update();
            let plugged = !present.borrow_and_update().is_empty();
            if asked_to_stop {
                // Said out loud because this is the one thing the program waits
                // for before it can leave: a stop that ends here and a stop that
                // ends somewhere below look the same from outside otherwise.
                tracing::info!("usb.watch_stopped reason=asked_to_stop");
                return;
            }
            // The cable looks for a phone when the slot is free or its own, and
            // also while the wireless link holds it with a phone that has named
            // the cable it would be on: that phone is the one this link may take
            // the session over from — see `take_over`.
            let may_look = held != Some(Side::Wireless) || declared;
            if car && may_look && plugged {
                break;
            }
            tokio::select! {
                _ = gate.changed() => {},
                _ = owner.changed() => {},
                _ = named.changed() => {},
                _ = present.changed() => {},
                _ = stopping.changed() => {},
            }
        }
        let phones = present.borrow_and_update().clone();
        if let Some(id) = &seen {
            if !phones.iter().any(|phone| &phone.id == id) {
                tracing::info!("carplay.unplugged phone={}", short(id));
                status.phone_gone();
                seen = None;
            }
        }
        // The wireless link is serving a phone. This one is only looked at for
        // that same phone arriving on the cable, which is what moves the session
        // onto it.
        if hu.admission.owner() == Some(Side::Wireless) {
            let wanted = hu.admission.serial();
            let arriving = wanted.as_deref().and_then(|name| on_cable(name, &phones));
            let Some(phone) = arriving else {
                // Nothing on the cable is the phone the wireless link is
                // serving, which is the usual case: wait for one of the three
                // things that could change that rather than ask again.
                tokio::select! {
                    _ = present.changed() => {},
                    _ = owner.changed() => {},
                    _ = named.changed() => {},
                    _ = gate.changed() => {},
                    _ = stopping.changed() => {},
                }
                continue;
            };
            tracing::info!(
                "carplay.takeover phone={} from=wireless to=usb",
                short(&phone.id)
            );
            let taken = take_over(hu, &mut owner).await;
            if !taken {
                // The wireless link did not let the slot go. Its session is
                // still the one serving the phone; this link waits for the next
                // thing that changes rather than asking again straight away.
                tokio::select! {
                    _ = owner.changed() => {},
                    _ = present.changed() => {},
                    _ = named.changed() => {},
                    _ = stopping.changed() => {},
                }
            }
            continue;
        }
        let Some(phone) = phones.into_iter().next() else {
            continue;
        };
        let claim = match hu.admission.claim(Side::Usb) {
            Some(claim) => claim,
            // The wireless link got there first in this same moment, and this
            // loop's next pass looks at it as a takeover instead.
            None => continue,
        };
        seen = Some(phone.id.clone());
        tracing::info!("carplay.attached phone={} transport=usb", short(&phone.id));
        status.with(|s| {
            s.carplay.phone = Some(phone.id.clone());
            s.carplay.identified = false;
            s.carplay.authenticated = false;
            s.carplay.subscribed = false;
            s.carplay.failed = None;
        });
        // Subscribed for this session only: a shutdown sent while no
        // session was running must not end the next one before it starts.
        let mut shutdown = hu.shutdown.subscribe();
        // The cable going away is not something the session itself can tell:
        // it is ended from outside, and this is what does the telling.
        let unplugged = watch_unplug(present.clone(), phone.id.clone(), hu.shutdown.clone());
        // Raced against the ask, because this is the one turn that cannot
        // answer it: before `run_bringup` the steps are the phone's own USB
        // configuration and carkit, which listen for nothing, and after it the
        // teardown waits for a driver task to notice it was aborted. A stop
        // that arrives in either leaves the session behind rather than waiting
        // for a phone that is not talking — the claim goes back below either
        // way, and the program leaves.
        let attached = tokio::select! {
            attached = attach(
                &cfg,
                auth.clone(),
                &phone,
                airplay_port,
                &accessory_key,
                &restart,
                &mut shutdown,
                &hu.admission,
                &panel,
                &status,
                &vehicle,
            ) => Some(attached),
            _ = stopping.changed() => None,
        };
        unplugged.abort();
        // The session is over, whatever ended it: the claim goes back and
        // both links are free to wait for the next phone.
        drop(claim);
        status.phone_gone();
        let Some(attached) = attached else {
            tracing::info!("usb.watch_stopped reason=stopped_in_session");
            return;
        };
        if let Err(e) = attached {
            tracing::warn!(
                "carplay.attach_failed phone={} err={e}",
                short(&phone.id)
            );
        }
        // A phone still on the cable after its session ended — a restart, or a
        // link that would not come up — is served again, but not in a tight
        // loop. One that has left is waited for by the top of this loop.
        let still_here = present.borrow().iter().any(|on| on.id == phone.id);
        if still_here {
            tokio::time::sleep(RETRY_PAUSE).await;
        }
    }
}

/// The phone on the cable that a session elsewhere is already serving.
///
/// The serial is the only thing the two links can agree on: what the cable
/// enumerates is a serial, and what the phone names when it identifies itself is
/// the serial its cable would carry.
fn on_cable<'a>(serial: &str, phones: &'a [Phone]) -> Option<&'a Phone> {
    phones
        .iter()
        .find(|phone| phone.id.eq_ignore_ascii_case(serial))
}

/// Ends the session the wireless link is serving, and waits for it to let the
/// slot go.
///
/// The `shutdown` the car's going away sends is what ends it from here: the
/// AirPlay session, the iAP2 session on the tunnel and the Bluetooth leg all
/// listen for it, and none of them can be ended from outside any other way. What
/// is waited for is the slot, not the link — the claim is dropped when that link
/// is done, and only then may the cable claim it. `true` when it came free.
async fn take_over(hu: &Arc<Hu>, owner: &mut watch::Receiver<Option<Side>>) -> bool {
    let _ = hu.shutdown.send(());
    let freed = tokio::time::timeout(TAKE_OVER_WAIT, async {
        loop {
            if *owner.borrow_and_update() != Some(Side::Wireless) {
                return;
            }
            if owner.changed().await.is_err() {
                return;
            }
        }
    })
    .await;
    if freed.is_ok() {
        return true;
    }
    tracing::warn!("carplay.takeover_stalled action=look_again");
    false
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
#[allow(clippy::too_many_arguments)]
async fn attach(
    cfg: &Machine,
    auth: SharedCoprocessor,
    phone: &Phone,
    airplay_port: u32,
    accessory_key: &str,
    restart: &watch::Sender<u64>,
    shutdown: &mut broadcast::Receiver<()>,
    slot: &Admission,
    panel: &Arc<dyn Panel>,
    status: &Report,
    vehicle: &Arc<Vehicle>,
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
            tracing::warn!("carplay.ncm_missing err={e} action=kernel_interface");
            av_interface().await
        }
    };
    tracing::info!("carplay.av_interface name={av_iface:?}");
    status.with(|s| s.carplay.av_iface = av_iface.clone());

    loop {
        let tunnel = carkit(&mux).await?;
        tracing::info!("carplay.carkit_up phone={} action=start_iap2", short(&phone.id));

        // A phone on the cable can still ask which Wi-Fi it should find this
        // accessory on (0x5702), and what it is told is what lets it join
        // unattended once the cable is pulled. The answer is read from the same
        // hostapd file the wireless sessions read, per session, so that a
        // network run differently from the default cannot be described two
        // ways by the same accessory.
        let (_path, ap) = access_point(&cfg.wifi_iface)
            .map_err(|e| anyhow::anyhow!("wifi config on {}: {e}", cfg.wifi_iface))?;

        let identity = Identity {
            name: cfg.name.clone(),
            ssid: cfg.name.clone(),
            bt_mac: parse_mac(&cfg.bt_address),
            fuels: Fuels::default(),
            // The board is not an EV and has no charge port to describe.
            plugs: Plugs::default(),
            // Themes are an accessory look the phone draws its UI in; there is
            // no such asset here, so the phone keeps its own.
            theme: None,
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
            // Only the 0x5702 answer reads these over the cable — the wired
            // start session names no Wi-Fi — but the phone that asked is the
            // one that will look for this network later.
            ssid: ap.ssid.clone(),
            passphrase: ap.passphrase.clone(),
            channel: Channel::of_number(ap.channel.into()),
            airplay_port,
            source_version: "320.17".into(),
            // The key the phone was given at pair-setup, so the cable and the
            // AirPlay session over it are the same accessory it already knows.
            public_key: accessory_key.to_string(),
            transport: Transport::Wired,
            av_iface: av_iface.clone(),
            // The USB network function is already up by the time we get here —
            // this is a wired session on a cable that has been plugged in for a
            // while — so there is nothing to wait for.
            av_iface_late: None,
            available_current_ma: 500,
            // No identifier of our own: the wired start names no Wi-Fi
            // interface, so there is nothing to fall back to and the field
            // stays out — as it did before this existed.
            device_id: None,
            ap_mac: None,
            ap_on_air: None,
            // Bluetooth-only: whether a phone we can see is also on the cable.
            on_cable: None,
            // Wired sends the start once, when the phone offers CarPlay. Every
            // start after that comes from a new session, below.
            start_again: None,
            theme: None,
            // The cable is not going anywhere, so this session keeps the media
            // on it.
            now_playing: true,
        };

        let (tx, rx) = mpsc::channel(64);
        // `Vehicle` owns the two watch senders and `feed` only hands out
        // receivers, so the `Vehicle` has to outlive the session it feeds. The
        // one the bridge shares does: it was made before either half started,
        // and the CarLife side holds the other reference to it.
        let vehicle = vehicle.clone();
        let feed = vehicle.feed();
        let accessory = tokio::spawn(async move {
            run_accessory(channel, signer, identity, cp, tx, feed).await
        });

        let ended = run_bringup(
            &short(&phone.id),
            "usb",
            airplay_port,
            None,
            rx,
            &mut changed,
            shutdown,
            slot,
            Side::Usb,
            panel,
            status,
        )
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
                    "carplay.restart phone={} action=reopen_carkit",
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
    trace: Trace,
}

impl<S> Latched<S> {
    pub(crate) fn new(inner: S) -> (Self, Arc<Shut>) {
        let shut = Arc::new(Shut::default());
        let latch = Self {
            inner,
            shut: shut.clone(),
            trace: Trace::default(),
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
        let filled = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Pending => {
                *this.shut.parked.lock().unwrap() = Some(cx.waker().clone());
                Poll::Pending
            }
            Poll::Ready(Ok(())) => {
                let got = buf.filled().len() - filled;
                this.trace.inbound(&buf.filled()[filled..filled + got]);
                Poll::Ready(Ok(()))
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
        let polled = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &polled {
            this.trace.outbound(&buf[..*n]);
        }
        polled
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

    fn phone(id: &str) -> Phone {
        Phone { id: id.into() }
    }

    /// The cable going away ends the session through the same signal the car's
    /// going away sends: that is the only thing that ends an iAP2 session from
    /// outside, and nothing on this link would notice the cable in time — the
    /// stream it carried falls silent five seconds later.
    #[tokio::test]
    async fn a_phone_leaving_the_cable_ends_the_session() {
        let (cable, present) = watch::channel(vec![phone("CP2CL0001")]);
        let (shutdown, mut ended) = broadcast::channel(1);
        let watching = watch_unplug(present, "CP2CL0001".into(), shutdown);

        cable.send(Vec::new()).expect("the watcher is listening");

        ended.recv().await.expect("the session is ended");
        watching.await.expect("the watcher stops with it");
    }

    /// Another phone arriving, or the same one read again, is not the phone
    /// being served going away.
    #[tokio::test]
    async fn a_phone_still_on_the_cable_ends_nothing() {
        let (cable, present) = watch::channel(vec![phone("CP2CL0001")]);
        let (shutdown, mut ended) = broadcast::channel(1);
        let watching = watch_unplug(present, "CP2CL0001".into(), shutdown);

        cable
            .send(vec![phone("CP2CL0001"), phone("CP2CL0002")])
            .expect("the watcher is listening");
        // The watcher reads the change in its own task; this is long enough for
        // it to have acted on it and still says nothing.
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(ended.try_recv().is_err(), "nothing was ended");
        watching.abort();
    }
}
