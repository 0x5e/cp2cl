//! Calling a paired phone back.
//!
//! Publishing the iAP service makes this machine findable; it does not make the
//! phone come. An iPhone does not reconnect to a wireless CarPlay accessory by
//! itself — the accessory pages it — and nothing we call does that for us, so
//! without this the log stops at `bt.service_up` and stays there until
//! someone taps the machine in the phone's Bluetooth settings.
//!
//! The page is the phone's own iAP2 service: BlueZ dials the channel the phone's
//! service record names, and the socket comes back through the same profile
//! callback an incoming connection does — see `linux`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zbus::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

use super::Handover;
use crate::admission::Side;

/// What the iPhone advertises for its own side of iAP2. Connecting to it is what
/// wakes the phone; the channel that follows is the one we take.
const IAP_CLIENT_UUID: &str = "00000000-deca-fade-deca-deafdecacafe";

/// Between two rounds of paging.
const ROUND: Duration = Duration::from_secs(5);
/// How long one phone is given to answer. Out of range nothing answers, and the
/// next round is a moment away.
const ANSWER: Duration = Duration::from_secs(15);
/// How long a phone may sit on the link without opening a channel. Past this it
/// is let go, because a link with nothing on it is a phone that will not come in
/// on its own: dropping it is what makes it page us again.
const STALE_SECS: u64 = 10;
const STALE: Duration = Duration::from_secs(STALE_SECS);
/// How many times a phone may refuse a page in a row before its entry here is
/// dropped. A phone refuses by having deleted its half of the pairing, and no
/// number of further pages brings a link key back: the address stays paired on
/// this side, so every round after that fails the same way and says nothing
/// above debug. A phone that is merely away times out instead, which is a
/// different error and is not counted here.
const REFUSALS: u32 = 3;

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
pub(super) async fn run(
    conn: Connection,
    adapter: String,
    handover: Arc<Handover>,
    mut slot: tokio::sync::watch::Receiver<Option<Side>>,
    car: tokio::sync::watch::Receiver<bool>,
) {    // A page that took its time must not be followed by a burst of retries, which
    // is what a missed tick would otherwise produce.
    let mut round = tokio::time::interval(ROUND);
    round.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // When each phone was first seen on the link with no session on it.
    let mut waiting: HashMap<String, Instant> = HashMap::new();
    // How many times each phone has refused a page in a row, since the pairing
    // was last seen working. At `REFUSALS` the address is unpaired rather than
    // called again, which is what takes it out of the list above.
    let mut refused: HashMap<String, u32> = HashMap::new();
    // Which state the page is being held by, said once on the way in and
    // once on the way out: five seconds apart, the lines would be the only
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
        // Nor does it keep its refusals: the count is about this pairing, and
        // one that is gone is not the one the next page is for.
        refused.retain(|mac, _| macs.contains(mac));
        if to_call.as_ref() != Some(&macs) {
            tracing::info!("bt.paired_phones count={} macs={macs:?}", macs.len());
            to_call = Some(macs);
        }
        for Phone { path, mac, connected } in phones {
            // A link is not a pairing. `Connected` goes up and down while the
            // controller retries one that keeps failing — the same attempt a
            // moment earlier reports `br-connection-already-connected` — and
            // clearing the count here meant three refusals never became three
            // in a row: every round that found the link up started from zero
            // again and the phone was called for ever. Only a page the phone
            // answered says the pairing works.
            if !connected {
                waiting.remove(&mac);
                match call(&conn, &path, &mac).await {
                    Answer::Answered => {
                        refused.remove(&mac);
                    }
                    Answer::Unanswered => {}
                    // BlueZ was not refusing the phone, it was answering from
                    // an attempt it still held. Taken down here so the next
                    // round is a fresh one rather than another answer from the
                    // same leftover.
                    Answer::Lingering => {
                        tracing::debug!("bt.page_cleared mac={mac} action=disconnect");
                        let _ = disconnect(&conn, &path).await;
                    }
                    Answer::Refused => {
                        let count = refused.entry(mac.clone()).or_insert(0);
                        *count += 1;
                        let times = *count;
                        if times >= REFUSALS {
                            refused.remove(&mac);
                            match unpair(&conn, &adapter, &path).await {
                                Ok(()) => tracing::warn!(
                                    "bt.page_given_up mac={mac} refused={times} action=unpaired"
                                ),
                                Err(e) => tracing::warn!(
                                    "bt.page_given_up mac={mac} refused={times} \
                                     action=unpair_failed err={e}"
                                ),
                            }
                        }
                    }
                }
                continue;
            }
            let on_link = waiting.entry(mac.clone()).or_insert_with(Instant::now).elapsed();
            if on_link < STALE {
                // On the link but no channel yet: asking for the profile again is
                // what LIVI does in the same state, and it costs one round trip.
                let _ = call(&conn, &path, &mac).await;
                continue;
            }
            waiting.remove(&mac);
            tracing::info!("bt.stale_peer mac={mac} idle_s={STALE_SECS} action=drop");
            let _ = tokio::time::timeout(ANSWER, disconnect(&conn, &path)).await;
        }
    }
}

/// What a page came to. The kinds are apart because only one of them is worth
/// counting: a phone that is out of range times out, and giving up on it after
/// three rounds would be giving up on a phone that was merely not there.
enum Answer {
    Answered,
    /// The phone refused the link: it has no pairing for us any more.
    Refused,
    /// The attempt ended with BlueZ still holding it — see `lingering`.
    Lingering,
    Unanswered,
}

/// Wakes one phone, or asks one that is already on the link for its iAP2 service
/// again, and says what came of it.
async fn call(conn: &Connection, path: &str, mac: &str) -> Answer {
    // How long the phone took to answer, which is the only thing that says when
    // the call started: a round that ends in `bt.page_answered` reads the same
    // whether it began a moment ago or was still hanging from the round before,
    // and the two mean opposite things about the phone.
    let started = Instant::now();
    let elapsed = || started.elapsed().as_millis();
    match tokio::time::timeout(ANSWER, page(conn, path)).await {
        Ok(Ok(())) => {
            tracing::info!("bt.page_answered mac={mac} elapsed_ms={}", elapsed());
            Answer::Answered
        }
        Ok(Err(e)) => {
            tracing::debug!(
                "bt.page_unanswered mac={mac} elapsed_ms={} err={e}",
                elapsed()
            );
            if refused(&e) {
                Answer::Refused
            } else if lingering(&e) {
                Answer::Lingering
            } else {
                Answer::Unanswered
            }
        }
        Err(_) => {
            tracing::debug!(
                "bt.page_deferred mac={mac} elapsed_ms={} reason=timeout",
                elapsed()
            );
            Answer::Unanswered
        }
    }
}

/// Whether BlueZ's error says the phone refused the link rather than missed it.
///
/// `br-connection-refused` is the name the kernel's `mgmt` layer gives the HCI
/// status of the same meaning, and BlueZ puts it in the error it returns — so
/// the string is the whole of what tells a phone that deleted its pairing apart
/// from one that is a room away.
fn refused(e: &zbus::Error) -> bool {
    e.to_string().contains("br-connection-refused")
}

/// Whether BlueZ's error says the attempt ended with something still held,
/// rather than with nothing on the other end.
///
/// The two names are from BlueZ's own error table. `br-connection-canceled` is
/// an attempt that was given up on; `br-connection-already-connected` is EALREADY
/// and its stated meaning is that the profile or the link is already there — so
/// neither is a phone that missed a page, and neither leaves the next round a
/// clean one. A page that times out is not one of these: nothing was ever up,
/// and there is nothing to take down.
///
/// Left alone, they are what a boot can spend half a minute on: one page is
/// cancelled, and every round after it is answered from the attempt that is
/// still held instead of being a page at all.
fn lingering(e: &zbus::Error) -> bool {
    let text = e.to_string();
    text.contains("br-connection-already-connected") || text.contains("br-connection-canceled")
}

/// One page: the link, and the phone's iAP2 service on top of it.
async fn page(conn: &Connection, path: &str) -> Result<(), zbus::Error> {
    conn.call_method(
        Some("org.bluez"),
        path,
        Some("org.bluez.Device1"),
        "ConnectProfile",
        &(IAP_CLIENT_UUID,),
    )
    .await
    .map(|_| ())
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
