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

use super::{
    acl, ap_channel, is_24ghz, page, parse_mac, power, radios_present, ready, run_session, short,
};
use crate::admission::Side;
use crate::machine::Machine;
use crate::Hu;
use crate::bringup::{carry_artwork, Ended};

/// Keeps serving phones until the program stops. The service is published
/// while the car has a CarLife session of its own, whichever link owns the one
/// CarPlay slot: while the cable has it, the service is one a phone can still
/// pair with — a channel a phone opens then is closed rather than answered,
/// and the pairing outlives the session that refused it. The one time the box
/// is undiscoverable by design is a 2.4 GHz wireless session, when the power
/// guard has the controller off.
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
    // The paging task's own view of who owns the CarPlay slot; nothing else
    // in this watcher needs it any more.
    let slot = hu.admission.subscribe();
    let mut stopping = hu.stopping.subscribe();
    // The boot loads the two modules and goes on, so neither radio is here when
    // this starts: an interface appears once brcmfmac has taken its firmware, a
    // controller once btbcm has pushed its patch, and bluetoothd claims its name
    // on the bus after both. Waited for here rather than in `main`, because this
    // is the only part of the program with any use for them.
    // Kept rather than swallowed: the light's first two states are the same
    // question asked from across a room, and they have to answer it with what
    // was actually found rather than with "the loop did not hang".
    let Some(radios) = wait_for_radios(&cfg, &mut stopping).await else {
        return;
    };
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
    // not published until the car has a session for one to run in. Whoever
    // owns the one CarPlay slot is not part of that question — see the doc on
    // `watch` for what a phone is given while the cable has it.
    //
    // Said out loud, because the wait is silent and can last as long as the car
    // takes to turn up: a log that stops here looks like a run that failed
    // rather than one that is waiting.
    if !*gate.borrow() {
        tracing::info!("bt.waiting_for_car consequence=nothing_published");
    }

    // The channel never closes while a round holds the connection that owns the
    // profiles, so being told to stop is the one way out of the loops below —
    // and the moment the controllers have to be put back. What tells us is the
    // program, which takes the signal once for the whole process; a handler
    // installed in a loop like this one would only be noticed while the loop is
    // sitting on a `select!`, and a run waiting for a car or inside a session is
    // not.
    loop {
        // Nothing is published while the car has no session. The cable owning
        // the slot is not a reason to take the service down: what a phone
        // cannot have then is a session, and what it still can — the pairing —
        // is what the service is published for.
        if !wait_ready(&mut gate, &mut stopping).await {
            return;
        }
        tracing::info!("bt.car_up action=publish_service");

        // The access point is a daemon the boot starts after this program, so
        // the network a phone is told to join may not be on air yet. It is named
        // in the start session either way, and a phone that is paged before its
        // network is there has nothing to come back to.
        let Some(ap) = wait_for_access_point(&mut stopping).await else {
            // Asked to stop while the network was coming up. Nothing is
            // published yet, so there is nothing to put back either.
            return;
        };

        // That wait can last ten seconds, and the car's session can end
        // inside it. Publishing a service then is a phone called back for a
        // CarPlay nobody is reading, so it is asked again rather than
        // answered by the wait that came before it.
        if !*gate.borrow() || *stopping.borrow() {
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

        // Powering the controller off is only worth doing when the access point
        // shares the 2.4 GHz band with Bluetooth: on 5 GHz the two radios do
        // not compete for airtime, and a controller left up is one a phone can
        // still be paged on. Read each round, like the credentials the phone is
        // handed, so an edit to hostapd's file is what this round acts on.
        let shared_band = match ap_channel(&cfg) {
            Ok(channel) => {
                if is_24ghz(channel) {
                    tracing::info!("bt.power_guard_started reason=wifi_24ghz channel={channel}");
                    true
                } else {
                    tracing::info!("bt.power_kept_on reason=wifi_5ghz channel={channel}");
                    false
                }
            }
            Err(e) => {
                // An unreadable configuration must not take the controller
                // down: nothing here can say the band is shared.
                tracing::warn!("wifi.ap_band_unknown err={e} consequence=bluetooth_kept_on");
                false
            }
        };
        // A task of its own, because the loop below sits inside a session for
        // as long as the controller has to stay off. See `power`.
        let guard = shared_band.then(|| {
            tokio::spawn(power::guard(
                conn.clone(),
                cfg.bt_adapter.clone(),
                handover.clone(),
            ))
        });

        // When the phone's link comes up, from BlueZ's own signal. The page asks
        // the same question, but a round is due every five seconds and is late
        // by the length of the call it was waiting on, so its answer is a range
        // rather than a moment — and a moment is what says whether the phone
        // woke at all. It lives as long as the connection the profiles are
        // registered on, because that is what the signal is about.
        let link = tokio::spawn(acl::watch(conn.clone()));

        // The peer of the last channel this round refused, so a phone that
        // keeps knocking while the cable serves it says so once rather than a
        // line per knock.
        let mut last_refused: Option<String> = None;
        loop {
            // The page holds itself back when it has nothing to call for: a
            // phone that is already coming in does not need calling, one that
            // is mid-session must not be paged on top of it, and one whose
            // session is on the cable is neither — see `page` and `Handover`.
            let paging = tokio::spawn(page::run(
                conn.clone(),
                cfg.bt_adapter.clone(),
                handover.clone(),
                slot.clone(),
                gate.clone(),
            ));
            // The car's session ending is checked between sessions and never
            // inside one: `run_session` has tasks of its own, and dropping it
            // half way leaves them behind.
            let next = tokio::select! {
                next = incoming.recv() => next,
                _ = wait_car_down(&mut gate) => None,
                _ = stopping.changed() => None,
            };
            paging.abort();
            let Some(IncomingConn { fd, peer_mac }) = next else {
                break;
            };
            let peer = short(&peer_mac);
            // The first link to see a phone takes the slot. A channel that
            // arrives after the cable won is answered by closing it — and the
            // service stays up for it, because the pairing a phone makes over
            // it is what the cable's turn cannot give it.
            let Some(claim) = hu.admission.claim(Side::Wireless) else {
                if last_refused.as_deref() != Some(&peer_mac) {
                    tracing::info!("bt.channel_refused peer={peer} reason=usb_active");
                }
                last_refused = Some(peer_mac);
                continue;
            };
            last_refused = None;
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
                &hu.admission,
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
            //
            // A session ended from outside is the one exception: the car going
            // away leaves nothing to serve, and the cable taking the phone over
            // needs the slot back to serve it. Either way no Wi-Fi leg is
            // coming, so the grace would only be waited out for nobody.
            let started = Instant::now();
            while ended != Ended::Stopped
                && *gate.borrow()
                && !*stopping.borrow()
                && (handover.live()
                    || status.airplay_up()
                    || started.elapsed() < SESSION_GRACE)
            {
                sleep(SESSION_POLL).await;
            }
            // Said before the slot goes: the link that takes it next starts by
            // writing the phone it now serves, and a page cleared afterwards
            // would be that write undone.
            status.phone_gone();
            drop(claim);
            if !*gate.borrow() || *stopping.borrow() {
                break;
            }
        }

        // Nothing is listening any more: the profiles went with the channel, and
        // the controllers go back to the name and discoverability they were
        // found with. The power state is not among them — leaving it off costs
        // nothing, because the next start powers the controller on as it always
        // did. A 5 GHz round never had a guard to stop.
        if let Some(guard) = guard {
            guard.abort();
        }
        link.abort();
        livi_runtime::bt::let_go(&conn, &cfg.bt_adapter, &held).await;
        // A signal ends the run. A round that ended for any other reason — the
        // car went away and came back before the controllers were restored — is
        // owed the next one, which starts by waiting for the car again.
        if *stopping.borrow() || *gate.borrow() {
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
/// CarLife session, and the program is not stopping. Whoever owns the one
/// CarPlay slot is not part of the question — see the doc on `watch`.
/// `false` is one of those channels being gone, which is the program
/// shutting down.
async fn wait_ready(
    gate: &mut watch::Receiver<bool>,
    stopping: &mut watch::Receiver<bool>,
) -> bool {
    loop {
        // `borrow_and_update` on both: `changed()` must wait for the next
        // change, not answer at once for one this loop has already read.
        let car = *gate.borrow_and_update();
        let asked_to_stop = *stopping.borrow_and_update();
        if asked_to_stop {
            return false;
        }
        if car {
            return true;
        }
        tokio::select! {
            changed = gate.changed() => { if changed.is_err() { return false; } },
            changed = stopping.changed() => { if changed.is_err() { return false; } },
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
/// that waited for them from one that went on without them — and `None` when
/// the program was asked to stop while it was waiting, which is the one answer
/// that is not about the radios.
async fn wait_for_radios(cfg: &Machine, stopping: &mut watch::Receiver<bool>) -> Option<bool> {
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
            return Some(true);
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
            return Some(false);
        }
        // Waited on rather than slept through: a stop that arrives while the
        // boot is still loading modules is one the wireless half has nothing to
        // say about, and twenty seconds of silence is a stop that looks ignored.
        tokio::select! {
            () = sleep(RADIOS_POLL) => {}
            _ = stopping.changed() => {
                tracing::info!(
                    "main.radios_wait_stopped after_ms={}",
                    started.elapsed().as_millis()
                );
                return None;
            }
        }
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

/// How long the service waits for the access point. `S55ap` runs after this
/// program and starts hostapd itself, which needs 3.4 seconds to beacon once it
/// does. Ten is twice that for a start that is slow for its own reasons — a
/// driver that probes for a while, a card that is late on the bus. A boot that
/// is slower than that is served by the next round: the service is published
/// again every time the slot comes free, and a phone that could not join is
/// paged again with it.
const AP_CEILING: Duration = Duration::from_secs(10);

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
/// difference between the light's first two states. `None` when the program was
/// asked to stop inside this wait: a round that is dropped rather than published.
async fn wait_for_access_point(stopping: &mut watch::Receiver<bool>) -> Option<bool> {
    let started = Instant::now();
    let mut said = false;
    loop {
        if Path::new(AP_PID).exists() {
            tracing::info!(
                "wifi.ap_ready after_ms={}",
                started.elapsed().as_millis()
            );
            return Some(true);
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
            return Some(false);
        }
        // Same as the radio wait: ten seconds of not answering a stop is ten
        // seconds of a stop that looks ignored.
        tokio::select! {
            () = sleep(AP_POLL) => {}
            _ = stopping.changed() => {
                tracing::info!(
                    "wifi.ap_wait_stopped after_ms={}",
                    started.elapsed().as_millis()
                );
                return None;
            }
        }
    }
}
