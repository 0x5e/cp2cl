//! The car's half of the wire: the USB device this program presents, the AOA
//! requests that arrive on it, and the two bulk endpoints the CarLife session
//! runs over.
//!
//! The device is this program's, start to finish. `usb` composes it and puts it
//! on the controller; everything here reacts to what then arrives on it — which
//! is what a head unit expects, since a car asks a device for a CarLife session
//! and never the other way round.
//!
//! It presents itself as an accessory from the start, which is the one form a
//! head unit talks CarLife to, so there is no phone to switch and no second
//! enumeration to wait for. The one of the three requests that asks for the
//! switch is answered as what the device already is, and followed by the edge
//! the car asked to see: off the bus and back — see `SWITCH_GAP`.
//!
//! A car that boots after this program misses the device arriving, so a device
//! it has configured and left silent is put back on the bus on a beat until a
//! session is up — see `RETRY`.

use std::fs;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::mpsc;

use super::aio::{Context as AioContext, IoEvent};
use super::auth;
use crate::desc;
use super::session;
use super::usb::{Endpoints, Usb};
use crate::report::Report;
use crate::ports::Ports;
use crate::machine::Machine;

/// How long to let the answer to `start accessory` land before the device is
/// taken away. The transfer is closed by `complete`, and the host takes its
/// result a moment later; the device going away in the same breath is a race the
/// host can lose even so.
const SETTLE: Duration = Duration::from_millis(100);

/// The shortest gap between two edges given to a car that asks for the accessory
/// form.
///
/// The device is an accessory already, so nothing has to be switched — but a car
/// can only tell a switch that worked from one that failed by watching the
/// device go and come back, which is what a phone does when it switches. So the
/// ask is answered with that edge. A car that asks again on every attach would
/// otherwise be handed an edge for ever, which is a loop rather than a switch.
const SWITCH_GAP: Duration = Duration::from_secs(30);

/// What FunctionFS numbers its events. `uapi/linux/usb/functionfs.h`.
const EVENT_ENABLE: u8 = 2;
const EVENT_DISABLE: u8 = 3;
const EVENT_SETUP: u8 = 4;

/// How often a device a car has configured and left silent is taken off the bus
/// and put back.
///
/// A head unit starts CarLife when it sees a device arrive, and one that boots
/// after this program has nothing left to notice: the attach it should have
/// reacted to is in the past, and no amount of waiting brings it back. Putting
/// the device back on the bus is a second attach, so this is tried again for as
/// long as a car configures the device and runs nothing on it.
const RETRY: Duration = Duration::from_secs(3);

/// What the control endpoint tells the session loop.
enum Event {
    /// The host has configured the device; the endpoints are usable.
    Enabled,
    /// It has gone.
    Disabled,
}

/// The CarLife half, ready to present itself to a head unit.
///
/// Built by the program and run in a task of its own: what it does is answer a
/// car, and the car decides when that is.
pub struct Md<'a> {
    settings: &'a Machine,
    ports: &'a Ports<'a>,
    replug: mpsc::Receiver<()>,
    /// The key the car's challenge is answered with, parsed by the program.
    ///
    /// Once rather than per session: it is one key, and a car plugged out and
    /// back in finds what the session before it had. `None` is a caller that
    /// holds no key it could read, which leaves the car unauthenticated rather
    /// than failed — the challenge is skipped, the head unit asks again in half
    /// a minute, and the streams run either way.
    auth: Option<auth::Signer>,
}

impl<'a> Md<'a> {
    pub fn new(
        settings: &'a Machine,
        ports: &'a Ports<'a>,
        replug: mpsc::Receiver<()>,
        auth: Option<auth::Signer>,
    ) -> Self {
        Self {
            settings,
            ports,
            replug,
            auth,
        }
    }

    /// Runs one CarLife session after another over the device this program
    /// composed.
    ///
    /// The device stays up for the life of the process, so a session that ends
    /// leaves it ready for the next head unit rather than needing to be rebuilt.
    pub async fn serve(self) -> Result<()> {
        let Self {
            settings,
            ports,
            mut replug,
            auth,
        } = self;
        let identity = desc::Identity::accessory(&settings.name, &settings.vendor);
        let (usb, endpoints) = Usb::open(&identity)?;
        let Endpoints {
            control,
            from_car,
            to_car,
        } = endpoints;
        // A device that is on the controller and not yet enumerated is a cable, a
        // port or a car, and saying so is what makes "nothing is happening" mean
        // something specific.
        ports.report.with(|s| {
            s.carlife.usb.controller = Some(usb.controller().to_string());
            s.carlife.usb.enumerated = Some(false);
        });
        describe_identity(&ports.report, &identity);

        // The form is not something a head unit asks for here, so it is said
        // once, where the device is put on the bus.
        tracing::info!("usb.accessory_mode state=on");
        tracing::info!("usb.waiting_for_host");

        let (events_tx, mut events_rx) = mpsc::channel(4);
        // When the last edge was given to a car that asked for the accessory
        // form. Kept where the answers are given, since that is the only place
        // that asks.
        let edges = Arc::new(Mutex::new(None::<Instant>));
        // The endpoint files come out of `Usb::open` as they came from the
        // kernel, and are wrapped here: a read on one of them waits for the host,
        // which is what tokio's blocking pool is for. The device goes back to
        // `serve` and is cloned here because an answer to one of the three
        // requests can put it off the bus and back.
        let controlling = tokio::spawn(control_loop(
            usb.clone(),
            tokio::fs::File::from_std(control),
            events_tx,
            ports.report.clone(),
            edges,
        ));

        // What the controller says about the link is read while an event is being
        // reported — see `usb::Usb::link` — and a replug is asked for through the
        // same handle.
        let bus = usb.clone();
        tokio::spawn(async move {
            while replug.recv().await.is_some() {
                tracing::info!("usb.replug action=disconnect");
                if let Err(e) = bus.replug().await {
                    tracing::error!("usb.replug_failed err={e:#}");
                }
            }
        });

        // A car that came up after this program did missed the attach, and a car
        // that is not talking on a device it configured is the one symptom of it
        // there is. Until a session is up, the device is put back on the bus on
        // this beat — which is the attach it missed, once every `RETRY`.
        let bouncing = usb.clone();
        let state = ports.report.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(RETRY);
            // A replug takes half a second, and the ticks it covers are not owed:
            // catching up on them would be a second replug in a row.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                // Nothing has been enumerated, so there is no car to make look
                // again, and one is talking, so there is nothing to fix.
                if !state.usb_enumerated() || state.session_up() {
                    continue;
                }
                tracing::info!("usb.replug reason=no_carlife_session");
                if let Err(e) = bouncing.replug().await {
                    tracing::error!("usb.replug_failed err={e:#}");
                }
            }
        });

        while let Some(event) = events_rx.recv().await {
            match event {
                // A session starts the moment a head unit has configured the
                // device, and nothing here waits for a greeting first: an
                // accessory is what the car came to find. Until the car says the
                // first thing on the channel a session is silent, so a car that
                // configured the device and has not spoken yet costs nothing.
                Event::Enabled => {
                    ports.report.with(|s| s.carlife.usb.enumerated = Some(true));
                    match bulk(&from_car, &to_car) {
                        Ok(pair) => {
                            // What the car negotiated, beside the fact that it
                            // configured the device: the two are what a session that
                            // never says anything has to be read against.
                            tracing::info!("usb.enumerated action=start_session {}", usb.link());
                            // The page can change the size while the program runs, so
                            // it is read again for each session.
                            let mut settings = settings.clone();
                            settings.screen = ports.delegate.screen_chosen();
                            if let Err(e) =
                                session::run(pair, &settings, auth.clone(), ports).await
                            {
                                // A session that ends because the device left the
                                // bus is not one that failed: every transfer on a
                                // disabled endpoint comes back with `ESHUTDOWN`, and
                                // what took the device away — a replug, or the car
                                // unplugging — reports itself where it happens.
                                if !off_bus(&e) {
                                    tracing::warn!("usb.session_failed err={e:#}");
                                }
                            }
                        }
                        Err(e) => tracing::error!("usb.endpoints_unusable err={e:#}"),
                    }
                }
                Event::Disabled => {
                    ports.report.with(|s| s.carlife.usb.enumerated = Some(false));
                    tracing::info!("usb.host_gone");
                }
            }
        }

        controlling.abort();
        tracing::info!("usb.control_closed");
        Ok(())
    }
}

/// Whether a session ended because the device left the bus.
///
/// FunctionFS answers every transfer on an endpoint that is no longer active with
/// `ESHUTDOWN`, whether the host went away or the gadget was unbound for a
/// replug; there is no third way to get it. So this is the pipe ending, not a
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
    mut ep0: tokio::fs::File,
    events: mpsc::Sender<Event>,
    status: Arc<Report>,
    edges: Arc<Mutex<Option<Instant>>>,
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
                if let Err(e) = answer(&usb, &mut ep0, &raw, &status, &edges).await {
                    tracing::warn!("usb.control_unanswered err={e:#}")
                }
            }
            other => tracing::debug!("usb.control_event event={other}"),
        }
    }
}

/// Answers one setup event.
///
/// Every request is reported twice, understood or not: once as it arrives
/// (`received`) and once with what came of it (`exchange`). Which ones a given
/// head unit sends is not settled, and two lines per request is the cheapest way
/// to find out without losing the ones that go wrong. The data stage of a
/// request that has one is always read, whatever the request is, because a
/// device that ignores it leaves the host waiting.
async fn answer(
    usb: &Usb,
    ep0: &mut tokio::fs::File,
    raw: &[u8; 12],
    status: &Report,
    edges: &Mutex<Option<Instant>>,
) -> Result<()> {
    let setup = desc::Setup::decode(raw).ok_or_else(|| anyhow!("a setup event with no setup packet"))?;
    received(&setup);

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
            Ok(())
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
            // the device keeps the name it enumerated under.
            status.with(|s| {
                if let Some(slot) = s.carlife.asked.get_mut(index) {
                    *slot = Some(text);
                }
            });
            Ok(())
        }
        // "Switch to accessory mode and re-enumerate."
        //
        // The device has been the accessory since it was put on the bus, so
        // there is nothing to write and nothing to call itself. The
        // re-enumeration is what the request is for, though, and a car that is
        // never shown the device go and come back has no way to tell a switch
        // that worked from one that failed. So the edge is given: off the bus and
        // back under the same numbers, which is what a phone does. The request is
        // ended first, because the host is sitting in the transfer and it is the
        // device that closes it — see `complete`.
        Some(desc::Accessory::Start) => {
            complete(ep0).await?;
            if !give_edge(edges) {
                exchange(&setup, format_args!("result=already_accessory"));
                return Ok(());
            }
            exchange(&setup, format_args!("result=already_accessory edge=replug"));
            tokio::time::sleep(SETTLE).await;
            if let Err(e) = usb.replug().await {
                tracing::error!("usb.replug_failed err={e:#}");
            }
            Ok(())
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
            Ok(())
        }
    }
}

/// Reports one control exchange, once.
///
/// The direction bit says which way the data stage goes, and that is the whole
/// of the name: data arriving is a receive, data going out is a send, the same
/// two words the CarLife lines use. It is the second of the two lines a request
/// gets — `received` is the first — so it carries only the code and what came
/// of it.
fn exchange(setup: &desc::Setup, outcome: std::fmt::Arguments<'_>) {
    let name = match setup.direction() {
        "in" => "usb.msg.send",
        _ => "usb.msg.recv",
    };
    tracing::info!("{name} code={} {outcome}", setup.request);
}

/// Reports one control request as it arrives.
///
/// A request and what came of it are two moments, not one: the transfer is
/// opened here and closed by `exchange`, and one that the exchange never closes
/// — a write that failed, a host that stalled — is a line with nothing after it
/// rather than a request that was never seen. The fields are the setup packet's,
/// which is the only place they are still whole. `code` is the packet's
/// `bRequest`: a number that names what the host is asking for, not a request
/// this program makes.
fn received(setup: &desc::Setup) {
    tracing::info!(
        "usb.msg.recv code={} type={:#04x} value={} index={} length={}",
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

/// Whether a car may be given another edge now, and records it if so.
///
/// One per `SWITCH_GAP`. The answer to `start accessory` never changes, so a car
/// that asks it again — on every attach, say — would be put off the bus and back
/// each time, and a device that keeps leaving is worse than one that never did.
fn give_edge(last: &Mutex<Option<Instant>>) -> bool {
    let mut slot = last.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if slot.is_some_and(|at| now.duration_since(at) < SWITCH_GAP) {
        return false;
    }
    *slot = Some(now);
    true
}

/// The numbers and strings of the device descriptor, as a host reads them.
///
/// Reported once, where the device is put on the bus: it has one form, and a
/// head unit reads it off the wire before this program has heard from it.
fn describe_identity(status: &Report, identity: &desc::Identity) {
    status.with(|s| {
        s.carlife.accessory = true;
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

    use crate::frame;

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
