# cp2aa

A headless CarPlay receiver: it presents itself to an iPhone as a wired CarPlay
head unit, and hands whatever the phone sends to PlayPort's browser client,
which decodes it with WebCodecs.

No window of our own, no UI, no Electron. The phone plugs into USB, you open one
page, and you drive CarPlay from it — pointer, keyboard and all.

The other half of the job — handing that picture on to an Android Auto head
unit, which means being an Android phone — is under construction in
`src/aa-client/`, see the end of this file.

```
iPhone ──USB──▶ this process ──HTTPS/WSS──▶ browser client
                                        └─▶ (or an Android Auto head unit)
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

Nothing between them decodes anything: the sealed frames are opened and handed
on as they are. The browser is the decoder, and so is the head unit.

## Build and run

```bash
git submodule update --init --recursive

# The browser client is PlayPort's, and is not committed — build it once.
cd vendor/playport/web && npm install && npm run build && cd -

cargo build --release

export CP2AA_AUTH_DIR=./authentication         # identity.pk8 + certificate.p7b
./target/release/cp2aa
```

It prints the page to open. The token is fixed, so it is the same URL every run:

```
[cp2aa] airplay receiver listening on 52843
[web] browser client: https://192.168.40.2:8080/?token=00000000000000000000000000000000
[web] accept the certificate warning once; WebCodecs needs HTTPS
```

Run it from the repository root: the browser client is served out of
`vendor/playport/web/dist`, relative to the working directory.

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

The board is aarch64 Alpine — musl, not glibc — and building for it from macOS
does not work: `aws-lc-sys` does not get through its cmake step. Build inside a
container that is already that platform instead, which turns it back into an
ordinary native build:

```bash
docker run --rm --platform linux/arm64 -v "$PWD":/work -w /work \
  -e CARGO_TARGET_DIR=/work/target/aarch64-unknown-linux-musl \
  rust:1-alpine sh -c 'apk add build-base cmake perl linux-headers pkgconf git; cargo build --release'
```

There is no `--target` to pass, because the container's architecture is the one
we want; the directory is named after that triple only so it sits beside the
ones cargo files by itself. The result is
`target/aarch64-unknown-linux-musl/release/cp2aa` — about 17 MB, static, linked
against nothing.

Every platform therefore gets a directory of its own under `target/`: a native
build is `target/release/`, where cargo puts it anyway, and anything
cross-compiled is filed under the triple it was built for. They cannot overwrite
each other, and the single `target/` line in `.gitignore` covers all of them.

The binary is not self-contained: `vendor/playport/web/dist` and
`authentication/` are read relative to the working directory, so copy the tree,
not just the file.

### Why HTTPS, and what to do about the certificate

WebCodecs — the browser's hardware decoder — only exists in a **secure context**.
A LAN address over plain HTTP is not one, so the page would load and then refuse
to decode anything. So the server is TLS, with a certificate it generates for
this machine at startup.

That certificate is self-signed by us, so the browser does not know it. Click
through the warning once and the page works. The certificate is regenerated on
every start, so nothing has to be stored or renewed.

The server listens wherever `CP2AA_WEB_ADDR` says — `0.0.0.0:8080` by default —
and the address printed is this machine's LAN address as the route to the phone
sees it, which is the one to use from another device.
`https://localhost:8080` works too, on the machine itself.

### The token

The page carries one in the URL, hands it back on the WebSocket and when
changing the display, and anything without it is refused. It is fixed —
sixteen zero bytes unless `CP2AA_WEB_TOKEN` says otherwise — so the printed URL,
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
| `cp2aa: reopening the iAP2 channel after the receiver restarted` | the macOS bridge noticed and is asking for the channel again |

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
decode. `CP2AA_HEVC` sets the default, and the panel overrides it per session.

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `CP2AA_AUTH_DIR` | `./authentication` | MFi credentials |
| `CP2AA_SINK` | `web` | where the streams go: `web` serves the browser client, `aa` sends them to a connected Android Auto head unit |
| `CP2AA_WEB_ADDR` | `0.0.0.0:8080` | where the browser client is served. HTTPS is not part of it because there is no other option — WebCodecs needs a secure context — and `0.0.0.0` is every interface |
| `CP2AA_WEB_TOKEN` | sixteen zero bytes | the token the page has to carry; fixed, so the URL is stable. Set something shorter if typing it in is annoying |
| `CP2AA_WIDTH` / `CP2AA_HEIGHT` | `1280` / `720` | what the phone renders, until the display panel says otherwise |
| `CP2AA_FPS` | `60` | frame rate上限 |
| `CP2AA_HEVC` | on | ask for H.265. At the bitrate the phone sends it is visibly sharper than H.264; set `0` to fall back |
| `CP2AA_WIDTH_MM` / `CP2AA_HEIGHT_MM` | derived | physical size; drives UI scale |
| `CP2AA_DEVICE_ID` / `CP2AA_BT_MAC` | `00:11:22:33:44:55` | identifiers in `/info` |
| `CP2AA_LOG` | `info` | log level: `error`, `warn`, `info`, `debug`, `trace` |
| `CP2AA_WIRELESS` | off | take CarPlay over Bluetooth and Wi-Fi instead of USB |
| `CP2AA_WIFI_SSID` / `CP2AA_WIFI_PASS` | — | the network the phone is told to join. Both are required once wireless is on |
| `CP2AA_WIFI_IFACE` | `en0` on macOS, `wlan0` elsewhere | the interface facing that network; its IPv6 link-local address is what the phone connects back to |
| `CP2AA_WIFI_CHANNEL` | `36` | channel reported to the phone |
| `CP2AA_AP_MAC` | this machine's Wi-Fi MAC | the router's BSSID, sent as the accessory identifier |
| `CP2AA_TARGET_BT` | any | only accept this phone's Bluetooth address |
| `CP2AA_BT_ADAPTER` | `hci0` | Linux: which Bluetooth adapter |

## Wireless CarPlay

`CP2AA_WIRELESS=1` moves the handshake from the cable to Bluetooth. The phone is
paired in the operating system first — this program pairs nothing — and then
finds the iAP2 service we publish. Bluetooth carries only that beginning: the
moment we answer `CarPlayStartSession` with a Wi-Fi network and our link-local
address, the phone hangs up and comes back over Wi-Fi, where the same airplay
receiver takes over. Nothing about the media changes.

What it needs:

* the phone and this machine paired in the OS, on the same Wi-Fi — one that lets
  devices reach each other, so not a guest network with client isolation
* `CP2AA_WIFI_SSID` and `CP2AA_WIFI_PASS` for that network. WPA2 personal only:
  the message has nowhere to put an 802.1X username, so enterprise networks are
  refused up front rather than failing at the phone
* macOS: the Bluetooth bridge is compiled by `build.rs` with clang, and the first
  run asks for Bluetooth permission

| | How the phone reaches us |
|---|---|
| Linux | `livi_runtime::bt` registers the iAP and CarPlay profiles with BlueZ and hands us the phone's RFCOMM socket. None of that is ours — it is LIVI's code called directly |
| macOS | a small IOBluetooth helper, `native/cp2aa-bluetooth/main.m`, publishes the two SDP records, advertises the controller as in-car audio/video, polls the paired iPhones, and relays RFCOMM bytes to us over a unix socket. CoreBluetooth cannot do any of this: CarPlay's wireless handshake is SDP plus RFCOMM, not GATT |

Two things worth knowing if it does not connect. The device class matters — the
helper advertises `0x200408` for 120 seconds at a time and renews it, because
without it the phone does not treat the machine as a car. And a channel that
opens but never carries anything usually means the phone's service records are
stale, which the bridge recovers from by asking again.

`CP2AA_TARGET_BT` narrows it to one phone when several are paired, and
`CP2AA_LOG=debug` brings up the bridge's own lines.

## Input

There is no keyboard grammar of our own any more: the browser client is the
control surface, and its bindings are the ones that count.

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
this a derivative work, so cp2aa inherits that.

## Layout

| File | Role |
|---|---|
| `src/session.rs` | the wired half — USB, iAP2, MFi, `CarPlayStartSession` |
| `src/wireless/mod.rs` | the wireless half of the same: iAP2 over Bluetooth, and the Wi-Fi answers |
| `src/wireless/linux.rs` | BlueZ, through LIVI's `livi_runtime::bt` |
| `src/wireless/macos.rs` | the unix socket the IOBluetooth helper relays into |
| `native/cp2aa-bluetooth/main.m` | macOS: SDP records, device class, RFCOMM → unix socket |
| `src/airplay/mod.rs` | RTSP server: pairing, SETUP, `/info`, streams |
| `src/airplay/pairing.rs` | SRP-6a pair-setup, X25519/Ed25519 pair-verify, every key HKDF derives |
| `src/airplay/crypto.rs` | HKDF-SHA512, ChaCha20-Poly1305, the two nonce shapes |
| `src/airplay/screen.rs` | video: sealed TCP frames → annex-B H.264/H.265 |
| `src/airplay/audio.rs` | audio: sealed UDP datagrams → PCM/AAC/Opus |
| `src/airplay/event.rs` | input: HID reports and session commands, over the event channel |
| `src/airplay/mic.rs` | the microphone uplink, sealed and sent back to the phone |
| `src/sink.rs` | what a stream *is* — codec and description as well as bytes — and where the Android Auto outlet takes it |
| `src/commands.rs` | the vocabulary of input, whichever client produced it |
| `src/web/mod.rs` | the browser client's server: the wire framing, the API, the WebSocket, TLS |
| `src/aa-client/` | the Android Auto *phone* side, see below |

## The Android Auto side

The other half of the bridge: once cp2aa is holding CarPlay's picture, it has to
hand that picture to an Android Auto head unit, which means being a phone. That
lives in `src/aa-client/` and is being written now.

```
iPhone ──CarPlay──▶ cp2aa ──Android Auto──▶ head unit
       ◀──HID─────        ◀──touch/keys───
```

Nothing in there decodes anything. Callers hand it encoded frames and it hands
back input events, which is the whole reason a bridge is worth building instead
of a transcoder.

| File | Role |
|---|---|
| `aa-client/consts.rs` | channels, message ids, flags |
| `aa-client/frame.rs` | `[ch][flags][len]` framing and reassembly |
| `aa-client/proto.rs` | the protobuf primitives, including a nested walk |
| `aa-client/control.rs` | control messages: discovery, channel open, ping, focus |
| `aa-client/av.rs` | media payloads: timestamps, setup, acks |
| `aa-client/input.rs` | the head unit's keys, wheel and touch, decoded |
| `aa-client/tls.rs` | TLS 1.2 **server** presenting the phone certificate |
| `aa-client/session.rs` | the handshake and the media/input pump |
| `aa-client/usb.rs` | Linux only: the USB gadget (configfs + FunctionFS) we are a phone on |
| `src/bridge.rs` | the join: head-unit capabilities → `/info`, frames → AA, AA input → HID |

`consts`, `frame`, `proto` and `av` came from LIVI's `livi-aa`, which implements
the head-unit side of the same protocol; the wire format is symmetric, so they
carry over. What flips direction is written fresh: TLS (we accept), the session
(the head unit speaks first) and, still to come, USB (we are a gadget, not a
host).

The phone certificate is the one AACS ships — `android_auto.crt` +
`android_auto.key`. It is looked for in `$CP2AA_AA_CERT_DIR`, then `./aa`,
then `./authentication`, then `../AACS/AAServer/ssl`. Android Auto certificates
are expired and self-signed by design and neither side checks the other, so the
TLS verifiers accept anything on purpose.

### What crosses the bridge

`src/bridge.rs` is the join. Three things cross it and none of them is decoded:

* **Negotiation.** The head unit's own answer to service discovery — resolution,
  frame rate, which codec it can decode, which buttons it has — is written into
  `GET /info`, so the iPhone renders for the screen actually at the far end. It
  only lands if the head unit speaks first, because `/info` is read once per
  session.
* **Media.** Encoded frames go out on the channels the head unit opened.
* **Input.** Keys, the rotary controller and the touchscreen come back as
  `InputEvent`s and leave as CarPlay HID reports — the same path the browser's
  input takes.

### Transports

| `CP2AA_AA_USB` | Transport | Notes |
|---|---|---|
| unset | TCP on `CP2AA_AA_TCP_PORT` (5288) | what Android Auto uses for wireless; runs anywhere, including a Mac |
| `1` | USB gadget (Linux only) | configfs + FunctionFS, needs root. Enumerates straight as an AOA accessory (0x18D1/0x2D00) rather than negotiating AOA |

**The USB path has not been run on hardware yet.** It is written against the
documented configfs and FunctionFS layout and compiles, but no board has been
plugged in. Expect to fix the UDC name and the endpoint ordering first.

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
