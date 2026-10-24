//! Linux: BlueZ publishes the service and hands us the phone's RFCOMM socket.
//!
//! This is LIVI's `livi_runtime::bt` doing all of it — registering the iAP
//! server profile, the iAP client profile, the CarPlay profile, and an agent
//! that answers the pairing prompt. All that is left for us is draining the
//! channel of sockets it produces — and calling a paired phone back, since a
//! phone never returns to an accessory on its own: see `page`.

use std::sync::Arc;

use iap2_link::LinkConfig;
use livi_runtime::bt::IncomingConn;
use livi_runtime::driver::spawn_link;
use livi_runtime::mfi_async::SharedCoprocessor;
use livi_runtime::vehicle::Vehicle;
use tokio::sync::broadcast;

use super::{page, parse_mac, ready, run_session, short, Handover};
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
    vehicle: Arc<Vehicle>,
    handover: Arc<Handover>,
) {
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
    while !*gate.borrow() {
        if gate.changed().await.is_err() {
            return;
        }
    }
    tracing::info!("wireless.car_up action=publish_service");

    let (conn, mut incoming, held) =
        match livi_runtime::bt::start(&cfg.bt_adapter, &cfg.name, true).await {
            Ok(started) => started,
            Err(e) => {
                tracing::error!(
                    "wireless.bt_start_failed adapter={} err={e} hint=\"is bluetoothd running, \
                     and is {} the adapter?\"",
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

    // The channel never closes while we hold the connection that owns the
    // profiles, so being told to stop is the only way out of the loop — and the
    // one moment the controllers have to be put back. Without the handler the
    // default action still ends the process, only without the putting back.
    let mut stop = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .inspect_err(|e| tracing::warn!("wireless.signal_handler_missing err={e} consequence=no_restore"))
        .ok();

    loop {
        // Only while nothing is on the link: a phone that is already coming in
        // does not need calling, and one that is mid-session must not be paged
        // again on top of it. And not at all while the phone has moved its iAP2
        // session onto the CarPlay data stream — see `Handover`.
        let paging = tokio::spawn(page::run(
            conn.clone(),
            cfg.bt_adapter.clone(),
            handover.clone(),
        ));
        let next = match &mut stop {
            Some(sig) => tokio::select! {
                next = incoming.recv() => next,
                _ = sig.recv() => None,
            },
            None => incoming.recv().await,
        };
        paging.abort();
        let Some(IncomingConn { fd, peer_mac }) = next else {
            break;
        };
        let peer = short(&peer_mac);
        tracing::info!("wireless.channel_opened peer={peer}");
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
        // BlueZ channel ever closes; paging the phone again is what brings it
        // back, and the loop above starts that over.
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
            &vehicle,
        )
        .await;
        carrying.abort();
        tracing::info!("wireless.channel_closed peer={peer} action=page_again");
        if let Ended::Restart = ended {
            tracing::info!("wireless.waiting peer={peer} action=wait_channel");
        }
    }

    // Nothing is listening any more: the profiles went with the channel, and the
    // controllers go back to the name and discoverability they were found with.
    livi_runtime::bt::let_go(&conn, &cfg.bt_adapter, &held).await;
    tracing::info!("wireless.bt_service_down adapter={} state=restored", cfg.bt_adapter);
}

/// This machine's Bluetooth address, which the phone checks against the one it
/// paired with. Read at the start of every session rather than taken from the
/// configuration at startup: a controller registers itself with the kernel
/// whenever it is brought up, and a session that begins after that is a session
/// that can have the real address.
fn local_mac(cfg: &Config) -> [u8; 6] {
    parse_mac(&bt_address(&cfg.bt_adapter))
}
