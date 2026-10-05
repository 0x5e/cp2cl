//! The event channel: how input reaches the phone.
//!
//! Despite the name this is not HID-over-UDP. The phone opens a TCP connection
//! to the event port we advertised, and we push length-framed records down it:
//!
//! ```text
//! [2 byte LE ciphertext length][ciphertext][16 byte tag]
//! ```
//!
//! The two-byte length doubles as the additional authenticated data, and the
//! nonce is a counter, incremented once per message. Inside each record is a
//! normal-looking RTSP request whose body is a binary plist.
//!
//! Its keys come from the same shared secret as everything else, but unlike the
//! control channel they are *not* read/write swapped (see `VerifyKeys`).
//!
//! The channel runs both ways. `EventSink` is the half the rest of the program
//! talks to — stdin and the Android Auto bridge both hand it commands — and
//! `pump_events` is the half that reads what the phone posts back and answers
//! it.

use std::sync::Arc;

use plist::{dictionary::Dictionary, Value};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;

use super::bplist::to_binary;
use super::crypto::{open_frames, seal_frames, SymKey};
use super::info::{KNOB_HID_UID, MAIN_UUID, MEDIA_HID_UID, TELEPHONY_HID_UID, TOUCH_HID_UID};
use super::rtsp;
use crate::commands::{self, Command, TouchContact};

/// How far the absolute X/Y axes deflect: a full throw in one direction. The
/// phone reads an absolute axis as a position, so a full throw scrolls many
/// rows at once.
const AXIS_DEFLECT: i8 = 127;

/// Input only becomes possible after pair-verify derives the keys, so the sink
/// exists the whole time and fills itself in as pieces arrive.
pub struct EventSink {
    key: Mutex<Option<SymKey>>,
    counter: Mutex<u64>,
    writer: Mutex<Option<OwnedWriteHalf>>,
}

impl EventSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            key: Mutex::new(None),
            counter: Mutex::new(0),
            writer: Mutex::new(None),
        })
    }

    /// Called once pair-verify has derived the keys. Until then every press is
    /// dropped, with a line saying so.
    pub(crate) async fn set_key(&self, raw: [u8; 32]) {
        *self.key.lock().await = SymKey::from_bytes(&raw).ok();
    }

    /// Hands over the write half of the connection the phone opened. Until that
    /// arrives there is nowhere to put a report.
    pub(crate) async fn attach_writer(&self, writer: OwnedWriteHalf) {
        // A new connection means the phone starts counting from zero again, so
        // ours has to as well or it cannot open what we send. The wired phone
        // connects once and never notices; the wireless one reconnects after
        // the switch to Wi-Fi, and every report after that would be dropped.
        let was = std::mem::replace(&mut *self.counter.lock().await, 0);
        if was != 0 {
            println!("[event] new event channel; write counter reset from {was}");
        }
        *self.writer.lock().await = Some(writer);
    }

    pub async fn dispatch(self: &Arc<Self>, cmd: Command) -> std::io::Result<()> {
        let key = self.key.lock().await.clone();
        let Some(key) = key else {
            eprintln!("[event] not paired yet — press ignored");
            return Ok(());
        };
        let mut guard = self.writer.lock().await;
        let Some(writer) = guard.as_mut() else {
            eprintln!("[event] phone has not opened the event channel yet");
            return Ok(());
        };
        let mut counter = self.counter.lock().await;
        let mut ch = EventChannel::new(key, &mut *writer);
        ch.set_counter(*counter);
        let outcome = ch.key(cmd).await;
        *counter = ch.counter();
        outcome
    }

    /// Answers one request the phone posted us on the event channel.
    ///
    /// The channel runs both ways: alongside our HID reports the phone sends its
    /// own requests, and each one needs a 200. Ignoring them is not neutral —
    /// the phone concludes the accessory stopped listening and stops sending
    /// video a couple of seconds later.
    async fn reply(&self, cseq: u32) -> std::io::Result<()> {
        let Some(key) = self.key.lock().await.clone() else {
            return Ok(());
        };
        let mut guard = self.writer.lock().await;
        let Some(writer) = guard.as_mut() else {
            return Ok(());
        };
        let mut counter = self.counter.lock().await;
        let out = seal_frames(&key, &mut counter, &rtsp::Response::new(200).encode(cseq))?;
        writer.write_all(&out).await
    }
}

/// Reads the phone's side of the event connection and answers every request.
pub(crate) async fn pump_events(
    mut read: OwnedReadHalf,
    read_key: SymKey,
    sink: Arc<EventSink>,
) {
    let mut buf = [0u8; 4096];
    let mut enc: Vec<u8> = Vec::new();
    let mut plain: Vec<u8> = Vec::new();
    let mut ctr: u64 = 0;

    loop {
        let n = match read.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        enc.extend_from_slice(&buf[..n]);

        // Same framing as the control channel: [2B length][ciphertext][tag].
        if let Err(e) = open_frames(&read_key, &mut ctr, &mut enc, &mut plain) {
            eprintln!("[event] frame failed to open: {e}");
            return;
        }

        while let Ok(Some((req, used))) = rtsp::parse(&plain) {
            plain.drain(..used);
            // The phone answers our commands on this same connection. Those
            // answers are chatter, and replying to one would make it answer
            // back — a loop that drowns the log in identical lines.
            if req.method.starts_with("RTSP/") || req.method.starts_with("HTTP/") {
                if req.path != "200" {
                    eprintln!("[event] response {} {}", req.path, req.version);
                }
                continue;
            }
            println!(
                "[event] <- {} {} ({}B)",
                req.method,
                req.path,
                req.body.len()
            );
            if let Err(e) = sink.reply(req.cseq()).await {
                eprintln!("[event] reply failed: {e}");
                return;
            }
        }
    }
    println!("[airplay] event channel closed");
}

pub struct EventChannel<W> {
    write: SymKey,
    counter: u64,
    writer: W,
}

impl<W: AsyncWrite + Unpin> EventChannel<W> {
    pub fn new(write: SymKey, writer: W) -> Self {
        Self {
            write,
            counter: 0,
            writer,
        }
    }

    pub fn set_counter(&mut self, counter: u64) {
        self.counter = counter;
    }

    pub fn counter(&self) -> u64 {
        self.counter
    }

    async fn send(&mut self, dict: Dictionary) -> std::io::Result<()> {
        let body = to_binary(&Value::Dictionary(dict))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let head = format!(
            "POST /command RTSP/1.0\r\n\
             Content-Type: application/x-apple-binary-plist\r\n\
             Content-Length: {}\r\n\
             CSeq: {}\r\n\r\n",
            body.len(),
            self.counter
        );
        let mut plain = head.into_bytes();
        plain.extend_from_slice(&body);

        // Same framing as the control channel: the two-byte length is the AAD
        // and counts the ciphertext only, with the tag following it.
        let frame = seal_frames(&self.write, &mut self.counter, &plain)?;
        self.writer.write_all(&frame).await
    }

    /// Sends a HID report to one of the four declared HID devices.
    async fn hid_report(&mut self, device: u32, report: Vec<u8>) -> std::io::Result<()> {
        let mut d = Dictionary::new();
        d.insert("type".into(), Value::String("hidSendReport".into()));
        d.insert("uuid".into(), Value::String(format!("{device:x}")));
        d.insert("hidReport".into(), Value::Data(report));
        self.send(d).await
    }

    /// Runs one parsed key press, including the release the phone expects.
    pub async fn key(&mut self, cmd: Command) -> std::io::Result<()> {
        use commands::*;

        // A command that works otherwise leaves no trace: the phone's `200` is
        // filtered out by `pump_events` as chatter, so only failures reach the
        // log. One line here is the difference between "nothing happened" and
        // "it worked".
        println!("[event] -> {cmd:?}");

        match cmd {
            Command::Play => self.media_momentary(MEDIA_PLAY).await?,
            Command::Pause => self.media_momentary(MEDIA_PAUSE).await?,
            Command::PlayPause => self.media_momentary(MEDIA_PLAY_PAUSE).await?,
            Command::Next => self.media_momentary(MEDIA_NEXT).await?,
            Command::Prev => self.media_momentary(MEDIA_PREV).await?,
            // Every button is momentary: pressed report, then an all-zero one.
            // LIVI's `sendKnob` does this by default, and a button that is never
            // released does not register as a press at all — `home` doing
            // nothing was exactly that.
            Command::Home => self.knob_momentary(0x02).await?,
            Command::Back => self.knob_momentary(0x04).await?,
            Command::Select => self.knob_momentary(0x01).await?,
            // A knob axis nudge is a press and a release, or the phone thinks the
            // finger never lifted.
            Command::Left => self.knob_axis(-AXIS_DEFLECT, 0).await?,
            Command::Right => self.knob_axis(AXIS_DEFLECT, 0).await?,
            // Up and down stay on the absolute Y axis. The wheel is *not* a
            // vertical axis: it follows the UI's own scroll direction, which is
            // horizontal on the app grid — routed through it, up/down came out
            // as left/right there.
            Command::Up => self.knob_axis(0, -AXIS_DEFLECT).await?,
            Command::Down => self.knob_axis(0, AXIS_DEFLECT).await?,
            Command::Wheel(v) => self.wheel(v).await?,
            Command::Touch(contacts) => {
                self.hid_report(TOUCH_HID_UID, touch_report(&contacts))
                    .await?
            }
            // The four below come from the AA bridge, off the head unit's own
            // call and media keys — none of them has a stdin command.
            Command::Answer => self.telephony_momentary(PHONE_HOOK_SWITCH).await?,
            Command::Hangup => self.telephony_momentary(PHONE_DROP).await?,
            Command::Mute => self.telephony_momentary(PHONE_MUTE).await?,
            Command::KeyFrame => {
                self.command("forceKeyFrame", "uuid", Value::String(MAIN_UUID.into()))
                    .await?
            }
            Command::Siri => {
                // press then release; 2 = down, 3 = up
                self.command("requestSiri", "siriAction", Value::Integer(2.into()))
                    .await?;
                self.command("requestSiri", "siriAction", Value::Integer(3.into()))
                    .await?;
            }
            Command::Quit => {}
        }
        Ok(())
    }

    /// A media key: the index, then the all-zero report that releases it.
    ///
    /// The descriptor is an array-typed consumer control whose index 0 is
    /// "unassigned", so a key that is never released stays down and the phone
    /// reads the next report carrying the same value as no press at all. LIVI's
    /// `sendMedia` sends both reports for the same reason.
    async fn media_momentary(&mut self, index: u8) -> std::io::Result<()> {
        self.hid_report(MEDIA_HID_UID, vec![index]).await?;
        self.hid_report(MEDIA_HID_UID, vec![0]).await
    }

    async fn knob(&mut self, buttons: u8, x: i8, y: i8, wheel: i8) -> std::io::Result<()> {
        self.hid_report(KNOB_HID_UID, vec![buttons, x as u8, y as u8, wheel as u8])
            .await
    }

    /// Press and release. The matching LIVI call sends the second, all-zero
    /// report for every button press unless told not to.
    async fn knob_momentary(&mut self, button: u8) -> std::io::Result<()> {
        self.knob(button, 0, 0, 0).await?;
        self.knob(0x00, 0, 0, 0).await
    }

    async fn knob_axis(&mut self, x: i8, y: i8) -> std::io::Result<()> {
        self.knob(0x00, x, y, 0).await?;
        self.knob(0x00, 0, 0, 0).await
    }

    /// One wheel tick, press and release. A relative axis: the phone reads the
    /// sign and ignores the magnitude, and it follows the UI's own scroll
    /// direction — horizontal on the app grid. Only the AA bridge produces
    /// these; there is no stdin command.
    async fn wheel(&mut self, ticks: i8) -> std::io::Result<()> {
        self.knob(0x00, 0, 0, ticks).await?;
        self.knob(0x00, 0, 0, 0).await
    }

    /// A telephony button: pressed report, then an all-zero one, the same
    /// press-and-release every other momentary switch gets.
    async fn telephony_momentary(&mut self, index: u8) -> std::io::Result<()> {
        self.hid_report(TELEPHONY_HID_UID, vec![index]).await?;
        self.hid_report(TELEPHONY_HID_UID, vec![0]).await
    }

    /// A session command.
    ///
    /// Unlike `hidSendReport`, whose fields sit at the top level, the phone
    /// wants a command's arguments one level down in a `params` dictionary.
    /// Sent flat it answers `400 Bad Request` and the press does nothing.
    async fn command(&mut self, kind: &str, field: &str, value: Value) -> std::io::Result<()> {
        let mut params = Dictionary::new();
        params.insert(field.into(), value);
        let mut d = Dictionary::new();
        d.insert("type".into(), Value::String(kind.into()));
        d.insert("params".into(), Value::Dictionary(params));
        self.send(d).await
    }
}

/// One multitouch input report: a six-byte slot per finger the descriptor
/// declares — `[index][touch][X LE][Y LE]` — including slots no finger is on,
/// because the phone identifies a finger by the slot it keeps arriving in.
/// Mirrors LIVI's `touchReport`, and has to agree with
/// `info::multitouch_descriptor`.
fn touch_report(contacts: &[TouchContact]) -> Vec<u8> {
    let mut out = Vec::with_capacity(contacts.len() * 6);
    for (slot, c) in contacts.iter().enumerate() {
        out.push(slot as u8);
        out.push(u8::from(c.down));
        out.extend_from_slice(&c.x.to_le_bytes());
        out.extend_from_slice(&c.y.to_le_bytes());
    }
    out
}
