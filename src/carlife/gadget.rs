//! The car's half of the wire: the USB device this program presents, the AOA
//! requests that arrive on it, and the two bulk endpoints the CarLife session
//! runs over.
//!
//! The device is this program's, start to finish. `usb` composes it and puts it
//! on the controller; everything here reacts to what then arrives on it — which
//! is what a head unit expects, since a car asks a device for a CarLife session
//! and never the other way round.
//!
//! Two of the three requests a head unit sends are answered on the spot. The
//! third asks the device to come back as an *accessory* rather than as the phone
//! it found, and that is done by taking it off the bus and putting it back with
//! the numbers and strings a switched phone carries: `usb::Usb::reidentify`. The
//! interface itself does not change — a head unit takes the accessory's endpoints
//! by position, and both forms have the same two.

use std::fs;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::mpsc;

use crate::airplay::EventSink;
use crate::config::Config;

use super::desc;
use super::session::{self, Frames};
use super::usb::{Endpoints, Usb};

/// How long to let the answer to `start accessory` leave before the device is
/// taken away. The transfer is closed by `complete`, and the host takes its
/// result a moment later; the device going away in the same breath is a race the
/// host can lose even so.
const SETTLE: Duration = Duration::from_millis(100);

/// What FunctionFS numbers its events. `uapi/linux/usb/functionfs.h`.
const EVENT_ENABLE: u8 = 2;
const EVENT_DISABLE: u8 = 3;
const EVENT_SETUP: u8 = 4;

/// What the control endpoint tells the session loop.
enum Event {
    /// The host has configured the device; the endpoints are usable.
    Enabled,
    /// It has gone.
    Disabled,
    /// The device has been switched to the accessory form and is on its way back
    /// to the host. What the host enumerates next is the same device under new
    /// numbers.
    Accessory,
}

/// What answering one request did.
enum Answered {
    Nothing,
    Switched,
}

/// Runs one CarLife session after another over the device this program composed.
///
/// The session outlives the form the device is in — the device stays up, and only
/// what it calls itself changes — so a session that ends leaves it ready for the
/// next head unit rather than needing to be rebuilt.
pub async fn run<F>(
    cfg: Arc<Config>,
    frames: &Frames,
    screen: session::ScreenReport<'_>,
    events: Arc<EventSink>,
    carplay: &mut F,
) -> Result<()>
where
    F: FnMut(bool),
{
    let identity = desc::Identity::phone(&cfg.name);
    let (usb, endpoints) = Usb::open(&identity)?;
    let Endpoints {
        control,
        from_car,
        to_car,
    } = endpoints;

    tracing::info!("[cl-usb] waiting for a head unit to enumerate the device");

    let (events_tx, mut events_rx) = mpsc::channel(4);
    // The endpoint files come out of `usb` as they came from the kernel, and are
    // wrapped here: a read on one of them waits for the host, which is what
    // tokio's blocking pool is for.
    let controlling = tokio::spawn(control_loop(
        usb,
        identity,
        tokio::fs::File::from_std(control),
        events_tx,
    ));

    while let Some(event) = events_rx.recv().await {
        match event {
            // A session runs in either form. A head unit that does the handshake
            // gets one once it has enumerated the accessory; one that talks
            // CarLife straight away gets one here. Until the car says the first
            // thing on the channel, a session is silent, so the one that turns out
            // not to be needed costs nothing.
            Event::Enabled => match bulk(&from_car, &to_car) {
                Ok(pair) => {
                    tracing::info!("[cl-usb] the host has enumerated us; starting the session");
                    if let Err(e) =
                        session::run(pair, frames, &cfg, &mut *screen, events.clone(), carplay).await
                    {
                        tracing::warn!("[cl] the session ended badly: {e:#}");
                    }
                }
                Err(e) => tracing::error!("[cl-usb] the endpoints are not usable: {e:#}"),
            },
            Event::Disabled => tracing::info!("[cl-usb] the host has gone"),
            Event::Accessory => {
                tracing::info!("[cl-usb] the device is an accessory from here on")
            }
        }
    }

    controlling.abort();
    tracing::info!("[cl-usb] the control endpoint is gone");
    Ok(())
}

/// Reads the control endpoint for the life of the process.
///
/// It owns the endpoint because it is also what answers on it: a setup event and
/// the reply to it are two halves of the same exchange, and nothing else may
/// interleave with them.
async fn control_loop(
    usb: Usb,
    mut identity: desc::Identity,
    mut ep0: tokio::fs::File,
    events: mpsc::Sender<Event>,
) {
    let mut asked = desc::Asked::default();

    loop {
        // The whole event is twelve bytes: the eight of a `usb_ctrlrequest`, the
        // type, and three of padding. Read as bytes and unpacked by hand, so
        // there is no packed struct to get wrong.
        let mut raw = [0u8; 12];
        if let Err(e) = ep0.read_exact(&mut raw).await {
            // Not a quiet ending: this endpoint is the only way a head unit
            // reaches us, so its loss is the loss of the CarLife half.
            tracing::warn!("[cl-usb] the control endpoint stopped reading: {e:#}");
            return;
        }
        match raw[8] {
            EVENT_ENABLE => {
                let _ = events.send(Event::Enabled).await;
            }
            EVENT_DISABLE => {
                let _ = events.send(Event::Disabled).await;
            }
            EVENT_SETUP => {
                match answer(&usb, &mut identity, &mut ep0, &mut asked, &raw).await {
                    Ok(Answered::Switched) => {
                        if events.send(Event::Accessory).await.is_err() {
                            return;
                        }
                    }
                    Ok(Answered::Nothing) => {}
                    Err(e) => tracing::warn!("[cl-usb] a control request could not be answered: {e:#}"),
                }
            }
            other => tracing::debug!("[cl-usb] control event {other}"),
        }
    }
}

/// Answers one setup event.
///
/// Every request is reported, understood or not: which ones a given head unit
/// sends is not settled, and a line per request is the cheapest way to find out.
/// The data stage of a request that has one is always read, whatever the request
/// is, because a device that ignores it leaves the host waiting.
async fn answer(
    usb: &Usb,
    identity: &mut desc::Identity,
    ep0: &mut tokio::fs::File,
    asked: &mut desc::Asked,
    raw: &[u8; 12],
) -> Result<Answered> {
    let setup = desc::Setup::decode(raw).ok_or_else(|| anyhow!("a setup event with no setup packet"))?;
    tracing::info!(
        "[cl-usb] control request {} {}, type {:#04x}, value {}, index {}, length {}",
        setup.request,
        setup.direction(),
        setup.request_type,
        setup.value,
        setup.index,
        setup.length
    );

    match setup.accessory() {
        // "What version of the accessory protocol do you speak?"
        Some(desc::Accessory::GetProtocol) => {
            let reply = desc::version_reply();
            ep0.write_all(&reply).await?;
            // Nothing may be left buffered: the host is sitting on a control
            // transfer that this is the answer to.
            ep0.flush().await?;
            tracing::info!("[cl-usb] answered with AOA {}", desc::AOA_VERSION);
            Ok(Answered::Nothing)
        }
        // One of the six strings the host wants the device to call itself.
        Some(desc::Accessory::Identify { index }) => {
            let mut text = vec![0u8; setup.length as usize];
            ep0.read_exact(&mut text).await?;
            tracing::info!(
                "[cl-usb] identifier {index}: {:?}",
                String::from_utf8_lossy(&text)
            );
            asked.set(index, String::from_utf8_lossy(&text).into_owned());
            Ok(Answered::Nothing)
        }
        // "Switch to accessory mode and re-enumerate."
        Some(desc::Accessory::Start) => {
            // The request has to be ended before the device is taken away: the
            // host is sitting in this transfer, and it is the device that closes
            // it. See `complete`.
            complete(ep0).await?;
            if identity.is_accessory() {
                tracing::info!(
                    "[cl-usb] the host asked us to switch to accessory mode; we are already one, \
                     so there is nothing to re-enumerate"
                );
                return Ok(Answered::Nothing);
            }
            switch(usb, identity, asked).await?;
            Ok(Answered::Switched)
        }
        // Not one of the three. Where it carries data, the data is still read.
        None => {
            if setup.direction() == "out" && setup.length > 0 {
                let mut dropped = vec![0u8; setup.length as usize];
                ep0.read_exact(&mut dropped).await?;
                tracing::info!(
                    "[cl-usb] that one is not acted on; its {} bytes are dropped",
                    setup.length
                );
            } else if setup.direction() == "in" {
                // Nothing to send, and an unanswered request leaves the host
                // waiting for it. A read while an incoming request is pending is
                // how FunctionFS is told to refuse it, which is a stall — the
                // answer USB has for a request a device does not implement.
                let mut refused = [0u8; 1];
                let _ = ep0.read(&mut refused).await;
                tracing::info!("[cl-usb] that one is not acted on, and is refused");
            } else {
                complete(ep0).await?;
                tracing::info!("[cl-usb] that one is not acted on");
            }
            Ok(Answered::Nothing)
        }
    }
}

/// Ends a control request that carries no data.
///
/// A transfer is not over when the request arrives; it ends with a status stage,
/// and for a request with nothing to carry the device is the one that sends it.
/// FunctionFS sends it when the queue it is handed comes back empty, so a read
/// that has nothing to read is what finishes the transfer — the read returns no
/// bytes, and that is the answer rather than an end of file.
///
/// Leaving it out is not a quiet mistake: the host stays in the transfer, and a
/// device that goes on to take itself off the bus fails a transfer the host was
/// still waiting in.
async fn complete(ep0: &mut tokio::fs::File) -> Result<()> {
    let mut nothing = [0u8; 1];
    ep0.read(&mut nothing).await?;
    Ok(())
}

/// Switches the device to the accessory form.
///
/// What the host asked for is not a new device but the same one coming back as
/// an accessory, which is the only way it can see a device it did not enumerate:
/// off the bus, new numbers and the strings it just named, back on. The request
/// itself is ended before any of that, and `usb` holds the device off the bus
/// long enough for the host to see it go.
async fn switch(usb: &Usb, identity: &mut desc::Identity, asked: &desc::Asked) -> Result<()> {
    tokio::time::sleep(SETTLE).await;

    let accessory = identity.accessory(asked);
    usb.reidentify(&accessory)
        .await
        .context("switching the device to its accessory form")?;
    tracing::info!(
        "[cl-usb] switched to an accessory ({}); the host will enumerate us again",
        accessory.describe()
    );
    *identity = accessory;
    Ok(())
}

/// A byte pipe over the two bulk endpoints, the shape `session` wants.
///
/// The descriptors are duplicated rather than reopened: there is one set of them
/// for the life of the process, and both copies refer to the same endpoint. Reads
/// and writes are blocking calls, handed to tokio's blocking pool, which is what a
/// bulk endpoint is — a read waits until the car sends something and a write waits
/// until it has taken it.
fn bulk(from_car: &fs::File, to_car: &fs::File) -> io::Result<Bulk> {
    Ok(Bulk {
        from_car: tokio::fs::File::from_std(from_car.try_clone()?),
        to_car: tokio::fs::File::from_std(to_car.try_clone()?),
    })
}

struct Bulk {
    from_car: tokio::fs::File,
    to_car: tokio::fs::File,
}

impl AsyncRead for Bulk {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().from_car).poll_read(cx, buf)
    }
}

impl AsyncWrite for Bulk {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().to_car).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().to_car).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().to_car).poll_shutdown(cx)
    }
}
