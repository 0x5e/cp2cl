//! Wireless CarPlay: the phone pairs in the OS, opens an RFCOMM channel to us,
//! and iAP2 runs on that instead of on a USB carkit stream.
//!
//! The protocol afterwards is the one `crate::session` already runs — same
//! identification, same MFi challenge, same `run_accessory`. What changes is
//! only:
//!
//! * the bytes come from Bluetooth, and both platforms hand them over
//!   differently (BlueZ over D-Bus on Linux, an IOBluetooth helper over a unix
//!   socket on macOS),
//! * `Transport::Wireless`, which puts a Bluetooth component and a wireless
//!   CarPlay component in the identification message,
//! * `CarPlayStartSession` has to name a Wi-Fi network and an address on it,
//!   because the phone is about to hang up Bluetooth and come back over Wi-Fi.
//!
//! This module owns none of the pairing: the operator pairs the phone in the
//! OS first. All we do is publish the service the phone looks for and accept
//! the channel it opens.

use std::sync::Arc;

use iap2_csm::messages::wifi::SecurityType;
use livi_runtime::bringup::{run_accessory, CpConfig, OnCable};
use livi_runtime::driver::LinkChannel;
use livi_runtime::ident::{Identity, Transport};
use livi_runtime::mfi_async::SharedCoprocessor;
use livi_runtime::vehicle::Vehicle;
use tokio::sync::{broadcast, mpsc, Notify};

use crate::config::Config;
use crate::session::{forward_restarts, run_bringup};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::watch;
#[cfg(target_os = "macos")]
pub use macos::watch;

/// Runs the iAP2 half of one wireless session on a channel that is already up.
///
/// `bt_mac` is this machine's own Bluetooth address: the phone checks that the
/// accessory it is talking to is the one it paired with, so a made-up value
/// gets the identification rejected.
pub(crate) async fn run_session(
    cfg: &Config,
    auth: SharedCoprocessor,
    channel: LinkChannel,
    airplay_port: u32,
    bt_mac: [u8; 6],
    peer: &str,
    restart: &broadcast::Sender<()>,
) {
    // Subscribed before anything goes out, so a display change made while the
    // identification is still running is waiting for us rather than lost.
    let changed = restart.subscribe();

    let identity = Identity {
        name: cfg.name.clone(),
        ssid: cfg.ssid.clone(),
        bt_mac,
    };

    let start_again = Arc::new(Notify::new());

    let cp = CpConfig {
        wifi_iface: cfg.wifi_iface.clone(),
        ssid: cfg.ssid.clone(),
        passphrase: cfg.passphrase.clone(),
        channel: cfg.wifi_channel,
        security_type: SecurityType::WpaWpa2,
        airplay_port,
        source_version: "320.17".into(),
        public_key: String::new(),
        transport: Transport::Wireless,
        // The media comes back over Wi-Fi, so the Wi-Fi interface is the AV
        // interface — there is no USB network function to name here.
        av_iface: Some(cfg.wifi_iface.clone()),
        // Known from the start: the phone joins an existing network, so no
        // interface appears part-way through the way a Mac's does.
        av_iface_late: None,
        available_current_ma: 500,
        // The router's BSSID when we know it. Without it the accessory's device
        // identifier is this machine's Wi-Fi MAC, which recent macOS SDKs report
        // as a placeholder, and the phone then has nothing stable to recognise.
        ap_mac: cfg.ap_mac.clone(),
        ap_on_air: None,
        // Nothing here watches the cable: a phone seen over Bluetooth is taken
        // as a wireless one, and if it is plugged in as well the wired watcher
        // is the one that will pick it up.
        on_cable: Some(OnCable(Arc::new(|_bt_mac: &str| false))),
        // Same door as the wired half: a display change drops the AirPlay
        // session, and this is what tells the phone to come back and read
        // `/info` afresh. The start is re-sent over the iAP2 session already up,
        // so Bluetooth is never reopened — and because nothing live is handed
        // to `send_start`, the wireless attributes come from this config, which
        // names the network the phone is already on.
        start_again: Some(start_again.clone()),
    };

    let (tx, rx) = mpsc::channel(64);
    let vehicle = Vehicle::default().feed();
    tokio::spawn(async move { run_accessory(channel, auth, identity, cp, tx, vehicle).await });

    let forward = tokio::spawn(forward_restarts(changed, start_again));

    run_bringup("[cp-wifi]", peer, airplay_port, Some(&cfg.ssid), rx).await;
    forward.abort();
}

/// `00:11:22:33:44:55` → bytes. Unparseable parts stay zero rather than
/// failing, because a malformed address is still worth trying.
pub(crate) fn parse_mac(text: &str) -> [u8; 6] {
    let mut out = [0u8; 6];
    for (i, part) in text.split(':').take(6).enumerate() {
        if let Ok(v) = u8::from_str_radix(part, 16) {
            out[i] = v;
        }
    }
    out
}

/// Short enough to keep in a log line, and enough to tell two phones apart.
pub(crate) fn short(mac: &str) -> String {
    if mac.len() > 8 {
        mac[mac.len() - 8..].to_uppercase()
    } else {
        mac.to_uppercase()
    }
}

/// Refuse to start a session we cannot finish: without a network to name, the
/// phone gets a start-session message it cannot act on.
pub(crate) fn ready(cfg: &Config) -> Result<(), String> {
    if cfg.ssid.is_empty() {
        return Err(
            "CP2AA_WIFI_SSID is empty — the phone is told to join a network, \
                    so name the one both ends are on"
                .into(),
        );
    }
    if cfg.passphrase.len() < 8 {
        return Err("CP2AA_WIFI_PASS is shorter than the 8 characters WPA needs".into());
    }
    Ok(())
}

/// Keeps asking the platform for phones until the program stops.
///
/// Neither platform pushes "a phone just paired", so both poll. A session that
/// ends puts us back to waiting for the same phone to come again.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub async fn watch(
    cfg: Arc<Config>,
    _auth: SharedCoprocessor,
    _airplay_port: u32,
    _restart: broadcast::Sender<()>,
) {
    eprintln!(
        "[cp-wifi] wireless CarPlay needs Bluetooth, and this platform has no \
         backend for it: {}",
        std::env::consts::OS
    );
    let _ = cfg;
    std::future::pending::<()>().await;
}
