//! The browser client: PlayPort's own front end, served from the submodule and
//! fed the streams over one WebSocket.
//!
//! Two protocols share that socket, and this module speaks both:
//!
//! * JSON control messages (`hello`, `session`, `mic` down; `touch`, `knob`,
//!   `media`, `siri`, `night`, `telephony`, `keyframe`, `ping` up), and
//! * a binary framing of the media itself, little-endian, one byte of type up
//!   front — the layout `web/src/protocol.ts` parses.
//!
//! That framing only ever travels one way. The other binary traffic is the
//! microphone: captured audio, encoded by the browser and passed straight to
//! `MicSink`, because this end has no more idea than the browser does what the
//! phone will take until it says.
//!
//! The browser is the decoder: it uses WebCodecs, so it has to be handed the
//! codec description (the raw avcC / hvcC record) rather than an elementary
//! stream, and WebCodecs only exists in a secure context, which is why this is
//! HTTPS even on a LAN address.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio::sync::mpsc::{self, Sender};

use crate::airplay::event::EventSink;
use crate::airplay::{MicConfig, MicSink, ScreenParams, SharedScreen};
use crate::commands::{Command, TouchContact, TOUCH_SLOTS};
use crate::config::Config;
use crate::sink::{AudioCodec, MediaSink, StreamEvent, VideoCodec};

/// The one screen stream this program declares. `/info` names it and nothing
/// else does, so the browser's stream type is a constant.
const SCREEN_STREAM: u8 = 110;
/// How many frames may queue for one browser before it is behind. Dropping is
/// the right answer for live media; queueing is how latency accumulates.
const SEND_QUEUE: usize = 1024;

// --- the binary wire -------------------------------------------------------

const VIDEO_CONFIG: u8 = 1;
const VIDEO_FRAME: u8 = 2;
const AUDIO_CONFIG: u8 = 3;
const AUDIO_PACKET: u8 = 4;
const SCREEN_ACTIVE: u8 = 5;
const AUDIO_STOPPED: u8 = 6;

const VIDEO_CODEC_H264: u8 = 1;
const VIDEO_CODEC_H265: u8 = 2;
const AUDIO_CODEC_AAC_LC: u8 = 1;
const AUDIO_CODEC_OPUS: u8 = 2;
const AUDIO_CODEC_LPCM: u8 = 3;

const FLAG_KEYFRAME: u8 = 1;

fn video_config_frame(
    stream: u8,
    codec: VideoCodec,
    width: u32,
    height: u32,
    data: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(11 + data.len());
    out.push(VIDEO_CONFIG);
    out.push(stream);
    out.push(match codec {
        VideoCodec::H264 => VIDEO_CODEC_H264,
        VideoCodec::H265 => VIDEO_CODEC_H265,
    });
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(data);
    out
}

fn video_frame_frame(stream: u8, keyframe: bool, nalu: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(11 + nalu.len());
    out.push(VIDEO_FRAME);
    out.push(stream);
    out.push(if keyframe { FLAG_KEYFRAME } else { 0 });
    // The browser timestamps frames itself; the field is here because the
    // protocol has it.
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(nalu);
    out
}

fn audio_config_frame(
    stream: u8,
    codec: AudioCodec,
    name: &str,
    description: &[u8],
) -> Vec<u8> {
    let name = name.as_bytes();
    let name = &name[..name.len().min(24)];
    let mut out = Vec::with_capacity(9 + name.len() + description.len());
    out.push(AUDIO_CONFIG);
    out.push(stream);
    out.push(match codec {
        AudioCodec::AacLc { .. } => AUDIO_CODEC_AAC_LC,
        AudioCodec::Opus { .. } => AUDIO_CODEC_OPUS,
        AudioCodec::Pcm { .. } => AUDIO_CODEC_LPCM,
    });
    out.extend_from_slice(&codec.rate().to_le_bytes());
    out.push(codec.channels() as u8);
    out.push(name.len() as u8);
    out.extend_from_slice(name);
    out.extend_from_slice(description);
    out
}

fn audio_packet_frame(stream: u8, timestamp_us: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + payload.len());
    out.push(AUDIO_PACKET);
    out.push(stream);
    out.extend_from_slice(&timestamp_us.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// MPEG-4 AudioSpecificConfig for AAC-LC: 5-bit object type, 4-bit rate index,
/// 4-bit channel config. WebCodecs wants this as the AAC decoder's description.
fn audio_specific_config(sample_rate: u32, channels: u8) -> Vec<u8> {
    let rate_index = match sample_rate {
        96_000 => 0,
        88_200 => 1,
        64_000 => 2,
        48_000 => 3,
        44_100 => 4,
        32_000 => 5,
        24_000 => 6,
        22_050 => 7,
        16_000 => 8,
        12_000 => 9,
        11_025 => 10,
        8_000 => 11,
        _ => 4,
    };
    let config = (2u32 << 11) | (rate_index << 7) | ((channels as u32) << 3);
    vec![(config >> 8) as u8, config as u8]
}

// --- messages from the browser ---------------------------------------------

#[derive(Debug, Deserialize)]
struct ClientMessage {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    contacts: Vec<Contact>,
    #[serde(default)]
    media: Option<u8>,
    #[serde(default)]
    knob: Option<Knob>,
    #[serde(default)]
    night: Option<bool>,
    #[serde(default)]
    telephony: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct Contact {
    /// The slot the browser keeps a finger in — it tracks a finger by slot, not
    /// by identity, which is why an empty slot has to be reported as lifted.
    id: u8,
    /// Normalised to 0..1: the browser is never told the screen size, so this is
    /// the only way it can point at it.
    x: f64,
    y: f64,
    down: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Knob {
    select: bool,
    home: bool,
    back: bool,
    x: i32,
    y: i32,
    wheel: i32,
}

// --- the hub ---------------------------------------------------------------

/// One connected browser.
struct Client {
    tx: Sender<Message>,
    /// Set once a keyframe has been queued for it. Every viewer starts decoding
    /// at a keyframe: one that joins mid-stream has nothing for the deltas to
    /// refer back to, and the phone only sends a keyframe when asked.
    ready: AtomicBool,
}

/// Fans the streams out to every connected browser, and takes their input back.
///
/// Also the `MediaSink` the airplay receiver writes into — one object rather
/// than two, because a video and an audio event differ only in what they are
/// converted to.
pub struct Hub {
    device_name: String,
    /// What the phone is being asked to render. Written by the display panel and
    /// read by `/info`, which is why it is the same slot the head unit writes.
    display: SharedScreen,
    /// The answer until something writes that slot: the size and codec the
    /// environment asked for.
    default_display: ScreenParams,
    clients: Mutex<HashMap<u64, Client>>,
    next_client: AtomicU64,
    /// Replayed to a viewer that joins after the streams have started.
    video_configs: Mutex<HashMap<u8, (VideoCodec, Vec<u8>)>>,
    audio_configs: Mutex<HashMap<u8, Vec<u8>>>,
    /// Whether a CarPlay session is up, as far as the browser is told. Driven by
    /// the screen stream opening and closing, which is the only signal this side
    /// of the program sees.
    session_active: AtomicBool,
    /// Where input goes. Live from the start and empty until pair-verify runs,
    /// so a press before then is dropped rather than queued.
    input: Arc<EventSink>,
    /// Where the browser's captured microphone audio goes, and where the news
    /// that it should start capturing comes from.
    mic: Arc<MicSink>,
    /// Fired to drop the phone's connection, so a display change lands. `/info`
    /// is answered once per session and never asked again, so nothing short of a
    /// new session changes what the phone renders.
    restart: broadcast::Sender<()>,
    /// Where each touch slot was last reported. The browser lists a finger only
    /// while it is down, so a lift arrives as a slot that is simply missing from
    /// the message — and reporting that slot at the origin instead would drop the
    /// finger in the top left corner, which the phone reads as a jump and stops
    /// the press it was in the middle of.
    last_touch: Mutex<[TouchContact; TOUCH_SLOTS]>,
}

impl Hub {
    pub fn new(
        cfg: &Config,
        display: SharedScreen,
        input: Arc<EventSink>,
        mic: Arc<MicSink>,
        restart: broadcast::Sender<()>,
    ) -> Arc<Self> {
        let hub = Arc::new(Self {
            device_name: cfg.name.clone(),
            display,
            default_display: ScreenParams {
                width: cfg.width,
                height: cfg.height,
                fps: cfg.fps,
                ui_scale: 100,
                hevc: cfg.hevc,
            },
            clients: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            video_configs: Mutex::new(HashMap::new()),
            audio_configs: Mutex::new(HashMap::new()),
            session_active: AtomicBool::new(false),
            input,
            mic,
            restart,
            last_touch: Mutex::new([TouchContact::default(); TOUCH_SLOTS]),
        });
        hub.watch_microphone();
        hub
    }

    /// Tells every browser when the phone starts and stops taking microphone
    /// audio. That is the only thing that decides whether one captures at all:
    /// there is nowhere to put the samples otherwise, and a browser holding the
    /// microphone open for nobody is a light that never goes out.
    fn watch_microphone(self: &Arc<Self>) {
        let mut state = self.mic.subscribe();
        let hub = self.clone();
        tokio::spawn(async move {
            loop {
                let now = *state.borrow_and_update();
                hub.broadcast_json(&mic_message(now.as_ref()));
                if state.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    /// One captured payload, straight to the phone. The framing is the sink's
    /// business; nothing here has to know how long a packet is.
    async fn mic_payload(&self, payload: &[u8]) {
        self.mic.send(payload).await;
    }

    /// What the phone has been asked to render.
    fn shown_display(&self) -> ScreenParams {
        self.display
            .lock()
            .ok()
            .and_then(|g| *g)
            .unwrap_or(self.default_display)
    }

    /// The size the stream is actually coded at, which is also the pixel space a
    /// touch is measured in. Not the same as `shown_display` when the UI scale
    /// asked the phone for a different canvas.
    fn canvas(&self) -> ScreenParams {
        let shown = self.shown_display();
        let (width, height) = shown.canvas();
        ScreenParams { width, height, ..shown }
    }

    fn viewer_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    fn add_client(&self, tx: Sender<Message>) -> u64 {
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().unwrap().insert(
            id,
            Client {
                tx,
                ready: AtomicBool::new(false),
            },
        );
        id
    }

    fn remove_client(&self, id: u64) {
        self.clients.lock().unwrap().remove(&id);
    }

    fn to_client(&self, id: u64, bytes: Vec<u8>) {
        let clients = self.clients.lock().unwrap();
        if let Some(client) = clients.get(&id) {
            let _ = client.tx.try_send(Message::Binary(bytes.into()));
        }
    }

    fn to_client_json(&self, id: u64, value: &serde_json::Value) {
        let text = Utf8Bytes::from(value.to_string());
        let clients = self.clients.lock().unwrap();
        if let Some(client) = clients.get(&id) {
            let _ = client.tx.try_send(Message::Text(text));
        }
    }

    fn broadcast_binary(&self, bytes: Vec<u8>) {
        let clients = self.clients.lock().unwrap();
        for client in clients.values() {
            // A full queue means this browser is behind, and dropping the frame
            // is the point of the bound: the alternative is latency that grows.
            let _ = client.tx.try_send(Message::Binary(bytes.clone().into()));
        }
    }

    fn broadcast_json(&self, value: &serde_json::Value) {
        let text = Utf8Bytes::from(value.to_string());
        let clients = self.clients.lock().unwrap();
        for client in clients.values() {
            let _ = client.tx.try_send(Message::Text(text.clone()));
        }
    }

    /// What a browser is told the moment it connects.
    fn hello(&self, id: u64) -> serde_json::Value {
        let shown = self.shown_display();
        serde_json::json!({
            "type": "hello",
            "deviceName": &self.device_name,
            "sessionActive": self.session_active.load(Ordering::Relaxed),
            "width": shown.width,
            "height": shown.height,
            "viewers": self.viewer_count(),
            "message": serde_json::Value::Null,
            "viewer": id,
        })
    }

    /// Everything a viewer needs to start decoding: the greeting, the codec
    /// descriptions of every stream already running, and whether the phone is
    /// currently taking microphone audio.
    fn greet(&self, id: u64) {
        let hello = self.hello(id);
        self.to_client_json(id, &hello);
        let mic = mic_message(self.mic.current().as_ref());
        self.to_client_json(id, &mic);
        let canvas = self.canvas();
        for (stream, (codec, data)) in self.video_configs.lock().unwrap().iter() {
            self.to_client(
                id,
                video_config_frame(*stream, *codec, canvas.width, canvas.height, data),
            );
        }
        for frame in self.audio_configs.lock().unwrap().values() {
            self.to_client(id, frame.clone());
        }
        self.request_keyframe();
    }

    /// A viewer needs a keyframe to start from, and the phone only sends one
    /// when asked.
    fn request_keyframe(&self) {
        let input = self.input.clone();
        tokio::spawn(async move {
            if let Err(e) = input.dispatch(Command::KeyFrame).await {
                tracing::warn!("[web] keyframe request failed: {e}");
            }
        });
    }

    /// Drops the phone's connection so a new display takes effect. An error just
    /// means no session is connected, which is fine: the next one asks `/info`
    /// fresh and gets the new size without any help.
    fn restart_session(&self) {
        let _ = self.restart.send(());
    }

    fn set_session(&self, active: bool) {
        if self.session_active.swap(active, Ordering::Relaxed) == active {
            return;
        }
        self.broadcast_json(&serde_json::json!({
            "type": "session",
            "deviceName": &self.device_name,
            "sessionActive": active,
            "viewers": self.viewer_count(),
            "message": serde_json::Value::Null,
        }));
    }

    /// Turns one browser message into the HID reports and commands the phone
    /// already understands. A press that cannot be routed is dropped, exactly as
    /// an unpaired one is on the receiver side.
    async fn handle(self: &Arc<Self>, message: ClientMessage) {
        match message.kind.as_str() {
            "keyframe" => {
                self.request_keyframe();
                return;
            }
            // The browser's keep-alive; it needs no answer.
            "ping" => return,
            _ => {}
        }
        for command in self.translate(message) {
            if let Err(e) = self.input.dispatch(command).await {
                tracing::warn!("[web] input failed: {e}");
            }
        }
    }

    fn translate(&self, message: ClientMessage) -> Vec<Command> {
        match message.kind.as_str() {
            "touch" => {
                // Finger positions are normalised, so they land in the phone's
                // own pixel space — the canvas, which the UI scale can make a
                // different size from the one the panel displays.
                let canvas = self.canvas();
                let mut last = self.last_touch.lock().unwrap();
                // Every slot starts lifted, still where it was. A slot the
                // message does not name is one the browser has dropped, and
                // dropping a finger is how it says the finger came up.
                let mut contacts = *last;
                for contact in contacts.iter_mut() {
                    contact.down = false;
                }
                for contact in message.contacts.iter().take(TOUCH_SLOTS) {
                    let slot = contact.id as usize;
                    if slot >= TOUCH_SLOTS {
                        continue;
                    }
                    contacts[slot] = TouchContact {
                        down: contact.down,
                        x: (contact.x.clamp(0.0, 1.0) * canvas.width as f64) as u16,
                        y: (contact.y.clamp(0.0, 1.0) * canvas.height as f64) as u16,
                    };
                }
                *last = contacts;
                vec![Command::Touch(contacts)]
            }
            "media" => message
                .media
                .and_then(|index| match index {
                    1 => Some(Command::Play),
                    2 => Some(Command::Pause),
                    3 => Some(Command::PlayPause),
                    4 => Some(Command::Next),
                    5 => Some(Command::Prev),
                    _ => None,
                })
                .into_iter()
                .collect(),
            "knob" => {
                let Some(knob) = message.knob else {
                    return Vec::new();
                };
                // The browser sends a knob *state*; every one of these commands
                // is momentary, which is what the phone expects from a press.
                if knob.home {
                    return vec![Command::Home];
                }
                if knob.back {
                    return vec![Command::Back];
                }
                if knob.select {
                    return vec![Command::Select];
                }
                if knob.x < 0 {
                    return vec![Command::Left];
                }
                if knob.x > 0 {
                    return vec![Command::Right];
                }
                if knob.y < 0 {
                    return vec![Command::Up];
                }
                if knob.y > 0 {
                    return vec![Command::Down];
                }
                if knob.wheel != 0 {
                    return vec![Command::Wheel(knob.wheel.clamp(-127, 127) as i8)];
                }
                Vec::new()
            }
            "siri" => vec![Command::Siri],
            "night" => message.night.map(Command::Night).into_iter().collect(),
            // Straight through: the index is into the telephony HID report, and
            // the browser and this program share one descriptor for it.
            "telephony" => message
                .telephony
                .map(Command::Telephony)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Converts one stream event into the binary frame the browser parses.
///
/// This is the whole of the contract with `web/src/protocol.ts`, and it is
/// deliberately the only place that knows the layout.
impl MediaSink for Hub {
    fn push(&self, event: StreamEvent) {
        match event {
            StreamEvent::VideoConfig {
                codec, codec_data, ..
            } => {
                self.video_configs
                    .lock()
                    .unwrap()
                    .insert(SCREEN_STREAM, (codec, codec_data.clone()));
                // The canvas, not the panel size: the browser configures its
                // decoder with these, and a mismatch is a decode error.
                let canvas = self.canvas();
                self.broadcast_binary(video_config_frame(
                    SCREEN_STREAM,
                    codec,
                    canvas.width,
                    canvas.height,
                    &codec_data,
                ));
            }
            StreamEvent::VideoFrame { keyframe, data } => {
                let frame = video_frame_frame(SCREEN_STREAM, keyframe, &data);
                let clients = self.clients.lock().unwrap();
                for client in clients.values() {
                    if keyframe {
                        client.ready.store(true, Ordering::Relaxed);
                    }
                    if client.ready.load(Ordering::Relaxed) {
                        let _ = client.tx.try_send(Message::Binary(frame.clone().into()));
                    }
                }
            }
            StreamEvent::ScreenActive(active) => {
                if !active {
                    self.video_configs.lock().unwrap().clear();
                    for client in self.clients.lock().unwrap().values() {
                        client.ready.store(false, Ordering::Relaxed);
                    }
                }
                self.broadcast_binary(vec![SCREEN_ACTIVE, SCREEN_STREAM, u8::from(active)]);
                self.set_session(active);
            }
            StreamEvent::AudioConfig {
                stream_type,
                codec,
                name,
            } => {
                let description = match codec {
                    AudioCodec::AacLc { rate, channels } => {
                        audio_specific_config(rate, channels as u8)
                    }
                    _ => Vec::new(),
                };
                let frame = audio_config_frame(stream_type, codec, &name, &description);
                self.audio_configs
                    .lock()
                    .unwrap()
                    .insert(stream_type, frame.clone());
                self.broadcast_binary(frame);
            }
            StreamEvent::AudioPacket {
                stream_type,
                timestamp_us,
                data,
            } => self.broadcast_binary(audio_packet_frame(stream_type, timestamp_us, &data)),
            StreamEvent::AudioStopped { stream_type } => {
                self.audio_configs.lock().unwrap().remove(&stream_type);
                self.broadcast_binary(vec![AUDIO_STOPPED, stream_type]);
            }
        }
    }
}

/// The `mic` message: everything a browser needs to configure its capture, or
/// just the flag when it should stop. The names are the ones `mic.ts` reads.
fn mic_message(config: Option<&MicConfig>) -> serde_json::Value {
    match config {
        Some(config) => serde_json::json!({
            "type": "mic",
            "micActive": true,
            "micRate": config.sample_rate,
            "micChannels": config.channels,
            "micCodec": config.codec.name(),
            "micSamplesPerPacket": config.samples_per_packet,
            "micBitrate": config.bitrate,
        }),
        None => serde_json::json!({ "type": "mic", "micActive": false }),
    }
}

// --- HTTP ------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

struct ServerState {
    hub: Arc<Hub>,
    token: String,
    web_root: PathBuf,
}

/// Everything the browser talks to: the API, the socket, and the files.
pub fn router(hub: Arc<Hub>, token: String, web_root: PathBuf) -> Router {
    let state = Arc::new(ServerState {
        hub,
        token,
        web_root,
    });
    Router::new()
        .route("/api/status", get(api_status))
        .route("/api/display", get(api_display_get).post(api_display_post))
        .route("/ws", get(ws))
        .fallback(serve_file)
        .with_state(state)
}

async fn api_status(State(state): State<Arc<ServerState>>) -> Response {
    let shown = state.hub.shown_display();
    axum::Json(serde_json::json!({
        "deviceName": &state.hub.device_name,
        "sessionActive": state.hub.session_active.load(Ordering::Relaxed),
        "phone": serde_json::Value::Null,
        "viewers": state.hub.viewer_count(),
        "display": {
            "width": shown.width,
            "height": shown.height,
            "fps": shown.fps,
            "uiScale": shown.ui_scale(),
            "hevc": shown.hevc,
            "orientation": orientation(shown.width, shown.height),
        },
    }))
    .into_response()
}

fn orientation(width: u32, height: u32) -> &'static str {
    if height > width {
        "portrait"
    } else {
        "landscape"
    }
}

/// Landscape and portrait presets, as the Kotlin server offered them.
const PRESETS: [(u32, u32); 8] = [
    (1280, 720),
    (1920, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
    (800, 480),
    (720, 1280),
    (1080, 1920),
];

fn presets_json() -> serde_json::Value {
    serde_json::Value::Array(
        PRESETS
            .iter()
            .map(|(width, height)| {
                serde_json::json!({
                    "label": format!("{width} x {height}"),
                    "width": width,
                    "height": height,
                    "orientation": orientation(*width, *height),
                })
            })
            .collect(),
    )
}

async fn api_display_get(State(state): State<Arc<ServerState>>) -> Response {
    let shown = state.hub.shown_display();
    axum::Json(serde_json::json!({
        "width": shown.width,
        "height": shown.height,
        "fps": shown.fps,
        "uiScale": shown.ui_scale(),
        "hevc": shown.hevc,
        "orientation": orientation(shown.width, shown.height),
        "presets": presets_json(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct DisplayRequest {
    width: u32,
    height: u32,
    fps: u32,
    #[serde(rename = "uiScale", default = "default_ui_scale")]
    ui_scale: u32,
    #[serde(default)]
    hevc: bool,
}

fn default_ui_scale() -> u32 {
    100
}

/// Validates a display change, stores it, and restarts CarPlay.
///
/// The same rules the Kotlin server applied, and for the same reason: the phone
/// rejects an odd width or a refresh rate outside its own range.
///
/// Restarting is not optional: the phone reads `/info` once at the start of a
/// session and never asks again, so a new size only lands on the next one. The
/// cost is that the current session goes black until the phone reconnects — the
/// alternative is a panel whose settings do nothing at all.
async fn api_display_post(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<TokenQuery>,
    body: Option<axum::Json<DisplayRequest>>,
) -> Response {
    fn fail(status: StatusCode, error: &str) -> Response {
        (status, axum::Json(serde_json::json!({ "error": error }))).into_response()
    }
    if query.token.as_deref() != Some(state.token.as_str()) {
        return fail(StatusCode::FORBIDDEN, "invalid token");
    }
    let Some(axum::Json(request)) = body else {
        return fail(StatusCode::BAD_REQUEST, "malformed request");
    };
    if !(480..=3840).contains(&request.width) || !(480..=3840).contains(&request.height) {
        return fail(StatusCode::BAD_REQUEST, "width and height must be between 480 and 3840");
    }
    if request.width % 2 != 0 || request.height % 2 != 0 {
        return fail(StatusCode::BAD_REQUEST, "width and height must be even");
    }
    if !(24..=60).contains(&request.fps) {
        return fail(StatusCode::BAD_REQUEST, "fps must be between 24 and 60");
    }
    let params = ScreenParams {
        width: request.width,
        height: request.height,
        fps: request.fps,
        ui_scale: request.ui_scale,
        hevc: request.hevc,
    };
    if let Ok(mut slot) = state.hub.display.lock() {
        *slot = Some(params);
    }
    let (width, height) = params.canvas();
    tracing::info!(
        "[web] display set to {}x{} @{}fps (uiScale {} -> canvas {width}x{height}, {}); \
         restarting the session",
        params.width,
        params.height,
        params.fps,
        params.ui_scale(),
        if params.hevc { "HEVC" } else { "H.264" },
    );
    state.hub.restart_session();
    axum::Json(serde_json::json!({ "ok": true })).into_response()
}

async fn ws(
    upgrade: WebSocketUpgrade,
    Query(query): Query<TokenQuery>,
    State(state): State<Arc<ServerState>>,
) -> Response {
    if query.token.as_deref() != Some(state.token.as_str()) {
        return (StatusCode::FORBIDDEN, "invalid token").into_response();
    }
    upgrade.on_upgrade(move |socket| client(socket, state))
}

async fn client(socket: WebSocket, state: Arc<ServerState>) {
    let hub = state.hub.clone();
    let (tx, mut rx) = mpsc::channel::<Message>(SEND_QUEUE);
    let id = hub.add_client(tx);
    hub.greet(id);

    let (mut sink, mut stream) = socket.split();
    let writer = async {
        while let Some(message) = rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    };
    let reader = async {
        while let Some(Ok(message)) = stream.next().await {
            match message {
                Message::Text(text) => {
                    if let Ok(parsed) = serde_json::from_str::<ClientMessage>(text.as_str()) {
                        hub.handle(parsed).await;
                    }
                }
                // Binary frames only ever travel one way, and this is it:
                // captured microphone audio, encoded by the browser because it
                // is the only end that knows how to encode anything.
                Message::Binary(payload) => hub.mic_payload(&payload).await,
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    tokio::select! {
        _ = writer => {}
        _ = reader => {}
    }
    hub.remove_client(id);
}

/// The browser client itself, straight out of the submodule's build output.
///
/// Its asset URLs are relative (`./assets/…`), so anything that is not a real
/// file falls back to `index.html` and the app resolves from there.
async fn serve_file(State(state): State<Arc<ServerState>>, uri: axum::http::Uri) -> Response {
    let relative = uri.path().trim_start_matches('/');
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };
    let root = &state.web_root;
    let candidate = root.join(relative);
    let path = if candidate.starts_with(root) && candidate.is_file() {
        candidate
    } else {
        root.join("index.html")
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type(&path))], bytes).into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            "cp2aa: the browser client is not built. Run \
             `cd vendor/playport/web && npm install && npm run build`.",
        )
            .into_response(),
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

// --- serving ---------------------------------------------------------------

/// Where the browser client lives inside the playport submodule.
pub fn web_root() -> PathBuf {
    PathBuf::from("vendor/playport/web/dist")
}

/// Serves the browser client over TLS for the life of the process.
///
/// TLS is not optional: WebCodecs only exists in a secure context, and a LAN
/// address over plain HTTP is not one. The certificate is generated here, so the
/// first visit is a warning to click through.
pub async fn serve(hub: Arc<Hub>, address: SocketAddr, token: String) -> anyhow::Result<()> {
    // What the page is opened at is not always what we listen on: `0.0.0.0` is
    // every interface, and no browser can open that.
    let host = advertised_ip(address);
    let port = address.port();
    let app = router(hub, token.clone(), web_root());
    let certificate = self_signed(host).await?;

    let authority = match host {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    tracing::info!("[web] browser client: https://{authority}:{port}/?token={token}");
    tracing::info!("[web] accept the certificate warning once; WebCodecs needs HTTPS");

    axum_server::bind_rustls(address, certificate)
        .serve(app.into_make_service())
        .await
        .map_err(|e| anyhow::anyhow!("web server stopped: {e}"))
}

/// A certificate for this machine, made here so there is nothing to configure.
/// Self-signed, so the browser has to be told once that it is fine.
async fn self_signed(host: IpAddr) -> anyhow::Result<axum_server::tls_rustls::RustlsConfig> {
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])?;
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    if host != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        params.subject_alt_names.push(rcgen::SanType::IpAddress(host));
    }
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    let config = axum_server::tls_rustls::RustlsConfig::from_pem(
        cert.pem().into_bytes(),
        key.serialize_pem().into_bytes(),
    );
    Ok(config.await?)
}

/// The address to advertise: the one bound when it names an interface, and this
/// machine's LAN address when we listen on all of them.
fn advertised_ip(address: SocketAddr) -> IpAddr {
    if address.ip().is_unspecified() {
        local_ipv4()
    } else {
        address.ip()
    }
}

/// This machine's LAN address, as seen by the route the phone is told to use.
fn local_ipv4() -> IpAddr {
    let fallback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") else {
        return fallback;
    };
    // Nothing is sent; connecting a UDP socket only asks the kernel which local
    // address the route would use.
    if socket.connect("8.8.8.8:80").is_err() {
        return fallback;
    }
    socket.local_addr().map(|addr| addr.ip()).unwrap_or(fallback)
}
