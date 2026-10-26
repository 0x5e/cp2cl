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
use livi_runtime::bt::IncomingConn;
use livi_runtime::driver::spawn_link;
use livi_runtime::mfi_async::SharedCoprocessor;
use livi_runtime::vehicle::Vehicle;
use tokio::sync::broadcast;
use tokio::time::sleep;

use super::{page, parse_mac, power, radios_present, ready, run_session, short, Handover};
use crate::config::{bt_address, Config};
use crate::session::{carry_artwork, Ended};
use crate::sink::ClSink;
use crate::web::Status;

/// Keeps serving phones until the program stops, and publishes the service only
/// while the car has a CarLife session of its own: a phone that finds it
/// otherwise starts a CarPlay that nothing is reading — see `wait_car_up`.
///
/// Nothing pushes "a phone just paired" from outside, so a session that ends
/// puts us back to waiting for the same phone to come again.
pub async fn watch(
    cfg: Arc<Config>,
    auth: SharedCoprocessor,
    airplay_port: u32,
    accessory_key: String,
    restart: broadcast::Sender<()>,
    mut gate: tokio::sync::watch::Receiver<bool>,
    panel: Arc<ClSink>,
    status: Arc<Status>,
    vehicle: Arc<Vehicle>,
    handover: Arc<Handover>,
) {
    // The boot loads the two modules and goes on, so neither radio is here when
    // this starts: an interface appears once brcmfmac has taken its firmware, a
    // controller once btbcm has pushed its patch, and bluetoothd claims its name
    // on the bus after both. Waited for here rather than in `main`, because this
    // is the only part of the program with any use for them.
    wait_for_radios(&cfg).await;
    // The page was handed an address before there was a controller to ask, so
    // it has the fallback one. What the phone is told is read per session and
    // needs no help; this is the copy a person reads off the page.
    status.with(|s| s.carplay.bt_mac = bt_address(&cfg.bt_adapter));

    if let Err(e) = ready(&cfg) {
        tracing::error!("wireless.ap_unusable err={e}");
        return;
    }

    // The iAP service is what tells the phone there is CarPlay here, so it is
    // not published until there is: with CarLife in the picture the CarPlay half
    // only exists for as long as the car's session does.
    //
    // Said out loud, because the wait is silent and can last as long as the car
    // takes to turn up: a log that stops here looks like a run that failed
    // rather than one that is waiting.
    if !*gate.borrow() {
        tracing::info!("wireless.waiting_for_car consequence=nothing_published");
    }

    // The channel never closes while a round holds the connection that owns the
    // profiles, so being told to stop is the one way out of the loops below —
    // and the moment the controllers have to be put back. Made once rather than
    // per round; without the handler the default action still ends the process,
    // only without the putting back.
    let mut stop = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .inspect_err(|e| tracing::warn!("wireless.signal_handler_missing err={e} consequence=no_restore"))
        .ok();

    loop {
        // Nothing is published while the car has no session, and the profiles
        // come out with it: BlueZ's own auto-connect follows whatever is
        // registered, so a profile left up is a phone called back for a CarPlay
        // nobody is reading.
        if !wait_car_up(&mut gate).await {
            return;
        }
        tracing::info!("wireless.car_up action=publish_service");

        // The access point is a daemon the boot starts after this program, so
        // the network a phone is told to join may not be on air yet. It is named
        // in the start session either way, and a phone that is paged before its
        // network is there has nothing to come back to.
        wait_for_access_point().await;

        let (conn, mut incoming, held) =
            match livi_runtime::bt::start(&cfg.bt_adapter, &cfg.name, true).await {
                Ok(started) => started,
                Err(e) => {
                    tracing::error!(
                        "wireless.bt_start_failed adapter={} err={e} hint=\"is bluetoothd \
                         running, and is {} the adapter?\"",
                        cfg.bt_adapter,
                        cfg.bt_adapter
                    );
                    return;
                }
            };
        // The D-Bus connection owns the registered profiles. Dropping it
        // unregisters them, so it has to be kept alive for as long as we are
        // listening — it is also what puts the controllers back at the end.
        tracing::info!(
            "wireless.bt_service_up adapter={} name={:?} action=pair_phone",
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

        loop {
            // Only while nothing is on the link: a phone that is already coming
            // in does not need calling, and one that is mid-session must not be
            // paged again on top of it. And not at all while the phone has moved
            // its iAP2 session onto the CarPlay data stream — see `Handover`.
            let paging = tokio::spawn(page::run(
                conn.clone(),
                cfg.bt_adapter.clone(),
                handover.clone(),
            ));
            // The car's session ending is checked between sessions and never
            // inside one: `run_session` has tasks of its own, and dropping it
            // half way leaves them behind.
            let next = match &mut stop {
                Some(sig) => tokio::select! {
                    next = incoming.recv() => next,
                    _ = sig.recv() => None,
                    _ = wait_car_down(&mut gate) => None,
                },
                None => tokio::select! {
                    next = incoming.recv() => next,
                    _ = wait_car_down(&mut gate) => None,
                },
            };
            paging.abort();
            let Some(IncomingConn { fd, peer_mac }) = next else {
                break;
            };
            let peer = short(&peer_mac);
            tracing::info!("wireless.channel_opened peer={peer}");
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
                // The phone takes this as the largest packet it may send, and
                // this link carries small iAP2 messages: 4096 is what it has
                // been in use with. The cable and the tunnel keep the default,
                // where a cover need not be cut into 4 KB pieces.
                max_len: 4096,
                ..LinkConfig::default()
            };
            // Over RFCOMM the phone speaks first, so we must not open with a
            // negotiate of our own.
            let (channel, artwork) = spawn_link(fd, link, false);
            // The cover art is a file transfer of its own on the same link, and
            // it goes to the car's panel the same way the song it belongs to
            // does.
            let carrying = tokio::spawn(carry_artwork(artwork, panel.clone()));
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
                &panel,
                &status,
                &vehicle,
            )
            .await;
            carrying.abort();
            tracing::info!("wireless.channel_closed peer={peer} action=page_again");
            if let Ended::Restart = ended {
                tracing::info!("wireless.waiting peer={peer} action=wait_channel");
            }
        }

        // Nothing is listening any more: the profiles went with the channel, and
        // the controllers go back to the name and discoverability they were
        // found with. The power state is not among them — leaving it off costs
        // nothing, because the next start powers the controller on as it always
        // did.
        guard.abort();
        livi_runtime::bt::let_go(&conn, &cfg.bt_adapter, &held).await;
        // A car whose session is still up ended this round for another reason —
        // a signal, or a channel that closed and cannot be reopened — and the
        // loops above have nothing left to do. One that is down gets the round
        // it is owed, which starts by waiting for it to come back.
        if *gate.borrow() {
            tracing::info!(
                "wireless.bt_service_down adapter={} state=restored",
                cfg.bt_adapter
            );
            return;
        }
        tracing::info!(
            "wireless.bt_service_down adapter={} reason=car_session_ended",
            cfg.bt_adapter
        );
    }
}

/// Waits until the car has a CarLife session, and says whether it is still going
/// to be told about one.
///
/// The CarPlay half exists only for as long as the car's session does, so this
/// is the gate the service is published behind. `false` is the channel that
/// carries the answer being gone, which is the program shutting down.
async fn wait_car_up(gate: &mut tokio::sync::watch::Receiver<bool>) -> bool {
    loop {
        if *gate.borrow_and_update() {
            return true;
        }
        if gate.changed().await.is_err() {
            return false;
        }
    }
}

/// Waits until the car's CarLife session is over, which is the same condition
/// seen from inside one: while it lasts there is a service to publish, and
/// after it nothing to publish it for.
async fn wait_car_down(gate: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if !*gate.borrow_and_update() {
            return;
        }
        if gate.changed().await.is_err() {
            return;
        }
    }
}

/// This machine's Bluetooth address, which the phone checks against the one it
/// paired with. Read at the start of every session rather than taken from the
/// configuration at startup: a controller registers itself with the kernel
/// whenever it is brought up, and a session that begins after that is a session
/// that can have the real address.
fn local_mac(cfg: &Config) -> [u8; 6] {
    parse_mac(&bt_address(&cfg.bt_adapter))
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
/// the line `radio.incomplete` rather than taking them down with it.
async fn wait_for_radios(cfg: &Config) {
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
            tracing::info!(
                "radio.ready iface={} adapter={} after_ms={}",
                cfg.wifi_iface,
                cfg.bt_adapter,
                started.elapsed().as_millis()
            );
            return;
        }
        // Once per change rather than once per poll: the first round says what
        // the boot has not got round to yet, and the next says what is left.
        if said.as_deref() != Some(missing.as_str()) {
            tracing::info!("radio.waiting missing={missing}");
            said = Some(missing.clone());
        }
        if started.elapsed() >= RADIOS_CEILING {
            tracing::warn!(
                "radio.incomplete after_ms={} missing={missing} consequence=wireless_will_fail",
                started.elapsed().as_millis()
            );
            return;
        }
        sleep(RADIOS_POLL).await;
    }
}

/// What is not up yet, named as the log and the loop both want it.
fn missing(cfg: &Config, wifi: bool, bt: bool, bluez: bool) -> String {
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
async fn wait_for_access_point() {
    let started = Instant::now();
    let mut said = false;
    loop {
        if Path::new(AP_PID).exists() {
            tracing::info!(
                "wireless.ap_ready after_ms={}",
                started.elapsed().as_millis()
            );
            return;
        }
        if !said {
            tracing::info!("wireless.ap_waiting pid_file={AP_PID}");
            said = true;
        }
        if started.elapsed() >= AP_CEILING {
            tracing::warn!(
                "wireless.ap_absent after_ms={} consequence=phone_told_a_network_that_is_not_on_air",
                started.elapsed().as_millis()
            );
            return;
        }
        sleep(AP_POLL).await;
    }
}
