//! Linux: BlueZ publishes the service and hands us the phone's RFCOMM socket.
//!
//! This is LIVI's `livi_runtime::bt` doing all of it — registering the iAP
//! server profile, the iAP client profile, the CarPlay profile, and an agent
//! that answers the pairing prompt. All that is left for us is draining the
//! channel of sockets it produces — and calling a paired phone back, since a
//! phone never returns to an accessory on its own: see `page`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iap2_link::LinkConfig;
use livi_runtime::bt::{IncomingConn, Wireless};
use livi_runtime::driver::spawn_link;
use tokio::sync::watch;
use tokio::time::sleep;

use super::{page, parse_mac, power, radios_present, ready, run_session, short};
use crate::admission::Side;
use crate::machine::Machine;
use crate::Hu;
use crate::bringup::{carry_artwork, Ended};

/// Keeps serving phones until the program stops. The service is published only
/// while the car has a CarLife session of its own and the cable does not own the
/// one CarPlay slot: a phone that finds it otherwise starts a CarPlay that
/// nothing is reading, or a second session on top of the wired one.
///
/// Nothing pushes "a phone just paired" from outside, so a session that ends
/// puts us back to waiting for the same phone to come again.
pub(crate) async fn watch(hu: &Arc<Hu>) {
    let cfg = hu.machine.clone();
    let auth = hu.mfi.clone();
    let airplay_port = hu.airplay_port;
    let accessory_key = hu.accessory_key.clone();
    let restart = hu.restart.clone();
    let panel = hu.panel.clone();
    let status = hu.status.clone();
    let vehicle = hu.vehicle.clone();
    let handover = hu.handover.clone();
    let delegate = hu.delegate.clone();
    let mut gate = hu.gate.clone();
    let mut owner = hu.admission.subscribe();
    let mut stopping = hu.stopping.subscribe();
    // The boot loads the two modules and goes on, so neither radio is here when
    // this starts: an interface appears once brcmfmac has taken its firmware, a
    // controller once btbcm has pushed its patch, and bluetoothd claims its name
    // on the bus after both. Waited for here rather than in `main`, because this
    // is the only part of the program with any use for them.
    // Kept rather than swallowed: the light's first two states are the same
    // question asked from across a room, and they have to answer it with what
    // was actually found rather than with "the loop did not hang".
    let radios = wait_for_radios(&cfg).await;
    // Said as soon as the radios have answered, rather than only when the
    // service is published: a wired session needs neither radio, and holding
    // the light in its first state until a wireless one is offered would be a
    // light that says the bridge is not ready while CarPlay is running.
    delegate.radios(radios);
    // The page was handed an address before there was a controller to ask, so
    // it has the fallback one. What the phone is told is read per session and
    // needs no help; this is the copy a person reads off the page.
    status.with(|s| s.carplay.bt_mac = cfg.bt_address.clone());

    if let Err(e) = ready(&cfg) {
        tracing::error!("wifi.ap_unusable err={e}");
        return;
    }

    // The iAP service is what tells the phone there is CarPlay here, so it is
    // not published until there is: with CarLife in the picture the CarPlay half
    // only exists for as long as the car's session does, and with the cable in
    // the picture only the link that saw a phone first may offer itself.
    //
    // Said out loud, because the wait is silent and can last as long as the car
    // takes to turn up: a log that stops here looks like a run that failed
    // rather than one that is waiting.
    if !*gate.borrow() {
        tracing::info!("bt.waiting_for_car consequence=nothing_published");
    }

    // The channel never closes while a round holds the connection that owns the
    // profiles, so being told to stop is the one way out of the loops below —
    // and the moment the controllers have to be put back. Made once rather than
    // per round; without the handler the default action still ends the process,
    // only without the putting back.
    let mut stop = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .inspect_err(|e| tracing::warn!("main.signal_handler_missing err={e} consequence=no_restore"))
        .ok();

    loop {
        // Nothing is published while the car has no session or while the cable
        // owns the slot, and the profiles come out with it: BlueZ's own
        // auto-connect follows whatever is registered, so a profile left up is
        // a phone called back for a CarPlay nobody is reading.
        if !wait_ready(&mut gate, &mut owner, &mut stopping).await {
            return;
        }
        tracing::info!("bt.car_up action=publish_service");

        // The access point is a daemon the boot starts after this program, so
        // the network a phone is told to join may not be on air yet. It is named
        // in the start session either way, and a phone that is paged before its
        // network is there has nothing to come back to.
        let ap = wait_for_access_point().await;

        // That wait can last fifteen seconds, and the car's session can end
        // inside it, or the cable can take the slot. Publishing a service then
        // is a phone called back for a session that has already gone or that
        // the receiver will not accept, so both are asked again rather than
        // answered by the wait that came before it.
        if !*gate.borrow()
            || (*owner.borrow()).is_some_and(|side| side != Side::Wireless)
            || *stopping.borrow()
        {
            tracing::info!("bt.publish_skipped reason=state_changed");
            continue;
        }

        // CarPlay and nothing else: this is what publishes the iAP and CarPlay
        // profiles and what makes the adapter discoverable. Android Auto would
        // add SAP and a second record.
        let wireless = Wireless {
            carplay: true,
            android_auto: false,
        };
        let (conn, mut incoming, held) =
            match livi_runtime::bt::start(&cfg.bt_adapter, &cfg.name, wireless).await {
                Ok(started) => started,
                Err(e) => {
                    tracing::error!(
                        "bt.start_failed adapter={} err={e} hint=\"is bluetoothd \
                         running, and is {} the adapter?\"",
                        cfg.bt_adapter,
                        cfg.bt_adapter
                    );
                    // Asked again rather than given up on: this is BlueZ
                    // refusing one of the steps that publish the service, and
                    // `main` awaits this watcher — returning here takes the
                    // page and the car's session down with it, which is a far
                    // larger failure than a phone that cannot find the board.
                    sleep(START_RETRY).await;
                    continue;
                }
            };
        // A phone can only come back to a board that has both halves: the radios
        // it is paged on and the network it is told to join. Reported here
        // because this is the moment the profiles are actually published — a
        // controller that is up without a service on it is a light beating for a
        // phone nothing is listening for. A round that finds them again says so
        // again, which costs nothing: the same answer is one the lights already
        // show.
        delegate.radios(radios && ap);
        // The D-Bus connection owns the registered profiles. Dropping it
        // unregisters them, so it has to be kept alive for as long as we are
        // listening — it is also what puts the controllers back at the end.
        tracing::info!(
            "bt.service_up adapter={} name={:?} action=pair_phone",
            cfg.bt_adapter, cfg.name
        );

        // The controller has nothing to do once the phone has moved its session
        // onto Wi-Fi, and every page it answers is a slice of the antenna the
        // stream needed — see `power`. A task of its own, because the loop below
        // sits inside a session for as long as the controller has to stay off.
        let guard = tokio::spawn(power::guard(
            conn.clone(),
            cfg.bt_adapter.clone(),
            handover.clone(),
        ));

        // Set when this round ended because the cable owns the slot. Such a
        // round is owed another one once the slot is free; a round that ended
        // on a signal is not.
        let mut refused = false;
        loop {
            // Only while nothing is on the link: a phone that is already coming
            // in does not need calling, and one that is mid-session must not be
            // paged again on top of it. And not at all while the phone has moved
            // its iAP2 session onto the CarPlay data stream — see `Handover`.
            let paging = tokio::spawn(page::run(
                conn.clone(),
                cfg.bt_adapter.clone(),
                handover.clone(),
                gate.clone(),
            ));
            // The car's session ending is checked between sessions and never
            // inside one: `run_session` has tasks of its own, and dropping it
            // half way leaves them behind. The same goes for the slot being
            // taken by the cable while this link is idle.
            let next = match &mut stop {
                Some(sig) => tokio::select! {
                    next = incoming.recv() => next,
                    _ = sig.recv() => {
                        // The wired watcher has no signal handler of its own:
                        // these two are what end it and everything it is in.
                        let _ = hu.stopping.send(true);
                        let _ = hu.shutdown.send(());
                        None
                    },
                    _ = wait_car_down(&mut gate) => None,
                    _ = wait_blocked(&mut owner, Side::Wireless) => {
                        refused = true;
                        None
                    },
                    _ = stopping.changed() => None,
                },
                None => tokio::select! {
                    next = incoming.recv() => next,
                    _ = wait_car_down(&mut gate) => None,
                    _ = wait_blocked(&mut owner, Side::Wireless) => {
                        refused = true;
                        None
                    },
                    _ = stopping.changed() => None,
                },
            };
            paging.abort();
            let Some(IncomingConn { fd, peer_mac }) = next else {
                break;
            };
            let peer = short(&peer_mac);
            // The first link to see a phone takes the slot. A channel that
            // arrives after the cable won is answered by closing it.
            let Some(claim) = hu.admission.claim(Side::Wireless) else {
                tracing::info!("bt.channel_refused peer={peer} reason=usb_active");
                refused = true;
                break;
            };
            tracing::info!("bt.channel_opened peer={peer} transport=wireless");
            // Over Bluetooth there is no serial to be had: the peer is an
            // address and nothing more, so that is what the page is given.
            status.with(|s| {
                s.carplay.phone = Some(peer_mac.clone());
                s.carplay.identified = false;
                s.carplay.authenticated = false;
                s.carplay.subscribed = false;
                s.carplay.failed = None;
            });
            let link = LinkConfig {
                max_outgoing: 4,
                control_version: 2,
                ..LinkConfig::default()
            };
            // Over RFCOMM the phone speaks first, so we must not open with a
            // negotiate of our own.
            let (channel, artwork) = spawn_link(fd, link, false);
            // The cover art is a file transfer of its own on the same link, and
            // it goes to the car's panel the same way the song it belongs to
            // does.
            let carrying = tokio::spawn(carry_artwork(artwork, panel.clone()));
            // Subscribed for this channel only: a shutdown sent while no
            // session was running must not end the next one before it starts.
            let mut shutdown = hu.shutdown.subscribe();
            // Ending the session drops the descriptor, which is the only way a
            // BlueZ channel ever closes; paging the phone again is what brings
            // it back, and the loop above starts that over.
            let ended = run_session(
                &cfg,
                auth.clone(),
                channel,
                airplay_port,
                local_mac(&cfg),
                &accessory_key,
                &peer,
                &restart,
                &mut shutdown,
                &panel,
                &status,
                &vehicle,
            )
            .await;
            carrying.abort();
            tracing::info!("bt.channel_closed peer={peer} action=page_again");
            if let Ended::Restart = ended {
                tracing::info!("bt.waiting peer={peer} action=wait_channel");
            }
            // The Bluetooth leg ending is not the end of CarPlay: the phone has
            // moved it onto Wi-Fi, where it stays until the AirPlay session
            // ends. The slot is held for that long, with a short grace for a
            // phone that has not opened the receiver yet, so the cable cannot
            // start a second session underneath it.
            let started = Instant::now();
            while *gate.borrow()
                && !*stopping.borrow()
                && (handover.live()
                    || status.airplay_up()
                    || started.elapsed() < SESSION_GRACE)
            {
                sleep(SESSION_POLL).await;
            }
            drop(claim);
            status.phone_gone();
            if !*gate.borrow() || *stopping.borrow() {
                break;
            }
        }

        // Nothing is listening any more: the profiles went with the channel, and
        // the controllers go back to the name and discoverability they were
        // found with. The power state is not among them — leaving it off costs
        // nothing, because the next start powers the controller on as it always
        // did.
        guard.abort();
        livi_runtime::bt::let_go(&conn, &cfg.bt_adapter, &held).await;
        // A signal ends the run. A round that ended for any other reason — the
        // car went away, or the cable took the slot — is owed the next one,
        // which starts by waiting for both to change back.
        if *stopping.borrow() || (!refused && *gate.borrow()) {
            tracing::info!(
                "bt.service_down adapter={} state=restored",
                cfg.bt_adapter
            );
            return;
        }
        tracing::info!(
            "bt.service_down adapter={} reason=state_changed",
            cfg.bt_adapter
        );
    }
}

/// Waits until there is something to publish a service for: the car has a
/// CarLife session, the cable does not own the one CarPlay slot, and the
/// program is not stopping. `false` is one of those channels being gone, which
/// is the program shutting down.
async fn wait_ready(
    gate: &mut watch::Receiver<bool>,
    owner: &mut watch::Receiver<Option<Side>>,
    stopping: &mut watch::Receiver<bool>,
) -> bool {
    loop {
        // `borrow_and_update` on all three: `changed()` must wait for the next
        // change, not answer at once for one this loop has already read.
        let car = *gate.borrow_and_update();
        let free = (*owner.borrow_and_update()).is_none();
        let asked_to_stop = *stopping.borrow_and_update();
        if asked_to_stop {
            return false;
        }
        if car && free {
            return true;
        }
        tokio::select! {
            changed = gate.changed() => { if changed.is_err() { return false; } },
            changed = owner.changed() => { if changed.is_err() { return false; } },
            changed = stopping.changed() => { if changed.is_err() { return false; } },
        }
    }
}

/// Waits until another link owns the slot, which is when this one has to take
/// its service down. A slot owned by this link is not a reason to stop.
async fn wait_blocked(owner: &mut watch::Receiver<Option<Side>>, mine: Side) {
    loop {
        if (*owner.borrow_and_update()).is_some_and(|side| side != mine) {
            return;
        }
        if owner.changed().await.is_err() {
            // The channel only goes away with the whole half, and with it any
            // reason to keep waiting here.
            std::future::pending::<()>().await;
        }
    }
}

/// Waits until the car's CarLife session is over, which is the same condition
/// seen from inside one: while it lasts there is a service to publish, and
/// after it nothing to publish it for.
async fn wait_car_down(gate: &mut watch::Receiver<bool>) {
    loop {
        if !*gate.borrow_and_update() {
            return;
        }
        if gate.changed().await.is_err() {
            return;
        }
    }
}

/// How long the slot is held after the Bluetooth leg ends with no receiver open
/// yet. Long enough for the phone to join the access point and connect to
/// AirPlay; short enough that a session which never started does not hold the
/// cable off for longer than a retry.
const SESSION_GRACE: Duration = Duration::from_secs(5);

/// How often the end of that Wi-Fi leg is looked for. Its two real signals —
/// the tunnel being torn down and the receiver closing — are both states rather
/// than events, so this is what notices them.
const SESSION_POLL: Duration = Duration::from_millis(200);

/// This machine's Bluetooth address, which the phone checks against the one it
/// paired with. Read at the start of every session rather than taken from the
/// configuration at startup: a controller registers itself with the kernel
/// whenever it is brought up, and a session that begins after that is a session
/// that can have the real address.
fn local_mac(cfg: &Machine) -> [u8; 6] {
    parse_mac(&cfg.bt_address)
}

/// How long a wireless run waits for the radios before it goes on without them.
/// Long enough for both drivers to take their firmware and for bluetoothd to
/// claim its name; short enough that a board whose radio never comes up still
/// has a page to say so on.
const RADIOS_CEILING: Duration = Duration::from_secs(20);

/// How often the question is asked again. Both drivers finish within a second
/// of being loaded, so this is fine enough not to be the reason a start takes
/// longer than it has to.
const RADIOS_POLL: Duration = Duration::from_millis(200);

/// How long a run that could not publish the service waits before asking again.
/// Long enough that a controller refusing a step is not asked in a loop, short
/// enough that one that was only slow is published while the car is still there.
const START_RETRY: Duration = Duration::from_secs(5);

/// The name bluetoothd takes on the system bus.
const BLUEZ: &str = "org.bluez";

/// Waits for the two radios and for BlueZ, because the boot no longer does.
///
/// Three things have to be true before a wireless run has anything to publish:
/// the interface, the controller, and bluetoothd on the system bus. The first
/// two are the kernel's half of it and the third is the half this program talks
/// to — `bt::start` fails without it — and none of them is a fault when it is
/// missing a moment after a module was loaded, so this waits rather than checks.
///
/// It gives up and goes on: the page, the receiver and the car's session are
/// already running by now, and the wireless half says what it could not do in
/// the line `main.radios_incomplete` rather than taking them down with it.
///
/// Answers whether all three were found, because the caller has to tell a run
/// that waited for them from one that went on without them.
async fn wait_for_radios(cfg: &Machine) -> bool {
    let started = Instant::now();
    // The bus is a handshake, so it is had once and kept: what is asked of it
    // afterwards is a question about a name, not about the bus itself.
    let mut bus = None;
    let mut said: Option<String> = None;
    loop {
        let (wifi, bt) = radios_present(&cfg.wifi_iface, &cfg.bt_adapter);
        if bus.is_none() {
            bus = zbus::Connection::system().await.ok();
        }
        let bluez = match &bus {
            Some(conn) => bluetoothd_up(conn).await,
            None => false,
        };
        let missing = missing(cfg, wifi, bt, bluez);
        if missing.is_empty() {
            let after_ms = started.elapsed().as_millis();
            tracing::info!("wifi.ready iface={} after_ms={after_ms}", cfg.wifi_iface);
            tracing::info!("bt.ready adapter={} after_ms={after_ms}", cfg.bt_adapter);
            return true;
        }
        // Once per change rather than once per poll: the first round says what
        // the boot has not got round to yet, and the next says what is left.
        if said.as_deref() != Some(missing.as_str()) {
            tracing::info!("main.radios_waiting missing={missing}");
            said = Some(missing.clone());
        }
        if started.elapsed() >= RADIOS_CEILING {
            tracing::warn!(
                "main.radios_incomplete after_ms={} missing={missing} consequence=wireless_will_fail",
                started.elapsed().as_millis()
            );
            return false;
        }
        sleep(RADIOS_POLL).await;
    }
}

/// What is not up yet, named as the log and the loop both want it.
fn missing(cfg: &Machine, wifi: bool, bt: bool, bluez: bool) -> String {
    let mut missing = Vec::new();
    if !wifi {
        missing.push(cfg.wifi_iface.as_str());
    }
    if !bt {
        missing.push(cfg.bt_adapter.as_str());
    }
    if !bluez {
        missing.push("bluetoothd");
    }
    missing.join(",")
}

/// Whether BlueZ holds its name on the bus, which is the same question as
/// whether bluetoothd is running.
async fn bluetoothd_up(conn: &zbus::Connection) -> bool {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else {
        return false;
    };
    match zbus::names::BusName::try_from(BLUEZ) {
        Ok(name) => dbus.name_has_owner(name).await.unwrap_or(false),
        Err(_) => false,
    }
}

/// Where `hostapd` is told to write the pid of the daemon it forks, and the one
/// thing on this system that says the access point is on air.
const AP_PID: &str = "/run/hostapd.pid";

/// How long the service waits for the access point. `S55ap` retries for ten
/// seconds and hostapd needs 3.4 to beacon, so this covers a boot that ran it
/// late; the usual case is that it is already up, because the car took longer.
const AP_CEILING: Duration = Duration::from_secs(15);

/// How often the pid file is looked for. The daemon is either there or about to
/// be: this only decides how much of the ceiling is spent noticing.
const AP_POLL: Duration = Duration::from_millis(200);

/// Waits for the access point to be on air, or for the ceiling.
///
/// A pid file rather than something cleverer: `/run` is a tmpfs, so the file
/// cannot outlive the boot that wrote it, and `S55ap` writes it on the attempt
/// that worked.
///
/// Answers whether the file was found: the interface is there long before the
/// daemon is, and a phone told to join a network that is not on air is the
/// difference between the light's first two states.
async fn wait_for_access_point() -> bool {
    let started = Instant::now();
    let mut said = false;
    loop {
        if Path::new(AP_PID).exists() {
            tracing::info!(
                "wifi.ap_ready after_ms={}",
                started.elapsed().as_millis()
            );
            return true;
        }
        if !said {
            tracing::info!("wifi.ap_waiting pid_file={AP_PID}");
            said = true;
        }
        if started.elapsed() >= AP_CEILING {
            tracing::warn!(
                "wifi.ap_absent after_ms={} consequence=phone_told_a_network_that_is_not_on_air",
                started.elapsed().as_millis()
            );
            return false;
        }
        sleep(AP_POLL).await;
    }
}
