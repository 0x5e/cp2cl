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

mod linux;

pub use linux::watch;

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

    // What the phone is told to join, and where. Read here rather than once at
    // startup so that editing the file and restarting the access point is
    // enough for a change to take effect.
    let (path, ap) = match access_point(&cfg.wifi_iface) {
        Ok(found) => found,
        Err(e) => {
            tracing::error!("[cp-wifi] {e}");
            return Ended::Finished;
        }
    };
    tracing::info!(
        "[cp-wifi] the phone is told to join {:?} on channel {} over {}, from {}",
        ap.ssid,
        ap.channel,
        cfg.wifi_iface,
        path.display()
    );

    let identity = Identity {
        name: cfg.name.clone(),
        ssid: ap.ssid.clone(),
        bt_mac,
    };

    let cp = CpConfig {
        wifi_iface: cfg.wifi_iface.clone(),
        ssid: ap.ssid.clone(),
        passphrase: ap.passphrase,
        channel: ap.channel,
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
        // Left to whoever knows the network: the accessory's identifier is then
        // the MAC of the interface named above, read off that interface.
        ap_mac: None,
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

    let ended = run_bringup(
        "[cp-wifi]",
        peer,
        airplay_port,
        Some(ap.ssid.as_str()),
        rx,
        &mut changed,
    )
    .await;

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

    /// The file `board/install.sh` installs. What it says is the operator's to
    /// change, so only that we can read it is asserted here.
    const SHIPPED: &str = include_str!("../../board/etc/hostapd/hostapd.conf");

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
