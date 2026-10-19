# [WIP] cp2cl

CarPlay to CarLife bridge service (cp2cl), running on a Linux device.

## Background

This project is mainly for **Benz MBUX NG6**. It support Carplay, Android Audo,
and CarLife, only CarLife is fullscreen, Carplay and Android Auto are limited
to half screen.

## How it works

```
                 USB / WiFi                                      USB
  iPhone  --------------------------->  Bridge device  ------------------------->  CarLife head unit
                                        (this program)
```

## Features

- No video transcoding on bridge, only transfer.
- Support both wired and wireless CarPlay.
- A status page showing both halves at once — what the head unit is doing and what
  the phone is doing.

## Requirements

### Hardware requirements

- Running Linux. The board this is developed against is a **NanoPi Duo2**,
  armv7 hard-float.
- 1 USB OTG for the CarLife head unit, which is the host on that side.
- 1 USB host for wired CarPlay (optional).
- WiFi + BLE for wireless CarPlay (optional).

### System requirements

<!-- TODO -->

## Build and run

This crate only builds for Linux: the CarLife half talks to configfs, FunctionFS
and nl80211 directly, so there is nothing for another platform to build. On a
Linux machine, from the checkout:

```bash
git submodule update --init --recursive

cargo build --release

export CP2CL_AUTH_DIR=./authentication         # MFi credentials, and the CarLife key
./target/release/cp2cl
```

Run it from the repository root: `authentication/` is read relative to the
working directory, and that is the only thing it needs from there. The board is
built in a container instead, since it cannot build this itself — see
[Cross-compiling for the board](#cross-compiling-for-the-board).

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
cannot build this crate itself, so the build runs in the container defined by
`docker/`. Build the image once:

```bash
docker build -t cp2cl-armv7-builder docker/
```

Then, from the repository root:

```bash
docker run --rm -u "$(id -u):$(id -g)" \
  -v "$PWD":/work -v cp2cl-cargo:/cargo \
  cp2cl-armv7-builder cargo build --release --target armv7-unknown-linux-musleabihf
```

`cp2cl-cargo` is a named volume holding the cargo cache, so only the first build
downloads anything. `-u` keeps the build products owned by you rather than by
root. Use `cargo check` in place of `cargo build --release` to type-check the
whole crate without generating code, and `cargo test` to run the unit tests:
**this crate only builds for Linux**, and the container is the shortest way to a
Linux compiler from here.

**The binary is statically linked, and that is the point.** The target is musl
rather than the board's glibc, and `.cargo/config.toml` asks for `+crt-static` on
it, so the C library is part of the file: it runs on any armv7 Linux, whichever C
library that system uses, without a `libc6` of its own. What a binary cannot carry
is the architecture, named under [Hardware requirements](#hardware-requirements),
and the kernel's USB gadget support — configfs, FunctionFS and a device controller
— named under [The accessory interface](#the-accessory-interface). The one visible
cost is name resolution — musl reads
`/etc/resolv.conf` and `/etc/hosts` and knows nothing of `/etc/nsswitch.conf`, so
nothing resolved through NSS or mDNS resolves here. Neither the CarPlay nor the
CarLife path resolves a name.

The result is `target/armv7-unknown-linux-musleabihf/release/cp2cl`. Copy it to
the board and run it there; it reads `authentication/` relative to the working
directory, and everything else comes from the environment — see
[Running it on the board](#running-it-on-the-board).

### What the phone renders at

The head unit is asked what its screen is, at the start of every CarLife session
— `VIDEO_ENCODER_INIT` carries a width, a height and a frame rate, and the answer
that goes back is what both ends work from. That answer is what the phone is
asked to render at, and `GET /info` is where it is said: one display, named
`cp2cl`, sized to the car's own pixels. A car that names nothing gets 1280x720 at
30.

Changing it costs a session, and that is not a shortcut: the phone reads `/info`
exactly once, at the start of a session, and never asks again, so a new size only
reaches it on the next one. A car whose screen is not the default has its CarPlay
session rebuilt once, early on, for exactly this reason — the log says so:

```
[cl] the car asked for 1920x480 at 24 fps; agreeing to 1920x480 at 24
[cp2cl] the car's screen is 1920x480 at 24 fps; the phone is asked for the same
```

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
stream in it. Wireless wraps the RFCOMM socket in the same thing, which is what
closes the BlueZ channel under the phone.

The phone is left alone for 750ms between the two sessions, because it has to
notice the first one ending before it will act on the second.

| Log line | What it means |
|---|---|
| `[cp] ... is being told to start over; reopening carkit` | wired: the old tunnel is closed and another is being opened |
| `[cp-wifi] ... is being told to start over; dropping the Bluetooth link` | wireless: the link is closed; the platform has to bring the phone back |

The wireless half depends on something outside this program bringing the phone
back: BlueZ's `AutoConnect` pages a phone that has paired before. **Wired has
been confirmed on hardware; wireless has not.**

Asking for **more pixels and expecting a sharper picture does not work**: the
phone keeps the bitrate, so twice the pixels means half the bits each and the
picture gets *softer*.

The codec is not a choice here. CarLife carries an elementary stream and its
head units decode `video/avc`, so `/info` offers H.264 and nothing else — a
phone told H.265 is available would send pictures the car cannot read.

`uiScale` is PlayPort's UI-size control, and it still means what it did there:
ask the phone for a canvas `100/percent` the size and fit the result back to the
panel, so a larger canvas draws proportionally *smaller* CarPlay controls. Touch
is mapped through the canvas rather than the panel size for the same reason.
Nothing sets it at the moment, so the car's own pixels are asked for as they are.

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `CP2CL_AUTH_DIR` | `./authentication` | MFi and CarLife credentials (identity.pk8, certificate.p7b, carlife_md_rsa_private.pem) |
| `CP2CL_PPI` | `120` | the panel's density, in pixels per inch |
| `CP2CL_LOG_LEVEL` | `info` | log level: `error`, `warn`, `info`, `debug` |
| `CP2CL_WIRELESS` | off | take CarPlay over Wi-Fi instead of USB |
| `CP2CL_WIFI_IFACE` | `wlan0` | the interface the access point runs on |
| `CP2CL_BT_ADAPTER` | `hci0` | which Bluetooth adapter |
| `CP2CL_INPUT_FILE` | — | test video stream file |

On the board every one of these is an environment variable of the process: the
program is started by hand and has no settings file of its own — see [Running it
on the board](#running-it-on-the-board). What the accessory is called and where it
is reachable are not settings: both come off the machine, the name being its
hostname and its address being read from the Bluetooth adapter, and the network
the phone comes back over is read from the access point's own configuration — see
[Wireless CarPlay](#wireless-carplay). The USB device is not one either: it is
composed by the program, and what it is made of is in `src/carlife/desc.rs` — see
[The accessory interface](#the-accessory-interface).

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
* an access point on the interface `CP2CL_WIFI_IFACE` names.
  `CarPlayStartSession` has to say which network the phone should join and where
  we are on it, and all of that is read from `/etc/hostapd`: whichever
  configuration in there is an access point on that interface — the one hostapd
  itself was started with, so what the phone is told is what the interface is on
  air with. A missing `ssid`, `wpa_passphrase` or `channel` in it, or no
  configuration naming that interface at all, is reported at startup
* WPA2 personal only: the message has nowhere to put an 802.1X username, so
  enterprise networks are refused up front rather than failing at the phone

`livi_runtime::bt` registers the iAP and CarPlay profiles with BlueZ and hands us
the phone's RFCOMM socket. None of that is ours — it is LIVI's code called
directly. Paging the phone is ours (`src/wireless/page.rs`): an iPhone does not
return to a wireless CarPlay accessory by itself, so while nothing is connected
the paired phone is called back every few seconds. Pairing once in the OS is
enough; tapping the machine in the phone's Bluetooth settings is not needed
afterwards.

Two things worth knowing if it does not connect. The device class matters — the
controller has to advertise itself as in-car audio/video, or the phone does not
treat the machine as a car. And a channel that opens but never carries anything
usually means the phone's service records are stale.

`CP2CL_LOG_LEVEL=debug` brings up the per-event detail.

## CarLife

The head unit is the only consumer. The device presents itself to the car as a
phone over the micro-USB port, and the two halves become one thing: the CarPlay
side is held shut until the car's session is up, and is torn down when the car's
session ends. Nothing is published to the phone and nothing is accepted from it
before a car is talking, which is why a head unit is what starts the whole thing —
see [Trying it without a car](#trying-it-without-a-car) for the one that stands in
for it on a desk.

That also means the car is the only thing that starts a session — but it is not
the only thing that can watch one. [The status page](#the-status-page) shows both
halves as they stand, and `CP2CL_LOG_LEVEL` is where a run is read in full.

What is there: the USB interface the car enumerates, the AOA control requests,
the session a head unit drives — the version handshake, device information,
authentication in both directions, feature configuration, the video encoder's
negotiation, and the heartbeat that keeps the channel alive — the video channel,
carrying CarPlay's frames from the moment the car starts the encoder, the media
channel, carrying the phone's sound with the AAC-LC taken off it on the way, and
the car's own touch and panel keys, which reach the phone as the events a phone
understands. What is not: the two voice channels, which are the car's own and
stay empty, and the phone's microphone, which nothing feeds.

### Trying it without a car

`../CarLifePort` is a head unit implementation that runs on macOS, and the
topology matches the real thing — the Mac is the host, the board is the device —
so everything up to the picture can be exercised without one:

```bash
# On the board, in the foreground. Ctrl-C ends the run and takes the device off
# the bus with it.
CP2CL_WIRELESS=1 ./cp2cl
```

The video channel then carries the phone's frames, which needs both a phone and a
car session. To drive the channel with no CarPlay in the picture at all, point it
at a file instead:

```bash
CP2CL_INPUT_FILE=/root/moving.h264 ./cp2cl
```

Any annex-B stream will do. Its access units go out one per tick, at the rate the
car agreed to, and the file starts over at the end — so a file with a stream in it
makes the picture move:

```bash
ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=60 -t 30 \
  -c:v libx264 -profile:v baseline -pix_fmt yuv420p -preset veryfast \
  -g 60 -f h264 moving.h264
```

That is thirty seconds of 1280x720 at 60 fps: 1800 access units, an IDR with its
parameter sets every second, no B-frames and nothing to reorder, which is the shape
a CarPlay stream arrives in. `testsrc2` draws a frame counter into the picture, so
a picture that is moving is a number that is counting. The profile is what a head
unit's decoder is surest of, and `-pix_fmt yuv420p` is not optional with it —
baseline has no 4:4:4, and libx264 refuses the combination. The level is left to
libx264, which picks 3.2 for this size and rate; naming a lower one is a refusal
rather than a smaller stream.

A file holding a single access unit is the other case worth having, and it is what
the instructions below use: one picture, sent again and again, which is nothing a
person would watch but is enough to prove the channel and the car's decoder end to
end. Any annex-B stream works, including one frame of one:

```bash
ffmpeg -f lavfi -i testsrc=size=1280x720:rate=30 -frames:v 1 -c:v libx264 \
  -profile:v baseline -x264-params keyint=1 -f h264 idr.h264
```

The frame rate lives in the raw stream's SPS and nowhere else, so `ffplay` falls
back on a guess when it is handed the file on its own. `ffplay -framerate 60
moving.h264` is what plays it at the speed the car sees it. With no file to read,
the video channel carries heartbeats only, which still shows the session coming up
and the encoder negotiation happening.

What to watch for, in the order it happens. The first line that does not appear
is where it stopped:

```bash
[cl-usb] waiting for a head unit to enumerate the device
[cl-usb] control request 51 in, type 0xc0, value 0, index 0, length 2
[cl-usb] answered with AOA 2
[cl-usb] control request 52 out, type 0x40, value 0, index 0, length 5
[cl-usb] identifier 0: "Baidu"                                 # and five more like it
[cl-usb] control request 53 out, type 0x40, value 0, index 0, length 0
[cl-usb] switched to an accessory (18d1:2d00 as Baidu / CarLife); the host will enumerate us again
[cl-usb] the host has enumerated us; starting the session
[cl] the car speaks CarLife 1.0
[cl] the car has our device information; the session is up   # the gate opens here
[cl] the car set VOICE_MIC = 0                               # whatever it asks for
[cl] the car asked for 1280x720 at 30 fps; agreeing to 1280x720 at 30
[cl] the car started the encoder; video begins
```

`[cl]` is the session, `[cl-usb]` is the USB side under it. A head unit that
skips the handshake — which is what the reference implementation does, and what a
car does when it has already switched this device once — goes straight from
`waiting for a head unit` to `the host has enumerated us`. The other half of the
bridge says so too, and its two lines bracket the one above:

```bash
[airplay] holding: the CarLife session is not up   # until "the session is up"
[airplay] <phone> connected                        # and only after it
```

If the car goes quiet and nothing from `[cl]` follows, the last line printed says
what it was waiting on. A stop at `the car asked us to authenticate and there is
no key` is the next section's subject.

### The accessory interface

The device a head unit enumerates is composed by this program when it starts and
goes away with it. Nothing on the board is involved: the port is idle until `cp2cl`
is running, and the shape a car finds on it is the program's. `src/carlife/desc.rs`
holds the descriptors and the three requests, `src/carlife/usb.rs` composes the
gadget in configfs, mounts the FunctionFS instance on `/dev/usb-ffs/carlife`,
writes the descriptors into `ep0` and binds the gadget to the controller, and
`src/carlife/gadget.rs` answers what then arrives on `ep0`.

Three files are what the interface is, under the names FunctionFS gives them:
`ep0`, the control endpoint the car's AOA requests arrive on, then the two bulk
endpoints the session runs over. `ep1` is the one the host reads — this program
writes there — and `ep2` the other way round. The order is not a matter of taste:
a head unit takes the interface's endpoints by position, and reads the first one.

What a car finds first is a phone and not an accessory: `18d1:4ee1`, the pair an
Android phone presents in file-transfer mode, with `Google` and `Android` as its
strings. Nothing here transfers files — the pair is there so that a host which
decides what it has found by number, or by manufacturer string, finds a phone,
which is what a head unit looks for before it does any of this.

**The switch is not a flag, it is a new device.** `start accessory` cannot be
answered by changing something in place: a host does not see a device it did not
enumerate, so the gadget is unbound, its numbers and strings are rewritten —
`18d1:2d00`, under the strings the car itself sent — and it is bound again. The
endpoint files stay open across that, because FunctionFS holds them and the
descriptors do not change, which is what lets the session carry on with nothing
reopened. The one thing the program does not do is race the host: it waits 100 ms
for the answer to leave the controller before the device goes.

**Two things are asked of the kernel, and neither is in a stock image by
accident**: `CONFIG_USB_CONFIGFS` with `CONFIG_USB_CONFIGFS_F_FS` — FunctionFS —
and a device controller, which is read from `/sys/class/udc` rather than named.
The `FUNCTIONFS_ALL_CTRL_RECIP` flag in the descriptor header is what makes the
three requests reach the program at all: without it the kernel answers
device-addressed control requests itself and the program never sees one. That flag
is also why no AOA gadget support is needed — nothing here is `g_android` or
`USB_F_ACC`, and the three requests are answered in user space.

**The descriptors, since they are binary.** `desc::descriptors()` builds a
`usb_functionfs_descs_head_v2` header followed by two copies of the same three
descriptors, the first for full speed and the second for high speed:

| Offset | Bytes | Meaning |
|---|---|---|
| 0 | `03 00 00 00` | format version 2 |
| 4 | `42 00 00 00` | length of the whole blob, 66 |
| 8 | `43 00 00 00` | flags: full speed, high speed, and `FUNCTIONFS_ALL_CTRL_RECIP` (64) |
| 12 | `03 00 00 00` | three descriptors follow for full speed |
| 16 | `03 00 00 00` | three for high speed |
| 20 | 9 | interface, class `ff`/`ff`/`00` — the class the car looks for |
| 29 | 7 | bulk IN on `0x81`, 64-byte packets |
| 36 | 7 | bulk OUT on `0x02`, 64-byte packets |
| 43 | 23 | the same three for high speed, 512-byte packets |

A `usb_functionfs_strings_head` with no strings in it goes in beside them, and it
is not optional: the instance stays unregistered until both writes have landed, and
the controller refuses to bind a gadget whose function is unregistered. The name a
head unit reads arrives over the accessory requests, not over USB string
descriptors.

**A CDC-ECM link used to be on this port, and is not any more.** The board was
reached at `172.16.0.1` over that same cable; a device that presents itself as a
phone and then as an accessory has no network in it, and the board's own half is
its access point alone. See [Setting the board up](#setting-the-board-up).

### Setting the board up

`board/` is the firmware half, and the only thing in it is the access point: the
board's radio, up at boot, which is what there is to log into. What is here is laid
out as the filesystem it lands on — `board/etc/hostapd/hostapd.conf` is
`/etc/hostapd/hostapd.conf`, and `board/etc/systemd/system/cp2cl-ap.service` is
`/etc/systemd/system/cp2cl-ap.service`. Installing it is one command, run on the
board as root, from the checkout:

```bash
board/install.sh
```

**The units.**

| Unit | What it is |
|---|---|
| `cp2cl-ap.service` | hostapd on `wlan0`, and the address that interface carries: `192.168.4.1/24` |
| `cp2cl-ap-dhcp.service` | dnsmasq for the access point: leases in `192.168.4.2`–`100`, the lowest free address first, and no router or DNS server advertised |

`hostapd.conf` is the one file the operator tunes — the interface, the SSID and
the passphrase are this board's — and `install.sh` writes it only when it is not
there yet, so a second run does not undo an edit. Changing it is also how the
network the phone is told to join is changed, and where on it we are: wireless
CarPlay reads its own configuration out of `/etc/hostapd`, taking the file that
names the interface `CP2CL_WIFI_IFACE` defaults to, so the two cannot drift apart
— see [Wireless CarPlay](#wireless-carplay). Everything else in `board/` is ours
and is replaced on every run. There are no unit files for the program here: it is
started by hand, and its settings are the environment it is started with — see
[Running it on the board](#running-it-on-the-board).

**The board is reached on its access point.** `192.168.4.1`, and nothing else: the
program has no port of its own to log into, and everything it has to say goes to
the terminal it was started from. Nothing about the board needs the USB port: it
composes the device itself, so a board with nothing running on the port at all
still boots, brings up its access point, and can be logged into.

**What is not here.** The gadget, the FunctionFS mount, the bind, the DHCP server
for the link that gadget carried and the socket that opened its control endpoint
were all here at one point; none of them are now, because the program does all of
it. This script installs what is in this tree and takes nothing down, so a board
that still has a unit from that arrangement enabled is the operator's to clear.

**A reboot is what puts the board into the new arrangement**, and the controller is
why. A controller takes one gadget; a gadget from the old arrangement is still bound
to it until the board restarts; and the program cannot put its own on until then. It
says so if it is started anyway — `musb-hdrc.4.auto is already held by g1: a gadget
from an earlier arrangement is the usual reason, and a reboot clears it`.

**Upgrading a board that is already running** is `board/install.sh` and a reboot;
the script's last lines say the same. One thing changes for whoever is logged in:
where they are logged in from. The old arrangement put the board at `172.16.0.1` on
the cable, and that interface is what goes away, so the access point — or a serial
console — is the way back onto it.

After the reboot, the board's own half is two units and one address:

```bash
systemctl status cp2cl-ap.service cp2cl-ap-dhcp.service
ip -brief addr show wlan0               # 192.168.4.1/24
```

The program is not one of them: it is started by hand, on the port, and it is the
one that puts something on the USB port — see [Running it on the
board](#running-it-on-the-board).

**If the port stays empty, the reason is on the program's own output.** It says
what it could not do: no configfs, a controller already held, a mount that did not
happen, an endpoint that would not open. A device that enumerates and then goes
quiet is a head unit waiting for an answer, which `CP2CL_LOG_LEVEL=debug` prints
request by request.

**The access point advertises no gateway.** Joining it does not take a machine's
default route away from wherever it was: the client keeps that route, and still
reaches this board on the route to `192.168.4.0/24` that its own address and netmask
give it. Both options are written with no value, which is what suppresses them —
removing the line would restore dnsmasq's default of advertising itself. The DNS
option is suppressed for a second reason: `--port=0` means nothing answers DNS here.

**A DHCP range on the wrong subnet is silent.** `cp2cl-ap-dhcp.service` serves
`192.168.4.2`–`100`; the address on the interface comes from the `ExecStartPre` of
`cp2cl-ap.service`. dnsmasq accepts a range that does not sit on that interface's
subnet and then serves nothing, saying so only in the journal. The two are one line
apart in different files, so change them together.

**On a board that is not the Duo2.** `hostapd.conf` names this board's interface,
SSID and passphrase; nothing else here is board-specific. The interface it names
is the one wireless CarPlay looks for, so `CP2CL_WIFI_IFACE` has to name it too
when it is not `wlan0`. The controller is the first one
under `/sys/class/udc`, so `musb-hdrc.4.auto` is not assumed, and neither is `usb0`
or any other interface name. What is assumed is the kernel, named under [The
accessory interface](#the-accessory-interface), and hostapd and dnsmasq for the
access point. The one step that belongs to the vendor image rather than to us —
unloading the `g_mass_storage` that image loads onto this port, which would hold
the controller — is done by the program, and is a no-op where it is not loaded.

### Running it on the board

There is no package and no unit: the binary is copied over and run in the
foreground. The one thing it reads from its working directory — the MFi
credentials — is copied over beside it:

```bash
# From the checkout, after the cross-build above.
ssh root@192.168.4.1 'mkdir -p /root/cp2cl/authentication'
scp target/armv7-unknown-linux-musleabihf/release/cp2cl root@192.168.4.1:/root/cp2cl/
scp authentication/* root@192.168.4.1:/root/cp2cl/authentication/
```

Then, on the board:

```bash
(cd cp2cl/; ./cp2cl)
```

Running it here rather than as a service is deliberate: the program owns the USB
device, so its lifetime is the device's. Ctrl-C takes the gadget off the
controller with it, which is what lets a build be swapped in and started again
without a reboot. It refuses to start while another gadget holds the controller,
and says which one by name.

**Nothing else may hold the controller.** The program composes its gadget at
startup and binds it, and a controller takes one gadget, so anything already
bound there has to go first — the vendor image's `g_mass_storage` is unloaded by
the program itself, but a gadget left by an earlier arrangement is not, and a unit
holding one is not something to stop over the link it carries. That is what
[the reboot after installing](#setting-the-board-up) is for.

**Output is `tracing`, to the terminal.** `CP2CL_LOG_LEVEL` is the one control over it
and there is no journal to read: the log is the answer to whether the board came
up, whether the device was composed, and what a head unit asked. See
[Environment](#environment) for the rest.

### Authentication

The car may ask this device to prove it is a phone, with a challenge only an
actual phone can answer. That answer is made with the CarLife app's own private
RSA key, which the car checks against Baidu's published half, so
`authentication/carlife_md_rsa_private.pem` is where the key goes; without it the
challenge is reported and left unanswered rather than guessed at. The other
direction is ours to decide: the car names a channel number along with its
statistics, and is told whether a phone would serve it. `src/carlife/auth.rs`
holds both halves, and a head unit has accepted the answer to its challenge.

## The status page

A single page is served on port 80, so it is the board's own address and nothing
else: `http://<board>/` from anything on the same network. It is the one place
both halves are visible at once, and they are two halves of one negotiation seen
from opposite ends: **CarLife连接** is what the head unit is doing,
**CarPlay连接** is what the phone is doing, and neither end can see the other's
side. **正在播放** is what the two agree on: the song the car's panel is showing,
which belongs to neither half and is the one part of the page someone sitting in
the car can check against what they see. It is shown only while both ends are
up — a song on a page whose halves are down describes a moment that has passed.

The port is not a setting, and neither is the page: it is one HTML file with no
build step, compiled into the binary along with its stylesheet. There is nothing
to install beside the program and nothing to fetch — which matters here, because
the board's access point deliberately announces no gateway, so there is no route
for a CDN to be reached over — see [Wireless CarPlay](#wireless-carplay).
`assets/web/index.html` is the page, `src/web/mod.rs` the router, and
`src/web/status.rs` the snapshot behind it.

Port 80 is below 1024, so it takes root to bind; the board runs the program as
root and this is one more thing that needs nothing configured. Where it is
refused — a machine running it as someone else — the page is what is lost: the
program says so and carries on, because the streams are the bridge and the page
is not.

The page asks for nothing over the network of its own: it polls `GET /api/status`
every two seconds and redraws from what comes back. That endpoint is the whole
API, and it answers with the same snapshot the page shows:

```json
{ "carlife": { ... }, "carplay": { ... }, "now_playing": { ... } }
```

The cover is the one thing not in it. It is a JPEG, fetched from `GET
/api/cover?id=<hash>`, where the hash is what `now_playing.cover.id` holds: the
name is the picture, so the page keeps the one it is drawing and downloads
another only when the car has been given one. A request naming anything else —
a cover from a song that has finished, or no name at all — is answered with
`404` rather than with whatever is current, because an answer under the wrong
name would be one song's picture drawn as another's and cached under it for a
year. A new song arrives without a cover and puts the last one down — see
[Now playing](#now-playing).

A field is either a value or `null`, and `null` means the thing it describes has
not happened yet — a car that has not asked for its six identifiers has six
`null`s, not six empty strings. Every value is one the program observed, apart from
the two the page adds itself: the vendor name a channel number maps to, and the
codec this bridge only ever sends. The fields belonging to a connection are cleared
when that connection ends, so a page left open shows the present rather than the
last session.

Both sections are short on purpose. What is worth reading there is the two ends
side by side — the resolution the car agreed to against the one the phone renders,
the protocol version and channel the car reports against the access method and the
codecs the phone brought — because that is the question the page answers: whether
the two halves met. What they said to each other is the log's.

## Input

The car is where input comes from: its touch and panel keys arrive on the CarLife
session, and that is the control surface. Nothing carries them any further yet —
they are logged, and the CarPlay side would be the thing to forward them to.

The vocabulary they are mapped onto is in `src/commands.rs`, and `event.rs` is the
one place that knows how each command reaches the phone: a HID report on one of
the four devices `/info` declares — touchscreen, knob, media and telephony — or a
session command for day/night and the call keys.

A press is always two reports, the press and an all-zero one. The phone tracks a
button by whether it is still down, so a key that is never released does not
register as a press at all; the same is true of the touchscreen, where a lifted
finger has to leave its slot behind rather than disappear from the list, and at
the position it was last seen at.

## The microphone

CarPlay has no microphone stream. The phone asks for one by naming a *send* port
in the SETUP of its main audio stream and labelling that stream `telephony` or
`speechRecognition`; whatever captures the audio then encodes it, and this process
seals the payloads and puts them on that port.

The codec follows the transport rather than being chosen: Opus over Wi-Fi, PCM
over the cable, at the rate and channel count the phone's own audio stream
arrived at.

Nothing captures it yet — there is no microphone on the board, and no path from
the car up to this side — so the uplink opens and is reported
(`[mic] uplink open to ...`) for a capture that does not exist. It is open while
the phone is listening and shut the rest of the time, so the way to see it is to
start a call or invoke Siri.

## Now playing

The car's panel is told what the phone is playing. iOS describes a song over iAP2
as `NowPlayingUpdate` — one attribute per message, with the cover art as a file
transfer of its own — while CarLife's `MSG_CMD_MEDIA_INFO` carries the whole song
in a single message. `src/playing.rs` is where the two shapes are reconciled:
`Track` folds the phone's partial updates together, and the CarLife session sends
the whole of what is known each time one arrives.

Every update therefore goes out as it arrives, one message per attribute, which
is what the phone's own panel does with the same messages: a title, then the
title and artist, then the three of them. There is no pause and nothing to
tune — the phone is what decides when the song changes.

The panel gets the song, the artist, the album, its length and where it is
playing from, plus the cover once the phone has sent one. A field is written
only when there is something behind it: one left out carries its default, which
is the same value an empty one would, so a song without a cover has no
`albumArt` at all rather than a field of length zero. Three of the car's nine
fields are left out rather than written as zero. `playlistNum` and `songId` are
subscription-gated — the phone is not subscribed for `persistent_id` or
`app_bundle_id` — and iAP2 has no message for the play `mode` at all. An absent
field and a zero are not the same claim: a `mode` of zero would be single-track
repeat.

The cover arrives after the update that named the song and names no song itself,
so it is taken as belonging to the newest one. What puts it down is a song
carrying a length: the length is what the phone sends when it describes a song
rather than a change of play state, and it comes with the rest of the new one
rather than after it, so it is the moment a song has started — and the picture
held belongs to the one before it. A title that changes without a length keeps
it, which is what an app that writes its lyrics into the title needs: it renames
the song for every line, and every one of those is the same song. A file transfer
that completes with no bytes in it is the phone saying this song has no picture.
It puts down the cover that is held, and the message that follows carries no
`albumArt` at all rather than a field with nothing in it to draw.

A cover is only sent while it fits. The song and its cover travel in one
command-channel message, whose body the car caps at 40 KB, and the reference app
keeps a cover under 30 KB for it — `proto::MAX_ART`. A cover larger than that is
re-encoded first, in `src/artwork.rs`: the picture is decoded, turned the way its
EXIF orientation says it is meant to be seen, and re-encoded as a baseline JPEG
with 4:2:0 chroma and no metadata. The size comes down to a 400px long edge, and
the quality with it — 90, 80, 70, 60 — before the edge goes to 350 and then 300,
six encodes at most. A cover that fits already is passed through untouched, and
one that will not fit at any of the six is dropped with a warning and the song
goes without it.

Every message is checked against its channel's cap before it is written, and one
that would not fit is dropped with a warning rather than sent: the car refuses
such a message, and a refused length leaves the channel waiting for bytes that
never come, which costs the session rather than the message.

The play position is not sent: `MSG_CMD_MEDIA_PROGRESS_BAR` is left alone, though
the phone does describe where the song has got to.

## Navigation

The car's turn-by-turn strip is told what the phone is guiding towards. iOS
describes a route over iAP2 as two messages — `RouteGuidanceUpdate` (0x5201)
carries the state of the route as a whole and names which of its turns is the
current one, while `RouteGuidanceManeuverUpdate` (0x5202) describes one turn at a
time, by index, with that turn's type and the road it leads onto.

`src/navigation.rs` is where the two shapes are reconciled: `Guidance::from_iap2`
reads one of the phone's messages, `merge` folds it into what is known, and
`current` answers which turn the strip is about. The CarLife session sends
`MSG_CMD_NAVI_NEXT_TURN_INFO` when the turn changes and `MSG_CMD_MODULE_STATUS`
when the route starts or stops.

The strip gets the turn kind, the road it leads onto, and how far away it is.
Two of the car's six fields are left empty rather than written as zero.
`totalDistance` is a second distance to the same turn — the guide's "total"
against the "remaining" — and iAP2 describes only one distance to a turn, so
there is nothing to put in it. `time` is the seconds to the turn, and the phone's
own countdown is to the destination: a countdown to the wrong place is worse than
no countdown.

`turn_kind` is deliberately incomplete. The phone's `EManeuverType` has 54 values
and the car's `TurnKind` has 58; a kind is named only where the two vocabularies
mean the same thing. Ramps, motorway changes and arrivals have no counterpart in
the car's list, and are left unnamed rather than rounded to the nearest arrow.

The strip is only sent once the car has asked for it, with
`MSG_CMD_CARLIFE_DATA_SUBSCRIBE_START`; before that the module state alone is
sent, because that is the phone saying something about itself rather than data
the car subscribes to.

## Depending on LIVI

LIVI is a git submodule under `vendor/`, pinned to an exact commit:

```bash
git submodule update --init --recursive
```

| Submodule | Upstream | Pinned to |
|---|---|---|
| `vendor/LIVI` | `0x5e/LIVI` | `feature/local-credential` |

`Cargo.toml` takes the LIVI crates by path from the submodule:

```toml
livi-runtime = { path = "vendor/LIVI/native/livi-helperd/crates/livi-runtime" }
```

LIVI's repository root is an Electron app with no top-level `Cargo.toml`; its
Rust lives under `native/`, which is why every path spells that out.

Because the submodule is pinned rather than tracking a branch, nothing moves on
its own — `git submodule update --remote vendor/LIVI` is the deliberate, manual
step that advances it.

Note the licence: LIVI is **GPL-3.0-or-later**, and linking its crates makes
this a derivative work, so cp2cl inherits that.

## Layout

| File | Role |
|---|---|
| `src/session.rs` | the wired half — USB, iAP2, MFi, `CarPlayStartSession` |
| `src/wireless/mod.rs` | the wireless half of the same: iAP2 over Bluetooth, and the Wi-Fi answers |
| `src/wireless/linux.rs` | BlueZ, through LIVI's `livi_runtime::bt` |
| `src/carlife/mod.rs` | the CarLife half: this device standing in for a phone |
| `src/carlife/desc.rs` | the accessory interface as bytes, and the three AOA requests |
| `src/carlife/usb.rs` | Linux only — the gadget, the FunctionFS instance, and the two forms the device takes |
| `src/carlife/gadget.rs` | Linux only — `ep0`: the requests arriving on it, and the answers |
| `src/carlife/auth.rs` | the two halves of authentication: the car's challenge answered, and its channel judged |
| `src/carlife/session.rs` | the session a head unit drives: handshake, heartbeats, pictures, sound, touch |
| `src/carlife/frame.rs` | the USB framing — the outer chunk header and the per-channel message head |
| `src/carlife/proto.rs` | as much protobuf as these messages need, written by hand |
| `src/airplay/mod.rs` | RTSP server: pairing, SETUP, `/info`, streams |
| `src/airplay/pairing.rs` | SRP-6a pair-setup, X25519/Ed25519 pair-verify, every key HKDF derives |
| `src/airplay/crypto.rs` | HKDF-SHA512, ChaCha20-Poly1305, the two nonce shapes |
| `src/airplay/screen.rs` | video: sealed TCP frames → annex-B H.264 |
| `src/airplay/audio.rs` | audio: sealed UDP datagrams → PCM, with the phone's AAC-LC decoded here |
| `src/aac.rs` | AAC-LC → the PCM the car's media channel takes, a frame at a time |
| `src/airplay/event.rs` | input: HID reports and session commands, over the event channel |
| `src/airplay/mic.rs` | the microphone uplink, sealed and sent back to the phone |
| `src/sink.rs` | what a stream *is* — codec and description as well as bytes — and the two channels the car drains |
| `src/playing.rs` | what the phone is playing, as the fields the car's panel is told |
| `src/artwork.rs` | the cover art, made small enough for the car's one media message |
| `src/navigation.rs` | where the phone is going, as the fields the car's turn-by-turn strip is told |
| `src/commands.rs` | the vocabulary of input |
| `src/web/status.rs` | the snapshot both halves report into, and the JSON behind `/api/status` |
| `src/web/mod.rs` | the status page's router and listener |
| `src/assets.rs` | the lookup over `assets/`: what the binary carries besides code |
| `assets/` | the icons, the channel table and the status page — `build.rs` includes every file here, so adding one is enough to be able to ask for it by name |
| `assets/icons/` | the icon `GET /info` puts on the phone's screen, two sizes per make, `Default-*` for the rest |
| `assets/carlife_channel_map.json` | the head unit's channel number, and the label worth showing for it |
| `assets/web/index.html` | the page itself — one file, two sections, polling |
| `assets/web/vendor/weui.min.css` | its stylesheet, vendored so the page needs no network |
| `docker/` | the image that cross-compiles for the board, and the CMake toolchain it needs |
| `board/` | the firmware half: the access point and its unit, laid out as the filesystem they land on |

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
* **A new screen size blanks the picture for a moment.** See [what the phone
  renders at](#what-the-phone-renders-at) above: the session has to be rebuilt for
  the new size to reach the phone.
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

* The CarLife video and media channels carry the phone's picture and its sound,
  the car's touch and panel keys reach the phone, and its panel is told what is
  playing and where the phone is guiding. The two voice channels are the car's
  own and stay empty, so navigation speech and the voice assistant are silent.
* The panel is told *which* song is playing but not where in it: the progress
  bar's message is not sent. See [Now playing](#now-playing).
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
