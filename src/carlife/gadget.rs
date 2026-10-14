//! The car's half of the wire: the AOA requests that arrive on the accessory
//! interface, and the two bulk endpoints the CarLife session runs over.
//!
//! Nothing here builds that interface. A systemd socket unit with
//! `ListenUSBFunction=` opens the FunctionFS control endpoint, systemd writes the
//! descriptors into it, and the endpoint files are handed to this process as file
//! descriptors when it starts. README, "The accessory interface", has the board
//! side of it.
//!
//! Three arrive, under the names FunctionFS gives them: `ep0`, the control
//! endpoint, then the bulk pair. The descriptors ask for every control recipient
//! (`FUNCTIONFS_ALL_CTRL_RECIP`), which is what makes the kernel hand a request
//! addressed to the device to this program rather than turning it away — that is
//! a property of the blob the board writes, and `PLAN-CL.md` records the source
//! lines for it.
//!
//! The interface is up before this program is and stays up after it, which is the
//! point of the arrangement: the link the board is usually reached over belongs to
//! the same gadget, so anything that assembled the interface here would take that
//! link down with it on every start.
//!
//! Only the accessory form is built. The car's guide describes a second route —
//! enumerate as a phone, be asked to switch, and re-enumerate — but the car's own
//! reference implementation on a real head unit skips straight past it, so a
//! device that is already an accessory is the case to get working first.

use std::fs;
use std::io;
use std::os::unix::io::{FromRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{anyhow, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::mpsc;

use crate::config::Config;

use super::session::{self, Frames};

const ACCESSORY_GET_PROTOCOL: u8 = 51;
const ACCESSORY_SEND_STRING: u8 = 52;
const ACCESSORY_START: u8 = 53;

/// The car's reference implementation accepts either, and version 2 is what a
/// head unit of this age will have been written against.
const ACCESSORY_VERSION: u16 = 2;

const EVENT_ENABLE: u8 = 2;
const EVENT_DISABLE: u8 = 3;
const EVENT_SETUP: u8 = 4;

const USB_DIR_IN: u8 = 0x80;

/// What FunctionFS calls the endpoint files. For the addresses the board's blob
/// declares these are `ep1` and `ep2`, and the names are what the inherited
/// descriptors are identified by.
const CONTROL: &str = "ep0";
const FROM_CAR: &str = "ep1";
const TO_CAR: &str = "ep2";

/// The first descriptor systemd passes to a socket-activated service.
const FIRST_LISTEN_FD: RawFd = 3;

/// What the control endpoint tells the session loop.
enum Event {
    /// The car has enumerated us and the endpoints are usable.
    Enabled,
    /// It has gone.
    Disabled,
}

/// Runs one CarLife session after another over the endpoints it was handed.
///
/// Nothing is set up and nothing is taken down: the interface outlives this
/// program in both directions, so a session that ends leaves it ready for the
/// next car rather than needing to be rebuilt.
pub async fn run<F>(cfg: Arc<Config>, frames: &Frames, carplay: &mut F) -> Result<()>
where
    F: FnMut(bool),
{
    let endpoints = Endpoints::inherited()?;
    let ep0 = tokio::fs::File::from_std(endpoints.control);
    tracing::info!("[cl-usb] the accessory interface is up; waiting for the car to enumerate us");

    let (events_tx, mut events_rx) = mpsc::channel(4);
    let control = tokio::spawn(control_loop(ep0, events_tx));

    while let Some(event) = events_rx.recv().await {
        match event {
            Event::Enabled => match bulk(&endpoints.from_car, &endpoints.to_car) {
                Ok(pair) => {
                    tracing::info!("[cl-usb] the car has enumerated us; starting the session");
                    if let Err(e) = session::run(pair, frames, &cfg, carplay).await {
                        tracing::warn!("[cl] the session ended badly: {e}");
                    }
                }
                Err(e) => tracing::error!("[cl-usb] the endpoints are not usable: {e}"),
            },
            Event::Disabled => tracing::info!("[cl-usb] the car has gone"),
        }
    }

    control.abort();
    tracing::info!("[cl-usb] the control endpoint is gone");
    Ok(())
}

/// Reads the control endpoint for the life of the process.
///
/// It owns the endpoint because it is also what answers on it: a setup event and
/// the reply to it are two halves of the same exchange, and nothing else may
/// interleave with them.
async fn control_loop(mut ep0: tokio::fs::File, events: mpsc::Sender<Event>) -> io::Result<()> {
    loop {
        // The whole event is twelve bytes: the eight of a `usb_ctrlrequest`, the
        // type, and three of padding. Read as bytes and unpacked by hand, so
        // there is no packed struct to get wrong.
        let mut raw = [0u8; 12];
        if let Err(e) = ep0.read_exact(&mut raw).await {
            tracing::debug!("[cl-usb] the control endpoint stopped reading: {e}");
            return Ok(());
        }
        match raw[8] {
            EVENT_ENABLE => {
                let _ = events.send(Event::Enabled).await;
            }
            EVENT_DISABLE => {
                let _ = events.send(Event::Disabled).await;
            }
            EVENT_SETUP => {
                if let Err(e) = answer(&mut ep0, &raw).await {
                    tracing::warn!("[cl-usb] a control request could not be answered: {e}");
                }
            }
            other => tracing::debug!("[cl-usb] control event {other}"),
        }
    }
}

/// Answers one setup event, when there is an answer to give.
///
/// Every request is reported whether or not it is understood: which ones a given
/// head unit sends is not known yet, and a line per request is the cheapest way
/// to find out. The ones that are understood are the three that ask a device to
/// become an accessory.
async fn answer(ep0: &mut tokio::fs::File, raw: &[u8; 12]) -> io::Result<()> {
    let request_type = raw[0];
    let request = raw[1];
    let value = u16::from_le_bytes([raw[2], raw[3]]);
    let index = u16::from_le_bytes([raw[4], raw[5]]);
    let length = u16::from_le_bytes([raw[6], raw[7]]);
    let direction = if request_type & USB_DIR_IN != 0 {
        "in"
    } else {
        "out"
    };
    tracing::info!(
        "[cl-usb] control request {request} {direction}, type {request_type:#04x}, value {value}, \
         index {index}, length {length}"
    );

    match (request_type, request) {
        // "What Android Open Accessory version do you speak?"
        (0xc0, ACCESSORY_GET_PROTOCOL) => {
            let reply = ACCESSORY_VERSION.to_le_bytes();
            ep0.write_all(&reply).await?;
            tracing::info!("[cl-usb] answered with AOA {ACCESSORY_VERSION}");
        }
        // One of the six strings the host wants the device to report.
        (0x40, ACCESSORY_SEND_STRING) => {
            let mut text = vec![0u8; length as usize];
            ep0.read_exact(&mut text).await?;
            tracing::info!(
                "[cl-usb] identifier {index}: {:?}",
                String::from_utf8_lossy(&text)
            );
        }
        // "Switch to accessory mode and re-enumerate." There is nothing to do:
        // this device only ever enumerates in that form, which is why the car's
        // host side is expected to skip the whole handshake.
        (0x40, ACCESSORY_START) => tracing::info!(
            "[cl-usb] the host asked us to switch to accessory mode; we are already enumerated \
             as one, so there is nothing to re-enumerate"
        ),
        _ => tracing::info!("[cl-usb] that one is not acted on"),
    }
    Ok(())
}

/// The three endpoints the socket unit opened, as they were handed over.
struct Endpoints {
    control: fs::File,
    from_car: fs::File,
    to_car: fs::File,
}

impl Endpoints {
    /// Takes the descriptors systemd passed to this process.
    ///
    /// Which descriptor is which is asked of `/proc/self/fd` rather than taken
    /// from the order they arrive in: the names FunctionFS gave the files are the
    /// part that is ours to rely on.
    fn inherited() -> Result<Self> {
        let (mut control, mut from_car, mut to_car) = (None, None, None);

        for fd in listen_fds()? {
            // Safe: these are this process's own descriptors, passed by systemd
            // and owned by nothing else. Anything unrecognised closes when its
            // `File` goes out of scope at the end of this iteration.
            let file = unsafe { fs::File::from_raw_fd(fd) };
            match name_of(fd).as_deref() {
                Some(CONTROL) => control = Some(file),
                Some(FROM_CAR) => from_car = Some(file),
                Some(TO_CAR) => to_car = Some(file),
                _ => {}
            }
        }

        let (Some(control), Some(from_car), Some(to_car)) = (control, from_car, to_car) else {
            return Err(anyhow!(
                "the interface handed over is not {CONTROL}, {FROM_CAR} and {TO_CAR}"
            ));
        };
        Ok(Self {
            control,
            from_car,
            to_car,
        })
    }
}

/// The descriptors systemd passes to a socket-activated service.
///
/// `$LISTEN_PID` has to name this process: the variables outlive the service and
/// are inherited by whatever it goes on to start, so a program that inherited
/// them rather than being handed them would find only its parent's. `$LISTEN_FDS`
/// counts them, and they start at 3.
fn listen_fds() -> Result<Vec<RawFd>> {
    let pid = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    let count = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);

    if pid != Some(std::process::id()) || count == 0 {
        return Err(anyhow!(
            "nothing was handed over: this program is meant to be started by the socket unit \
             that opens the endpoints, not by hand. See README, \"The accessory interface\""
        ));
    }
    Ok((0..count as RawFd).map(|n| FIRST_LISTEN_FD + n).collect())
}

/// What an inherited descriptor points at, by the name FunctionFS gave the file.
fn name_of(fd: RawFd) -> Option<String> {
    let path = fs::read_link(format!("/proc/self/fd/{fd}")).ok()?;
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
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
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().from_car).poll_read(cx, buf)
    }
}

impl AsyncWrite for Bulk {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().to_car).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().to_car).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().to_car).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    #[test]
    fn an_inherited_descriptor_is_named_by_the_file_it_points_at() {
        // This is what makes the order the descriptors arrive in irrelevant.
        let dir = std::env::temp_dir().join("cp2cl-name-of");
        fs::create_dir_all(&dir).expect("a temporary directory can be made");
        let path = dir.join(FROM_CAR);
        fs::write(&path, b"").expect("a named file can be written");

        let file = fs::File::open(&path).expect("a named file can be opened");
        let named = name_of(file.as_raw_fd());
        assert_eq!(named.as_deref(), Some(FROM_CAR));
        drop(file);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }
}
