//! Calling a paired phone back.
//!
//! Publishing the iAP service makes this machine findable; it does not make the
//! phone come. An iPhone does not reconnect to a wireless CarPlay accessory by
//! itself — the accessory pages it — and nothing we call does that for us, so
//! without this the log stops at `wireless.bt_service_up` and stays there until
//! someone taps the machine in the phone's Bluetooth settings.
//!
//! The page is the phone's own iAP2 service: BlueZ dials the channel the phone's
//! service record names, and the socket comes back through the same profile
//! callback an incoming connection does — see `linux`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::Handover;

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

/// BlueZ's object tree, as `GetManagedObjects` returns it.
type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

/// A device worth calling: where BlueZ keeps it, and whether it is on the link.
struct Phone {
    path: String,
    mac: String,
    connected: bool,
}

/// Pages every paired phone, again and again, until the caller stops the task.
pub(super) async fn run(conn: Connection, adapter: String, handover: Arc<Handover>) {
    // A page that took its time must not be followed by a burst of retries, which
    // is what a missed tick would otherwise produce.
    let mut round = tokio::time::interval(ROUND);
    round.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // When each phone was first seen on the link with no session on it.
    let mut waiting: HashMap<String, Instant> = HashMap::new();
    // Said when the state changes rather than once a round: five seconds apart,
    // the line would be the only thing in the log for as long as a tunnel lasts.
    let mut held = false;
    // What the last round had to call. An empty list is the one state that
    // leaves the log at `bt_service_up` with nothing after it, and telling that
    // apart from a phone that is merely away — which does log, one line per
    // round — is the whole question such a log asks.
    let mut to_call: Option<Vec<String>> = None;
    loop {
        round.tick().await;
        // The phone took its iAP2 session to Wi-Fi and asked us to drop
        // Bluetooth. Calling it back now would give it a second session and a
        // second `CarPlayStartSession`, which the one on Wi-Fi cannot survive —
        // see `Handover`.
        if handover.live() {
            if !held {
                held = true;
                tracing::info!("wireless.page_held reason=handover");
            }
            continue;
        }
        if held {
            held = false;
            tracing::info!("wireless.page_resumed reason=handover_cleared");
        }
        let phones = match paired(&conn, &adapter).await {
            Ok(phones) => phones,
            Err(e) => {
                tracing::debug!("wireless.paired_list_failed err={e}");
                continue;
            }
        };
        // A phone that left keeps no timer: one that comes back starts over.
        waiting.retain(|mac, _| phones.iter().any(|p| &p.mac == mac));
        let macs: Vec<String> = phones.iter().map(|p| p.mac.clone()).collect();
        if to_call.as_ref() != Some(&macs) {
            tracing::info!("wireless.paired_phones count={} macs={macs:?}", macs.len());
            to_call = Some(macs);
        }
        for Phone { path, mac, connected } in phones {
            if !connected {
                waiting.remove(&mac);
                call(&conn, &path, &mac).await;
                continue;
            }
            let on_link = waiting.entry(mac.clone()).or_insert_with(Instant::now).elapsed();
            if on_link < STALE {
                // On the link but no channel yet: asking for the profile again is
                // what LIVI does in the same state, and it costs one round trip.
                call(&conn, &path, &mac).await;
                continue;
            }
            waiting.remove(&mac);
            tracing::info!("wireless.stale_peer mac={mac} idle_s={STALE_SECS} action=drop");
            let _ = tokio::time::timeout(ANSWER, disconnect(&conn, &path)).await;
        }
    }
}

/// Wakes one phone, or asks one that is already on the link for its iAP2 service
/// again, and says what came of it.
async fn call(conn: &Connection, path: &str, mac: &str) {
    match tokio::time::timeout(ANSWER, page(conn, path)).await {
        Ok(Ok(())) => tracing::info!("wireless.page_answered mac={mac}"),
        Ok(Err(e)) => tracing::debug!("wireless.page_unanswered mac={mac} err={e}"),
        Err(_) => tracing::debug!("wireless.page_deferred mac={mac} reason=timeout"),
    }
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
) -> Result<Vec<Phone>, Box<dyn std::error::Error>> {
    let reply = conn
        .call_method(
            Some("org.bluez"),
            "/",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .await?;
    let objects: Objects = reply.body().deserialize()?;
    Ok(wanted(&objects, adapter))
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
fn mac(tail: &str) -> String {
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
