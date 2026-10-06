//! Linux: BlueZ publishes the service and hands us the phone's RFCOMM socket.
//!
//! This is LIVI's `livi_runtime::bt` doing all of it — registering the iAP
//! server profile, the iAP client profile with `AutoConnect` (so a phone that
//! has paired before is paged back on its own), the CarPlay profile, and an
//! agent that answers the pairing prompt. All that is left for us is draining
//! the channel of sockets it produces.

use std::sync::Arc;

use iap2_link::LinkConfig;
use livi_runtime::bt::IncomingConn;
use livi_runtime::driver::spawn_link;
use livi_runtime::mfi_async::SharedCoprocessor;
use tokio::sync::broadcast;

use super::{parse_mac, ready, run_session, short};
use crate::config::Config;
use crate::session::Ended;

pub async fn watch(
    cfg: Arc<Config>,
    auth: SharedCoprocessor,
    airplay_port: u32,
    restart: broadcast::Sender<()>,
) {
    if let Err(e) = ready(&cfg) {
        eprintln!("[cp-wifi] {e}");
        return;
    }

    let (conn, mut incoming) = match livi_runtime::bt::start(&cfg.bt_adapter, &cfg.name, true).await
    {
        Ok(started) => started,
        Err(e) => {
            eprintln!(
                "[cp-wifi] bluetooth did not start on {}: {e}. Is bluetoothd \
                     running, and is {} the adapter?",
                cfg.bt_adapter, cfg.bt_adapter
            );
            return;
        }
    };
    // The D-Bus connection owns the registered profiles. Dropping it
    // unregisters them, so it has to be kept alive for as long as we are
    // listening — hence the binding rather than a bare `_`.
    let _conn = conn;
    println!(
        "[cp-wifi] publishing the iAP service on {} as {:?}; pair the phone now",
        cfg.bt_adapter, cfg.name
    );

    while let Some(IncomingConn { fd, peer_mac }) = incoming.recv().await {
        if let Some(target) = &cfg.target_bt {
            if !same_mac(target, &peer_mac) {
                // Closing the descriptor is the refusal: BlueZ has no other
                // way to decline a connection it already accepted.
                drop(fd);
                continue;
            }
        }
        let peer = short(&peer_mac);
        println!("[cp-wifi] {peer} opened a channel");
        let link = LinkConfig {
            max_outgoing: 4,
            control_version: 2,
            ..LinkConfig::default()
        };
        // Over RFCOMM the phone speaks first, so we must not open with a
        // negotiate of our own.
        let (channel, _artwork) = spawn_link(fd, link, false);
        // Ending the session drops the descriptor, which is the only way a
        // BlueZ channel ever closes; the profile's `AutoConnect` is what brings
        // the phone back.
        let ended = run_session(
            &cfg,
            auth.clone(),
            channel,
            airplay_port,
            local_mac(&cfg),
            &peer,
            &restart,
        )
        .await;
        println!("[cp-wifi] {peer} done, waiting for the next one");
        if let Ended::Restart = ended {
            println!("[cp-wifi] waiting for the phone to open the channel again");
        }
    }
}

/// This machine's Bluetooth address, which the phone checks against the one it
/// paired with. sysfs knows it; the configured value is the fallback.
fn local_mac(cfg: &Config) -> [u8; 6] {
    let path = format!("/sys/class/bluetooth/{}/address", cfg.bt_adapter);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_mac(text.trim()),
        Err(_) => parse_mac(&cfg.bt_mac),
    }
}

/// BlueZ separates with colons; people paste addresses with dashes.
fn same_mac(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace('-', ":").to_lowercase();
    norm(a) == norm(b)
}
