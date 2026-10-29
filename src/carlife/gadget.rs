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
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use livi_runtime::vehicle::Vehicle;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{broadcast, mpsc};

use crate::airplay::{EventSink, SharedChannel};
use crate::config::{dt_vendor, hostname, Config};
use crate::web::Status;

use super::aio::{Context as AioContext, IoEvent};
use super::auth;
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
    channel: SharedChannel,
    events: Arc<EventSink>,
    carplay: &mut F,
    desktop: &broadcast::Sender<()>,
    status: Arc<Status>,
    vehicle: Arc<Vehicle>,
    mut replug: mpsc::Receiver<()>,
) -> Result<()>
where
    F: FnMut(bool),
{
    let identity = desc::Identity::phone(&hostname(), &dt_vendor());
    let (usb, endpoints) = Usb::open(&identity)?;
    let Endpoints {
        control,
        from_car,
        to_car,
    } = endpoints;
    // A device that is on the controller and not yet enumerated is a cable, a
    // port or a car, and saying so is what makes "nothing is happening" mean
    // something specific.
    status.with(|s| {
        s.carlife.usb.controller = Some(usb.controller().to_string());
        s.carlife.usb.enumerated = Some(false);
    });
    describe_identity(&status, &identity);

    tracing::info!("usb.waiting_for_host");

    // The key the car's challenge is answered with, read out of the data
    // directory the firmware installs it in. Once rather than per session: it is
    // one key, and a car plugged out and back in finds what the session before it
    // had.
    //
    // Without it the car is left unauthenticated rather than failed: the
    // challenge is skipped, the head unit asks again in half a minute and is
    // skipped again, and the streams run either way. The key is Baidu's and not
    // every build is going to install it, so this is said once here and never
    // again.
    let auth = auth::Signer::load();

    let (events_tx, mut events_rx) = mpsc::channel(4);
    // Kept back from the task below, which owns the device: what the controller
    // says about the link is read here, and only while an event is being
    // reported — see `usb::Usb::link`.
    let udc = usb.clone();
    // The endpoint files come out of `usb` as they came from the kernel, and are
    // wrapped here: a read on one of them waits for the host, which is what
    // tokio's blocking pool is for.
    let controlling = tokio::spawn(control_loop(
        usb,
        identity,
        tokio::fs::File::from_std(control),
        events_tx,
        status.clone(),
    ));

    let bus = udc.clone();
    tokio::spawn(async move {
        while replug.recv().await.is_some() {
            tracing::info!("usb.replug action=disconnect");
            if let Err(e) = bus.replug().await {
                tracing::error!("usb.replug_failed err={e:#}");
            }
        }
    });

    while let Some(event) = events_rx.recv().await {
        match event {
            // A session runs in either form. A head unit that does the handshake
            // gets one once it has enumerated the accessory; one that talks
            // CarLife straight away gets one here. Until the car says the first
            // thing on the channel, a session is silent, so the one that turns out
            // not to be needed costs nothing.
            Event::Enabled => {
                status.with(|s| s.carlife.usb.enumerated = Some(true));
                match bulk(&from_car, &to_car) {
                    Ok(pair) => {
                        // What the car negotiated, beside the fact that it
                        // configured the device: the two are what a session that
                        // never says anything has to be read against.
                        tracing::info!("usb.enumerated action=start_session {}", udc.link());
                        let cfg = Config {
                            screen: crate::settings::load(),
                            ..(*cfg).clone()
                        };
                        if let Err(e) = session::run(
                            pair,
                            frames,
                            &cfg,
                            auth.clone(),
                            &mut *screen,
                            channel.clone(),
                            events.clone(),
                            carplay,
                            desktop,
                            status.clone(),
                            vehicle.clone(),
                        )
                        .await
                        {
                            // A session that ends because the device left the
                            // bus is not one that failed: every transfer on a
                            // disabled endpoint comes back with `ESHUTDOWN`, and
                            // what took the device away — the switch to
                            // accessory, or the car unplugging — reports itself
                            // where it happens.
                            if !off_bus(&e) {
                                tracing::warn!("usb.session_failed err={e:#}");
                            }
                        }
                    }
                    Err(e) => tracing::error!("usb.endpoints_unusable err={e:#}"),
                }
            }
            Event::Disabled => {
                status.with(|s| s.carlife.usb.enumerated = Some(false));
                tracing::info!("usb.host_gone");
            }
            Event::Accessory => {
                status.with(|s| s.carlife.accessory = true);
                tracing::info!("usb.accessory_mode state=on")
            }
        }
    }

    controlling.abort();
    tracing::info!("usb.control_closed");
    Ok(())
}

/// Whether a session ended because the device left the bus.
///
/// FunctionFS answers every transfer on an endpoint that is no longer active with
/// `ESHUTDOWN`, whether the host went away or the gadget was unbound for its
/// other form; there is no third way to get it. So this is the pipe ending, not a
/// fault in it.
fn off_bus(e: &anyhow::Error) -> bool {
    e.root_cause()
        .downcast_ref::<io::Error>()
        .is_some_and(|e| e.raw_os_error() == Some(libc::ESHUTDOWN))
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
    status: Arc<Status>,
) {
    loop {
        // The whole event is twelve bytes: the eight of a `usb_ctrlrequest`, the
        // type, and three of padding. Read as bytes and unpacked by hand, so
        // there is no packed struct to get wrong.
        let mut raw = [0u8; 12];
        if let Err(e) = ep0.read_exact(&mut raw).await {
            // Not a quiet ending: this endpoint is the only way a head unit
            // reaches us, so its loss is the loss of the CarLife half.
            tracing::warn!("usb.control_read_failed err={e:#}");
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
                match answer(&usb, &mut identity, &mut ep0, &raw, &status).await {
                    Ok(Answered::Switched) => {
                        if events.send(Event::Accessory).await.is_err() {
                            return;
                        }
                    }
                    Ok(Answered::Nothing) => {}
                    Err(e) => {
                        tracing::warn!("usb.control_unanswered err={e:#}")
                    }
                }
            }
            other => tracing::debug!("usb.control_event event={other}"),
        }
    }
}

/// Answers one setup event.
///
/// Every request is reported, understood or not: which ones a given head unit
/// sends is not settled, and a line per request is the cheapest way to find out.
/// A request and whatever its data stage carries are one exchange, so they are
/// one line — the data is read first and reported on the line that requests it.
/// The data stage of a request that has one is always read, whatever the request
/// is, because a device that ignores it leaves the host waiting.
async fn answer(
    usb: &Usb,
    identity: &mut desc::Identity,
    ep0: &mut tokio::fs::File,
    raw: &[u8; 12],
    status: &Status,
) -> Result<Answered> {
    let setup = desc::Setup::decode(raw).ok_or_else(|| anyhow!("a setup event with no setup packet"))?;

    match setup.accessory() {
        // "What version of the accessory protocol do you speak?"
        Some(desc::Accessory::GetProtocol) => {
            let reply = desc::version_reply();
            ep0.write_all(&reply).await?;
            // Nothing may be left buffered: the host is sitting on a control
            // transfer that this is the answer to.
            ep0.flush().await?;
            status.with(|s| s.carlife.aoa_version = Some(desc::AOA_VERSION));
            exchange(&setup, format_args!("version={}", desc::AOA_VERSION));
            Ok(Answered::Nothing)
        }
        // One of the six strings the host wants the device to call itself.
        Some(desc::Accessory::Identify { index }) => {
            let mut text = vec![0u8; setup.length as usize];
            ep0.read_exact(&mut text).await?;
            let text = String::from_utf8_lossy(&text).into_owned();
            exchange(&setup, format_args!("text={text:?}"));
            // The six are the car's own CarLife application naming itself, and
            // they are worth keeping: they are the only description of the head
            // unit anything here ever sees. They go to the page and no further —
            // the device keeps the name it was enumerated under, as a phone does.
            status.with(|s| {
                if let Some(slot) = s.carlife.asked.get_mut(index) {
                    *slot = Some(text);
                }
            });
            Ok(Answered::Nothing)
        }
        // "Switch to accessory mode and re-enumerate."
        Some(desc::Accessory::Start) => {
            // The request has to be ended before the device is taken away: the
            // host is sitting in this transfer, and it is the device that closes
            // it. See `complete`.
            complete(ep0).await?;
            if identity.is_accessory() {
                exchange(&setup, format_args!("result=already_accessory"));
                return Ok(Answered::Nothing);
            }
            switch(usb, identity).await?;
            describe_identity(status, identity);
            exchange(&setup, format_args!("result=ok identity={:?}", identity.describe()));
            Ok(Answered::Switched)
        }
        // Not one of the three. Where it carries data, the data is still read.
        None => {
            if setup.direction() == "out" && setup.length > 0 {
                let mut dropped = vec![0u8; setup.length as usize];
                ep0.read_exact(&mut dropped).await?;
                exchange(
                    &setup,
                    format_args!("result=ignored bytes_dropped={}", setup.length),
                );
            } else if setup.direction() == "in" {
                // Nothing to send, and an unanswered request leaves the host
                // waiting for it. A read while an incoming request is pending is
                // how FunctionFS is told to refuse it, which is a stall — the
                // answer USB has for a request a device does not implement.
                let mut refused = [0u8; 1];
                let _ = ep0.read(&mut refused).await;
                exchange(&setup, format_args!("result=refused"));
            } else {
                complete(ep0).await?;
                exchange(&setup, format_args!("result=ignored"));
            }
            Ok(Answered::Nothing)
        }
    }
}

/// Reports one control exchange, once.
///
/// The direction bit says which way the data stage goes, and that is the whole
/// of the name: data arriving is a receive, data going out is a send, the same
/// two words the CarLife lines use. What came of it ends the same line, so a
/// request and its data stage are never two lines that have to be read
/// together.
fn exchange(setup: &desc::Setup, outcome: std::fmt::Arguments<'_>) {
    let name = match setup.direction() {
        "in" => "usb.msg.send",
        _ => "usb.msg.recv",
    };
    tracing::info!(
        "{name} request={} type={:#04x} value={} index={} length={} {outcome}",
        setup.request,
        setup.request_type,
        setup.value,
        setup.index,
        setup.length,
    );
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
/// off the bus, new numbers, back on. The request itself is ended before any of
/// that, and `usb` holds the device off the bus long enough for the host to see
/// it go. Nothing is reported here: the exchange that asked for it reports the
/// identity it came back with.
async fn switch(usb: &Usb, identity: &mut desc::Identity) -> Result<()> {
    tokio::time::sleep(SETTLE).await;

    let accessory = identity.accessory();
    usb.reidentify(&accessory)
        .await
        .context("switching the device to its accessory form")?;
    *identity = accessory;
    Ok(())
}

/// The numbers and strings of the device descriptor, as a host reads them.
///
/// Reported wherever the identity changes rather than once: the device presents
/// two of them, and which one is on the wire is the difference between a car
/// seeing a phone and a car seeing an accessory.
fn describe_identity(status: &Status, identity: &desc::Identity) {
    status.with(|s| {
        s.carlife.usb.vid = Some(identity.vid);
        s.carlife.usb.pid = Some(identity.pid);
        s.carlife.usb.manufacturer = Some(identity.manufacturer.clone());
        s.carlife.usb.product = Some(identity.product.clone());
        s.carlife.usb.serial = Some(identity.serial.clone());
    });
}

/// How much one read on the car's endpoint asks for.
///
/// A read there becomes one `usb_request` of that size, and the controller ends
/// the request at the first short packet. A request with no room for a whole
/// packet loses the rest of it: the packet is acknowledged to the host, the
/// request takes what fits, and the remainder is dropped. At this size a packet
/// always fits. Sixteen kilobytes is what adbd asks for on the same kind of
/// endpoint, on its note that not every controller takes more.
const READ_SIZE: usize = 16 * 1024;

/// How many reads are kept outstanding — a queue, not a buffer.
///
/// A packet is safe only while a request is queued for it. The controller takes
/// a packet into its receive fifo whether or not one is, and flushes that fifo
/// when the next request is queued. A queue that never empties is what removes
/// the window rather than narrowing it. Eight is adbd's depth, for the same
/// reason.
const AHEAD: usize = 8;

/// A byte pipe over the two bulk endpoints, the shape `session` wants.
///
/// The descriptors are duplicated rather than reopened: there is one set of them
/// for the life of the process, and both copies refer to the same endpoint.
/// Writes are blocking calls handed to tokio's blocking pool — a write waits
/// until the endpoint has taken it. Reads are not: see `read_ahead`.
fn bulk(from_car: &fs::File, to_car: &fs::File) -> io::Result<Bulk> {
    Ok(Bulk {
        from_car: read_ahead(from_car.try_clone()?)?,
        to_car: tokio::fs::File::from_std(to_car.try_clone()?),
    })
}

/// Reads the endpoint ahead of whoever consumes it, with several requests
/// queued at once.
///
/// This is what an application can do about the lost packet. FunctionFS gives a
/// read whatever one transfer delivered and no more, so a read that was late
/// cannot get it back, and reading in a loop with one request at a time only
/// shortens the gap between reads. Keeping the endpoint's queue full closes it.
/// `aio` has the rest.
///
/// The thread ends when a read comes back with nothing or with an error. A host
/// that goes quiet without either leaves it waiting: the session has dropped its
/// end of the channel, but a wait on the endpoint has no way to notice that, and
/// no deadline either. It holds `AHEAD` buffers until the endpoint is disabled,
/// which is what a re-enumeration does.
fn read_ahead(source: fs::File) -> io::Result<Arrivals> {
    let (tx, pieces) = mpsc::channel(AHEAD);
    std::thread::Builder::new()
        .name("carlife-read".into())
        .spawn(move || {
            let fd = source.as_raw_fd();
            if let Err(e) = fill(fd, &tx) {
                let _ = tx.blocking_send(Err(e));
            }
        })?;
    Ok(Arrivals {
        pieces,
        pending: Vec::new(),
        at: 0,
    })
}

/// Keeps `AHEAD` reads outstanding and hands each one over as it finishes.
fn fill(fd: RawFd, tx: &mpsc::Sender<io::Result<Vec<u8>>>) -> io::Result<()> {
    let ctx = AioContext::new(AHEAD)?;
    // One buffer per request. A buffer belongs to the kernel from the moment it
    // is submitted until its event comes back.
    let mut buffers: Vec<Vec<u8>> = (0..AHEAD).map(|_| vec![0u8; READ_SIZE]).collect();
    for (slot, buffer) in buffers.iter_mut().enumerate() {
        ctx.submit(fd, buffer, slot as u64)?;
    }

    let mut events = [IoEvent::default(); AHEAD];
    loop {
        let done = ctx.wait(&mut events)?;
        for event in &events[..done] {
            let slot = event.data as usize;
            if event.res < 0 {
                return Err(io::Error::from_raw_os_error(-event.res as i32));
            }
            // Nothing at all is the host gone; closing the channel is how the
            // reader downstream is told.
            if event.res == 0 {
                return Ok(());
            }
            // The replacement is queued before the bytes are handed over, so
            // the queue is never short while the consumer is being woken.
            let mut arrived = std::mem::replace(&mut buffers[slot], vec![0u8; READ_SIZE]);
            arrived.truncate(event.res as usize);
            ctx.submit(fd, &mut buffers[slot], slot as u64)?;
            if tx.blocking_send(Ok(arrived)).is_err() {
                return Ok(());
            }
        }
    }
}

/// What the reading thread has handed over and nobody has taken yet.
struct Arrivals {
    pieces: mpsc::Receiver<io::Result<Vec<u8>>>,
    /// The piece being consumed, and how much of it has been.
    pending: Vec<u8>,
    at: usize,
}

impl AsyncRead for Arrivals {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let room = buf.remaining();
        if room == 0 {
            return Poll::Ready(Ok(()));
        }
        // A caller reading a header asks for eight bytes, and the piece in hand
        // is a whole packet: the rest of it stays here for the read after.
        if me.at < me.pending.len() {
            let taken = room.min(me.pending.len() - me.at);
            buf.put_slice(&me.pending[me.at..me.at + taken]);
            me.at += taken;
            return Poll::Ready(Ok(()));
        }
        match me.pieces.poll_recv(cx) {
            Poll::Ready(Some(Ok(piece))) => {
                let taken = room.min(piece.len());
                buf.put_slice(&piece[..taken]);
                me.pending = piece;
                me.at = taken;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Err(e)),
            // Nothing filled and nothing left: the end of the stream.
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

struct Bulk {
    from_car: Arrivals,
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::carlife::frame;

    /// A reader holding a queue that can be filled the way the reading thread
    /// fills it.
    fn fed(pieces: Vec<Vec<u8>>) -> Arrivals {
        let (tx, pieces_rx) = mpsc::channel(pieces.len().max(1));
        for piece in pieces {
            tx.try_send(Ok(piece)).expect("room for it");
        }
        drop(tx);
        Arrivals {
            pieces: pieces_rx,
            pending: Vec::new(),
            at: 0,
        }
    }

    fn message() -> Vec<u8> {
        frame::encode(frame::CMD, 0x0001_8001, 0, &[0x08, 0x03, 0x10, 0x00])
    }

    #[tokio::test]
    async fn a_body_in_its_own_read_is_joined_to_its_header() {
        // A head unit writes the eight-byte outer header and then the message
        // as two transfers, so two reads. Where one read ends must not matter.
        let message = message();
        let mut reader = fed(vec![
            frame::outer_head(frame::CMD, message.len()).to_vec(),
            message.clone(),
        ]);

        let chunk = frame::read_chunk(&mut reader).await.unwrap().expect("a chunk");
        assert_eq!(chunk.channel, frame::CMD);
        assert_eq!(chunk.body, message);
    }

    #[tokio::test]
    async fn a_header_is_taken_out_of_a_read_that_carries_more() {
        // The other half: whatever a read brought in and the caller did not
        // want stays for the read after it, rather than being dropped.
        let message = message();
        let mut both = frame::outer_head(frame::CMD, message.len()).to_vec();
        both.extend_from_slice(&message);
        let mut reader = fed(vec![both]);

        let chunk = frame::read_chunk(&mut reader).await.unwrap().expect("a chunk");
        assert_eq!(chunk.channel, frame::CMD);
        assert_eq!(chunk.body, message);
    }
}
