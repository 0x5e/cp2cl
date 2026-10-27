//! Wireless CarPlay: the phone pairs in the OS, opens an RFCOMM channel to us,
//! and iAP2 runs on that instead of on a USB carkit stream.
//!
//! The protocol afterwards is the one `crate::session` already runs — same
//! identification, same MFi challenge, same `run_accessory`. What changes is
//! only:
//!
//! * the bytes come from Bluetooth, by way of BlueZ,
//! * `Transport::Wireless`, which puts a Bluetooth component and a wireless
//!   CarPlay component in the identification message,
//! * `CarPlayStartSession` has to name a Wi-Fi network and an address on it,
//!   because the phone is about to hang up Bluetooth and come back over Wi-Fi.
//!
//! This module owns none of the pairing: the operator pairs the phone in the
//! OS first. All we do is publish the service the phone looks for and accept
//! the channel it opens.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use livi_runtime::bringup::{run_accessory, CpConfig, OnCable};
use livi_runtime::driver::LinkChannel;
use livi_runtime::ident::{Identity, Transport};
use livi_runtime::mfi_async::SharedCoprocessor;
use livi_runtime::vehicle::{Fuels, Plugs, Vehicle};
use livi_wifi::{Channel, OnAir, Security};
use tokio::sync::{broadcast, mpsc};

use crate::config::Config;
use crate::session::{run_bringup, Ended};
use crate::sink::ClSink;
use crate::web::Status;

mod linux;
mod page;
mod power;

pub use linux::watch;

/// Whether the two radios this board needs have come up: an interface for the
/// access point, and a Bluetooth controller.
///
/// Both appear when the driver behind them has finished taking its firmware,
/// which is a second or two after the boot has loaded the module — "not there
/// yet" is a state to wait in rather than a fault. Asked from two places, which
/// is why it is here rather than in either of them.
pub(crate) fn radios_present(iface: &str, adapter: &str) -> (bool, bool) {
    let net = Path::new("/sys/class/net").join(iface).exists();
    let bt = Path::new("/sys/class/bluetooth").join(adapter).exists();
    (net, bt)
}

/// What the access point is on air with, for the bringup to ask about.
///
/// `AskOnAir` is a bare `fn` — no argument, no captures — so there is nowhere to
/// hand it the interface. The session puts the answer here before the bringup
/// starts and the bringup reads it back, which is what makes the start session
/// describe the network the phone is actually joining rather than a guess.
static ON_AIR: Mutex<Option<OnAir>> = Mutex::new(None);

fn on_air() -> Option<OnAir> {
    ON_AIR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Whether the phone has moved its iAP2 session off Bluetooth and onto the
/// AirPlay data stream.
///
/// While it has, Bluetooth must not be paged: a page pulls the phone back onto
/// a second iAP2 session and gets another `CarPlayStartSession` ready for it —
/// the one thing the stream that just took over cannot survive. Set when the
/// data stream's connection is accepted rather than when the phone asks for it:
/// a stream that is never connected is a phone that will never send
/// `disableBluetooth`, and holding the page back for that would only leave it
/// unreachable. Cleared when the AirPlay session the stream belongs to ends,
/// which is also the moment paging should resume.
pub(crate) struct Handover(AtomicBool);

impl Handover {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(false)))
    }

    pub(crate) fn live(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub(crate) fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub(crate) fn clear(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Everything a wireless iAP2 session is built from.
///
/// Two transports carry that session — the Bluetooth link that hands the phone
/// its Wi-Fi credentials, and the CarPlay data stream the phone moves the same
/// session onto once CarPlay is running — and both have to describe the access
/// point and themselves identically, or the phone has no reason to believe the
/// second one is the accessory it was already talking to.
pub(crate) struct SessionParams {
    /// The hostapd file the access point was read from, for the log line.
    pub path: PathBuf,
    pub ssid: String,
    pub channel: u8,
    pub cp: CpConfig,
    pub identity: Identity,
}

/// Reads the access point and builds what both wireless sessions hand the
/// phone. The passphrase never leaves this function's caller: it is a secret,
/// and a page is not a place to keep one.
///
/// `accessory_key` is the AirPlay identity's public key in hex, handed in
/// rather than read here: it belongs to the pairing the caller already holds
/// open, and a second copy read off disk could disagree with it.
pub(crate) fn session_params(
    cfg: &Config,
    airplay_port: u32,
    bt_mac: [u8; 6],
    accessory_key: &str,
) -> Result<SessionParams, String> {
    // Read here rather than once at startup so that editing the file and
    // restarting the access point is enough for a change to take effect.
    let (path, ap) = access_point(&cfg.wifi_iface)?;

    let identity = Identity {
        name: cfg.name.clone(),
        ssid: ap.ssid.clone(),
        bt_mac,
        // Nothing here knows what the car runs on, so no fuel type is claimed:
        // the phone is told the accessory has no range to report.
        fuels: Fuels::default(),
        // Nor is there a charge port: the board draws its power from the cable.
        plugs: Plugs::default(),
        theme: None,
    };

    // What the phone is about to be told, kept where the bringup can ask for
    // it. The start session derives the Wi-Fi security from this: a WPA2
    // network is named by leaving the field out, a WPA3 or transition one is
    // not.
    *ON_AIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(OnAir {
        ssid: ap.ssid.clone(),
        channel: Channel::of_number(ap.channel.into()),
        security: ap.security,
    });

    let cp = CpConfig {
        wifi_iface: cfg.wifi_iface.clone(),
        ssid: ap.ssid.clone(),
        passphrase: ap.passphrase,
        channel: Channel::of_number(ap.channel.into()),
        airplay_port,
        source_version: "320.17".into(),
        // Who the phone is talking to, in the answer that starts CarPlay: the
        // AirPlay identity's Ed25519 public key, hex. It is the same key the
        // phone was given at pair-setup and checks the pair-verify signature
        // against, and an empty value here is a start session the phone takes
        // but never acts on.
        public_key: accessory_key.to_string(),
        transport: Transport::Wireless,
        // The media comes back over Wi-Fi, so the Wi-Fi interface is the AV
        // interface — there is no USB network function to name here.
        av_iface: Some(cfg.wifi_iface.clone()),
        // Known from the start: the phone joins an existing network, so no
        // interface appears part-way through the way a Mac's does.
        av_iface_late: None,
        available_current_ma: 500,
        // Left to whoever knows the network: the accessory's identifier is then
        // the MAC of the interface named above, read off that interface.
        device_id: None,
        ap_mac: None,
        // The network the phone joins, as hostapd runs it — see `ON_AIR`.
        ap_on_air: Some(on_air),
        // Nothing here watches the cable: a phone seen over Bluetooth is taken
        // as a wireless one, and if it is plugged in as well the wired watcher
        // is the one that will pick it up.
        on_cable: Some(OnCable(Arc::new(|_bt_mac: &str| false))),
        // A display change drops the AirPlay session, and the phone then has to
        // be given something to start CarPlay from. That cannot be done on this
        // session: `CarPlayStartSession` is the answer to the phone's own offer,
        // and it has already had one. So the whole session is torn down instead
        // — ending it drops the link, and the platform paging the phone again
        // is what gets us a new one.
        start_again: None,
        // No look of its own to hand the phone.
        theme: None,
    };

    Ok(SessionParams {
        path,
        ssid: ap.ssid,
        channel: ap.channel,
        cp,
        identity,
    })
}

/// Runs the iAP2 half of one wireless session on a channel that is already up,
/// and says whether the phone should be asked back for another one.
///
/// `bt_mac` is this machine's own Bluetooth address: the phone checks that the
/// accessory it is talking to is the one it paired with, so a made-up value
/// gets the identification rejected.
///
/// One argument per thing the caller holds, which is one more than clippy counts
/// by default. They are left apart rather than bundled: no two of them belong
/// with each other, and a struct over them would exist only to be taken apart
/// again on the first line.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_session(
    cfg: &Config,
    auth: SharedCoprocessor,
    channel: LinkChannel,
    airplay_port: u32,
    bt_mac: [u8; 6],
    accessory_key: &str,
    peer: &str,
    restart: &broadcast::Sender<()>,
    panel: &ClSink,
    status: &Status,
    vehicle: &Arc<Vehicle>,
) -> Ended {
    // Subscribed before anything goes out, so a display change made while the
    // identification is still running is waiting for us rather than lost.
    let mut changed = restart.subscribe();

    let params = match session_params(cfg, airplay_port, bt_mac, accessory_key) {
        Ok(found) => found,
        Err(e) => {
            tracing::error!("wifi.ap_unusable err={e}");
            return Ended::Finished;
        }
    };
    tracing::info!(
        "wifi.ap ssid={:?} channel={} iface={} config={} pk_len={}",
        params.ssid,
        params.channel,
        cfg.wifi_iface,
        params.path.display(),
        params.cp.public_key.len()
    );
    // The passphrase is deliberately left out: it is a secret, and a page is not
    // a place to keep one. What is here is what the interface is on air with.
    // Spelled out rather than imported: this module has an `AccessPoint` of its
    // own, which is the parsed configuration rather than the reported one.
    status.with(|s| {
        s.carplay.access_point = Some(crate::web::AccessPoint {
            ssid: params.ssid.clone(),
            channel: params.channel,
            iface: cfg.wifi_iface.clone(),
        });
    });

    let SessionParams { cp, identity, ssid, .. } = params;

    let (tx, rx) = mpsc::channel(64);
    // `Vehicle` owns the two watch senders and `feed` only hands out receivers,
    // so the `Vehicle` has to outlive the session it feeds. The one the bridge
    // shares does: it was made before either half started, and the CarLife side
    // holds the other reference to it.
    let vehicle = vehicle.clone();
    let feed = vehicle.feed();
    let accessory = tokio::spawn(async move {
        run_accessory(channel, auth, identity, cp, tx, feed).await
    });

    let ended = run_bringup(
        peer,
        "bt",
        airplay_port,
        Some(ssid.as_str()),
        rx,
        &mut changed,
        panel,
        status,
    )
    .await;

    // The session's handle to the link has to go for the link to close, and the
    // driver's own read half has to be let go with it — `Latched`, which the
    // caller wrapped the stream in, is what does the second half.
    accessory.abort();
    let _ = accessory.await;

    if let Ended::Restart = ended {
        tracing::info!("carplay.restart peer={peer} action=drop_link");
    }
    ended
}

/// Where the access point's settings come from: hostapd's own configuration
/// directory. What the phone is told to join is then what the interface is on
/// air with — passphrase included. Asking the interface instead answers for the
/// SSID and the channel and never for the passphrase, which is what used to make
/// a second source of truth, and a divergence, possible.
const HOSTAPD_DIR: &str = "/etc/hostapd";

/// The access point the interface is meant to be running: the configuration
/// that names it, and what the phone is told to join it with.
///
/// The file is found by what it says rather than by what it is called. The
/// directory holds whatever configurations the operator keeps, and the one that
/// matters is the one that names this interface.
fn access_point(iface: &str) -> Result<(PathBuf, AccessPoint), String> {
    find_access_point(Path::new(HOSTAPD_DIR), iface)
}

fn find_access_point(dir: &Path, iface: &str) -> Result<(PathBuf, AccessPoint), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        // Whatever else is in there — a directory, something binary — is not
        // this interface's configuration.
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if setting(&text, "interface") != Some(iface) {
            continue;
        }
        let ap = parse_access_point(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok((path, ap));
    }
    Err(format!(
        "{} holds no access point on {iface}: no file there sets `interface={iface}`",
        dir.display()
    ))
}

/// The network the access point is on air with, as its own configuration
/// describes it.
#[derive(Debug, PartialEq, Eq)]
struct AccessPoint {
    ssid: String,
    passphrase: String,
    channel: u8,
    /// What the access point asks a phone to authenticate with.
    security: Security,
}

/// What a hostapd configuration gives `key`, if it gives one. Its format is one
/// `key=value` per line with `#` starting a comment; everything after the first
/// `=` is the value.
fn setting<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .filter_map(|line| line.trim().split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim())
}

/// The settings the phone is told to join with. Settings we have no use for are
/// skipped; an `ssid`, `wpa_passphrase` or `channel` that is absent or unusable
/// is an error, because the alternative is a phone told to join nothing. The
/// error says which setting, and the caller says which file.
fn parse_access_point(text: &str) -> Result<AccessPoint, String> {
    let ssid = setting(text, "ssid").filter(|s| !s.is_empty());
    let passphrase = setting(text, "wpa_passphrase").filter(|s| s.len() >= 8);
    // hostapd's own range. Reported to the phone so it can find the network
    // faster; a wrong value is not fatal, the phone scans anyway — but a channel
    // that is not a channel is a mistake worth naming rather than sending.
    let channel = match setting(text, "channel") {
        Some(value) => match value.parse::<u8>() {
            Ok(n @ 1..=196) => Some(n),
            _ => return Err(format!("`channel={value}` is not 1..196")),
        },
        None => None,
    };
    Ok(AccessPoint {
        ssid: ssid.ok_or("`ssid` is not set")?.to_string(),
        passphrase: passphrase
            .ok_or(
                "`wpa_passphrase` is not set, or is shorter than the 8 characters WPA needs",
            )?
            .to_string(),
        channel: channel.ok_or("`channel` is not set")?,
        // Read off `wpa_key_mgmt`, the same way hostapd runs it: WPA-PSK alone
        // is WPA2, WPA-PSK and SAE together is transition, SAE alone is WPA3.
        security: Security::of_hostapd(text),
    })
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

/// Drops the phone's Bluetooth link, which is what `disableBluetooth` asks for.
///
/// By the time the phone sends that it has moved iAP2 onto the CarPlay data
/// stream, so the Bluetooth link is carrying nothing worth keeping — and
/// leaving it up is what would let a page pull the phone back onto it. BlueZ
/// owns the link, so this is one D-Bus call on the device object the address
/// names; a failure is not fatal, because the phone closing the RFCOMM channel
/// itself does the same job a moment later.
pub(crate) async fn disconnect_phone(mac: &str, adapter: &str) -> Result<(), String> {
    let path = device_path(adapter, mac);
    let conn = zbus::Connection::system()
        .await
        .map_err(|e| format!("system bus: {e}"))?;
    conn.call_method(
        Some("org.bluez"),
        path.as_str(),
        Some("org.bluez.Device1"),
        "Disconnect",
        &(),
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("{path}: {e}"))
}

/// `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF`, which is how BlueZ names a device.
/// The inverse of what `page` reads back out of the object path.
fn device_path(adapter: &str, mac: &str) -> String {
    format!(
        "/org/bluez/{adapter}/dev_{}",
        mac.replace(':', "_").to_uppercase()
    )
}

/// Refuse to start a session we cannot finish: without a network to name, the
/// phone gets a start-session message it cannot act on.
///
/// Checked before the Bluetooth service is published, so a configuration
/// mistake is one line at startup rather than a pairing prompt followed by
/// nothing.
pub(crate) fn ready(cfg: &Config) -> Result<(), String> {
    access_point(&cfg.wifi_iface).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file the image installs. What it says is the operator's to change,
    /// so only that we can read it is asserted here.
    const SHIPPED: &str =
        include_str!("../../buildroot/board/nanopi-duo2/rootfs-overlay/etc/hostapd/hostapd.conf");

    /// A directory to put configurations in, removed when the test ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cp2cl-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a temporary directory must be creatable");
            Self(path)
        }

        fn write(&self, name: &str, text: &str) {
            std::fs::write(self.0.join(name), text).expect("the file must be writable");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_access_point_we_ship_is_one_we_can_read() {
        assert_eq!(setting(SHIPPED, "interface"), Some("wlan0"));
        parse_access_point(SHIPPED).expect("the shipped hostapd.conf must be readable");
    }

    #[test]
    fn the_settings_come_out_of_the_file_whatever_else_is_in_it() {
        let ap = parse_access_point(
            "# an access point\n\
             \n\
             interface=wlan0\n\
             driver=nl80211\n\
             ssid=car\n\
             country_code=CN\n\
             hw_mode=g\n\
             channel=36\n\
             wpa=2\n\
             wpa_passphrase=12345678\n\
             wpa_key_mgmt=WPA-PSK\n",
        )
        .unwrap();
        assert_eq!(
            ap,
            AccessPoint {
                ssid: "car".into(),
                passphrase: "12345678".into(),
                channel: 36,
                security: Security::Wpa2,
            }
        );
    }

    #[test]
    fn the_access_point_is_the_configuration_that_names_the_interface() {
        let dir = TempDir::new("hostapd-pick");
        dir.write(
            "hostapd.conf",
            "interface=wlan0\nssid=car\nchannel=6\nwpa_passphrase=12345678\n",
        );
        dir.write(
            "hostapd.conf.bak",
            "interface=wlan1\nssid=other\nchannel=11\nwpa_passphrase=abcdefgh\n",
        );

        let (path, ap) = find_access_point(&dir.0, "wlan1").expect("wlan1 is named");
        assert_eq!(path.file_name().unwrap(), "hostapd.conf.bak");
        assert_eq!(ap.ssid, "other");
        assert_eq!(ap.channel, 11);

        let e = find_access_point(&dir.0, "wlan2").expect_err("wlan2 is named by nothing");
        assert!(e.contains("wlan2"), "{e} must name the interface");
    }

    #[test]
    fn a_named_configuration_that_cannot_be_used_says_which_file_it_was() {
        let dir = TempDir::new("hostapd-broken");
        dir.write("hostapd.conf", "interface=wlan0\nssid=car\nchannel=6\n");

        let e = find_access_point(&dir.0, "wlan0").expect_err("a passphrase is required");
        assert!(
            e.contains("hostapd.conf") && e.contains("wpa_passphrase"),
            "{e} must name the file and the setting"
        );
    }

    #[test]
    fn a_setting_that_is_missing_or_unusable_is_named_and_refused() {
        let base = "interface=wlan0\nssid=car\nchannel=6\nwpa_passphrase=12345678\n";
        let without = |key: &str| {
            let line = base
                .lines()
                .find(|l| l.starts_with(key))
                .expect("the base must set it");
            parse_access_point(&base.replace(line, ""))
        };
        for key in ["ssid", "channel", "wpa_passphrase"] {
            let e = without(key).expect_err("a missing setting must be refused");
            assert!(e.contains(key), "{e} must name {key}");
        }
        for (key, value) in [
            ("ssid", ""),
            ("channel", "0"),
            ("channel", "x"),
            ("wpa_passphrase", "short"),
        ] {
            let line = base.lines().find(|l| l.starts_with(key)).unwrap();
            let text = base.replace(line, &format!("{key}={value}"));
            assert!(parse_access_point(&text).is_err(), "{key}={value}");
        }
    }
}
