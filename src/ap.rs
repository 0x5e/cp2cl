//! What the access point says about the phones on it.
//!
//! hostapd is the only thing on this board that knows when a phone has arrived
//! on the access point, finished the four-way handshake, or left. It says so on
//! its control socket, and that is what is read here: the association, the
//! handshake and the departure are three separate moments, and a neighbour
//! table or a lease file has none of them — it has the last one, late.
//!
//! Nothing is acted on. The phone's way back onto Wi-Fi is the longest stretch
//! of a session's first seconds that no other line accounts for, and these are
//! what cut it into the part the phone spends arriving and the part it spends
//! getting an address and opening AirPlay.

use std::time::Duration;

use tokio::net::UnixDatagram;
use tokio::time::sleep;

/// Where hostapd keeps one socket per interface, from its own `ctrl_interface`.
/// `/var/run` is `/run` on this image; the daemon and this program both name the
/// same directory, one for the pid file and one for the socket.
const DIR: &str = "/var/run/hostapd";

/// How long between looks for the socket, and between re-attachments once it
/// goes. Half a second is well inside the several seconds a phone spends
/// between being told the network's name and turning up on it, and at boot this
/// costs the boot's own time: hostapd starts after this program.
const POLL: Duration = Duration::from_millis(500);

/// Follows one interface for as long as the program runs.
///
/// A socket that is not there yet and a daemon that has gone away are the same
/// thing here — wait, then attach again — but only one of them is news. The
/// boot's own timing is not, and saying so every half second would be the only
/// thing in the log while it lasts.
pub async fn run(iface: String) {
    let path = format!("{DIR}/{iface}");
    let mut attached = false;
    loop {
        match attach(&path).await {
            Ok(socket) => {
                attached = true;
                tracing::info!("wifi.ap_attached path={path}");
                if let Err(e) = follow(&socket).await {
                    // Cleared here rather than left to the failing attach that
                    // follows: the drop is worth one line, not one per look
                    // until the daemon comes back.
                    attached = false;
                    tracing::warn!("wifi.ap_gone path={path} err={e}");
                }
            }
            Err(e) => {
                if attached {
                    tracing::warn!("wifi.ap_gone path={path} err={e}");
                    attached = false;
                }
            }
        }
        sleep(POLL).await;
    }
}

/// The control socket, and this end on its event list.
///
/// `ATTACH` is what puts it there. After that hostapd sends one datagram per
/// event, and a `PING` it expects a `PONG` to — a client that stops answering
/// is dropped from the list, which is why the reply goes out before anything is
/// parsed.
///
/// The local end needs an address of its own, and that is the whole of why this
/// binds: hostapd answers to whatever `recvfrom` gave it, and a socket that
/// never bound has nothing to answer to. `hostapd_cli` binds a temporary path
/// for the same reason. Named after the process, so a restart finds the last
/// one and takes it over rather than adding to a pile.
async fn attach(path: &str) -> std::io::Result<UnixDatagram> {
    let local = format!("/tmp/cp2cl-ap.{}", std::process::id());
    let _ = std::fs::remove_file(&local);
    let socket = UnixDatagram::bind(&local)?;
    socket.connect(path)?;
    socket.send(b"ATTACH").await?;
    Ok(socket)
}

/// Everything hostapd has to say on one attachment, until it goes away.
async fn follow(socket: &UnixDatagram) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    loop {
        let read = socket.recv(&mut buf).await?;
        let line = String::from_utf8_lossy(&buf[..read]);
        let line = line.trim_end();
        if line.starts_with("PING") {
            socket.send(b"PONG").await?;
            continue;
        }
        // `<3>AP-STA-CONNECTED 74:51:af:…`: a priority in front of the name of
        // the event, and the address after it.
        let body = line.split_once('>').map_or(line, |(_, rest)| rest);
        let mut fields = body.split_whitespace();
        let Some(event) = fields.next() else { continue };
        let Some(mac) = fields.next() else { continue };
        match event {
            "AP-STA-ASSOCIATED" => tracing::info!("wifi.sta_associated mac={mac}"),
            "AP-STA-CONNECTED" => tracing::info!("wifi.sta_connected mac={mac}"),
            "AP-STA-DISCONNECTED" => tracing::info!("wifi.sta_left mac={mac}"),
            // The phone had the passphrase wrong or the handshake failed another
            // way. Said at warn because it is the one failure here a reader has
            // to act on: everything else in the log says the phone never came.
            "AP-STA-POSSIBLE-PSK-MISMATCH" => {
                tracing::warn!("wifi.sta_psk_mismatch mac={mac} consequence=phone_cannot_join")
            }
            _ => tracing::debug!("wifi.ap_event event={event} mac={mac}"),
        }
    }
}
