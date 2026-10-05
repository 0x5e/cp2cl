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
use tokio::sync::mpsc;

use crate::config::Config;
use crate::wireless::{parse_mac, short};

/// How often the system usbmuxd is asked who is attached. Cheap enough that a
/// second is not rude, and fast enough that plugging in feels immediate.
const SCAN_INTERVAL: Duration = Duration::from_secs(1);

/// Keeps asking who is plugged in until we stop being asked to stop.
pub async fn watch(cfg: Arc<Config>, auth: SharedCoprocessor, airplay_port: u32) {
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
                        match attach(&cfg, auth.clone(), &device, airplay_port).await {
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
) -> anyhow::Result<()> {
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
        // Wired sends the start once; there is no second iAP2 session to
        // re-send it on.
        start_again: None,
    };

    let (tx, mut rx) = mpsc::channel(64);
    let vehicle = Vehicle::default().feed();
    tokio::spawn(async move { run_accessory(channel, auth, identity, cp, tx, vehicle).await });

    while let Some(event) = rx.recv().await {
        match event {
            BringupEvent::Identified => println!("[cp] identified"),
            BringupEvent::Authenticated => println!("[cp] MFi authentication passed"),
            BringupEvent::Subscribed => println!("[cp] subscriptions sent"),
            BringupEvent::CarPlayStartSent { ip } => {
                println!(
                    "[cp] CarPlayStartSession sent — the phone should knock on {ip}:{airplay_port}"
                )
            }
            BringupEvent::Failed(reason) => {
                eprintln!("[cp] session failed: {reason}");
                break;
            }
            BringupEvent::Closed => {
                println!("[cp] session closed");
                break;
            }
            _ => {}
        }
    }
    Ok(())
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
