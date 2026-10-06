//! macOS: an IOBluetooth helper holds the phone's RFCOMM channel and relays its
//! bytes to us over a unix socket.
//!
//! macOS will not lend its Bluetooth controller to a plain program the way
//! BlueZ does — there is no D-Bus profile to register — so the bridge is a
//! separate process with a run loop, built by `build.rs`. It publishes our two
//! service records, polls the paired devices, and opens the phone's iAP2
//! channel. What reaches us is the raw byte stream plus one header line naming
//! the two Bluetooth addresses, which is all the receiver needs.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iap2_link::LinkConfig;
use livi_runtime::driver::spawn_link_stream;
use livi_runtime::mfi_async::SharedCoprocessor;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use tokio::sync::broadcast;

use super::{parse_mac, ready, run_session, short};
use crate::config::Config;
use crate::session::{Ended, Latched};

/// The longest the helper's `local|remote` header line can be.
const HEADER_LIMIT: usize = 80;
const HEADER_TIMEOUT: Duration = Duration::from_secs(3);

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

    let helper = PathBuf::from(env!("CP2AA_BT_HELPER"));
    if helper.as_os_str().is_empty() {
        eprintln!(
            "[cp-wifi] the Bluetooth bridge was not built (clang is needed); \
             wireless CarPlay is unavailable"
        );
        return;
    }

    // Named for this process so two receivers on one machine do not collide.
    let path = std::env::temp_dir().join(format!("cp2aa-bluetooth-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("[cp-wifi] could not open {}: {e}", path.display());
            return;
        }
    };

    let mut child = match tokio::process::Command::new(&helper)
        .arg(&path)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("[cp-wifi] could not start the Bluetooth bridge: {e}");
            let _ = std::fs::remove_file(&path);
            return;
        }
    };
    println!("[cp-wifi] Bluetooth bridge up; pair the iPhone in system settings");

    loop {
        let accepted = tokio::select! {
            result = listener.accept() => result,
            result = child.wait() => {
                eprintln!("[cp-wifi] the Bluetooth bridge ended: {result:?}");
                break;
            }
        };
        let Ok((mut stream, _)) = accepted else { break };

        // Byte at a time, so nothing belonging to the iAP2 conversation is
        // swallowed along with the header.
        let mut header = Vec::new();
        while header.len() < HEADER_LIMIT {
            match tokio::time::timeout(HEADER_TIMEOUT, stream.read_u8()).await {
                Ok(Ok(b'\n')) => break,
                Ok(Ok(b)) => header.push(b),
                _ => break,
            }
        }
        let line = String::from_utf8_lossy(&header);
        let mut parts = line.split('|');
        let local = parts.next().unwrap_or("").replace('-', ":");
        let peer = parts.next().unwrap_or("").replace('-', ":");

        let mac = parse_mac(&local);
        if mac == [0u8; 6] {
            eprintln!("[cp-wifi] the bridge sent an address we cannot read: {line:?}");
            continue;
        }
        println!(
            "[cp-wifi] {} opened a channel (this Mac is {local})",
            short(&peer)
        );

        // Over RFCOMM the phone speaks first.
        let link = LinkConfig {
            max_outgoing: 4,
            control_version: 2,
            ..LinkConfig::default()
        };
        // The helper watches this socket: when the receiver lets go of it, it
        // closes the RFCOMM channel under the phone and asks for another one.
        // That close is the whole point — the phone will not start CarPlay over
        // until it sees the iAP2 session go — so the socket has to really close
        // rather than merely be forgotten. Hence the latch.
        let (stream, shut) = Latched::new(stream);
        let (channel, artwork) = spawn_link_stream(stream, link, false);
        let ended = run_session(
            &cfg,
            auth.clone(),
            channel,
            airplay_port,
            mac,
            &peer,
            &restart,
        )
        .await;
        shut.shutdown();
        drop(artwork);
        println!("[cp-wifi] {} done, waiting for the next one", short(&peer));
        if let Ended::Restart = ended {
            println!("[cp-wifi] waiting for the bridge to open the channel again");
        }
    }

    let _ = std::fs::remove_file(&path);
}
