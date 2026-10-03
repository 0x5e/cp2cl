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

use plist::{dictionary::Dictionary, Value};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::bplist::to_binary;
use super::crypto::{SymKey, nonce_counter, seal};
use crate::airplay::MAX_PAYLOAD;
use crate::commands::{self, Command};

pub struct EventChannel<W> {
    write: SymKey,
    counter: u64,
    writer: W,
}

impl<W: AsyncWrite + Unpin> EventChannel<W> {
    pub fn new(write: SymKey, writer: W) -> Self {
        Self { write, counter: 0, writer }
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
        let mut frame = Vec::new();
        for chunk in plain.chunks(MAX_PAYLOAD) {
            let header = (chunk.len() as u16).to_le_bytes();
            let sealed = seal(&self.write, &nonce_counter(self.counter), chunk, &header)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            self.counter += 1;
            frame.extend_from_slice(&header);
            frame.extend_from_slice(&sealed);
        }
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
        use super::info::{MEDIA_HID_UID, TELEPHONY_HID_UID};
        use commands::*;

        match cmd {
            Command::Play => self.hid_report(MEDIA_HID_UID, vec![MEDIA_PLAY]).await?,
            Command::Pause => self.hid_report(MEDIA_HID_UID, vec![MEDIA_PAUSE]).await?,
            Command::PlayPause => self.hid_report(MEDIA_HID_UID, vec![MEDIA_PLAY_PAUSE]).await?,
            Command::Next => self.hid_report(MEDIA_HID_UID, vec![MEDIA_NEXT]).await?,
            Command::Prev => self.hid_report(MEDIA_HID_UID, vec![MEDIA_PREV]).await?,
            Command::NavGuidance => {
                self.hid_report(MEDIA_HID_UID, vec![MEDIA_NAV_GUIDANCE]).await?
            }
            // Every button is momentary: pressed report, then an all-zero one.
            // LIVI's `sendKnob` does this by default, and a button that is never
            // released does not register as a press at all — `home` doing
            // nothing was exactly that.
            Command::Home => self.knob_momentary(0x02).await?,
            Command::Back => self.knob_momentary(0x04).await?,
            Command::Select => self.knob_momentary(0x01).await?,
            // A knob axis nudge is a press and a release, or the phone thinks the
            // finger never lifted.
            Command::Left => self.knob_axis(-127, 0).await?,
            Command::Right => self.knob_axis(127, 0).await?,
            Command::Up => self.knob_axis(0, -127).await?,
            Command::Down => self.knob_axis(0, 127).await?,
            Command::Wheel(v) => {
                self.knob(0x00, 0, 0, v).await?;
                self.knob(0x00, 0, 0, 0).await?;
            }
            Command::Answer => {
                self.hid_report(TELEPHONY_HID_UID, vec![PHONE_HOOK_SWITCH]).await?;
                self.hid_report(TELEPHONY_HID_UID, vec![0]).await?;
            }
            Command::Hangup => {
                self.hid_report(TELEPHONY_HID_UID, vec![PHONE_DROP]).await?;
                self.hid_report(TELEPHONY_HID_UID, vec![0]).await?;
            }
            Command::Mute => {
                self.hid_report(TELEPHONY_HID_UID, vec![PHONE_MUTE]).await?;
                self.hid_report(TELEPHONY_HID_UID, vec![0]).await?;
            }
            Command::Key(c) if c.is_ascii_digit() => {
                let index = PHONE_KEY0 + (c as u8 - b'0');
                self.hid_report(TELEPHONY_HID_UID, vec![index]).await?;
                self.hid_report(TELEPHONY_HID_UID, vec![0]).await?;
            }
            Command::Star => {
                self.hid_report(TELEPHONY_HID_UID, vec![15]).await?;
                self.hid_report(TELEPHONY_HID_UID, vec![0]).await?;
            }
            Command::Pound => {
                self.hid_report(TELEPHONY_HID_UID, vec![16]).await?;
                self.hid_report(TELEPHONY_HID_UID, vec![0]).await?;
            }
            Command::Del => {
                self.hid_report(TELEPHONY_HID_UID, vec![17]).await?;
                self.hid_report(TELEPHONY_HID_UID, vec![0]).await?;
            }
            Command::Key(_) => {}
            Command::Night(on) => self.simple("setNightMode", Value::Boolean(on)).await?,
            Command::Siri => {
                // press then release; 2 = down, 3 = up
                self.simple_with("requestSiri", "siriAction", Value::Integer(2.into())).await?;
                self.simple_with("requestSiri", "siriAction", Value::Integer(3.into())).await?;
            }
            Command::KeyFrame => self.simple("forceKeyFrame", Value::Boolean(true)).await?,
            Command::Quit => {}
        }
        Ok(())
    }

    async fn knob(&mut self, buttons: u8, x: i8, y: i8, wheel: i8) -> std::io::Result<()> {
        use super::info::KNOB_HID_UID;
        self.hid_report(KNOB_HID_UID, vec![buttons, x as u8, y as u8, wheel as u8]).await
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

    async fn simple(&mut self, kind: &str, value: Value) -> std::io::Result<()> {
        let mut d = Dictionary::new();
        d.insert("type".into(), Value::String(kind.into()));
        d.insert(Self::value_field(kind), value);
        self.send(d).await
    }

    async fn simple_with(&mut self, kind: &str, field: &str, value: Value) -> std::io::Result<()> {
        let mut d = Dictionary::new();
        d.insert("type".into(), Value::String(kind.into()));
        d.insert(field.into(), value);
        self.send(d).await
    }

    fn value_field(kind: &str) -> String {
        match kind {
            "setNightMode" => "nightMode".into(),
            "forceKeyFrame" => "forced".into(),
            other => other.to_ascii_lowercase(),
        }
    }
}
