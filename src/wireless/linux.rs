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
use crate::config::{bt_address, Config};
use crate::session::{carry_artwork, Ended};
use crate::sink::ClSink;
use crate::web::Status;

/// Keeps serving phones until the program stops. Nothing pushes "a phone just
/// paired" from outside, so a session that ends puts us back to waiting for the
/// same phone to come again.
pub async fn watch(
    cfg: Arc<Config>,
    auth: SharedCoprocessor,
    airplay_port: u32,
    restart: broadcast::Sender<()>,
    mut gate: tokio::sync::watch::Receiver<bool>,
    panel: Arc<ClSink>,
    status: Arc<Status>,
) {
    if let Err(e) = ready(&cfg) {
        tracing::error!("[cp-wifi] {e}");
        return;
    }

    // The iAP service is what tells the phone there is CarPlay here, so it is
    // not published until there is: with CarLife in the picture the CarPlay half
    // only exists for as long as the car's session does.
    while !*gate.borrow() {
        if gate.changed().await.is_err() {
            return;
        }
    }

    let (conn, mut incoming) = match livi_runtime::bt::start(&cfg.bt_adapter, &cfg.name, true).await
    {
        Ok(started) => started,
        Err(e) => {
            tracing::error!(
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
    tracing::info!(
        "[cp-wifi] publishing the iAP service on {} as {:?}; pair the phone now",
        cfg.bt_adapter, cfg.name
    );

    while let Some(IncomingConn { fd, peer_mac }) = incoming.recv().await {
        let peer = short(&peer_mac);
        tracing::info!("[cp-wifi] {peer} opened a channel");
        // Over Bluetooth there is no serial to be had: the peer is an address
        // and nothing more, so that is what the page is given.
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
        // The cover art is a file transfer of its own on the same link, and it
        // goes to the car's panel the same way the song it belongs to does.
        let carrying = tokio::spawn(carry_artwork(artwork, panel.clone()));
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
            &panel,
            &status,
        )
        .await;
        carrying.abort();
        tracing::info!("[cp-wifi] {peer} done, waiting for the next one");
        if let Ended::Restart = ended {
            tracing::info!("[cp-wifi] waiting for the phone to open the channel again");
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
