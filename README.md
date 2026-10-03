# cp2aa

A headless CarPlay receiver: it presents itself to an iPhone as a wired CarPlay
head unit, and plays whatever the phone sends with two `ffplay` processes.

No window of our own, no UI, no Electron. The phone plugs into USB, its picture
and sound come out the other end directly, and you drive it from stdin.

```
iPhone ──USB──▶ this process ──▶ ffplay (video)
                             └─▶ ffplay (audio)
```

## What actually happens

Two unrelated conversations with the phone, in this order:

1. **iAP2 over USB.** Lockdown opens `com.apple.carkit.service`, we run
   identification, answer the MFi challenge with local credentials, and send
   `CarPlayStartSession` naming the port our second listener is on.
2. **AirPlay over TCP.** The phone calls back on that port, runs a pairing
   handshake, asks who we are, opens the video and audio streams and starts
   streaming. Every frame is sealed with ChaCha20-Poly1305.

Only step 1 uses LIVI. Step 2 is implemented here, because LIVI only has it in
TypeScript.

## Build and run

```bash
cargo build --release

export CP2AA_AUTH_DIR=./authentication         # identity.pk8 + certificate.p7b
export CP2AA_PLAYER=ffplay                     # optional; any ffmpeg-family player
./target/release/cp2aa
```

Then plug the phone in and unlock it. You should see:

```
[cp] 000081301A46 found over usb
[cp] identified
[cp] MFi authentication passed
[cp] CarPlayStartSession sent — the phone should knock on 52843
[airplay] 192.168.20.3:53412 connected
[airplay] pair-verify complete; shared secret ready
[airplay] answered GET /info
[airplay] video stream attached
```

### Viewing it

Two windows' worth of output appear on their own: ffplay opens one for the
screen, and plays the audio without a window (`-nodisp`).

The video window is opened at `CP2AA_WIDTH` × `CP2AA_HEIGHT` regardless of the
pixel count the phone renders, so it is the size you asked for rather than
whatever SDL picks from the backing store.

### Where the sharpness comes from — and where it stops

The one thing that measurably improved it was the **codec**: MacPlay logs
`SETUP screen ... codec=h265` while we were being handed H.264, and at the
bitrate the phone allocates H.265 holds far more detail. So `CP2AA_HEVC` is on by
default, and the player gets the matching demuxer (`-f hevc`) — an annex-B stream
carries no header saying which codec it is.

Beyond that the picture is softer than MacPlay, and that is a **player limit, not
a data problem**. `/info` was compared field by field against MacPlay and is
identical (features `0x615653aee2`, display features `0x0a`, empty `hevcInfo`,
same resolution, same physical size); the difference is that MacPlay renders
through GStreamer + OpenGL with `contentsScale=2.0`, while ffplay renders through
SDL, which scales bilinearly.

Things that were tried and do **not** help, so nobody repeats them:

* `-sws_flags lanczos` and `SDL_RENDER_SCALE_QUALITY=best` — the final scale is
  SDL's, and it ignores both.
* Asking the phone for more pixels — it keeps the bitrate, so twice the pixels
  means half the bits each and the picture gets *softer*.
* Inflating the reported dpi — MacPlay's own works out near 200 mm, the value we
  already default to; raising it only shrinks the UI.

The way past it is a different player (`CP2AA_PLAYER`), not different numbers.

The window is separate again, because SDL measures it in **points** on a HiDPI
display: it defaults to half the logical size, which is what puts one stream
pixel on each physical pixel.

```bash
export CP2AA_HEVC=0           # fall back to H.264
export CP2AA_WINDOW=1280x720  # if your display is not HiDPI
```

One process per stream, on purpose: audio and video open at different times and
the phone may open only one of them, so neither may be allowed to hold the other
up.

Feeding a player directly rather than an RTSP server also means there is no
muxer, no server and no network hop in the way when something does not show
up — the remaining suspects are only our decoding and the player itself.

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `CP2AA_PLAYER` | `ffplay` | player binary fed over stdin (`-f h264` / `-f s16be`) |
| `CP2AA_AUTH_DIR` | `./authentication` | MFi credentials |
| `CP2AA_WIDTH` / `CP2AA_HEIGHT` | `1280` / `720` | what the phone renders |
| `CP2AA_FPS` | `60` | frame rate上限 |
| `CP2AA_HEVC` | on | ask for H.265. At the bitrate the phone sends it is visibly sharper than H.264; set `0` to fall back |
| `CP2AA_WINDOW` | half the logical size | video window, `1280x720` form. SDL measures it in **points** on HiDPI, hence the default |
| `CP2AA_WIDTH_MM` / `CP2AA_HEIGHT_MM` | derived | physical size; drives UI scale |
| `CP2AA_HEVC` | unset | advertise HEVC |
| `CP2AA_FFMPEG` | `ffmpeg` | which ffmpeg |
| `CP2AA_AV_IFACE` | auto | USB network interface carrying media |
| `CP2AA_DEVICE_ID` / `CP2AA_BT_MAC` | `00:11:22:33:44:55` | identifiers in `/info` |
| `CP2AA_LOG` | `info` | tracing filter |

## Keyboard

One command per line, echoed on start:

| | |
|---|---|
| media | `play` `pause` `playpause` `next` `prev` `nav` |
| knob | `home` `back` `select` `left` `right` `up` `down` `wheel <n>` |
| phone | `answer` `hangup` `mute` `key 0`…`key 9` `star` `pound` `del` |
| session | `night [on\|off]` `siri` `keyframe` `quit` |

Touch is deliberately not implemented.

## Depending on LIVI

`Cargo.toml` points at a sibling checkout:

```toml
livi-runtime = { path = "../LIVI/native/livi-helperd/crates/livi-runtime" }
```

That is fine for now but will not survive sharing the project, because LIVI's
repository root is an Electron app with no `Cargo.toml` at the top, so a plain
`git = "..."` dependency cannot find the workspace. When you are ready:

1. fork LIVI,
2. either add it as a submodule and keep the `path` dependencies, or build a
   pure-Rust mirror repo containing only `native/livi-helperd` and depend on it
   by tag.

Note the licence: LIVI is **GPL-3.0-or-later**, and linking its crates makes
this a derivative work, so cp2aa inherits that.

## Layout

| File | Role |
|---|---|
| `src/session.rs` | the wired half — USB, iAP2, MFi, `CarPlayStartSession` |
| `src/airplay/mod.rs` | RTSP server: pairing, SETUP, `/info`, streams |
| `src/airplay/pairing.rs` | SRP-6a pair-setup, X25519/Ed25519 pair-verify |
| `src/airplay/crypto.rs` | HKDF-SHA512, ChaCha20-Poly1305, the two nonce shapes |
| `src/airplay/screen.rs` | video: sealed TCP frames → annex-B H.264 |
| `src/airplay/audio.rs` | audio: sealed UDP datagrams → PCM/AAC/Opus |
| `src/airplay/event.rs` | input: HID reports over the event channel |
| `src/sink.rs` | ffplay command lines, the WAV header, stdin feeding |
| `src/commands.rs` | stdin grammar |

## Expected behaviour, not bugs

* **Audio starts a few seconds after video.** The phone opens the screen stream
  first and the audio stream several seconds later; the player is already waiting
  on stdin, so playback is real time from the first sample onward.
* **The phone stops sending video when nothing moves.** A still screen means no
  frames — it is not a stall. Press a key (`home`, `select`, …) and the frame
  count picks up again.

## Known gaps

* Audio codec is announced as PCM and then guessed, so if the phone picks Opus or
  AAC the player needs the matching `-f`. The negotiated codec is in the
  stream-level SETUP and should replace the guess.
* Our clock is not steered onto the phone's. The offset is measured and printed
  but not applied, so `/feedback` reports our own time; wired sessions survive
  this, but it is not right.
* `ffplay` on a pipe is deliberately run with no tuning at all (see `sink.rs`).
  Latency options can be added back one at a time once there is a picture to
  judge them against.
* The picture is softer than MacPlay's because SDL scales bilinearly. Switching
  `CP2AA_PLAYER` to a player with HiDPI-aware, high-quality scaling (mpv) is the
  fix, and would need its own argument set (`--geometry` rather than `-x`/`-y`).
* Single phone, no reconnect state machine beyond "is it still plugged in".
* `/auth-setup` needs the same MFi credentials the iAP2 side uses. With
  `NoCoprocessor` it fails and the phone hangs up right after pair-verify.
* The controller's long-term key from pair-setup is not stored, so pair-verify
  accepts any controller that can open a valid session.
