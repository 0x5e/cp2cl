//! One CarLife session: the conversation the car starts, over whichever pipe the
//! USB side handed us.
//!
//! The car speaks first and every step is its idea, so this is a state machine
//! that reacts rather than one that drives. It is written against
//! `AsyncRead + AsyncWrite` rather than against the gadget so it can be run —
//! and tested — over an ordinary in-memory pipe; `gadget` supplies the other
//! end of that pipe on the board.
//!
//! The order of the messages, the ids and the field numbers all come from the
//! car's own library and the official guide; see the plan for the table and the
//! citations. Two things are deliberately incomplete:
//!
//! * **Authentication is not answered.** The car's challenge can only be met
//!   with a private key that lives on the phone this device is pretending to be,
//!   so there is nothing honest to reply with. What arrives is reported and left
//!   alone. A car that insists is where this ends.
//! * **Input is not acted on.** Touch and panel keys arrive on the sixth channel
//!   and are reported, but there is no CarPlay side attached to send them to yet.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{frame, proto};
use crate::config::Config;
use crate::sink::ClOutlet;

// What the car sends, by name — the numbers are CarLifeVehicleLib's
// `EPackageHeadType`.
const HU_PROTOCOL_VERSION: u32 = 0x0001_8001;
const HU_INFO: u32 = 0x0001_8003;
const HU_VIDEO_ENCODER_INIT: u32 = 0x0001_8007;
const HU_VIDEO_ENCODER_START: u32 = 0x0001_8009;
const HU_VIDEO_ENCODER_PAUSE: u32 = 0x0001_800A;
const HU_VIDEO_ENCODER_RESET: u32 = 0x0001_800B;
const HU_VIDEO_ENCODER_FRAME_RATE_CHANGE: u32 = 0x0001_800C;
const HU_GO_TO_FOREGROUND: u32 = 0x0001_8025;
const HU_STATISTIC_INFO: u32 = 0x0001_8027;
const HU_AUTHEN_REQUEST: u32 = 0x0001_8048;
const HU_AUTHEN_RESULT: u32 = 0x0001_804A;
const HU_FEATURE_CONFIG_RESPONSE: u32 = 0x0001_8052;
const HU_ERROR_CODE: u32 = 0x0001_8055;

// What we send.
const MATCH_STATUS: u32 = 0x0001_0002;
const MD_INFO: u32 = 0x0001_0004;
const VIDEO_ENCODER_INIT_DONE: u32 = 0x0001_0008;
const VIDEO_ENCODER_FRAME_RATE_CHANGE_DONE: u32 = 0x0001_000D;
const MD_AUTHEN_RESULT: u32 = 0x0001_004B;
const GO_TO_FOREGROUND_RESPONSE: u32 = 0x0001_004C;
const FEATURE_CONFIG_REQUEST: u32 = 0x0001_0051;
const VIDEO_DATA: u32 = 0x0002_0001;
const VIDEO_HEARTBEAT: u32 = 0x0002_0002;

const TOUCH_ACTION: u32 = 0x0006_8001;
const TOUCH_HARD_KEY: u32 = 0x0006_8008;

/// How often the video channel is touched when there is no picture to send. The
/// reference does the same whenever its own interface is not in the foreground.
const HEARTBEAT: Duration = Duration::from_secs(1);

/// Where the video channel's pictures come from. `CP2CL_INPUT` picks between the
/// two that have a source; the third is what a caller with neither passes.
pub enum Frames {
    /// Nothing but heartbeats: the session is up and the car is kept waiting.
    None,
    /// One access unit, repeated at the agreed rate.
    ///
    /// Not a picture a person would watch — every frame is a keyframe and none
    /// of them change — but it is a real H.264 stream, so it is enough to prove
    /// the channel and the car's decoder end to end with no encoder and no phone
    /// in the picture. `CP2CL_CARLIFE_FRAME` names the file it comes from.
    Still(Vec<u8>),
    /// The CarPlay side, as it arrives. Frames are written to the car as they
    /// come in rather than on the session's own clock: they are a coded stream,
    /// and it is the phone's encoder that decides the rate.
    Live(Arc<ClOutlet>),
}

impl Frames {
    /// One access unit from the file `CP2CL_CARLIFE_FRAME` names.
    ///
    /// A file that cannot be read is worth a warning and not a failure: the
    /// session is still useful without it, and saying so beats refusing to start.
    pub fn from_file() -> Self {
        let Some(path) = std::env::var("CP2CL_CARLIFE_FRAME")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        else {
            tracing::warn!(
                "[cl] CP2CL_INPUT=file, but CP2CL_CARLIFE_FRAME names no file; \
                 sending heartbeats only"
            );
            return Self::None;
        };
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => {
                tracing::info!(
                    "[cl] the video channel will repeat {path} ({} bytes)",
                    bytes.len()
                );
                Self::Still(bytes)
            }
            Ok(_) => {
                tracing::warn!("[cl] {path} is empty; sending heartbeats only");
                Self::None
            }
            Err(e) => {
                tracing::warn!("[cl] {path} could not be read ({e}); sending heartbeats only");
                Self::None
            }
        }
    }
}

/// Runs one session until the car goes away.
///
/// `carplay` is told `true` the first time the car has been given our device
/// information — from that point the session is up and the CarPlay side has
/// somewhere to hand its streams — and `false` once, when the session ends for
/// any reason. On the board that switch is what starts and stops the whole
/// CarPlay half; see `main`.
pub async fn run<S, F>(stream: S, frames: &Frames, cfg: &Config, carplay: &mut F) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    F: FnMut(bool),
{
    // A session is what drains the bridge, so it takes the receiving half for as
    // long as it runs. The car unplugged and plugged back in is a new session and
    // a new call to this function, which is why the half is given back on the way
    // out rather than dropped with everything else.
    let (mut video, source) = match frames {
        Frames::Live(outlet) => match outlet.take_receiver() {
            Some(rx) => (Some(rx), Source::Live),
            None => {
                tracing::warn!(
                    "[cl] the video channel is already being read; sending heartbeats only"
                );
                (None, Source::None)
            }
        },
        Frames::Still(still) => (None, Source::Still(still.clone())),
        Frames::None => (None, Source::None),
    };

    // Reads are their own task. A frame can be six hundred kilobytes and the
    // video tick fires while one is arriving, so the read cannot live inside a
    // `select!` that would drop it half way and lose the bytes it had taken.
    // The channel in between is what makes the tick safe to wait on.
    let (mut reader, writer) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<ChunkOrEnd>(4);
    let reading = tokio::spawn(async move {
        loop {
            let arriving = match frame::read_chunk(&mut reader).await {
                Ok(Some(chunk)) => ChunkOrEnd::Chunk(chunk),
                Ok(None) => ChunkOrEnd::End,
                Err(e) => ChunkOrEnd::Failed(e),
            };
            let last = !matches!(arriving, ChunkOrEnd::Chunk(_));
            if tx.send(arriving).await.is_err() || last {
                return;
            }
        }
    });

    let mut session = Session {
        writer,
        cfg,
        source,
        channel_touched: None,
        agreed: None,
        started_at: None,
        next_tick: Instant::now(),
        up: false,
    };

    let outcome = loop {
        let deadline = session.next_tick;
        tokio::select! {
            arriving = rx.recv() => match arriving {
                Some(ChunkOrEnd::Chunk(chunk)) => {
                    if let Err(e) = session.handle(chunk, carplay).await {
                        break Err(e);
                    }
                }
                Some(ChunkOrEnd::End) => break Ok(()),
                Some(ChunkOrEnd::Failed(e)) => break Err(e.into()),
                // The reader task is gone, and it only goes away when the
                // channel is closed, which only happens here.
                None => break Ok(()),
            },
            // A picture from the CarPlay side, written straight through. It is
            // not put on the tick: the phone's encoder decides how often frames
            // come, and holding them back to a schedule of our own would only
            // add delay.
            access_unit = recv_video(&mut video) => match access_unit {
                Some(access_unit) => {
                    if let Err(e) = session.send_video(access_unit).await {
                        break Err(e);
                    }
                }
                // The CarPlay side has gone, taking its frames with it. The
                // heartbeats are what is left to keep the channel alive.
                None => video = None,
            },
            // Only a tick moves the schedule forward, so a busy stream of
            // messages cannot starve the video channel.
            _ = tokio::time::sleep_until(deadline) => {
                if let Err(e) = session.tick().await {
                    break Err(e);
                }
                session.next_tick = Instant::now() + session.period();
            }
        }
    };

    reading.abort();
    if let (Some(rx), Frames::Live(outlet)) = (video, frames) {
        outlet.return_receiver(rx);
    }
    if session.up {
        carplay(false);
        tracing::info!("[cl] the session is over; the CarPlay side goes with it");
    }
    outcome
}

/// What the reader task hands over.
enum ChunkOrEnd {
    Chunk(frame::Chunk),
    End,
    Failed(std::io::Error),
}

/// Where the pictures come from, holding everything the session needs to know
/// about it: the receiving half is not here, because it belongs to the `select!`.
enum Source {
    None,
    Still(Vec<u8>),
    Live,
}

/// The next access unit from the CarPlay side, or never if there is no CarPlay
/// side. Waiting on a future that never resolves is what keeps the branch out of
/// the way without a second code path in the loop.
async fn recv_video(video: &mut Option<mpsc::Receiver<Vec<u8>>>) -> Option<Vec<u8>> {
    match video {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

struct Session<'a, W> {
    writer: W,
    cfg: &'a Config,
    source: Source,
    /// When the video channel last carried anything at all. With pictures
    /// arriving on their own clock, this is what decides whether the heartbeat
    /// still has a job to do.
    channel_touched: Option<Instant>,
    /// What the car asked for and what we agreed to, once it has asked.
    agreed: Option<proto::VideoEncoderInfo>,
    /// When the encoder started, which is what the video channel's timestamps
    /// are measured from.
    started_at: Option<Instant>,
    next_tick: Instant,
    up: bool,
}

impl<W: AsyncWrite + Unpin> Session<'_, W> {
    /// One message from the car.
    async fn handle<F: FnMut(bool)>(&mut self, chunk: frame::Chunk, carplay: &mut F) -> Result<()> {
        if chunk.channel != frame::CMD {
            self.other_channel(chunk);
            return Ok(());
        }
        let Some(message) = frame::decode(frame::CMD, &chunk.body) else {
            tracing::warn!("[cl] a command chunk that does not decode was dropped");
            return Ok(());
        };
        if !frame::from_car(message.service_type) {
            tracing::debug!(
                "[cl] ignoring {:#010x} — the car's direction bit is clear",
                message.service_type
            );
            return Ok(());
        }

        match message.service_type {
            HU_PROTOCOL_VERSION => {
                let (major, minor) = proto::decode_protocol_version(&message.body);
                tracing::info!("[cl] the car speaks CarLife {major}.{minor}");
                let body = proto::protocol_version_match_status(proto::MATCH);
                self.send(MATCH_STATUS, &body).await?;
            }
            HU_INFO => {
                let body = proto::device_info(&proto::DeviceInfo {
                    os: "Android".into(),
                    brand: "cp2cl".into(),
                    model: self.cfg.name.clone(),
                    release: "10".into(),
                    sdk_int: 29,
                    bt_address: self.cfg.bt_mac.clone(),
                });
                self.send(MD_INFO, &body).await?;
                if !self.up {
                    self.up = true;
                    tracing::info!("[cl] the car has our device information; the session is up");
                    carplay(true);
                }
            }
            HU_STATISTIC_INFO => {
                let info = proto::decode_statistic_info(&message.body);
                // `connect_time` is reported as-is: the field is an int32 and
                // neither the guide nor the head-unit library says in what unit.
                tracing::info!(
                    "[cl] the car reports cuid {:?} version {:?} (build {}), channel {:?}; \
                     {} connections, {} of them successful, connect time {}",
                    info.cuid,
                    info.version_name,
                    info.version_code,
                    info.channel,
                    info.connect_count,
                    info.connect_success_count,
                    info.connect_time
                );
                // The car will not configure anything until it is asked, and it
                // has a two-minute limit on sending the statistics above.
                self.send(FEATURE_CONFIG_REQUEST, &[]).await?;
            }
            HU_FEATURE_CONFIG_RESPONSE => {
                for (key, value) in proto::decode_feature_configs(&message.body) {
                    tracing::info!("[cl] the car set {key} = {value}");
                }
            }
            HU_AUTHEN_REQUEST => {
                let challenge = proto::decode_authen_request(&message.body).unwrap_or_default();
                tracing::warn!("[cl] the car asked us to authenticate with {challenge:?}");
                tracing::warn!(
                    "[cl] it is not answered — the key that would answer it belongs to the \
                     phone, not to us. If the car insists, this is where it disconnects"
                );
            }
            HU_AUTHEN_RESULT => {
                let passed = proto::decode_authen_result(&message.body).unwrap_or(false);
                tracing::info!("[cl] the car's own verdict on us: {passed}");
                let body = proto::authen_result(passed);
                self.send(MD_AUTHEN_RESULT, &body).await?;
            }
            HU_VIDEO_ENCODER_INIT => {
                let asked = proto::decode_video_encoder_info(&message.body);
                // What we can actually produce, which is what `INIT_DONE` is
                // for: the reference says both ends work from the device's
                // answer, and the car scales if the two disagree.
                let agreed = proto::VideoEncoderInfo {
                    width: if asked.width > 0 {
                        asked.width
                    } else {
                        self.cfg.width as i32
                    },
                    height: if asked.height > 0 {
                        asked.height
                    } else {
                        self.cfg.height as i32
                    },
                    frame_rate: match asked.frame_rate {
                        3..=30 => asked.frame_rate,
                        _ => self.cfg.fps as i32,
                    },
                };
                tracing::info!(
                    "[cl] the car asked for {}x{} at {} fps; agreeing to {}x{} at {}",
                    asked.width,
                    asked.height,
                    asked.frame_rate,
                    agreed.width,
                    agreed.height,
                    agreed.frame_rate
                );
                self.agreed = Some(agreed);
                let body = proto::video_encoder_info(agreed);
                self.send(VIDEO_ENCODER_INIT_DONE, &body).await?;
            }
            HU_VIDEO_ENCODER_START => {
                tracing::info!("[cl] the car started the encoder; video begins");
                let now = Instant::now();
                self.started_at = Some(now);
                // The start itself is what the car hears next, so the first
                // heartbeat is only owed if no picture arrives within a period.
                self.channel_touched = Some(now);
                self.next_tick = now;
            }
            HU_VIDEO_ENCODER_PAUSE | HU_VIDEO_ENCODER_RESET => {
                tracing::info!("[cl] the car stopped the encoder");
                self.started_at = None;
            }
            HU_VIDEO_ENCODER_FRAME_RATE_CHANGE => {
                let asked = proto::decode_frame_rate(&message.body).unwrap_or(0);
                let accepted = match asked {
                    3..=30 => asked,
                    _ => self.agreed.map(|agreed| agreed.frame_rate).unwrap_or(30),
                };
                tracing::info!("[cl] the car asked for {asked} fps; agreeing to {accepted}");
                if let Some(agreed) = self.agreed.as_mut() {
                    agreed.frame_rate = accepted;
                }
                let body = proto::frame_rate(accepted);
                self.send(VIDEO_ENCODER_FRAME_RATE_CHANGE_DONE, &body)
                    .await?;
            }
            HU_GO_TO_FOREGROUND => {
                // There is no window here to bring forward, so the honest reply
                // is sent at once rather than after the five seconds the guide
                // gives a device that is actually switching.
                tracing::debug!("[cl] the car asked us to come to the foreground");
                self.send(GO_TO_FOREGROUND_RESPONSE, &[]).await?;
            }
            HU_ERROR_CODE => {
                tracing::warn!(
                    "[cl] the car reported an error, {} bytes of it",
                    message.body.len()
                );
            }
            other => tracing::debug!("[cl] {other:#010x} arrived and is not acted on"),
        }
        Ok(())
    }

    /// A chunk on any channel but the command channel.
    ///
    /// Only the sixth channel is decoded, and only to be reported: it is where
    /// the car's touch and panel keys arrive, and whether it sends any at all —
    /// and with what — is one of the things the bench exists to find out. There
    /// is nothing to forward them to until the CarPlay side is attached.
    fn other_channel(&self, chunk: frame::Chunk) {
        if chunk.channel != frame::TOUCH {
            tracing::debug!("[cl] channel {} sent {} bytes", chunk.channel, chunk.body.len());
            return;
        }
        let Some(message) = frame::decode(frame::TOUCH, &chunk.body) else {
            tracing::warn!("[cl] a touch chunk that does not decode was dropped");
            return;
        };
        match message.service_type {
            TOUCH_ACTION => match proto::decode_touch_action(&message.body) {
                Some(touch) => tracing::debug!(
                    "[cl] touch action {} at {}, {}",
                    touch.action,
                    touch.x,
                    touch.y
                ),
                None => tracing::warn!("[cl] a touch action with fields missing was dropped"),
            },
            TOUCH_HARD_KEY => match proto::decode_hard_key(&message.body) {
                Some(key) => tracing::debug!("[cl] panel key {key:#04x}"),
                None => tracing::warn!("[cl] a panel key with its field missing was dropped"),
            },
            other => tracing::debug!("[cl] {other:#010x} arrived on the touch channel"),
        }
    }

    /// Sends whatever the video channel owes right now.
    async fn tick(&mut self) -> Result<()> {
        let Some(started_at) = self.started_at else {
            // Nothing has asked for a picture, so nothing is sent. The tick is
            // still alive, because it is what notices when that changes.
            return Ok(());
        };
        match &self.source {
            Source::Still(still) => {
                let elapsed = started_at.elapsed().as_millis() as u32;
                let still = still.clone();
                self.send_frame(VIDEO_DATA, elapsed, &still).await
            }
            // Pictures arrive on the phone's own clock, so there is nothing for
            // the tick to send while they do. A heartbeat goes out once they
            // stop, which is what tells the car the channel is still ours.
            Source::Live => {
                let idle = self
                    .channel_touched
                    .map_or(true, |touched| touched.elapsed() >= HEARTBEAT);
                if idle {
                    self.channel_touched = Some(Instant::now());
                    self.send(VIDEO_HEARTBEAT, &[]).await
                } else {
                    Ok(())
                }
            }
            Source::None => self.send(VIDEO_HEARTBEAT, &[]).await,
        }
    }

    /// One access unit from the CarPlay side, on the video channel.
    ///
    /// A frame that arrives before the car has started the encoder, or while it
    /// has it paused, is dropped rather than queued: the car is not expecting
    /// pictures then, and one held back would arrive attached to a timestamp
    /// measured from a start that has since been reset.
    async fn send_video(&mut self, access_unit: Vec<u8>) -> Result<()> {
        let Some(started_at) = self.started_at else {
            return Ok(());
        };
        let timestamp_ms = started_at.elapsed().as_millis() as u32;
        self.channel_touched = Some(Instant::now());
        self.send_frame(VIDEO_DATA, timestamp_ms, &access_unit).await
    }

    /// How long until the next tick: a frame's worth of time when there is a
    /// picture to send on a schedule and one has been agreed, otherwise the
    /// heartbeat's.
    fn period(&self) -> Duration {
        match (&self.source, self.started_at, self.agreed) {
            (Source::Still(_), Some(_), Some(agreed)) if agreed.frame_rate > 0 => {
                Duration::from_millis(1000 / agreed.frame_rate as u64)
            }
            _ => HEARTBEAT,
        }
    }

    /// One message out on the command channel.
    async fn send(&mut self, service_type: u32, body: &[u8]) -> Result<()> {
        self.send_frame(service_type, 0, body).await
    }

    /// One message out, on the channel its number names.
    ///
    /// The channel is the number's top half-word and is written from it rather
    /// than passed separately, so the two cannot disagree.
    async fn send_frame(&mut self, service_type: u32, timestamp_ms: u32, body: &[u8]) -> Result<()> {
        let channel = service_type >> 16;
        let message = frame::encode(channel, service_type, timestamp_ms, body);
        frame::write_chunk(&mut self.writer, channel, &message)
            .await
            .with_context(|| format!("write {service_type:#010x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::{MediaSink, StreamEvent};
    use std::sync::{Arc, Mutex};
    use tokio::io::DuplexStream;

    /// The other end of the pipe: enough of a car to walk the conversation, and
    /// no more. Everything it sends is what the reference sends.
    struct Car {
        pipe: DuplexStream,
    }

    impl Car {
        async fn send(&mut self, service_type: u32, body: &[u8]) {
            let channel = service_type >> 16;
            let message = frame::encode(channel, service_type, 0, body);
            frame::write_chunk(&mut self.pipe, channel, &message)
                .await
                .expect("the car can write");
        }

        /// The next message, which is expected to be the one named.
        async fn expect(&mut self, service_type: u32) -> frame::Message {
            let deadline = Duration::from_secs(2);
            let chunk = tokio::time::timeout(deadline, frame::read_chunk(&mut self.pipe))
                .await
                .expect("something arrived before the deadline")
                .expect("the pipe is intact")
                .expect("the session is still there");
            let message = frame::decode(chunk.channel, &chunk.body).expect("it decodes");
            assert_eq!(
                message.service_type, service_type,
                "expected {service_type:#010x}"
            );
            message
        }

        /// Everything the car asks before it will start the encoder.
        async fn handshake(&mut self, asked: proto::VideoEncoderInfo) {
            let mut version = Vec::new();
            proto::put_int(&mut version, 1, 1);
            proto::put_int(&mut version, 2, 0);
            self.send(HU_PROTOCOL_VERSION, &version).await;
            self.expect(MATCH_STATUS).await;

            self.send(HU_INFO, &[]).await;
            self.expect(MD_INFO).await;

            self.send(HU_STATISTIC_INFO, &[]).await;
            self.expect(FEATURE_CONFIG_REQUEST).await;

            self.send(HU_VIDEO_ENCODER_INIT, &proto::video_encoder_info(asked))
                .await;
            let agreed = self.expect(VIDEO_ENCODER_INIT_DONE).await;
            assert_eq!(
                proto::decode_video_encoder_info(&agreed.body),
                proto::decode_video_encoder_info(&proto::video_encoder_info(asked))
            );
        }

        async fn start_the_encoder(&mut self) {
            self.send(HU_VIDEO_ENCODER_START, &[]).await;
        }

        /// Nothing at all on the pipe for this long.
        async fn expect_silence(&mut self, quiet_for: Duration) {
            let heard = tokio::time::timeout(quiet_for, frame::read_chunk(&mut self.pipe)).await;
            assert!(heard.is_err(), "the session spoke when it should not have");
        }
    }

    fn asked() -> proto::VideoEncoderInfo {
        proto::VideoEncoderInfo {
            width: 1280,
            height: 720,
            frame_rate: 30,
        }
    }

    #[tokio::test]
    async fn the_handshake_is_answered_and_the_session_is_reported_up_then_down() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let switches = Arc::new(Mutex::new(Vec::new()));
        let seen = switches.clone();
        let mut carplay = move |up: bool| seen.lock().unwrap().push(up);

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(asked()).await;
            car.start_the_encoder().await;
            car.expect(VIDEO_HEARTBEAT).await;
            // Dropping the pipe is the car going away.
        });

        run(device, &Frames::None, &cfg, &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();

        assert_eq!(*switches.lock().unwrap(), vec![true, false]);
    }

    #[tokio::test]
    async fn the_repeated_frame_goes_out_on_the_video_channel_with_a_timestamp() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(asked()).await;
            car.start_the_encoder().await;
            let first = car.expect(VIDEO_DATA).await;
            assert_eq!(first.body, vec![0x00, 0x00, 0x00, 0x01, 0x65]);
            assert!(first.timestamp_ms.is_some());
        });

        let still = Frames::Still(vec![0x00, 0x00, 0x00, 0x01, 0x65]);
        run(device, &still, &cfg, &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    #[tokio::test]
    async fn nothing_is_sent_on_the_video_channel_until_the_car_starts_it() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(asked()).await;
            // Long enough for a heartbeat to have arrived had one been coming.
            car.expect_silence(Duration::from_millis(500)).await;
        });

        let still = Frames::Still(vec![0x00, 0x00, 0x00, 0x01, 0x65]);
        run(device, &still, &cfg, &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    #[tokio::test]
    async fn the_challenge_is_left_unanswered_by_default() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            let mut challenge = Vec::new();
            proto::put_str(&mut challenge, 1, "A_B");
            car.send(HU_AUTHEN_REQUEST, &challenge).await;
            car.expect_silence(Duration::from_millis(500)).await;
        });

        run(device, &Frames::None, &cfg, &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }

    #[tokio::test]
    async fn a_frame_from_the_carplay_side_goes_out_as_it_arrives() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(asked()).await;
            car.start_the_encoder().await;
            let _ = started_tx.send(());
            let first = car.expect(VIDEO_DATA).await;
            assert_eq!(first.body, vec![0x00, 0x00, 0x00, 0x01, 0x41]);
            assert!(first.timestamp_ms.is_some());
        });

        // What the CarPlay side would push, once the car is ready for it. The
        // pause is not slack: the session has to have read START and marked the
        // encoder running, or the frame is dropped on arrival, the same way it
        // would be from a phone that sent one too early.
        let pushing = tokio::spawn(async move {
            let _ = started_rx.await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            outlet.video().push(StreamEvent::VideoFrame {
                keyframe: false,
                data: vec![0x00, 0x00, 0x00, 0x01, 0x41],
            });
        });

        run(device, &frames, &cfg, &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();
        pushing.await.unwrap();
    }

    #[tokio::test]
    async fn the_video_channel_is_given_back_when_the_session_ends() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};
        let outlet = Arc::new(ClOutlet::new());
        let frames = Frames::Live(outlet.clone());

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            car.handshake(asked()).await;
            // Dropping the pipe is the car going away.
        });

        run(device, &frames, &cfg, &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();

        // The next session after the car is plugged back in gets a receiver that
        // still works, which means the first one was handed back rather than
        // dropped with the session.
        let rx = outlet.take_receiver().expect("it was handed back");
        outlet.return_receiver(rx);
    }

    #[tokio::test]
    async fn a_panel_key_is_read_rather_than_dropped_on_the_floor() {
        let cfg = Config::from_env();
        let (device, pipe) = tokio::io::duplex(64 * 1024);
        let mut carplay = |_: bool| {};

        let car = tokio::spawn(async move {
            let mut car = Car { pipe };
            let mut key = Vec::new();
            proto::put_int(&mut key, 1, 0x14);
            car.send(TOUCH_HARD_KEY, &key).await;
            // Nothing answers it, so the session must simply carry on.
            car.expect_silence(Duration::from_millis(300)).await;
        });

        run(device, &Frames::None, &cfg, &mut carplay)
            .await
            .expect("the session ends cleanly");
        car.await.unwrap();
    }
}
