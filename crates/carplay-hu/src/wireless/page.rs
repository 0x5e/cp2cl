//! Calling a paired phone back.
//!
//! Publishing the iAP service makes this machine findable; it does not make the
//! phone come. An iPhone does not reconnect to a wireless CarPlay accessory by
//! itself — the accessory pages it — and nothing we call does that for us, so
//! without this the log stops at `bt.service_up` and stays there until
//! someone taps the machine in the phone's Bluetooth settings.
//!
//! The page is the phone's own iAP2 service: the channel the phone's service
//! record names is dialled directly, and the socket that comes back is the one
//! the session runs on — see `dial`.

use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use livi_runtime::bt::IncomingConn;
use tokio::sync::mpsc;
use zbus::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

use super::{Handover, dial};
use crate::admission::Side;

/// Between two rounds of paging.
///
/// How long this is decides how late the one attempt that gets through can be:
/// the phone becomes answerable at a moment of its own, and the next round after
/// it is the one that works, so up to this much is spent waiting for a round.
/// A failed attempt costs 0.6–1.2 s when the phone is not answering yet — there
/// is no link to set up and nothing to tear down — so the rest of a five-second
/// round was idle: every success in the logs sat exactly on that grid after a
/// four-second gap. Three seconds cuts that gap to two, and the cost is one more
/// call per round to a phone that is already being called.
const ROUND: Duration = Duration::from_secs(3);
/// How long one phone is given to answer. Out of range nothing answers, and the
/// next round is a moment away.
const ANSWER: Duration = Duration::from_secs(15);
/// How long a phone may sit on the link without opening a channel. Past this it
/// is let go, because a link with nothing on it is a phone that will not come in
/// on its own: dropping it is what makes it page us again.
const STALE_SECS: u64 = 10;
const STALE: Duration = Duration::from_secs(STALE_SECS);

/// How many times a boot asks the kernel what it is holding for a phone whose
/// call came back `EALREADY`. Three is enough to tell one stuck entry from a
/// state that clears: the answer does not change while it lasts, and a boot that
/// has been waiting a minute has already said everything there is to say.
const BUSY_PROBES: u32 = 3;

/// What a removal from BlueZ's tree came back as, at every step.
type Failure = Box<dyn std::error::Error + Send + Sync>;

/// How many times a removal BlueZ called busy is tried again, and how long
/// between the tries.
///
/// BlueZ will not remove a device it is connecting to, nor one that is still on
/// the link, and answers `Resource busy`. The disconnect in `unpair` is the
/// whole answer to the second case; the first runs its course in a moment, so
/// this waits it out rather than handing the caller a failure it cannot act on.
const REMOVE_TRIES: u32 = 5;
const REMOVE_PAUSE: Duration = Duration::from_millis(500);

/// BlueZ's object tree, as `GetManagedObjects` returns it.
type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

/// A device worth calling: where BlueZ keeps it, and whether it is on the link.
struct Phone {
    path: String,
    mac: String,
    connected: bool,
}

/// Pages every paired phone, again and again, until the caller stops the task.
///
/// `slot` is who owns the one CarPlay session: while the cable does, the page
/// is held, because the phone on the cable is being served and a page woken now
/// is a second `CarPlayStartSession` that the session on the cable cannot
/// survive — see `Handover` for the same pause over the Wi-Fi leg.
///
/// `car` is whether the car still has a CarLife session, which is what the phone
/// is being called back for: a phone woken for a session nobody reads starts a
/// CarPlay that goes nowhere, and leaves it on the car's screen to be dismissed.
///
/// `dialed` is where the channel goes when a call comes back with one. This task
/// ends on that call: a channel is `linux`'s to serve, and a second round in
/// here would only be a second channel.
pub(super) async fn run(
    conn: Connection,
    adapter: String,
    handover: Arc<Handover>,
    mut slot: tokio::sync::watch::Receiver<Option<Side>>,
    car: tokio::sync::watch::Receiver<bool>,
    dialed: mpsc::UnboundedSender<IncomingConn>,
) {
    // A page that took its time must not be followed by a burst of retries, which
    // is what a missed tick would otherwise produce.
    let mut round = tokio::time::interval(ROUND);
    round.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // When each phone was first seen on the link with no session on it.
    let mut waiting: HashMap<String, Instant> = HashMap::new();
    // How many of those probes have been made this boot — see `BUSY_PROBES`.
    let mut probes: u32 = 0;
    // Which state the page is being held by, said once on the way in and
    // once on the way out: every round apart, the lines would be the only
    // thing in the log for as long as the state lasts.
    let mut held: Option<&'static str> = None;
    // What the last round had to call. An empty list is the one state that
    // leaves the log at `bt_service_up` with nothing after it, and telling that
    // apart from a phone that is merely away — which does log, one line per
    // round — is the whole question such a log asks.
    let mut to_call: Option<Vec<String>> = None;
    loop {
        round.tick().await;
        // The loop that spawns this task stops it as soon as the car goes, but
        // the first round is due the moment it starts: without this, the round
        // that began just before the car's session ended would still call the
        // phone once. Checked rather than reported, because it can only be true
        // for the one round — `linux` says the same thing as `bt_service_down`.
        if !*car.borrow() {
            continue;
        }
        // The phone took its iAP2 session to Wi-Fi and asked us to drop
        // Bluetooth. Calling it back now would give it a second session and a
        // second `CarPlayStartSession`, which the one on Wi-Fi cannot survive —
        // see `Handover`.
        if handover.live() {
            if held.is_none() {
                held = Some("handover");
                tracing::info!("bt.page_held reason=handover");
            }
            continue;
        }
        // The cable is serving the phone. The one on the cable has its session,
        // and waking it again is a second one the cable's session cannot
        // survive; a phone that is only paired — not served — is one this round
        // will reach once the cable's turn is over, which is why the page waits
        // the turn out rather than going away.
        if *slot.borrow_and_update() == Some(Side::Usb) {
            if held.is_none() {
                held = Some("usb_session");
                tracing::info!("bt.page_held reason=usb_session");
            }
            continue;
        }
        if let Some(reason) = held {
            held = None;
            tracing::info!("bt.page_resumed reason={reason}_cleared");
        }
        let phones = match paired(&conn, &adapter).await {
            Ok(phones) => phones,
            Err(e) => {
                tracing::debug!("bt.paired_list_failed err={e}");
                continue;
            }
        };
        // A phone that left keeps no timer: one that comes back starts over.
        waiting.retain(|mac, _| phones.iter().any(|p| &p.mac == mac));
        let macs: Vec<String> = phones.iter().map(|p| p.mac.clone()).collect();
        if to_call.as_ref() != Some(&macs) {
            tracing::info!("bt.paired_phones count={} macs={macs:?}", macs.len());
            to_call = Some(macs);
        }
        for Phone { path, mac, connected } in phones {
            // A link with nothing on it is a phone that will not open a channel
            // on its own, and one left up is what the next call is answered
            // from instead of being a page.
            if connected {
                let on_link = waiting.entry(mac.clone()).or_insert_with(Instant::now).elapsed();
                if on_link >= STALE {
                    waiting.remove(&mac);
                    tracing::info!("bt.stale_peer mac={mac} idle_s={STALE_SECS} action=drop");
                    let _ = tokio::time::timeout(ANSWER, disconnect(&conn, &path)).await;
                    continue;
                }
            } else {
                waiting.remove(&mac);
            }
            match wake(&mac).await {
                Ok(fd) => {
                    // `linux` owns the channel from here: this task is dropped
                    // on the way into it, and a call after this one would be a
                    // second channel to the same phone.
                    let _ = dialed.send(IncomingConn { fd, peer_mac: mac });
                    return;
                }
                // `EALREADY`: the controller still has an attempt to this
                // address open, so this round never reached the phone, and the
                // rounds after it are answered from that same entry until it
                // clears on its own — retrying is not what clears it. No link is
                // up while it lasts, which is why nothing else in the log shows
                // it; the kernel's connection table is the only place it can be
                // seen, and it says whether the entry is ours at all.
                Err(e) if busy(&e) => {
                    if probes < BUSY_PROBES {
                        probes += 1;
                        tracing::info!("bt.dial_busy mac={mac} acl={}", dial::held(&mac));
                    }
                }
                // A phone that is away, or one whose record could not be read:
                // the next round asks again.
                Err(_) => {}
            }
        }
    }
}

/// Calls one phone and hands back the channel the call opened.
///
/// Off the runtime, because a call to a phone that is away is fifteen seconds of
/// waiting and nothing else in this program is waiting on it.
async fn wake(mac: &str) -> Result<OwnedFd, io::Error> {
    let started = Instant::now();
    let target = mac.to_string();
    match tokio::task::spawn_blocking(move || dial::open(&target, ANSWER)).await {
        Ok(Ok(dialed)) => {
            tracing::info!(
                "bt.dial_ok mac={mac} channel={} sdp_ms={} connect_ms={} elapsed_ms={}",
                dialed.channel,
                dialed.sdp.as_millis(),
                dialed.connect.as_millis(),
                started.elapsed().as_millis()
            );
            Ok(dialed.fd)
        }
        Ok(Err(e)) if e.kind() == io::ErrorKind::NotFound => {
            // A board without `sdptool` is a board that cannot make this call
            // at all, and one line per round is worth having at warn: nothing
            // else in this path says so.
            tracing::warn!("bt.dial_failed mac={mac} err={e} consequence=no_sdptool");
            Err(e)
        }
        Ok(Err(e)) => {
            tracing::debug!(
                "bt.dial_failed mac={mac} elapsed_ms={} err={e}",
                started.elapsed().as_millis()
            );
            Err(e)
        }
        // The call was dropped with the task that ran it, which says nothing
        // about the phone.
        Err(e) => {
            tracing::debug!("bt.dial_failed mac={mac} err={e}");
            Err(io::Error::other(e))
        }
    }
}

/// Whether the call was answered by the controller rather than by the phone.
///
/// `EALREADY` is `connect` saying an attempt to this address is already open.
/// The phone was not asked anything at all.
fn busy(e: &io::Error) -> bool {
    e.to_string().contains("already in progress")
}

async fn disconnect(conn: &Connection, path: &str) -> Result<(), zbus::Error> {
    conn.call_method(Some("org.bluez"), path, Some("org.bluez.Device1"), "Disconnect", &())
        .await
        .map(|_| ())
}

/// Every device BlueZ knows as paired on this controller.
///
/// Paired is the whole test: the only devices that are paired with this machine
/// are the ones an operator paired to use it. Whether one is on the link is part
/// of the answer rather than a reason to drop it — a link with no session on it
/// is a state to get out of, not one to ignore.
async fn paired(
    conn: &Connection,
    adapter: &str,
) -> Result<Vec<Phone>, Box<dyn std::error::Error + Send + Sync>> {
    Ok(wanted(&managed(conn).await?, adapter))
}

/// Everything BlueZ has an object for: every adapter, and every device it
/// remembers.
async fn managed(conn: &Connection) -> Result<Objects, Box<dyn std::error::Error + Send + Sync>> {
    let reply = conn
        .call_method(
            Some("org.bluez"),
            "/",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .await?;
    Ok(reply.body().deserialize()?)
}

/// Unpairs every phone known to this controller, and says how many went.
///
/// A pairing lives in two places and only one of them is ours: a phone that was
/// unpaired on its own side keeps its entry here, and BlueZ will not start a
/// pairing it believes is already done. Handing the entry back is what gives the
/// phone something to pair with again — it is the same thing the phone's own
/// "Forget This Device" does from the other end.
///
/// One address that will not go does not stop the rest: a press that clears
/// three phones but leaves the fourth is worth more than one that clears none,
/// and the one that failed is named rather than counted. The total is what
/// went, so a caller that reads zero knows nothing was cleared.
pub async fn unpair_all(adapter: &str) -> Result<usize, Failure> {
    let conn = Connection::system().await?;
    let mut removed = 0;
    for phone in wanted(&managed(&conn).await?, adapter) {
        match unpair(&conn, adapter, &phone.path).await {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::warn!("bt.unpair_failed adapter={adapter} mac={} err={e}", phone.mac);
            }
        }
    }
    Ok(removed)
}

/// Removes one device, which takes its link key with it. The pairing is what is
/// useless, not the phone: nothing here decides a phone is unwelcome, only that
/// the key beside this address no longer opens anything.
///
/// The link is dropped first, and that is not a courtesy: BlueZ refuses to
/// remove a device that is still connected, and a press taken while a phone is
/// on the link is the ordinary case — see `REMOVE_TRIES` for the half-way state
/// that has to be waited out.
async fn unpair(conn: &Connection, adapter: &str, path: &str) -> Result<(), Failure> {
    // Nothing to drop on an idle address, and an error here says only that.
    let _ = disconnect(conn, path).await;
    let mut busy: Option<Failure> = None;
    for attempt in 0..REMOVE_TRIES {
        if attempt > 0 {
            tokio::time::sleep(REMOVE_PAUSE).await;
        }
        match remove(conn, adapter, path).await {
            Ok(()) => return Ok(()),
            Err(e) if occupied(&e) => busy = Some(e),
            Err(e) => return Err(e),
        }
    }
    match busy {
        Some(e) => Err(e),
        // Unreachable: the loop above leaves only by returning or by recording
        // the error that kept it going.
        None => Ok(()),
    }
}

/// One `Adapter1.RemoveDevice`, the only thing that takes a link key with it.
async fn remove(conn: &Connection, adapter: &str, path: &str) -> Result<(), Failure> {
    let device = ObjectPath::try_from(path)?;
    conn.call_method(
        Some("org.bluez"),
        format!("/org/bluez/{adapter}").as_str(),
        Some("org.bluez.Adapter1"),
        "RemoveDevice",
        &(device,),
    )
    .await?;
    Ok(())
}

/// Whether BlueZ's error says the address is still occupied rather than gone.
///
/// It has two ways of saying it, and both come back through the same `Failed`:
/// `Resource busy` for a device that is still connected, and `InProgress` for
/// one whose connection is being set up.
fn occupied(e: &Failure) -> bool {
    let text = e.to_string().to_lowercase();
    text.contains("busy") || text.contains("in progress")
}

/// The devices worth calling, each with the address it is known by.
fn wanted(objects: &Objects, adapter: &str) -> Vec<Phone> {
    let prefix = format!("/org/bluez/{adapter}/dev_");
    let mut phones: Vec<Phone> = objects
        .iter()
        .filter_map(|(path, interfaces)| {
            let device = interfaces.get("org.bluez.Device1")?;
            let tail = path.as_str().strip_prefix(&prefix)?;
            flag(device, "Paired").then(|| Phone {
                path: path.as_str().to_string(),
                mac: mac(tail),
                connected: flag(device, "Connected"),
            })
        })
        .collect();
    phones.sort_by(|a, b| a.path.cmp(&b.path));
    phones
}

fn flag(properties: &HashMap<String, OwnedValue>, key: &str) -> bool {
    properties
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| bool::try_from(v).ok())
        == Some(true)
}

/// `dev_AA_BB_CC_DD_EE_FF` → `AA:BB:CC:DD:EE:FF`, for a log line.
pub(super) fn mac(tail: &str) -> String {
    tail.strip_prefix("dev_").unwrap_or(tail).replace('_', ":").to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn objects() -> Objects {
        let mut objects = Objects::new();
        let mut add = |path: &str, paired: bool, connected: bool| {
            let mut device = HashMap::new();
            device.insert("Paired".to_string(), OwnedValue::from(paired));
            device.insert("Connected".to_string(), OwnedValue::from(connected));
            let mut interfaces = HashMap::new();
            interfaces.insert("org.bluez.Device1".to_string(), device);
            objects.insert(OwnedObjectPath::try_from(path).expect("a valid path"), interfaces);
        };
        add("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01", true, false);
        add("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_02", true, true);
        add("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_03", false, false);
        add("/org/bluez/hci1/dev_AA_BB_CC_DD_EE_04", true, false);
        objects
    }

    #[test]
    fn only_a_paired_device_on_our_controller_is_listed() {
        let macs: Vec<String> = wanted(&objects(), "hci0").into_iter().map(|p| p.mac).collect();
        assert_eq!(macs, ["AA:BB:CC:DD:EE:01", "AA:BB:CC:DD:EE:02"]);
    }

    #[test]
    fn a_device_on_the_link_is_still_listed_and_says_so() {
        let phones = wanted(&objects(), "hci0");
        assert!(!phones[0].connected);
        assert!(phones[1].connected);
        assert_eq!(phones[1].path, "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_02");
    }
}
