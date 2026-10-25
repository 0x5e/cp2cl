//! Turning the controller off while the phone streams over Wi-Fi.
//!
//! This board's Wi-Fi and Bluetooth halves are one chip on one antenna, and an
//! access point on 2.4 GHz leaves the Bluetooth side almost no room. A phone
//! left sitting on its CarPlay page keeps paging the accessory it is bonded to,
//! and every page answered costs the stream a slice of the air: the video
//! stutters for as long as the phone stays on that page.
//!
//! Dropping `Discoverable` is not enough. A bonded phone does not look the
//! machine up again — it pages the address it already has — and page scanning
//! answers for as long as the controller is powered. So the controller is
//! powered down: BlueZ closes the HCI device, nothing answers anything, and the
//! radio stops taking the antenna away from the stream.
//!
//! When it goes is `Handover`: the phone has moved its iAP2 session onto the
//! CarPlay data stream and Bluetooth carries nothing worth keeping. When it
//! comes back is that gate clearing, which is the AirPlay session ending. A
//! phone that dropped off the network ends it within seconds of saying nothing
//! — see `PHONE_SILENT` in `crate::airplay::server` — so the machine is never
//! out of reach for much longer than that. Bringing the controller back is all
//! that is needed: `page` then calls the phone, exactly as it does after any
//! other session.

use std::sync::Arc;
use std::time::{Duration, Instant};

use zbus::Connection;
use zbus::zvariant::Value;

use super::Handover;

/// How often the gate is looked at. Short enough that the settle below is what
/// decides the timing, and cheap: a load and a comparison.
const TICK: Duration = Duration::from_millis(250);

/// How long the gate has to stay up before the controller is turned off.
///
/// Not on the first sighting: a stream can be accepted, torn down and accepted
/// again within a second when the phone reconnects, and a controller that
/// followed that would power-cycle the radio under the stream it is protecting.
const SETTLE_OFF: Duration = Duration::from_secs(3);

/// How long it has to stay down before the controller comes back.
///
/// The opposite asymmetry, and for the same reason in reverse: the machine is
/// unreachable while the controller is off, so it comes back as soon as the
/// session that made it go is over.
const SETTLE_ON: Duration = Duration::from_secs(1);

/// Powers the controller off and back on as the handover gate rises and falls.
///
/// Runs as its own task rather than inside the loop that waits for the phone:
/// that loop is inside `run_session` for the whole time the controller has to be
/// off, so nothing there would be reached while it matters.
pub(super) async fn guard(conn: Connection, adapter: String, handover: Arc<Handover>) {
    let path = format!("/org/bluez/{adapter}");
    // What the controller was last left in, which is not the same thing as what
    // the gate asks for: a call that failed must not read as if it had worked.
    let mut off = false;
    // The gate's state and when it last changed, which is what the settle is
    // measured from. Seeded from the gate rather than assumed.
    let mut seen = handover.live();
    let mut since = Instant::now();
    // A controller that will not go is retried on the settle; saying so at warn
    // level every time would put a line every few seconds in the log for as long
    // as the session lasts, so only the first attempt of a run is a warning.
    let mut complained = false;

    loop {
        tokio::time::sleep(TICK).await;
        let live = handover.live();
        if live != seen {
            seen = live;
            since = Instant::now();
            complained = false;
            continue;
        }
        let Some(off_wanted) = due(live, off, since.elapsed()) else {
            continue;
        };
        let outcome = if off_wanted {
            set(&conn, &path, "Powered", Value::from(false)).await
        } else {
            bring_back(&conn, &path).await
        };
        match outcome {
            Ok(()) => {
                off = off_wanted;
                complained = false;
                if off_wanted {
                    tracing::info!("wireless.bt_off adapter={adapter} reason=carplay_on_wifi");
                } else {
                    tracing::info!("wireless.bt_on adapter={adapter} reason=carplay_over");
                }
            }
            Err(e) => {
                // Retried after the same settle rather than every tick.
                since = Instant::now();
                let powered = !off_wanted;
                if complained {
                    tracing::debug!(
                        "wireless.bt_set_failed adapter={adapter} powered={powered} err={e}"
                    );
                } else {
                    complained = true;
                    let consequence = if off_wanted {
                        "page_answers_share_the_antenna"
                    } else {
                        "phone_cannot_be_paged_back"
                    };
                    tracing::warn!(
                        "wireless.bt_set_failed adapter={adapter} powered={powered} err={e} \
                         consequence={consequence}"
                    );
                }
            }
        }
    }
}

/// What the controller should be in, if a call is due at all.
///
/// `Some(off)` is the state to put it in — off while the gate is up — and
/// `None` means leave it alone: either it is already there, or the gate has
/// not said so for long enough to be believed. `steady_for` is how long the gate
/// has held the state it holds now.
fn due(live: bool, off: bool, steady_for: Duration) -> Option<bool> {
    if live == off {
        return None;
    }
    let settle = if live { SETTLE_OFF } else { SETTLE_ON };
    (steady_for >= settle).then_some(live)
}

/// Powers the controller back on, then restores what `watch` asked `bt::start`
/// for: discoverable and pairable while there is nothing on the link.
///
/// The two values are that call's, spelled again here because a power cycle is
/// the one thing BlueZ does not promise to carry them across, and a machine that
/// comes back invisible is one an operator cannot pair a phone to. Not being
/// able to set them is not fatal — the phone bonded to this machine is paged by
/// `page` and does not need to find it.
async fn bring_back(conn: &Connection, path: &str) -> Result<(), zbus::Error> {
    set(conn, path, "Powered", Value::from(true)).await?;
    for prop in ["Discoverable", "Pairable"] {
        if let Err(e) = set(conn, path, prop, Value::from(true)).await {
            tracing::warn!("wireless.bt_prop_failed prop={prop} err={e}");
        }
    }
    Ok(())
}

/// One `org.bluez.Adapter1` property.
async fn set(
    conn: &Connection,
    path: &str,
    name: &str,
    value: Value<'_>,
) -> Result<(), zbus::Error> {
    conn.call_method(
        Some("org.bluez"),
        path,
        Some("org.freedesktop.DBus.Properties"),
        "Set",
        &("org.bluez.Adapter1", name, value),
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_that_has_held_long_enough_asks_for_the_controller_to_go() {
        assert_eq!(due(true, false, SETTLE_OFF), Some(true));
    }

    #[test]
    fn a_stream_that_only_just_started_asks_for_nothing_yet() {
        assert_eq!(due(true, false, SETTLE_OFF - TICK), None);
    }

    #[test]
    fn the_controller_comes_back_sooner_than_it_went() {
        // A session that ended a second ago is reason enough to be reachable
        // again, where a session that started a second ago is not reason enough
        // to take the radio away.
        assert_eq!(due(false, true, SETTLE_ON), Some(false));
        assert_eq!(due(false, true, SETTLE_ON - TICK), None);
    }

    #[test]
    fn a_controller_already_where_the_gate_wants_it_is_left_alone() {
        assert_eq!(due(true, true, SETTLE_OFF), None);
        assert_eq!(due(false, false, SETTLE_OFF), None);
    }
}
