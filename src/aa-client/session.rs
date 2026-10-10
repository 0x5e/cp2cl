// One head unit connection, driven from the phone's side.
//
// The conversation, in order: the head unit asks for a version, we answer; it
// starts TLS, we accept; it asks what we are, we say, and it answers with the
// channels it has; we open the ones we need and wait for focus; from then on we
// send media and it sends input. Every step is the head unit's move first.
//
// Media is never touched here — callers hand us encoded frames and we hand back
// decoded input, which is what makes this usable as a bridge.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use super::consts::*;
use super::control;
use super::frame::{self, FrameParser, FrameSplitter, RawFrame};
use super::input::{self, InputEvent};
use super::tls::{Message, PhoneIdentity, TlsEngine};

const READ_CHUNK: usize = 65536;
/// How long we wait for the head unit to finish setting up before giving up.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
const MEDIA_BACKLOG: usize = 256;
const EVENT_BACKLOG: usize = 64;

/// One encoded frame on its way to the head unit. No codec knowledge here: the
/// bytes are whatever the AV channel expects, H.264 or HEVC in byte-stream form
/// for video, raw PCM for audio.
#[derive(Debug)]
pub struct MediaFrame {
    pub ch: u8,
    /// Microseconds, which is what an AV channel timestamp carries. None means
    /// "no timestamp", the message id without one.
    pub ts_us: Option<u64>,
    pub data: Vec<u8>,
}

/// What the head unit said back, for whatever is really producing the picture.
#[derive(Debug)]
pub enum PhoneEvent {
    /// Its answer to our service discovery: the channels it offers.
    Discovered(control::Discovered),
    /// It started or stopped showing our video.
    VideoFocus(bool),
    /// A key, a wheel turn, a touch.
    Input(InputEvent),
    /// The session is over, and why.
    Closed(String),
}

/// The two ends a caller talks to.
pub struct PhoneLink {
    /// Frames to send. Dropping it ends the media half; the session lives on.
    pub media: mpsc::Sender<MediaFrame>,
    /// What comes back.
    pub events: mpsc::Receiver<PhoneEvent>,
}

/// Starts serving one head unit connection in the background.
///
/// `video_codec` is the `MediaCodecType` we declare for the video channel, and
/// it has to be what the frames actually are: `MEDIA_CODEC_VIDEO_H265` if the
/// source is HEVC, `MEDIA_CODEC_VIDEO_H264_BP` otherwise. The head unit decodes
/// by this number, so a mismatch is a picture that never assembles.
pub fn serve<RW>(io: RW, identity: Arc<PhoneIdentity>, label: String, video_codec: u64) -> PhoneLink
where
    RW: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (media_tx, media_rx) = mpsc::channel(MEDIA_BACKLOG);
    let (event_tx, event_rx) = mpsc::channel(EVENT_BACKLOG);
    tokio::spawn(async move {
        let mut s = Session {
            label,
            identity,
            media: media_rx,
            events: event_tx,
            phase: Phase::Version,
            parser: FrameParser::default(),
            splitter: FrameSplitter::default(),
            tls: None,
            video: None,
            audio: None,
            input_ch: None,
            input_buttons: Vec::new(),
            video_codec,
            media_live: true,
            complained: false,
        };
        if let Err(e) = s.run(io).await {
            s.closed(&e).await;
        }
    });
    PhoneLink {
        media: media_tx,
        events: event_rx,
    }
}

#[derive(PartialEq, Eq)]
enum Phase {
    Version,
    Handshake,
    /// TLS is up, channels are being opened.
    Discovery,
    Running,
}

/// One AV channel's progress: opened, then configured, then started.
#[derive(Debug, Default)]
struct AvState {
    ch: u8,
    opened: bool,
    set_up: bool,
    started: bool,
    /// The flow control window the head unit named when it took the channel:
    /// how many frames may be outstanding before we owe it a pause.
    max_unacked: u32,
}

struct Session {
    label: String,
    identity: Arc<PhoneIdentity>,
    media: mpsc::Receiver<MediaFrame>,
    events: mpsc::Sender<PhoneEvent>,
    phase: Phase,
    parser: FrameParser,
    splitter: FrameSplitter,
    tls: Option<TlsEngine>,
    video: Option<AvState>,
    audio: Option<AvState>,
    input_ch: Option<u8>,
    /// The buttons the head unit has, echoed back in the input handshake.
    input_buttons: Vec<u32>,
    /// What we declare to the head unit for the video channel, and so what it
    /// decodes by. It has to match the frames handed to us, see `MediaFrame`.
    video_codec: u64,
    media_live: bool,
    /// Media arriving before its channel is ready is reported once, not per frame.
    complained: bool,
}

impl Session {
    async fn run<RW>(&mut self, io: RW) -> Result<(), String>
    where
        RW: AsyncRead + AsyncWrite + Unpin,
    {
        let (mut rd, mut wr) = tokio::io::split(io);
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            let setting_up = self.phase != Phase::Running;
            tokio::select! {
                r = rd.read(&mut buf) => match r {
                    Ok(0) => return Err("head unit closed the connection".into()),
                    Ok(n) => self.on_bytes(&buf[..n], &mut wr).await?,
                    Err(e) => return Err(format!("read: {e}")),
                },
                frame = self.media.recv(), if self.media_live => match frame {
                    Some(frame) => self.send_media(frame, &mut wr).await?,
                    None => self.media_live = false,
                },
                _ = tokio::time::sleep(SETUP_TIMEOUT), if setting_up => {
                    return Err("the head unit never finished setting up".into());
                }
            }
        }
    }

    async fn on_bytes<W: AsyncWrite + Unpin>(
        &mut self,
        bytes: &[u8],
        wr: &mut W,
    ) -> Result<(), String> {
        // Before TLS a message can be split across frames, so the parser
        // reassembles. After it, one frame is one TLS record and the session
        // does the reassembling instead.
        match self.phase {
            Phase::Discovery | Phase::Running => self.splitter.push(bytes),
            _ => self.parser.push(bytes),
        }
        loop {
            match self.phase {
                Phase::Discovery | Phase::Running => {
                    let Some(f) = self.splitter.next_frame() else {
                        break;
                    };
                    self.on_frame(f, wr).await?;
                }
                _ => {
                    let Some(f) = self.parser.next_frame() else {
                        break;
                    };
                    self.on_setup_frame(f, wr).await?;
                }
            }
        }
        Ok(())
    }

    /// Version and TLS: plaintext frames on channel 0.
    async fn on_setup_frame<W: AsyncWrite + Unpin>(
        &mut self,
        f: RawFrame,
        wr: &mut W,
    ) -> Result<(), String> {
        if f.flags & FLAG_ENCRYPTED != 0 {
            // An encrypted frame riding along with the handshake.
            return self.on_encrypted(f, wr).await;
        }
        let Some((msg_id, body)) = f.message() else {
            return Ok(());
        };
        match msg_id {
            CTRL_VERSION_REQUEST => {
                if body.len() < 2 {
                    return Err("version request too short".into());
                }
                let major = u16::from_be_bytes([body[0], body[1]]);
                if major != VERSION_MAJOR {
                    return Err(format!("head unit asked for version {major}"));
                }
                let mut data = VERSION_MAJOR.to_be_bytes().to_vec();
                data.extend_from_slice(&PHONE_VERSION_MINOR.to_be_bytes());
                data.extend_from_slice(&0u16.to_be_bytes()); // status: match
                self.write(
                    wr,
                    &frame::encode(CH_CONTROL, FLAGS_PLAINTEXT, CTRL_VERSION_RESPONSE, &data),
                )
                .await
            }
            CTRL_SSL_HANDSHAKE => {
                let body = body.to_vec();
                if self.tls.is_none() {
                    self.tls = Some(TlsEngine::new(&self.identity).map_err(|e| e.to_string())?);
                    self.phase = Phase::Handshake;
                }
                let tls = self.tls.as_mut().expect("just created");
                tls.inject_handshake(&body).map_err(|e| e.to_string())?;
                let out = tls.take_output();
                let done = !tls.is_handshaking();
                if !out.is_empty() {
                    self.write(
                        wr,
                        &frame::encode(CH_CONTROL, FLAGS_PLAINTEXT, CTRL_SSL_HANDSHAKE, &out),
                    )
                    .await?;
                }
                if done {
                    self.become_established(wr).await?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// TLS is up: switch to the raw splitter and introduce ourselves.
    ///
    /// The usual sequence has the head unit send AuthComplete first, but head
    /// units that skip it exist (LIVI's does), and speaking first breaks
    /// nothing — so we always do, and treat AuthComplete as a no-op.
    async fn become_established<W: AsyncWrite + Unpin>(
        &mut self,
        wr: &mut W,
    ) -> Result<(), String> {
        self.splitter = std::mem::take(&mut self.parser).into_splitter();
        self.phase = Phase::Discovery;
        tracing::info!("[aa] {}: tls up", self.label);
        let body = control::service_discovery_request("cp2cl", "cp2cl");
        self.send_control(wr, CTRL_SERVICE_DISCOVERY_REQUEST, &body)
            .await
    }

    async fn on_frame<W: AsyncWrite + Unpin>(
        &mut self,
        f: RawFrame,
        wr: &mut W,
    ) -> Result<(), String> {
        if f.flags & FLAG_ENCRYPTED == 0 {
            // Plaintext after the handshake means the head unit restarted the
            // whole conversation — most often because it rebooted.
            return self.on_setup_frame(f, wr).await;
        }
        self.on_encrypted(f, wr).await
    }

    async fn on_encrypted<W: AsyncWrite + Unpin>(
        &mut self,
        f: RawFrame,
        wr: &mut W,
    ) -> Result<(), String> {
        let messages = {
            let tls = self.tls.as_mut().ok_or("encrypted frame before tls")?;
            tls.inject_record(f.ch, f.flags, &f.payload)
                .map_err(|e| e.to_string())?
        };
        for m in messages {
            self.on_message(m, wr).await?;
        }
        Ok(())
    }

    async fn on_message<W: AsyncWrite + Unpin>(
        &mut self,
        m: Message,
        wr: &mut W,
    ) -> Result<(), String> {
        let ch = m.ch;
        let video_ch = self.video.as_ref().map(|v| v.ch);
        let audio_ch = self.audio.as_ref().map(|a| a.ch);
        if ch == CH_CONTROL {
            return self.on_control_message(m.msg_id, &m.payload, wr).await;
        }
        if Some(ch) == self.input_ch {
            return self.on_input_message(m.msg_id, &m.payload, wr).await;
        }
        if Some(ch) == video_ch {
            return self.on_av_message(true, m.msg_id, &m.payload, wr).await;
        }
        if Some(ch) == audio_ch {
            return self.on_av_message(false, m.msg_id, &m.payload, wr).await;
        }
        Ok(())
    }

    async fn on_control_message<W: AsyncWrite + Unpin>(
        &mut self,
        msg_id: u16,
        payload: &[u8],
        wr: &mut W,
    ) -> Result<(), String> {
        match msg_id {
            CTRL_AUTH_COMPLETE => Ok(()),
            CTRL_SERVICE_DISCOVERY_RESPONSE => self.on_discovery(payload, wr).await,
            CTRL_PING_REQUEST => {
                let ts = control::parse_ping_request(payload).unwrap_or(0);
                let body = control::ping_response(ts);
                self.send_control(wr, CTRL_PING_RESPONSE, &body).await
            }
            // Audio focus is asked for on the control channel but belongs to an
            // AV channel, so the answer there is what starts media.
            CTRL_AUDIO_FOCUS_REQUEST => {
                let (audio_type, state) = control::parse_audio_focus_request(payload);
                tracing::info!(
                    "[aa] {}: audio focus type={audio_type} state={state}",
                    self.label
                );
                let body = control::audio_focus_response(1);
                self.send_control(wr, CTRL_AUDIO_FOCUS_RESPONSE, &body)
                    .await?;
                self.start_av(false, wr).await?;
                Ok(())
            }
            CTRL_NAV_FOCUS_REQUEST => {
                let body = control::nav_focus_response(1);
                self.send_control(wr, CTRL_NAV_FOCUS_RESPONSE, &body).await
            }
            _ => Ok(()),
        }
    }

    /// The head unit's capabilities: open the channels we can drive.
    async fn on_discovery<W: AsyncWrite + Unpin>(
        &mut self,
        payload: &[u8],
        wr: &mut W,
    ) -> Result<(), String> {
        let discovered = control::parse_service_discovery_response(payload);
        tracing::info!(
            "[aa] {}: head unit offers video={:?} audio={:?} input={:?}",
            self.label,
            discovered.video_channel(),
            discovered.media_audio_channel(),
            discovered.input.as_ref().map(|i| i.ch)
        );
        if let Some(offer) = &discovered.video {
            self.video = Some(AvState {
                ch: offer.ch,
                ..Default::default()
            });
        }
        if let Some(ch) = discovered.media_audio_channel() {
            self.audio = Some(AvState {
                ch,
                ..Default::default()
            });
        }
        if let Some(offer) = &discovered.input {
            self.input_ch = Some(offer.ch);
            self.input_buttons = offer.buttons.clone();
        }
        let _ = self.events.send(PhoneEvent::Discovered(discovered)).await;

        // Each channel is opened on its own number, and the head unit answers
        // there rather than on the control channel.
        let mut channels: Vec<u8> = Vec::new();
        if let Some(v) = &self.video {
            channels.push(v.ch);
        }
        if let Some(a) = &self.audio {
            channels.push(a.ch);
        }
        if let Some(ch) = self.input_ch {
            channels.push(ch);
        }
        for ch in channels {
            let body = control::channel_open_request(ch);
            self.send_channel(wr, ch, CTRL_CHANNEL_OPEN_REQUEST, &body, true)
                .await?;
        }
        Ok(())
    }

    /// An AV channel's messages: its open response, its setup response, the
    /// focus that lets us start, and the acks we can ignore.
    async fn on_av_message<W: AsyncWrite + Unpin>(
        &mut self,
        is_video: bool,
        msg_id: u16,
        payload: &[u8],
        wr: &mut W,
    ) -> Result<(), String> {
        match msg_id {
            CTRL_CHANNEL_OPEN_RESPONSE => {
                let status = control::parse_channel_open_response(payload).unwrap_or(1);
                if status != 0 {
                    tracing::warn!("[aa] {}: channel open refused ({status})", self.label);
                    return Ok(());
                }
                self.mark_opened(is_video);
                let body = if is_video {
                    control::setup_request_video(self.video_codec)
                } else {
                    control::setup_request_audio()
                };
                let ch = self.av_ch(is_video).ok_or("no channel")?;
                self.send_channel(wr, ch, AV_SETUP_REQUEST, &body, false)
                    .await
            }
            AV_SETUP_RESPONSE => {
                let r = control::parse_av_setup_response(payload);
                let kind = if is_video { "video" } else { "audio" };
                if !r.ok() {
                    tracing::warn!(
                        "[aa] {}: {kind} setup refused (status {})",
                        self.label, r.status
                    );
                    return Ok(());
                }
                tracing::info!(
                    "[aa] {}: {kind} set up (max_unacked {})",
                    self.label, r.max_unacked
                );
                self.mark_set_up(is_video, r.max_unacked);
                Ok(())
            }
            // Focus is the head unit saying it is showing us; only then may
            // media start, and the channel says so with a start indication.
            AV_VIDEO_FOCUS_INDICATION if is_video => {
                let started = self.start_av(true, wr).await?;
                if started {
                    self.phase = Phase::Running;
                    let _ = self.events.send(PhoneEvent::VideoFocus(true)).await;
                }
                Ok(())
            }
            // The head unit acknowledging frames we sent. There is nothing to
            // answer; when sending is throttled by how far behind the head unit
            // is, `av::parse_media_ack` is where the count comes from.
            AV_MEDIA_ACK => Ok(()),
            _ => Ok(()),
        }
    }

    async fn on_input_message<W: AsyncWrite + Unpin>(
        &mut self,
        msg_id: u16,
        payload: &[u8],
        wr: &mut W,
    ) -> Result<(), String> {
        match msg_id {
            CTRL_CHANNEL_OPEN_RESPONSE => {
                let status = control::parse_channel_open_response(payload).unwrap_or(1);
                if status != 0 {
                    tracing::warn!("[aa] {}: input channel refused ({status})", self.label);
                    return Ok(());
                }
                // We claim the buttons the head unit said it has: claiming ones
                // it does not would only send it presses we never get.
                let body = control::input_handshake_request(&self.input_buttons);
                let ch = self.input_ch.ok_or("no input channel")?;
                self.send_channel(wr, ch, INPUT_HANDSHAKE_REQUEST, &body, false)
                    .await
            }
            INPUT_EVENT => {
                for event in input::parse_input_report(payload) {
                    let _ = self.events.send(PhoneEvent::Input(event)).await;
                }
                Ok(())
            }
            INPUT_HANDSHAKE_RESPONSE => {
                tracing::info!("[aa] {}: input channel ready", self.label);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn av_mut(&mut self, is_video: bool) -> Option<&mut AvState> {
        if is_video {
            self.video.as_mut()
        } else {
            self.audio.as_mut()
        }
    }

    fn av_ch(&self, is_video: bool) -> Option<u8> {
        if is_video {
            self.video.as_ref().map(|v| v.ch)
        } else {
            self.audio.as_ref().map(|a| a.ch)
        }
    }

    fn mark_opened(&mut self, is_video: bool) {
        if let Some(state) = self.av_mut(is_video) {
            state.opened = true;
        }
    }

    fn mark_set_up(&mut self, is_video: bool, max_unacked: u32) {
        if let Some(state) = self.av_mut(is_video) {
            state.set_up = true;
            state.max_unacked = max_unacked;
        }
    }

    /// Announces that media is about to flow. True when it actually started.
    async fn start_av<W: AsyncWrite + Unpin>(
        &mut self,
        is_video: bool,
        wr: &mut W,
    ) -> Result<bool, String> {
        let Some((ch, opened, set_up, started)) = self
            .av_mut(is_video)
            .map(|s| (s.ch, s.opened, s.set_up, s.started))
        else {
            return Ok(false);
        };
        if started || !opened || !set_up {
            return Ok(false);
        }
        self.av_mut(is_video).expect("just borrowed").started = true;
        tracing::info!(
            "[aa] {}: {} starting on ch={ch}",
            self.label,
            if is_video { "video" } else { "audio" }
        );
        let body = control::av_start_indication(0);
        self.send_channel(wr, ch, AV_START_INDICATION, &body, false)
            .await?;
        Ok(true)
    }

    async fn send_control<W: AsyncWrite + Unpin>(
        &mut self,
        wr: &mut W,
        msg_id: u16,
        body: &[u8],
    ) -> Result<(), String> {
        self.send_channel(wr, CH_CONTROL, msg_id, body, false).await
    }

    async fn send_channel<W: AsyncWrite + Unpin>(
        &mut self,
        wr: &mut W,
        ch: u8,
        msg_id: u16,
        body: &[u8],
        specific: bool,
    ) -> Result<(), String> {
        let flags = if specific {
            FLAGS_ENC_SPECIFIC
        } else {
            FLAGS_ENC_SIGNAL
        };
        let wire = {
            let tls = self.tls.as_mut().ok_or("no tls")?;
            tls.encrypt(ch, flags, msg_id, body)
                .map_err(|e| e.to_string())?
        };
        self.write(wr, &wire).await
    }

    async fn send_media<W: AsyncWrite + Unpin>(
        &mut self,
        frame: MediaFrame,
        wr: &mut W,
    ) -> Result<(), String> {
        let started = match (&self.video, &self.audio) {
            (Some(v), _) if v.ch == frame.ch => v.started,
            (_, Some(a)) if a.ch == frame.ch => a.started,
            _ => false,
        };
        if !started {
            if !self.complained {
                self.complained = true;
                tracing::warn!(
                    "[aa] {}: ch={} not started yet, dropping media",
                    self.label, frame.ch
                );
            }
            return Ok(());
        }
        let (msg_id, payload) = match frame.ts_us {
            Some(ts) => {
                let mut p = ts.to_be_bytes().to_vec();
                p.extend_from_slice(&frame.data);
                (AV_MEDIA_WITH_TIMESTAMP, p)
            }
            None => (AV_MEDIA_INDICATION, frame.data),
        };
        let wire = {
            let tls = self.tls.as_mut().ok_or("no tls")?;
            tls.encrypt(frame.ch, FLAGS_ENC_SIGNAL, msg_id, &payload)
                .map_err(|e| e.to_string())?
        };
        self.write(wr, &wire).await
    }

    async fn write<W: AsyncWrite + Unpin>(&self, wr: &mut W, bytes: &[u8]) -> Result<(), String> {
        wr.write_all(bytes).await.map_err(|e| format!("write: {e}"))
    }

    async fn closed(&mut self, reason: &str) {
        tracing::info!("[aa] {}: closed ({reason})", self.label);
        let _ = self
            .events
            .send(PhoneEvent::Closed(reason.to_owned()))
            .await;
    }
}

/// Microseconds since the epoch, for frames that arrive without a timestamp.
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}
