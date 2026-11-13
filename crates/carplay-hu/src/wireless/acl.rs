//! When a paired phone's link comes up and goes down.
//!
//! BlueZ keeps this as a property of its own — `Connected` on the device object
//! — and the page reads it every round to decide whether to call. Reading it is
//! not the same as knowing when it changed: a round is due every few seconds
//! and is late by the length of whatever call it was waiting on, so a value
//! first seen at 24.1 s may have become true anywhere back to 21.1 s. That is
//! the question here, which is why it is a signal and not one more read of the
//! same property.
//!
//! The link is not the channel on it. This is the baseband connection and it
//! says the phone woke; the iAP2 channel is `bt.channel_opened`, and how far
//! apart the two are is how long the phone took afterwards.

use std::collections::HashMap;

use futures_util::TryStreamExt;
use zbus::message::Type;
use zbus::zvariant::OwnedValue;
use zbus::{Connection, MatchRule, Message, MessageStream};

/// The interface the watched property belongs to. One object per device, all of
/// them on the same daemon.
const DEVICE: &str = "org.bluez.Device1";

/// Watches `Connected` for as long as the connection is there.
pub(super) async fn watch(conn: Connection) {
    // The standard signal rather than one of the device's own: the daemon emits
    // it on the device's path with the interface it is about as its first
    // argument, which is what tells a phone from the adapter beside it.
    let rule = match MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.bluez")
        .and_then(|rule| rule.member("PropertiesChanged"))
    {
        Ok(rule) => rule.build(),
        Err(e) => {
            tracing::warn!("bt.link_watch state=unavailable reason=rule err={e}");
            return;
        }
    };
    let mut stream = match MessageStream::for_match_rule(rule, &conn, None).await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::warn!("bt.link_watch state=unavailable reason=stream err={e}");
            return;
        }
    };
    // Said once, because the failure this rules out is a silent one: a log with
    // no link events under this line means no phone came, and under nothing at
    // all means nobody was listening.
    tracing::info!("bt.link_watch state=armed");

    loop {
        let message = match stream.try_next().await {
            Ok(Some(message)) => message,
            // Either the stream is finished or the bus said something that is
            // not a message we can read; nothing more arrives either way.
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("bt.link_watch state=ended err={e}");
                return;
            }
        };
        let Some((connected, mac)) = link_change(&message) else {
            continue;
        };
        tracing::info!("bt.acl_{} mac={mac}", if connected { "up" } else { "down" });
    }
}

/// Whether one signal is a phone's link changing, and which phone.
///
/// `None` for the daemon's other property announcements — an adapter's, a
/// battery's, or a device property that is not this one — which arrive on the
/// same rule because they are all the same signal.
fn link_change(message: &Message) -> Option<(bool, String)> {
    let (interface, changed, _) = message
        .body()
        .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
        .ok()?;
    if interface != DEVICE {
        return None;
    }
    let connected = bool::try_from(changed.get("Connected")?.try_clone().ok()?).ok()?;
    // Held in a binding of its own: `header()` hands back a value, and the path
    // it names borrows from it.
    let header = message.header();
    let path = header.path()?;
    let (_, tail) = path.as_str().split_once("/dev_")?;
    Some((connected, super::page::mac(tail)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    /// One `PropertiesChanged`, as BlueZ sends them: a body of the interface it
    /// is about, the properties that changed, and the ones that went away.
    fn signal(interface: &str, changed: &[(&str, Option<bool>)]) -> Message {
        let mut properties: HashMap<&str, Value> = HashMap::new();
        for (name, connected) in changed {
            properties.insert(
                name,
                match connected {
                    Some(connected) => Value::from(*connected),
                    None => Value::from(0u8),
                },
            );
        }
        let body = (interface, properties, Vec::<&str>::new());
        Message::signal(
            "/org/bluez/hci0/dev_F4_39_A6_74_51_AF",
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
        )
        .expect("a signal")
        .build(&body)
        .expect("a body")
    }

    #[test]
    fn a_link_coming_up_names_the_phone() {
        let message = signal(DEVICE, &[("Connected", Some(true))]);
        assert_eq!(
            link_change(&message),
            Some((true, "F4:39:A6:74:51:AF".to_string()))
        );
    }

    #[test]
    fn a_link_going_down_is_the_same_signal() {
        let message = signal(DEVICE, &[("Connected", Some(false))]);
        assert_eq!(
            link_change(&message),
            Some((false, "F4:39:A6:74:51:AF".to_string()))
        );
    }

    /// The rule is on the signal, not on the object, so everything the daemon
    /// announces arrives here.
    #[test]
    fn another_interface_is_not_a_link() {
        let message = signal("org.bluez.Adapter1", &[("Connected", Some(true))]);
        assert_eq!(link_change(&message), None);
    }

    /// A device property that is not this one says nothing about the link.
    #[test]
    fn another_property_is_not_a_link() {
        let message = signal(DEVICE, &[("Paired", Some(true))]);
        assert_eq!(link_change(&message), None);
    }

    /// A property of the wrong type is not worth an error, and certainly not a
    /// link state.
    #[test]
    fn a_property_that_is_not_a_flag_is_ignored() {
        let message = signal(DEVICE, &[("Connected", None)]);
        assert_eq!(link_change(&message), None);
    }
}
