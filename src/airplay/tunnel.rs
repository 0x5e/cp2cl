//! iAP2 over the CarPlay data stream: the phone's control channel, moved onto
//! Wi-Fi once CarPlay is running.
//!
//! The phone does not open a second connection to carry it. It asks for a
//! stream of type 130 in the same SETUP that opens the media streams, hands over
//! a seed, and then speaks iAP2 into the TCP connection that seed names — sealed
//! with a key derived from pair-verify's shared secret and that seed. The other
//! direction has no socket of its own: what the accessory says goes back over
//! the event channel as `iAPSendMessage`.
//!
//! ```text
//! phone  -> accessory   SETUP {type: 130, clientTypeUUID: E9459FD0-…, seed: n}
//! accessory -> phone    {type: 130, streamID, dataPort}
//! phone  -> accessory   TCP dataPort: [2B len][sealed], inside it 32-byte
//!                       package headers, and the 'comm' bodies are iAP2
//! accessory -> phone    event channel: {"type":"iAPSendMessage","params":{"data":…}}
//! ```
//!
//! This is the same construction LIVI's `iap_tunnel`, MacPlay's `iapTunnel` and
//! playport's `IapTunnel` use, down to the HKDF labels and the package header.
//! Nothing here invents a step: the only difference is that this program has an
//! iAP2 link in its own process, so the tunnel is wired to a duplex stream
//! rather than to a socket somebody else is listening on.
//!
//! What it is *for*: once the tunnel carries the control channel, the phone
//! sends `disableBluetooth` and the Bluetooth link can go. See `Handover`.

use std::sync::Arc;

use iap2_link::LinkConfig;
use livi_runtime::bringup::run_accessory;
use livi_runtime::driver::spawn_link_stream;
use livi_runtime::mfi_async::SharedCoprocessor;
use livi_runtime::vehicle::Vehicle;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use super::crypto::{hkdf32, open_frames, SymKey};
use super::event::EventSink;
use crate::config::Config;
use crate::session::{carry_artwork, run_bringup, Ended};
use crate::sink::ClSink;
use crate::web::Status;
use crate::wireless::{session_params, Handover, SessionParams};

/// The DataStream the phone names when it means the iAP2 channel. Type 130 is
/// not enough on its own: video settings and video overlay ask for the same
/// number under different identifiers, and opening this tunnel for one of those
/// would take the control channel away from the phone that asked for it.
pub(crate) const IAP_DATASTREAM_UUID: &str = "E9459FD0-BCAD-4C45-820F-1E72447EF2F2";
/// Every package the phone sends begins with this many bytes of header; of its
/// fields only the type is of any use here.
const PACKAGE_HEADER: usize = 32;
/// `'comm'` read big-endian: the packages whose body is raw iAP2.
const PACKAGE_COMM: u32 = 0x636f_6d6d;
/// A package claiming to be larger than this is a decrypt that went wrong
/// rather than a message, and the framing is gone with it.
const MAX_PACKAGE: usize = 4 * 1024 * 1024;

/// What the tunnel's iAP2 session is built from.
///
/// It is the wireless session, described exactly as the Bluetooth half
/// described it — which is why the parameters are read when the phone asks for
/// the stream rather than held from startup: hostapd's file is what that half
/// read a moment ago, and the two have to agree or the phone is being offered a
/// different accessory from the one it is already talking to.
pub(crate) struct Setup {
    pub cfg: Arc<Config>,
    pub auth: SharedCoprocessor,
    pub bt_mac: [u8; 6],
    /// The AirPlay identity's public key, hex, which the start session names so
    /// the phone recognises the accessory on this stream as the one it paired
    /// with. Read once at startup from the pairing the receiver is already
    /// using, so both halves of the session describe the same key.
    pub accessory_key: String,
    pub airplay_port: u32,
    pub panel: Arc<ClSink>,
    pub vehicle: Arc<Vehicle>,
    pub status: Arc<Status>,
    pub events: Arc<EventSink>,
    pub handover: Arc<Handover>,
    /// Fires when what the phone renders changes. The AirPlay session ends on
    /// it, so the session on the tunnel is told the same thing.
    pub restart: broadcast::Sender<()>,
}

/// One open stream of type 130, from the SETUP that named it to the end of the
/// AirPlay session it belongs to.
pub(crate) struct Tunnel {
    port: u16,
    /// Accepts the phone's data connection.
    accept: JoinHandle<()>,
    /// The task reading the newest data connection. Held here rather than inside
    /// the accept loop so that ending the tunnel ends it too: dropping a
    /// `JoinHandle` only detaches it, and a reader left behind keeps the phone's
    /// socket — and a copy of the key — alive for as long as the phone holds it
    /// open.
    reader: Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
    /// Everything downstream of that connection, once RECORD has started it.
    link: Option<Link>,
    /// The phone's bytes on their way into the iAP2 link. Also where an
    /// `iAPSendMessage` the phone sent down the event channel goes.
    to_link: mpsc::UnboundedSender<Vec<u8>>,
    /// The receiving end of the same channel, until there is a link to give it
    /// to. Bytes that arrive first wait here rather than being dropped.
    from_link: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    /// The wireless session as it was when the phone asked for this stream.
    params: SessionParams,
    setup: Arc<Setup>,
}

/// The four tasks one tunnel's iAP2 session is made of. Aborting them is how the
/// session ends: dropping the link drops the phone's stream with it.
struct Link {
    pump: JoinHandle<()>,
    accessory: JoinHandle<()>,
    carrying: JoinHandle<()>,
    events: JoinHandle<()>,
}

impl Tunnel {
    /// Answers a SETUP for type 130: derives the key, binds a port, and returns
    /// the port to tell the phone about.
    ///
    /// Binding on `::` with a kernel-chosen port, like every other listener
    /// here: a session that has just ended leaves a socket behind for a moment,
    /// and a fixed number would collide with it.
    pub(crate) async fn open(
        shared: [u8; 32],
        seed: &str,
        setup: Arc<Setup>,
    ) -> std::io::Result<(Self, u16)> {
        let params = session_params(
            &setup.cfg,
            setup.airplay_port,
            setup.bt_mac,
            &setup.accessory_key,
        )
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let key = SymKey::from_bytes(&hkdf32(
            &shared,
            &salt(seed),
            "DataStream-Output-Encryption-Key",
        ))?;
        let listener = TcpListener::bind(("::", 0)).await?;
        let port = listener.local_addr()?.port();
        let (to_link, from_link) = mpsc::unbounded_channel();
        let reader = Arc::new(std::sync::Mutex::new(None));
        let accept = tokio::spawn(accept_loop(
            listener,
            key,
            to_link.clone(),
            setup.clone(),
            reader.clone(),
        ));
        tracing::info!(
            "airplay.iap_tunnel_ready seed={seed} data_port={port} ssid={:?} channel={} config={} \
             pk_len={}",
            params.ssid,
            params.channel,
            params.path.display(),
            params.cp.public_key.len()
        );
        Ok((
            Self {
                port,
                accept,
                reader,
                link: None,
                to_link,
                from_link: Some(from_link),
                params,
                setup,
            },
            port,
        ))
    }

    /// Bytes the phone posted down the event channel instead of the stream.
    /// The reference handles both paths; the stream is the one it normally uses.
    pub(crate) fn feed(&self, data: Vec<u8>) {
        let _ = self.to_link.send(data);
    }

    /// Starts the iAP2 session on the tunnel, at RECORD.
    ///
    /// Not at SETUP: the phone asks for the stream and connects to it in the
    /// middle of the media handshake, and a session offered before the phone is
    /// listening for it would negotiate with nobody. LIVI starts its relay at
    /// the same point, and so does MacPlay.
    pub(crate) fn start_iap2(&mut self) {
        if self.link.is_some() {
            return;
        }
        let Some(from_link) = self.from_link.take() else {
            return;
        };
        let setup = self.setup.clone();

        // The link reads what the phone sends from one end and writes back what
        // the accessory wants to send; the pump below moves those bytes between
        // it, the encrypted stream and the event channel.
        let (phone_side, link_side) = tokio::io::duplex(64 * 1024);
        // The accessory speaks first: the phone is already inside CarPlay and
        // has no negotiation of its own to offer. `zero_ack` because a data
        // stream has no acknowledgement channel of its own.
        let link_config = LinkConfig {
            max_outgoing: 4,
            control_version: 2,
            zero_ack: true,
            ..LinkConfig::default()
        };
        let (channel, artwork) = spawn_link_stream(link_side, link_config, true);
        let carrying = tokio::spawn(carry_artwork(artwork, setup.panel.clone()));

        let (tx, rx) = mpsc::channel(64);
        let auth = setup.auth.clone();
        let (identity, cp) = (self.params.identity.clone(), self.params.cp.clone());
        // `Vehicle` owns the watch senders and `feed` only hands out receivers,
        // so it has to outlive the session it feeds — the bridge holds it.
        let vehicle = setup.vehicle.clone();
        let feed = vehicle.feed();
        let accessory = tokio::spawn(async move {
            run_accessory(channel, auth, identity, cp, tx, feed).await
        });

        let pump = tokio::spawn(pump(phone_side, from_link, setup.events.clone()));

        let mut changed = setup.restart.subscribe();
        let (panel, status, ssid, airplay_port) = (
            setup.panel.clone(),
            setup.status.clone(),
            self.params.ssid.clone(),
            setup.airplay_port,
        );
        let events = tokio::spawn(async move {
            // The area is `tunnel` so the log says which of the two iAP2
            // sessions a line belongs to: during the handover both are running.
            let ended = run_bringup(
                "tunnel",
                "wifi",
                airplay_port,
                Some(ssid.as_str()),
                rx,
                &mut changed,
                &panel,
                &status,
            )
            .await;
            match ended {
                Ended::Restart => tracing::info!("tunnel.iap2_ended reason=render_changed"),
                Ended::Finished => tracing::info!("tunnel.iap2_ended"),
            }
        });

        tracing::info!("tunnel.iap2_started data_port={}", self.port);
        self.link = Some(Link {
            pump,
            accessory,
            carrying,
            events,
        });
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept.abort();
        if let Some(reader) = self.reader.lock().unwrap_or_else(|e| e.into_inner()).take() {
            reader.abort();
        }
        if let Some(link) = self.link.take() {
            link.pump.abort();
            link.accessory.abort();
            link.carrying.abort();
            link.events.abort();
        }
        // The AirPlay session this belonged to is over, so the phone is no
        // longer reachable on Wi-Fi and has to be paged back onto Bluetooth —
        // whether or not it ever got as far as asking for the link to go.
        self.setup.handover.clear();
        tracing::info!("tunnel.closed data_port={}", self.port);
    }
}

/// The HKDF salt the phone derives the same key from.
fn salt(seed: &str) -> String {
    format!("DataStream-Salt{seed}")
}

/// Accepts the phone's data connection, and keeps only the newest one.
///
/// A phone that reconnects leaves the old connection behind half open, and two
/// readers on the same stream would each carry half the packages.
async fn accept_loop(
    listener: TcpListener,
    key: SymKey,
    to_link: mpsc::UnboundedSender<Vec<u8>>,
    setup: Arc<Setup>,
    reader: Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
) {
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!("tunnel.accept_failed err={e}");
                return;
            }
        };
        tracing::info!("tunnel.connected peer={peer}");
        // The phone only sends `disableBluetooth` once this is carrying iAP2, so
        // this is the moment Bluetooth has to stop paging.
        setup.handover.set();
        let task = tokio::spawn(read_stream(socket, key.clone(), to_link.clone()));
        let old = reader.lock().unwrap_or_else(|e| e.into_inner()).replace(task);
        if let Some(old) = old {
            old.abort();
        }
    }
}

/// Opens the phone's stream and hands out the iAP2 in it.
async fn read_stream(mut socket: TcpStream, key: SymKey, to_link: mpsc::UnboundedSender<Vec<u8>>) {
    let mut sealed = Vec::new();
    let mut plain = Vec::new();
    let mut counter = 0u64;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match socket.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("tunnel.read_failed err={e}");
                break;
            }
        };
        sealed.extend_from_slice(&buf[..n]);
        // The counter advances only on a frame that opens, so one bad frame is
        // the end of the stream rather than the start of every later frame
        // opening into garbage.
        if let Err(e) = open_frames(&key, &mut counter, &mut sealed, &mut plain) {
            tracing::warn!("tunnel.decrypt_failed err={e}");
            return;
        }
        let mut bodies = Vec::new();
        if let Err(size) = take_packages(&mut plain, &mut bodies) {
            tracing::warn!("tunnel.package_implausible size={size}");
            return;
        }
        for body in bodies {
            if to_link.send(body).is_err() {
                return;
            }
        }
    }
    tracing::info!("tunnel.disconnected");
}

/// Splits whole packages off the front of `plain`, keeping the iAP2 ones.
///
/// The header's first four bytes are the package's total length, big-endian,
/// and its type sits at byte 16. Anything whose length cannot be a package is
/// reported rather than skipped: there is no way to find the next header once
/// the length is wrong, so the stream is finished.
fn take_packages(plain: &mut Vec<u8>, out: &mut Vec<Vec<u8>>) -> Result<(), usize> {
    while plain.len() >= PACKAGE_HEADER {
        let size = u32::from_be_bytes([plain[0], plain[1], plain[2], plain[3]]) as usize;
        if !(PACKAGE_HEADER..=MAX_PACKAGE).contains(&size) {
            return Err(size);
        }
        if plain.len() < size {
            break;
        }
        let kind = u32::from_be_bytes([plain[16], plain[17], plain[18], plain[19]]);
        if kind == PACKAGE_COMM {
            out.push(plain[PACKAGE_HEADER..size].to_vec());
        }
        plain.drain(..size);
    }
    Ok(())
}

/// Moves iAP2 between the link and the two channels it is carried on: the
/// phone's stream on the way in, the event channel on the way out.
async fn pump(
    phone_side: DuplexStream,
    mut from_stream: mpsc::UnboundedReceiver<Vec<u8>>,
    events: Arc<EventSink>,
) {
    let (mut read, mut write) = tokio::io::split(phone_side);

    let into_link = async {
        while let Some(data) = from_stream.recv().await {
            if write.write_all(&data).await.is_err() {
                break;
            }
        }
    };
    let out_of_link = async {
        let mut buf = vec![0u8; 8 * 1024];
        loop {
            match read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    // One `iAPSendMessage` per read: the phone reassembles the
                    // frames out of them, and a frame split across two messages
                    // is no different from one split across two reads.
                    if let Err(e) = events.send_iap(buf[..n].to_vec()).await {
                        tracing::warn!("tunnel.event_send_failed err={e}");
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("tunnel.link_read_failed err={e}");
                    break;
                }
            }
        }
    };
    tokio::select! {
        _ = into_link => {}
        _ = out_of_link => {}
    }
    tracing::info!("tunnel.pump_ended");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(kind: u32, body: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; PACKAGE_HEADER];
        p[..4].copy_from_slice(&((PACKAGE_HEADER + body.len()) as u32).to_be_bytes());
        p[16..20].copy_from_slice(&kind.to_be_bytes());
        p.extend_from_slice(body);
        p
    }

    #[test]
    fn the_salt_is_the_one_the_phone_derives_from() {
        // The same construction LIVI's `iap_tunnel` and playport's `IapTunnel`
        // use; a separator or a prefix here is a key that never matches.
        assert_eq!(salt("42"), "DataStream-Salt42");
        assert_eq!(salt(""), "DataStream-Salt");
    }

    #[test]
    fn only_comm_packages_reach_iap2() {
        let stream = [
            package(PACKAGE_COMM, b"iap2"),
            package(1, b"other"),
            package(PACKAGE_COMM, b""),
            package(PACKAGE_COMM, b"more"),
        ]
        .concat();

        // Across read boundaries: the same stream in three arbitrary pieces
        // must come out the same way.
        let mut plain = Vec::new();
        let mut out = Vec::new();
        for piece in [&stream[..5], &stream[5..PACKAGE_HEADER + 8], &stream[PACKAGE_HEADER + 8..]] {
            plain.extend_from_slice(piece);
            take_packages(&mut plain, &mut out).expect("no package is implausible");
        }
        assert_eq!(out, [b"iap2".to_vec(), Vec::new(), b"more".to_vec()]);
        assert!(plain.is_empty(), "every whole package must be consumed");
    }

    #[test]
    fn a_package_that_is_still_arriving_is_left_alone() {
        let stream = package(PACKAGE_COMM, b"iap2").to_vec();
        let mut plain = stream[..stream.len() - 2].to_vec();
        let mut out = Vec::new();
        take_packages(&mut plain, &mut out).expect("nothing has gone wrong yet");
        assert!(out.is_empty());
        assert_eq!(plain.len(), stream.len() - 2, "the partial package must stay");
    }

    #[test]
    fn an_implausible_size_ends_the_connection() {
        for size in [0u32, PACKAGE_HEADER as u32 - 1, MAX_PACKAGE as u32 + 1] {
            let mut plain = vec![0u8; PACKAGE_HEADER];
            plain[..4].copy_from_slice(&size.to_be_bytes());
            let mut out = Vec::new();
            assert_eq!(take_packages(&mut plain, &mut out), Err(size as usize));
        }
    }
}
