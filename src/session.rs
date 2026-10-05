//! The wired half: find the phone, carry iAP2 over USB, start the session.
//!
//! This is the only place cp2aa touches LIVI. Everything up to and including
//! `CarPlayStartSession` is LIVI's `bringup::run_accessory`; we supply a control
//! channel (the USB carkit stream wrapped in the iAP2 link) and an auth source
//! (local MFi credentials), then watch the event stream until the phone leaves.
//! After that the media arrives over a completely separate connection handled by
//! `crate::airplay`.

use std::sync::Arc;
use std::time::Duration;

use iap2_link::LinkConfig;
use livi_runtime::bringup::{run_accessory, BringupEvent, CpConfig};
use livi_runtime::driver::spawn_link_stream;
use livi_runtime::ident::Identity;
// `CpConfig::transport` is a `Transport`, but `bringup` only imports it
// privately, so it has to be named through the module that defines it.
use livi_runtime::ident::Transport;
use livi_runtime::mfi_async::SharedCoprocessor;
use livi_runtime::vehicle::Vehicle;
use tokio::sync::{broadcast, mpsc, Notify};

use crate::config::Config;
use crate::wireless::{parse_mac, short};

/// How often the system usbmuxd is asked who is attached. Cheap enough that a
/// second is not rude, and fast enough that plugging in feels immediate.
const SCAN_INTERVAL: Duration = Duration::from_secs(1);

/// How long to wait before asking the phone for another start. The AirPlay
/// session is dropped in the same breath as the signal that triggers this, and
/// a phone still closing the old one cannot act on a start that arrives early.
const START_AGAIN_DELAY: Duration = Duration::from_millis(500);

/// Keeps asking who is plugged in until we stop being asked to stop.
pub async fn watch(
    cfg: Arc<Config>,
    auth: SharedCoprocessor,
    airplay_port: u32,
    restart: broadcast::Sender<()>,
) {
    let mut busy: Option<String> = None;
    loop {
        match iap2_wired::usbmuxd::devices().await {
            Ok(devices) => {
                let current: Vec<String> = devices.iter().map(|d| d.udid.clone()).collect();
                match &busy {
                    Some(udid) if !current.contains(udid) => {
                        println!("[cp] {} unplugged", short(udid));
                        busy = None;
                    }
                    _ => {}
                }
                if busy.is_none() {
                    if let Some(device) = devices.into_iter().next() {
                        let udid = device.udid.clone();
                        println!("[cp] {} found over usb", short(&udid));
                        match attach(&cfg, auth.clone(), &device, airplay_port, &restart).await {
                            Ok(()) => busy = Some(udid),
                            Err(e) => eprintln!("[cp] {} could not attach: {e}", short(&udid)),
                        }
                    }
                }
            }
            Err(e) => {
                // Silence is normal before any phone has ever been trusted.
                tracing::trace!("usbmuxd query failed: {e}");
            }
        }
        tokio::time::sleep(SCAN_INTERVAL).await;
    }
}

/// One phone: open carkit, negotiate iAP2, run the session, report progress.
async fn attach(
    cfg: &Config,
    auth: SharedCoprocessor,
    device: &idevice::usbmuxd::UsbmuxdDevice,
    airplay_port: u32,
    restart: &broadcast::Sender<()>,
) -> anyhow::Result<()> {
    // Subscribed before anything opens, so a display change made while the
    // carkit is coming up is still waiting for us rather than lost.
    let changed = restart.subscribe();

    // The stream the phone sends its media over. On macOS this is the enX the
    // USB network function creates; on Linux it is whatever gadget we set up.
    let av_iface = av_interface(cfg, &device.udid).await;
    println!("[cp] av interface: {:?}", av_iface);

    // `ProbeError` is only Display, not std::error::Error, so go through format.
    let stream = iap2_wired::usbmuxd::open(device)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("[cp] {} carkit up, starting iAP2", short(&device.udid));

    let link = LinkConfig {
        max_outgoing: 4,
        control_version: 2,
        zero_ack: true,
        ..LinkConfig::default()
    };
    let (channel, _artwork) = spawn_link_stream(stream, link, true);

    let identity = Identity {
        name: cfg.name.clone(),
        ssid: cfg.name.clone(),
        bt_mac: parse_mac(&cfg.bt_mac),
    };

    // The display panel can change what we render, and the phone reads `/info`
    // once per session and never asks again — so a change means dropping the
    // session and giving the phone a reason to come back. `start_again` is
    // LIVI's own door for that second half: the start is sent over the iAP2
    // session that is already up, so the carkit is never reopened.
    let start_again = Arc::new(Notify::new());

    let cp = CpConfig {
        wifi_iface: String::new(),
        ssid: cfg.name.clone(),
        passphrase: "00000000".into(),
        channel: 36,
        security_type: iap2_csm::messages::wifi::SecurityType::WpaWpa2,
        airplay_port,
        source_version: "320.17".into(),
        public_key: String::new(),
        transport: Transport::Wired,
        av_iface,
        // The USB network function is already up by the time we get here —
        // this is a wired session on a cable that has been plugged in for a
        // while — so there is nothing to wait for.
        av_iface_late: None,
        available_current_ma: 500,
        ap_mac: None,
        ap_on_air: None,
        // Bluetooth-only: whether a phone we can see is also on the cable.
        on_cable: None,
        // Wired sends the start once on its own; this is how it is sent again
        // without the iAP2 session ending under it.
        start_again: Some(start_again.clone()),
    };

    let (tx, rx) = mpsc::channel(64);
    let vehicle = Vehicle::default().feed();
    tokio::spawn(async move { run_accessory(channel, auth, identity, cp, tx, vehicle).await });

    let forward = tokio::spawn(forward_restarts(changed, start_again));

    run_bringup("[cp]", &short(&device.udid), airplay_port, None, rx).await;
    forward.abort();
    Ok(())
}

/// Turns each "what to render changed" into another `CarPlayStartSession`.
///
/// Dropping the AirPlay session on its own leaves the phone nothing to knock on:
/// iAP2 is still up, so nothing tells it we want it back and it simply stays
/// away. LIVI's `start_again` re-sends the start over that live session, which is
/// what makes the phone dial us again and read `/info` afresh.
///
/// Shared with the wireless half, which has the same iAP2 session to re-send
/// over and so needs the same one-sided nudge.
pub(crate) async fn forward_restarts(mut changed: broadcast::Receiver<()>, again: Arc<Notify>) {
    loop {
        match changed.recv().await {
            // Lagged means more than one change arrived before this one was
            // read; either way the phone only needs to hear it once more.
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                tokio::time::sleep(START_AGAIN_DELAY).await;
                again.notify_one();
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Prints the progress of one iAP2 bringup, and returns when it ends.
///
/// Both transports run the same conversation, and differ only in their log
/// prefix and in which events they can produce, so the loop lives here once.
/// `ssid` is the network a wireless session tells the phone to join; the wired
/// one never has that step and passes `None`.
pub(crate) async fn run_bringup(
    prefix: &str,
    peer: &str,
    airplay_port: u32,
    ssid: Option<&str>,
    mut rx: mpsc::Receiver<BringupEvent>,
) {
    while let Some(event) = rx.recv().await {
        match event {
            BringupEvent::Identified => println!("{prefix} {peer} identified"),
            BringupEvent::Authenticated => println!("{prefix} {peer} MFi authentication passed"),
            BringupEvent::Subscribed => println!("{prefix} {peer} subscriptions sent"),
            BringupEvent::WifiConfigSent => {
                if let Some(ssid) = ssid {
                    println!("{prefix} {peer} told to join {ssid:?}");
                }
            }
            BringupEvent::CarPlayStartSent { ip } => println!(
                "{prefix} {peer} CarPlayStartSession sent — the phone should knock on \
                 {ip}:{airplay_port}"
            ),
            BringupEvent::Failed(reason) => {
                eprintln!("{prefix} {peer} session failed: {reason}");
                break;
            }
            BringupEvent::Closed => {
                println!("{prefix} {peer} session closed");
                break;
            }
            _ => {}
        }
    }
}

/// Which interface carries CarPlay's own media, per platform.
async fn av_interface(cfg: &Config, _udid: &str) -> Option<String> {
    if let Some(iface) = &cfg.av_iface {
        return Some(iface.clone());
    }
    #[cfg(target_os = "macos")]
    {
        // The phone's own USB network function, looked up by UDID.
        return iap2_wired::mac_network::discover(_udid).await.ok();
    }
    #[cfg(target_os = "linux")]
    {
        // A board usually names it usb0; take the first one that looks like it.
        let entries = std::fs::read_dir("/sys/class/net").ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("usb") || name.starts_with("enp") {
                return Some(name);
            }
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}
