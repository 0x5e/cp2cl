# [WIP] cp2cl

CarPlay to CarLife bridge service (cp2cl), running on a Linux device.

## Background

The head unit this is built for is Mercedes-Benz's **MBUX NG6**. It takes three
phone-projection protocols over the cable, and it does not treat them alike:

| Protocol | Display |
|---|---|
| CarPlay | a window, not the full panel |
| CarLife | the full panel |
| Android Auto | a window, not the full panel |

Only CarLife draws full-bleed, so CarLife is the target: the bridge takes the
CarPlay session an iPhone offers and hands it to the head unit over the one
protocol it will render full screen.

## How it works

```
                        USB                                       USB
  iPhone  --------------------------->  Bridge device  ------------------------->  CarLife head unit
 (CarPlay: the phone is                  (this program)         (CarLife: the head unit
  a USB device)                                                 is the USB host)
                                                |
                                                |  HTTPS + WebSocket
                                                +------------------------------->  browser client
```

The CarPlay half and the browser client exist and work. The CarLife half — this
device presenting itself to a head unit as a phone — is written as far as its
protocol: the USB interface, the AOA control requests, and the session a head
unit drives. Which of the two reads the streams is a choice, not both at once;
with the car as the reader the video channel carries CarPlay's frames, and the
audio channels stay empty because what the car takes and what the phone sends are
different codecs.

## Features

- Support both wired and wireless CarPlay.
- Video is passed through untouched.
- No hard-coded audio/video encoding formats — what the phone offers is
  negotiated with it directly, and the far end's request is answered with what
  this device can actually do. Where the two ends do not agree, the audio is
  decoded rather than re-encoded: wireless CarPlay hands media over as AAC-LC and
  CarLife's media channel takes PCM. That decoder is not written.
- Present the device to a CarLife head unit as a phone, so the car can be driven at all.

## Requirements

Bridge device:

- Running Linux. The board this is developed against is a **NanoPi Duo2**,
  armv7 hard-float.
- 1 USB OTG for the CarLife head unit, which is the host on that side. With
  CarPlay taken wirelessly this is the only USB port the device needs.
- 1 USB host for wired CarPlay, on a board that has a second controller.
- WiFi + BLE for wireless CarPlay (optional).

## Build and run

```bash
git submodule update --init --recursive

# The browser client is PlayPort's, and is not committed — build it once.
cd vendor/playport/web && npm install && npm run build && cd -

cargo build --release

export CP2CL_AUTH_DIR=./authentication         # identity.pk8 + certificate.p7b
./target/release/cp2cl
```

It prints an address per interface, since which of them the browser is on the
other side of depends on where you are. All of them carry the same token:

```
[cp2cl] airplay receiver listening on 52843
[web] browser client: https://127.0.0.1:8080/?token=0
[web] browser client on wlan0: https://192.168.4.1:8080/?token=0
[web] browser client on usb0: https://172.16.0.1:8080/?token=0
[web] accept the certificate warning once; WebCodecs needs HTTPS
```

Run it from the repository root: the browser client is served out of
`vendor/playport/web/dist`, relative to the working directory. That is the build
for the machine you are on; the board is built in a container instead, since it
cannot build this itself — see [Cross-compiling for the
board](#cross-compiling-for-the-board).

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

### Cross-compiling for the board

The board is a **NanoPi Duo2** — armv7 hard-float, Ubuntu 24.04, glibc 2.39. It
cannot build this crate itself, and macOS has no armv7 toolchain, so the build
runs in the container defined by `docker/`. Build the image once:

```bash
docker build --platform linux/arm64 -t cp2cl-armv7-builder docker/
```

Then, from the repository root:

```bash
docker run --rm --platform linux/arm64 -u "$(id -u):$(id -g)" \
  -v "$PWD":/work -v cp2cl-cargo:/cargo \
  cp2cl-armv7-builder cargo build --release --target armv7-unknown-linux-gnueabihf
```

`cp2cl-cargo` is a named volume holding the cargo cache, so only the first build
downloads anything. `-u` keeps the build products owned by you rather than by
root. Use `cargo check` in place of `cargo build --release` to type-check the
Linux-only `#[cfg]` branches — the only way to reach them from macOS.

The result is `target/armv7-unknown-linux-gnueabihf/release/cp2cl`. It is not
self-contained: `vendor/playport/web/dist` and `authentication/` are read
relative to the working directory, so copy the tree, not just the file.

### Why HTTPS, and what to do about the certificate

WebCodecs — the browser's hardware decoder — only exists in a **secure context**.
A LAN address over plain HTTP is not one, so the page would load and then refuse
to decode anything. So the server is TLS, with a certificate it generates for
this machine at startup.

That certificate is self-signed by us, so the browser does not know it. Click
through the warning once and the page works. The certificate is regenerated on
every start, so nothing has to be stored or renewed.

The server listens wherever `CP2CL_WEB_ADDR` says — `0.0.0.0:8080` by default.
Every address it prints is named in the certificate, so any of them works once
the warning has been accepted: the loopback one on the machine itself, the rest
from other devices.

### The token

The page carries one in the URL, hands it back on the WebSocket and when
changing the display, and anything without it is refused. It is fixed —
`0` unless `CP2CL_WEB_TOKEN` says otherwise — so the printed URL,
and any bookmark of it, works across restarts. The front end will not connect
without a token, so there is no token-less mode to fall back to. This is a debug
surface on a LAN, not something to expose: a fixed token is a convenience, not a
substitute for keeping the port off the network at large.

### What the browser is doing

Three things, all in PlayPort's own front end — which is used unmodified, so the
backend is written to it rather than the other way round:

* **One WebSocket, two protocols.** JSON control messages down (`hello`,
  `session`, `mic`) and up (`touch`, `knob`, `media`, `siri`, `night`,
  `telephony`, `keyframe`, `ping`); binary frames down for the media itself,
  little-endian with one type byte up front, and binary frames up for captured
  microphone audio.
* **`GET /api/status` and `GET /api/display`.** Who we are, whether a session is
  up, and what the display panel should show.
* **`POST /api/display`.** A new size, frame rate, UI scale and codec — see the
  display panel below.

### The display panel

Changing anything there restarts CarPlay. That is not a shortcut: the phone reads
`/info` exactly once, at the start of a session, and never asks again, so a new
size only reaches it on the next one.

Two things have to happen, and closing the connection is only the first:

1. the AirPlay session is dropped, so the phone cannot keep rendering the old
   size, and
2. the iAP2 session is dropped with it, and a new one is started in its place.
   The phone only revisits `/info` when it sees CarPlay end, and CarPlay ends
   when the iAP2 session goes away — so the whole session is torn down and built
   again. This is what unplugging and replugging does, minus the cable.

**Sending the start again over the live session does not work.** `CarPlayStartSession`
is the accessory's *answer* to the phone's own `CarPlayAvailability`, and a phone
that has already started CarPlay has no question outstanding — it ignores a
second start, which is exactly what the logs showed. (LIVI's `start_again` sends
one, but for a different reason: it holds the answer back until the MFi chip is
ready, still inside the phone's original request.)

Tearing the link down is more than dropping a handle. LIVI's link driver splits
the stream it is given and parks the read half in a task of its own, so a read
has to be made to return before the socket actually closes —
`src/session.rs::Latched` is that, and it is why the wired path wraps its carkit
stream in it. On macOS wireless the same wrapper is around the bridge's unix
socket, which is what makes the helper close the RFCOMM channel under the phone.

The phone is left alone for 750ms between the two sessions, because it has to
notice the first one ending before it will act on the second.

| Log line | What it means |
|---|---|
| `[cp] ... is being told to start over; reopening carkit` | wired: the old tunnel is closed and another is being opened |
| `[cp-wifi] ... is being told to start over; dropping the Bluetooth link` | wireless: the link is closed; the platform has to bring the phone back |
| `cp2cl: reopening the iAP2 channel after the receiver restarted` | the macOS bridge noticed and is asking for the channel again |

The wireless half depends on something outside this program bringing the phone
back: on macOS the bridge polls the paired devices and opens the channel itself,
and on Linux BlueZ's `AutoConnect` pages it. **Wired has been confirmed on
hardware; wireless has not.**

Asking for **more pixels and expecting a sharper picture does not work**: the
phone keeps the bitrate, so twice the pixels means half the bits each and the
picture gets *softer*. What measurably helps is the codec — H.265 at the same
bitrate holds visibly more detail than H.264, which is why it is the default.

`uiScale` is PlayPort's UI-size control, and it works by asking the phone for a
larger canvas (`100/percent` of the size) and letting the browser fit it back to
the same surface — so a bigger canvas draws proportionally *smaller* CarPlay
controls. Touch is mapped through the canvas, not the panel size, for the same
reason.

The codec choice is real, and matters: it is what the browser has to be able to
decode. `CP2CL_HEVC` sets the default, and the panel overrides it per session.

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `CP2CL_AUTH_DIR` | `./authentication` | MFi credentials |
| `CP2CL_WEB_ADDR` | `0.0.0.0:8080` | where the browser client is served. HTTPS is not part of it because there is no other option — WebCodecs needs a secure context — and `0.0.0.0` is every interface |
| `CP2CL_WEB_TOKEN` | `0` | the token the page has to carry; fixed, so the URL is stable and short enough to type. Give it a real value before the port is reachable from anywhere but this LAN |
| `CP2CL_WIDTH` / `CP2CL_HEIGHT` | `1280` / `720` | what the phone renders, until the display panel says otherwise |
| `CP2CL_FPS` | `60` | max frame rate |
| `CP2CL_HEVC` | on | ask for H.265. At the bitrate the phone sends it is visibly sharper than H.264; set `0` to fall back |
| `CP2CL_WIDTH_MM` / `CP2CL_HEIGHT_MM` | derived | physical size; drives UI scale |
| `CP2CL_DEVICE_ID` / `CP2CL_BT_MAC` | `00:11:22:33:44:55` | identifiers in `/info` |
| `CP2CL_LOG` | `info` | log level: `error`, `warn`, `info`, `debug`, `trace` |
| `CP2CL_WIRELESS` | off | take CarPlay over Bluetooth and Wi-Fi instead of USB |
| `CP2CL_WIFI_PASS` | — | the passphrase of the network the phone is told to join. Required once wireless is on: it is the one value no reading can supply |
| `CP2CL_WIFI_SSID` | — | the same network, for when no access point is on air. An interface that is beaconing answers this itself |
| `CP2CL_WIFI_IFACE` | `en0` on macOS, `wlan0` elsewhere | the interface facing that network; its IPv6 link-local address is what the phone connects back to, and on Linux it is the one asked about an access point |
| `CP2CL_WIFI_CHANNEL` | `36` | channel reported to the phone, for when no access point is on air |
| `CP2CL_AP_MAC` | this machine's Wi-Fi MAC | the router's BSSID, sent as the accessory identifier |
| `CP2CL_TARGET_BT` | any | only accept this phone's Bluetooth address |
| `CP2CL_BT_ADAPTER` | `hci0` | Linux: which Bluetooth adapter |
| `CP2CL_SINK` | `web` | who reads the streams: `web` is the browser client, `cl` is a CarLife head unit over the micro-USB port, and there is no browser client in that mode. See [CarLife](#carlife) |
| `CP2CL_INPUT` | `cp` | what the CarLife video channel sends: `cp` is the CarPlay session, `file` is the file below, sent over and over |
| `CP2CL_CARLIFE_FRAME` | — | that file: an annex-B stream. A name that cannot be read falls back to heartbeats |
| `CP2CL_CARLIFE_VIDPID` | `18d1:2d00` | the identity the gadget is lent; `none` leaves the gadget's own alone |
| `CP2CL_CARLIFE_GADGET` / `CP2CL_CARLIFE_UDC` | the only one there is | which gadget and which controller to use, when the board has more than one |
| `CP2CL_CARLIFE_FFS` | `carlife` | the FunctionFS instance name, which is also where it is mounted under `/dev/usb-ffs` |

## Wireless CarPlay

`CP2CL_WIRELESS=1` moves the handshake from the cable to Bluetooth. The phone is
paired in the operating system first — this program pairs nothing — and then
finds the iAP2 service we publish. Bluetooth carries only that beginning: the
moment we answer `CarPlayStartSession` with a Wi-Fi network and our link-local
address, the phone hangs up and comes back over Wi-Fi, where the same airplay
receiver takes over. Nothing about the media changes.

What it needs:

* the phone and this machine paired in the OS, on the same Wi-Fi — one that lets
  devices reach each other, so not a guest network with client isolation
* `CP2CL_WIFI_PASS` for that network — the passphrase, which no reading can
  supply. WPA2 personal only: the message has nowhere to put an 802.1X username,
  so enterprise networks are refused up front rather than failing at the phone
* `CP2CL_WIFI_SSID` and `CP2CL_WIFI_CHANNEL` only when nothing is beaconing:
  the interface is asked what it is on air with, and that answer is what the
  phone is told. macOS cannot make that reading, so the two variables are
  always the answer there
* macOS: the Bluetooth bridge is compiled by `build.rs` with clang, and the first
  run asks for Bluetooth permission

| | How the phone reaches us |
|---|---|
| Linux | `livi_runtime::bt` registers the iAP and CarPlay profiles with BlueZ and hands us the phone's RFCOMM socket. None of that is ours — it is LIVI's code called directly |
| macOS | a small IOBluetooth helper, `native/cp2cl-bluetooth/main.m`, publishes the two SDP records, advertises the controller as in-car audio/video, polls the paired iPhones, and relays RFCOMM bytes to us over a unix socket. CoreBluetooth cannot do any of this: CarPlay's wireless handshake is SDP plus RFCOMM, not GATT |

Two things worth knowing if it does not connect. The device class matters — the
helper advertises `0x200408` for 120 seconds at a time and renews it, because
without it the phone does not treat the machine as a car. And a channel that
opens but never carries anything usually means the phone's service records are
stale, which the bridge recovers from by asking again.

`CP2CL_TARGET_BT` narrows it to one phone when several are paired, and
`CP2CL_LOG=debug` brings up the bridge's own lines.

## CarLife

`CP2CL_SINK=cl` makes the head unit the reader instead of the browser client.
The device presents itself to the car as a phone over the micro-USB port, and the
two halves become one thing: the CarPlay side is held shut until the car's session
is up, and is torn down when the car's session ends. Being the reader is what
turns the CarLife half on, so there is no second switch for it.

There is no browser client in this mode, which means no page to open and no
display panel: what the phone is asked to render comes from `CP2CL_WIDTH`,
`CP2CL_HEIGHT` and `CP2CL_FPS`.

What is there: the USB interface the car enumerates, the AOA control requests,
the session a head unit drives — the version handshake, device information,
feature configuration, the video encoder's negotiation, and the heartbeat that
keeps the channel alive — and the video channel itself, carrying CarPlay's frames
from the moment the car starts the encoder. What is not: the audio channels, which
want PCM and are handed AAC-LC and are therefore dropped; and input, which the car
sends and this program reports but does not act on.

### Trying it without a car

`../CarLifePort` is a head unit implementation that runs on macOS, and the
topology matches the real thing — the Mac is the host, the board is the device —
so everything up to the picture can be exercised without one:

```bash
# On the board. Wireless CarPlay, because the micro-USB port belongs to CarLife.
CP2CL_SINK=cl CP2CL_WIRELESS=1 \
CP2CL_WIFI_SSID=<the network> CP2CL_WIFI_PASS=<its passphrase> \
./target/release/cp2cl
```

The video channel then carries the phone's frames, which needs both a phone and a
car session. To drive the channel with no CarPlay in the picture at all, point it
at a file instead:

```bash
CP2CL_SINK=cl CP2CL_INPUT=file CP2CL_CARLIFE_FRAME=/root/idr.h264 ./target/release/cp2cl
```

Any annex-B stream will do:

```bash
ffmpeg -f lavfi -i testsrc=size=1280x720:rate=30 -frames:v 1 -c:v libx264 \
  -profile:v baseline -x264-params keyint=1 -f h264 idr.h264
```

That one access unit is then sent over and over, which is not a picture a person
would watch but is enough to prove the channel and the car's decoder end to end.
With no file to read, the video channel carries heartbeats only, which still shows
the session coming up and the encoder negotiation happening.

What to watch for, in the order it happens. The first line that does not appear
is where it stopped:

```bash
[cl-usb] the gadget is up; waiting for the car to enumerate us
[cl-usb] control request 51 in, type 0xc0, value 0, index 0, length 2
[cl-usb] answered with AOA 2
[cl-usb] the car has enumerated us; starting the session
[cl] the car speaks CarLife 1.0
[cl] the car has our device information; the session is up   # the gate opens here
[cl] the car set VOICE_MIC = 0                               # whatever it asks for
[cl] the car asked for 1280x720 at 30 fps; agreeing to 1280x720 at 30
[cl] the car started the encoder; video begins
```

`[cl]` is the session, `[cl-usb]` is the USB side under it. The other half of the
bridge says so too, and its two lines bracket the one above:

```bash
[airplay] holding: the CarLife session is not up   # until "the session is up"
[airplay] <phone> connected                        # and only after it
```

If the car goes quiet and nothing from `[cl]` follows, the last line printed says
what it was waiting on. A stop at `the car asked us to authenticate` is the next
section's subject.

### The gadget it borrows

This device does not own the USB gadget: the board already has one, made at boot
to provide the ECM link the board is normally reached over. CarLife adds a
FunctionFS function to that gadget and binds it again, which means two things
worth knowing before the first run:

* **The management link drops while the gadget is unbound.** It comes back with
  the function added, and everything is undone on the way out — but a run that
  ends badly leaves the gadget changed. configfs lives in memory, so a reboot is
  a clean slate.
* **The gadget's identity is replaced** with the accessory one the guide expects,
  `18d1:2d00`. A host that keys its network settings off the USB identity may
  treat the link as a new device and not reapply them, which on macOS could mean
  losing the management link. `CP2CL_CARLIFE_VIDPID=none` leaves the identity
  alone; that is the setting to try if it happens.

### Authentication

The car may ask this device to prove it is a phone, by presenting a challenge
that only an actual phone can encrypt. That key lives on the phone this device is
pretending to be, so it is not obtainable, and the request is reported and left
unanswered — there is nothing honest to reply with. An invented certificate or a
guessed constant would be a lie the car might act on, which is worse than the car
refusing us. What the car makes of the silence is the car's decision, and finding
out is what the hardware is for.

## Input

Input goes through the browser client, which is the control surface: the
bindings below are the ones that count. With `CP2CL_SINK=cl` there is no browser
client, so there is no input panel at all — the car's own touch and panel keys
arrive on the CarLife session and are reported rather than forwarded.

| | In the browser |
|---|---|
| touch | anywhere on the picture, two fingers at once |
| arrows | the knob's absolute X/Y axes |
| `Enter` | select |
| `Escape` / `Backspace` | back |
| `Home` | home |
| `s` | Siri |
| `Space` | play/pause |
| `MediaTrackNext` / `MediaTrackPrevious` | next / previous |

Buttons for the call keys and for day/night sit next to those. Day/night and the
call keys go out as session commands rather than HID reports; everything else is
a HID report on one of the four devices `/info` declares — touchscreen, knob,
media and telephony.

A press is always two reports, the press and an all-zero one. The phone tracks a
button by whether it is still down, so a key that is never released does not
register as a press at all; the same is true of the touchscreen, where a lifted
finger has to leave its slot behind rather than disappear from the list, and at
the position it was last seen at. The browser drops a finger the moment it lifts,
so a slot a message does not name is carried over from the last report with only
its touch bit cleared. Naming the origin instead reads as the finger jumping
there, and the phone cancels a press that moves off the control before it lifts.

## The microphone

CarPlay has no microphone stream. The phone asks for one by naming a *send* port
in the SETUP of its main audio stream and labelling that stream `telephony` or
`speechRecognition`; the browser then captures, encodes, and sends the payloads
back up the same WebSocket, and this process seals them and puts them on that
port.

The codec follows the transport rather than being chosen: Opus over Wi-Fi, PCM
over the cable, at the rate and channel count the phone's own audio stream
arrived at.

It is live only while the phone is listening — the browser is told when to start
and stop, because holding a microphone open for nobody is a light that never
goes out. Which means the way to test it is to start a call or invoke Siri.

## Depending on LIVI

LIVI and the browser client are git submodules under `vendor/`, pinned to
exact commits:

```bash
git submodule update --init --recursive
```

| Submodule | Upstream | Pinned to |
|---|---|---|
| `vendor/LIVI` | `0x5e/LIVI` | `feature/local-credential` |
| `vendor/playport` | `youcci/playport` | `main` |

`Cargo.toml` takes the LIVI crates by path from the submodule:

```toml
livi-runtime = { path = "vendor/LIVI/native/livi-helperd/crates/livi-runtime" }
```

LIVI's repository root is an Electron app with no top-level `Cargo.toml`; its
Rust lives under `native/`, which is why every path spells that out.

Because the submodule is pinned rather than tracking a branch, nothing moves on
its own — `git submodule update --remote vendor/LIVI` and
`git submodule update --remote vendor/playport` are the deliberate, manual steps
that advance them.

Note the licence: LIVI is **GPL-3.0-or-later**, and linking its crates makes
this a derivative work, so cp2cl inherits that.

## Layout

| File | Role |
|---|---|
| `src/session.rs` | the wired half — USB, iAP2, MFi, `CarPlayStartSession` |
| `src/wireless/mod.rs` | the wireless half of the same: iAP2 over Bluetooth, and the Wi-Fi answers |
| `src/wireless/linux.rs` | BlueZ, through LIVI's `livi_runtime::bt` |
| `src/wireless/macos.rs` | the unix socket the IOBluetooth helper relays into |
| `native/cp2cl-bluetooth/main.m` | macOS: SDP records, device class, RFCOMM → unix socket |
| `src/carlife/mod.rs` | the CarLife half: this device standing in for a phone |
| `src/carlife/gadget.rs` | Linux only — configfs, FunctionFS, the accessory control requests |
| `src/carlife/session.rs` | the session a head unit drives: handshake, heartbeats, touch |
| `src/carlife/frame.rs` | the USB framing — the outer chunk header and the per-channel message head |
| `src/carlife/proto.rs` | as much protobuf as these messages need, written by hand |
| `src/airplay/mod.rs` | RTSP server: pairing, SETUP, `/info`, streams |
| `src/airplay/pairing.rs` | SRP-6a pair-setup, X25519/Ed25519 pair-verify, every key HKDF derives |
| `src/airplay/crypto.rs` | HKDF-SHA512, ChaCha20-Poly1305, the two nonce shapes |
| `src/airplay/screen.rs` | video: sealed TCP frames → annex-B H.264/H.265 |
| `src/airplay/audio.rs` | audio: sealed UDP datagrams → PCM/AAC/Opus |
| `src/airplay/event.rs` | input: HID reports and session commands, over the event channel |
| `src/airplay/mic.rs` | the microphone uplink, sealed and sent back to the phone |
| `src/sink.rs` | what a stream *is* — codec and description as well as bytes |
| `src/commands.rs` | the vocabulary of input the browser client produces |
| `src/web/mod.rs` | the browser client's server: the wire framing, the API, the WebSocket, TLS |
| `docker/` | the image that cross-compiles for the board, and the CMake toolchain it needs |

## Expected behaviour, not bugs

* **Audio starts a few seconds after video.** The phone opens the screen stream
  first and the audio stream several seconds later.
* **The phone stops sending video when nothing moves.** A still screen means no
  frames — it is not a stall. Touch the picture and it picks up again.
* **The first picture waits for a keyframe.** A viewer that joins mid-stream has
  nothing for the delta frames to refer back to, and the phone only sends a
  keyframe when asked. One is requested on connect, so the picture appears within
  a frame or two rather than immediately.
* **Media keys do nothing until something has played.** With nothing playing,
  iOS has no now-playing app for them to reach. Play something once and they work
  for the rest of the session.
* **A display change blanks the screen for a moment.** See the display panel
  above: the session has to be rebuilt for the new size to reach the phone.
* **Wireless audio is re-timed on the way out.** A *buffered* media stream does
  not arrive packet by packet: the phone hands it over in lumps — measured at a
  steady 18 packets, 384 ms of audio, with nothing in between — which is more
  than the client's own 120–350 ms playback buffer holds, so it reschedules in
  the middle of a lump and the sound stutters. Wired sessions never see this,
  because the phone sends PCM packet by packet there. The audio pump therefore
  releases the buffered stream one frame per 21.33 ms, and prints how it is
  going every five seconds:
  `[airplay] audio: N packets, M missing, worst gap X ms, B bursts of LO-HI (avg A), N held`.
  A rising `missing` is loss; a large `worst gap` is the phone's lumping and is
  expected; `held` is how much audio is waiting to go out.

## Known gaps

* The CarLife video channel carries the phone's frames, but the audio channels
  stay empty, and what the car sends back is logged and dropped. So the head unit
  gets a picture and no sound, and touches it in vain.
* Whether a head unit accepts a device that is not a phone is not known — the
  authentication challenge cannot be answered, and no hardware has been tried on
  yet. See `PLAN-CL.md` §3.5.
* The CarLife audio cannot be passed through: a phone on wireless CarPlay hands
  media over as AAC-LC, which the CarLife media channel does not take, so it has
  to be decoded on the way across. That decoder is not written, and whether the
  board can do it in real time is not measured.
* `up`/`down` overshoot. The knob's absolute Y axis is a *position* to the phone
  rather than a step, so a full throw scrolls several rows. Wired it is reliable,
  wireless it is intermittent. The knob's wheel axis cannot stand in for it: that
  one follows the UI's own scroll direction, which is horizontal on the app grid.
  The remaining knob is the throw itself — `AXIS_DEFLECT` in `event.rs` — and it
  has not been measured.
* Our clock is not steered onto the phone's. The offset is measured and printed
  but not applied, so `/feedback` reports our own time; wired sessions survive
  this, but it is not right.
* Single phone, no reconnect state machine beyond "is it still plugged in".
* `/auth-setup` needs the same MFi credentials the iAP2 side uses. With
  `NoCoprocessor` it fails and the phone hangs up right after pair-verify.
* The controller's long-term key from pair-setup is not stored, so pair-verify
  accepts any controller that can open a valid session.
* The event channel's writer is held behind one lock and written without a
  timeout, so a connection that has gone quiet can stall input until TCP gives
  up. The same lock is what the next session has to take to attach its writer.

### Credits and license

cp2cl is free software, licensed under the GNU General Public License v3.0 or later (GPL-3.0-or-later).

- [LIVI](https://github.com/f-io/LIVI)
- [MacPlay](https://github.com/Roylyl/MacPlay)
- [PlayPort](https://github.com/youcci/playport)
