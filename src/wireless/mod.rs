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
use tokio::sync::{broadcast, mpsc};

use crate::config::Config;
use crate::session::{run_bringup, Ended};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::watch;
#[cfg(target_os = "macos")]
pub use macos::watch;

/// Runs the iAP2 half of one wireless session on a channel that is already up,
/// and says whether the phone should be asked back for another one.
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
) -> Ended {
    // Subscribed before anything goes out, so a display change made while the
    // identification is still running is waiting for us rather than lost.
    let mut changed = restart.subscribe();

    // What the phone is told to join. Read here rather than left to the
    // environment, because an interface that is on air as an access point is
    // the authority on the network the phone will actually find — see
    // `wifi_parameters`. Not named `channel`: that is this link's.
    let (ssid, passphrase, wifi_channel) = wifi_parameters(cfg);

    let identity = Identity {
        name: cfg.name.clone(),
        ssid: cfg.ssid.clone(),
        bt_mac,
    };

    let cp = CpConfig {
        wifi_iface: cfg.wifi_iface.clone(),
        ssid: ssid.clone(),
        passphrase,
        channel: wifi_channel,
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
        // A display change drops the AirPlay session, and the phone then has to
        // be given something to start CarPlay from. That cannot be done on this
        // session: `CarPlayStartSession` is the answer to the phone's own offer,
        // and it has already had one. So the whole session is torn down instead
        // — ending this function drops the link, and the platform paging the
        // phone again is what gets us a new one.
        start_again: None,
    };

    let (tx, rx) = mpsc::channel(64);
    // `Vehicle` owns the two watch senders and `feed` only hands out receivers,
    // so the `Vehicle` has to outlive the session it feeds. As a temporary it
    // would be dropped at the end of this statement, `changed()` would then
    // report `Err` at once, and `run_accessory` spins on that until the session
    // ends. Kept in this scope until the accessory is aborted, below.
    let vehicle = Vehicle::default();
    let feed = vehicle.feed();
    let accessory = tokio::spawn(async move {
        run_accessory(channel, auth, identity, cp, tx, feed).await
    });

    let ended =
        run_bringup("[cp-wifi]", peer, airplay_port, Some(ssid.as_str()), rx, &mut changed).await;

    // The session's handle to the link has to go for the link to close, and the
    // driver's own read half has to be let go with it — `Latched`, which the
    // caller wrapped the stream in, is what does the second half.
    accessory.abort();
    let _ = accessory.await;

    if let Ended::Restart = ended {
        tracing::info!("[cp-wifi] {peer} is being told to start over; dropping the Bluetooth link");
    }
    ended
}

/// The SSID and channel an interface is on air with. Linux answers from
/// nl80211; a platform without it never has an answer.
#[cfg(target_os = "linux")]
fn on_air(iface: &str) -> Option<(String, u8)> {
    let (ssid, channel) = livi_runtime::net::ap_ssid_channel(iface);
    ssid.zip(channel)
}

/// There is no reading here — what an interface beacons is asked of nl80211 —
/// so this is always nothing.
#[cfg(not(target_os = "linux"))]
fn on_air(_iface: &str) -> Option<(String, u8)> {
    None
}

/// The SSID and channel of the access point `cfg.wifi_iface` is on air with,
/// or nothing when it is not one. What an interface beacons is the network the
/// phone will find, which is what makes this the authority on it.
fn access_point(cfg: &Config) -> Option<(String, u8)> {
    on_air(&cfg.wifi_iface)
}

/// What the phone is told to join: an SSID, its passphrase, and the channel to
/// look for it on.
///
/// The SSID and the channel come from the interface whenever it is on air as
/// an access point, and from `CP2CL_WIFI_SSID` / `CP2CL_WIFI_CHANNEL` when it
/// is not. The passphrase is not part of any reading — nl80211 never hands one
/// back — so it is always `CP2CL_WIFI_PASS`.
///
/// LIVI builds this answer again when the phone offers CarPlay, reading the
/// same interface to do it. The two agree unless the access point comes up
/// between the two readings.
fn wifi_parameters(cfg: &Config) -> (String, String, u8) {
    if let Some((ssid, channel)) = access_point(cfg) {
        tracing::info!(
            "[cp-wifi] {} is on air as {ssid:?} on channel {channel}; the phone is told that \
             network, with the passphrase from CP2CL_WIFI_PASS",
            cfg.wifi_iface
        );
        return (ssid, cfg.passphrase.clone(), channel);
    }
    #[cfg(target_os = "linux")]
    tracing::info!(
        "[cp-wifi] {} is not on air as an access point; the phone is told {:?} on channel {} \
         from CP2CL_WIFI_SSID / CP2CL_WIFI_CHANNEL",
        cfg.wifi_iface,
        cfg.ssid,
        cfg.wifi_channel
    );
    // Nothing to read with on a Mac, so the environment is the whole of the
    // answer there.
    #[cfg(not(target_os = "linux"))]
    tracing::info!(
        "[cp-wifi] {} cannot be read on this platform; the phone is told {:?} on channel {} \
         from CP2CL_WIFI_SSID / CP2CL_WIFI_CHANNEL",
        cfg.wifi_iface,
        cfg.ssid,
        cfg.wifi_channel
    );
    (cfg.ssid.clone(), cfg.passphrase.clone(), cfg.wifi_channel)
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
///
/// An access point that is already on air supplies the SSID and the channel
/// itself, so `CP2CL_WIFI_SSID` is only required when there is none. The
/// passphrase is required either way: it is the one value no reading supplies.
pub(crate) fn ready(cfg: &Config) -> Result<(), String> {
    if cfg.passphrase.len() < 8 {
        return Err("CP2CL_WIFI_PASS is shorter than the 8 characters WPA needs".into());
    }
    if cfg.ssid.is_empty() && access_point(cfg).is_none() {
        return Err(
            "CP2CL_WIFI_SSID is empty and no access point is on air — the phone is told to \
             join a network, so either name the one both ends are on or bring it up first"
                .into(),
        );
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
    _gate: tokio::sync::watch::Receiver<bool>,
) {
    tracing::error!(
        "[cp-wifi] wireless CarPlay needs Bluetooth, and this platform has no \
         backend for it: {}",
        std::env::consts::OS
    );
    let _ = cfg;
    std::future::pending::<()>().await;
}
