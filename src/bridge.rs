//! Where CarPlay and Android Auto meet.
//!
//! Three things cross here, and nothing is decoded on the way:
//!
//! * **Negotiation.** What the head unit says it can show — resolution, frame
//!   rate, audio rate, which buttons it has — is fed back into `GET /info`, so
//!   the iPhone renders for the screen that is actually at the far end. It only
//!   lands if the head unit is heard from first: `/info` is read once.
//! * **Media.** Encoded frames go out on the channels the head unit opened,
//!   untouched. No transcoding, no resampling.
//! * **Input.** Keys, the rotary controller and the touchscreen arrive as
//!   `InputEvent`s and leave as CarPlay HID reports, which is the same path
//!   stdin commands take.
//!
//! The phone certificate is the only credential involved: the head unit is a
//! TLS client and we answer it, exactly as `aa_client::session` describes.

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;

use crate::aa_client::consts::{MEDIA_CODEC_VIDEO_H264_BP, MEDIA_CODEC_VIDEO_H265};
use crate::aa_client::control;
use crate::aa_client::input::{self, ButtonCode, Pointer, TouchAction};
use crate::aa_client::session::{self, PhoneEvent};
use crate::aa_client::tls::PhoneIdentity;
use crate::airplay::{EventSink, ScreenParams, SharedScreen};
use crate::commands::{Command, TouchContact, TOUCH_SLOTS};
use crate::config::Config;
use crate::sink::AaOutlet;

/// One finger we are tracking, and where it is.
#[derive(Debug, Clone, Copy)]
struct TouchSlot {
    id: u32,
    x: u16,
    y: u16,
}

pub struct Bridge {
    cfg: Arc<Config>,
    identity: Arc<PhoneIdentity>,
    outlet: Arc<AaOutlet>,
    screen: SharedScreen,
    events: Arc<EventSink>,
    /// Which pointer occupies each touch slot. The phone identifies a finger by
    /// the slot it keeps arriving in, not by an id, so an id has to hold the
    /// same slot for as long as it is down.
    touch: std::sync::Mutex<[Option<TouchSlot>; TOUCH_SLOTS]>,
}

impl Bridge {
    pub fn new(
        cfg: Arc<Config>,
        identity: Arc<PhoneIdentity>,
        outlet: Arc<AaOutlet>,
        screen: SharedScreen,
        events: Arc<EventSink>,
    ) -> Self {
        Self {
            cfg,
            identity,
            outlet,
            screen,
            events,
            touch: std::sync::Mutex::new([None; TOUCH_SLOTS]),
        }
    }

    /// Waits for head units and serves one at a time. Never returns unless the
    /// listener dies.
    pub async fn run(self: Arc<Self>) -> std::io::Result<()> {
        #[cfg(target_os = "linux")]
        if self.cfg.aa_usb {
            return crate::aa_client::usb::watch(self).await;
        }
        let listener = TcpListener::bind(("::", self.cfg.aa_tcp_port)).await?;
        println!(
            "[aa] waiting for a head unit on tcp/{}",
            listener.local_addr()?.port()
        );
        loop {
            let (socket, peer) = listener.accept().await?;
            socket.set_nodelay(true)?;
            let label = peer.to_string();
            println!("[aa] {label}: head unit connected");
            let bridge = self.clone();
            tokio::spawn(async move { bridge.serve(socket, label).await });
        }
    }

    /// One connection, from whichever transport produced it.
    pub async fn serve<RW>(self: Arc<Self>, io: RW, label: String)
    where
        RW: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        // One session at a time: a second head unit would have nothing to
        // receive on, since there is only one CarPlay picture to give it.
        let Some(mut media_rx) = self.outlet.take_receiver() else {
            eprintln!("[aa] {label}: a session is already running; refusing");
            return;
        };
        let codec = if self.cfg.hevc {
            MEDIA_CODEC_VIDEO_H265
        } else {
            MEDIA_CODEC_VIDEO_H264_BP
        };
        let link = session::serve(io, self.identity.clone(), label.clone(), codec);
        let media_tx = link.media;
        let mut events = link.events;

        loop {
            tokio::select! {
                frame = media_rx.recv() => match frame {
                    Some(frame) => {
                        if media_tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                    // Every sink is gone, so nothing will ever arrive again.
                    None => break,
                },
                event = events.recv() => match event {
                    Some(event) => self.handle(&label, event).await,
                    None => break,
                },
            }
        }
        // Handed back so a reconnection can use it.
        self.outlet.return_receiver(media_rx);
        println!("[aa] {label}: session over");
    }

    async fn handle(&self, label: &str, event: PhoneEvent) {
        match event {
            PhoneEvent::Discovered(d) => self.discovered(label, &d),
            PhoneEvent::VideoFocus(on) => {
                println!(
                    "[aa] {label}: video focus {}",
                    if on { "gained" } else { "lost" }
                );
                // The head unit is looking at us again and may have missed
                // whatever came before. A keyframe is the cheap way to be sure
                // what it shows is whole.
                if on {
                    self.send(Command::KeyFrame).await;
                }
            }
            PhoneEvent::Input(event) => {
                for cmd in self.translate(&event) {
                    self.send(cmd).await;
                }
            }
            PhoneEvent::Closed(reason) => println!("[aa] {label}: {reason}"),
        }
    }

    /// The head unit's answer to service discovery: what it has, and therefore
    /// what the iPhone should be asked for.
    fn discovered(&self, label: &str, d: &control::Discovered) {
        let video_ch = d.video_channel();
        let audio_ch = d.media_audio_channel();
        self.outlet.set_channels(video_ch, audio_ch);

        let mut width = self.cfg.width;
        let mut height = self.cfg.height;
        let mut fps = self.cfg.fps;
        if let Some(offer) = &d.video {
            // The first config it listed is its preference.
            if let Some((w, h, f)) = offer
                .configs
                .iter()
                .find_map(|c| c.pixels().map(|(w, h)| (w, h, c.fps)))
            {
                (width, height, fps) = (w, h, f);
            }
            println!(
                "[aa] {label}: video ch {:?} codec {} at {width}x{height}@{fps}",
                video_ch, offer.codec
            );
        }
        if let Some(ch) = audio_ch {
            let rate = d
                .audio
                .iter()
                .find(|a| a.ch == ch)
                .and_then(|a| a.configs.first())
                .map(|c| format!("{}Hz/{}ch", c.sample_rate, c.channel_count))
                .unwrap_or_else(|| "unspecified rate".into());
            println!("[aa] {label}: audio ch {ch} {rate}");
        }
        if let Some(input) = &d.input {
            println!(
                "[aa] {label}: input ch {} with {} buttons",
                input.ch,
                input.buttons.len()
            );
        }
        // Fed back into `GET /info`, which is the whole point: the phone renders
        // at a size the head unit asked for rather than one we guessed.
        if let Ok(mut guard) = self.screen.lock() {
            *guard = Some(ScreenParams {
                width,
                height,
                fps,
                // A head unit has no UI-size concept; the browser panel is the
                // only thing that asks for one.
                ui_scale: 100,
                // What the head unit says it can decode. Sending it HEVC it did
                // not list is how the picture disappears entirely.
                hevc: d
                    .video
                    .as_ref()
                    .map(|offer| offer.codec as u64 == MEDIA_CODEC_VIDEO_H265)
                    .unwrap_or(self.cfg.hevc),
            });
        }
        println!("[aa] {label}: asking the phone for {width}x{height}@{fps}");
    }

    /// Android Auto input in, CarPlay HID commands out.
    fn translate(&self, event: &input::InputEvent) -> Vec<Command> {
        match event {
            input::InputEvent::Key { keycode, down, .. } => {
                // A CarPlay button is a press and a release, which `EventSink`
                // does for us, so only the press is translated.
                if !down {
                    return Vec::new();
                }
                key_command(*keycode).into_iter().collect()
            }
            input::InputEvent::Rotary { delta, .. } => {
                // The wire carries direction and magnitude, no axis; CarPlay's
                // knob takes a signed velocity.
                vec![Command::Wheel((*delta).clamp(-127, 127) as i8)]
            }
            input::InputEvent::Touch {
                action,
                pointers,
                action_index: _,
            } => self.touch_command(*action, pointers).into_iter().collect(),
        }
    }

    /// Turns a touch into the report the phone expects, keeping each pointer in
    /// the slot it started in.
    fn touch_command(&self, action: u32, pointers: &[Pointer]) -> Option<Command> {
        let Ok(mut slots) = self.touch.lock() else {
            return None;
        };
        let (width, height) = match self.screen.lock().ok().and_then(|g| *g) {
            Some(s) => (s.width, s.height),
            None => (self.cfg.width, self.cfg.height),
        };
        // The head unit reports in its own screen space, which is the space
        // `/info` now asks the phone for, so the two agree — but clamp anyway
        // rather than send an out-of-range position.
        let clamp = |p: &Pointer| {
            (
                p.x.min(width.saturating_sub(1)) as u16,
                p.y.min(height.saturating_sub(1)) as u16,
            )
        };
        for p in pointers {
            let (x, y) = clamp(p);
            // The slot an id already holds, which every action below needs.
            let held = slots
                .iter()
                .position(|s| matches!(s, Some(s) if s.id == p.id));
            match action {
                TouchAction::DOWN | TouchAction::POINTER_DOWN => match held {
                    Some(slot) => slots[slot] = Some(TouchSlot { id: p.id, x, y }),
                    // Every slot taken: the extra finger is dropped rather than
                    // stealing one from a finger that is already down.
                    None => {
                        if let Some(free) = slots.iter().position(|s| s.is_none()) {
                            slots[free] = Some(TouchSlot { id: p.id, x, y });
                        }
                    }
                },
                TouchAction::UP | TouchAction::POINTER_UP => {
                    if let Some(slot) = held {
                        slots[slot] = None;
                    }
                }
                // A move only updates the position; the finger keeps its slot.
                _ => {
                    if let Some(slot) = held {
                        slots[slot] = Some(TouchSlot { id: p.id, x, y });
                    }
                }
            }
        }

        let mut contacts = [TouchContact::default(); TOUCH_SLOTS];
        for (contact, slot) in contacts.iter_mut().zip(slots.iter()) {
            if let Some(s) = slot {
                *contact = TouchContact {
                    down: true,
                    x: s.x,
                    y: s.y,
                };
            }
        }
        Some(Command::Touch(contacts))
    }

    async fn send(&self, cmd: Command) {
        if let Err(e) = self.events.dispatch(cmd).await {
            eprintln!("[aa] input dropped: {e}");
        }
    }
}

/// Android `KeyEvent` codes to CarPlay commands. Anything unrecognised is
/// dropped rather than guessed at: a wrong button is worse than no button.
fn key_command(keycode: u32) -> Option<Command> {
    Some(match keycode {
        ButtonCode::DPAD_UP => Command::Up,
        ButtonCode::DPAD_DOWN => Command::Down,
        ButtonCode::DPAD_LEFT => Command::Left,
        ButtonCode::DPAD_RIGHT => Command::Right,
        ButtonCode::DPAD_CENTER | ButtonCode::ENTER => Command::Select,
        ButtonCode::HOME => Command::Home,
        ButtonCode::BACK => Command::Back,
        ButtonCode::MEDIA_PLAY_PAUSE | ButtonCode::HEADSETHOOK => Command::PlayPause,
        ButtonCode::MEDIA_PLAY => Command::Play,
        ButtonCode::MEDIA_PAUSE | ButtonCode::MEDIA_STOP => Command::Pause,
        ButtonCode::MEDIA_NEXT | ButtonCode::NAVIGATE_NEXT => Command::Next,
        ButtonCode::MEDIA_PREV | ButtonCode::NAVIGATE_PREVIOUS => Command::Prev,
        ButtonCode::PHONE_ACCEPT => Command::Answer,
        ButtonCode::PHONE_DECLINE => Command::Hangup,
        ButtonCode::MUTE => Command::Mute,
        // Every one of these is a voice assistant request to CarPlay.
        ButtonCode::SEARCH | ButtonCode::ASSIST | ButtonCode::VOICE_ASSIST => Command::Siri,
        _ => return None,
    })
}
