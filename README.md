# cp2cl

CarPlay to CarLife bridge service (cp2cl), running on Linux embedded device.

## Background

This project is mainly for **Benz MBUX NTG6.0 head unit**. It support Carplay, Android Audo,
and CarLife, only CarLife is fullscreen, Carplay and Android Auto are limited
to half screen.

## How it works

```
                 USB / WiFi                                      USB
  iPhone  --------------------------->  Bridge device  ------------------------->  CarLife head unit
                                        (this program)
```

## Features

- No video transcoding.
- Support both wired and wireless CarPlay.
- Website with status/log/setting page.

## Requirements

### Hardware requirements

- 1 USB OTG for CarLife head unit.
- 1 USB host for wired CarPlay (optional).
- WiFi + BLE for wireless CarPlay (optional).

## Build and run

Run `make all`, Then app and firmware output in `dist/`.

## Credentials

Put key & cert in `/etc/cp2cl/` folder with following name, or `authentication/` before build:

| File | Comment |
|---|---|
| `identity.pk8` | MFi private key |
| `certificate.p7b` | MFi certificate |
| `carlife_md_rsa_private.pem` | CarLife private key |

## Configuration

Config file at `/etc/cp2cl/config.json`.

| Keys | Default | Comment |
|---|---|---|
| `log_level` | `info` | log level: `error`, `warn`, `info`, `debug` |
| `wireless` | `true` | wireless or wired CarPlay (will be removed in the future) |
| `wifi_iface` | `wlan0` | wifi interface name |
| `bt_adapter` | `hci0` | bluetooth adapter name |
| `screen` | — | force resolution and framerate (example: ``) |

Example
```json
{
  "log_level": "debug",
  "wireless": true,
  "wifi_iface": "wlan0",
  "bt_adapter": "hci0",
  "screen": { "width": 1920, "height": 720, "fps": 60 }
}
```

## Debug with CarLife Head Unit running in macOS

Try this: [Setting the board up](https://github.com/0x5e/CarLifePort)

## Known issues

- The direction key in right side of the steering wheel is not fully supported.
Whenever you press left/right or up/down, the MBUX NTG6.0 head unit only send
`KEYCODE_SELECTOR_NEXT`/`KEYCODE_SELECTOR_PREVIOUS`, we can not differ.

### Credits and license

cp2cl is free software, licensed under the GNU General Public License v3.0 or later (GPL-3.0-or-later).

- [LIVI](https://github.com/f-io/LIVI)
- [MacPlay](https://github.com/Roylyl/MacPlay)
- [PlayPort](https://github.com/youcci/playport)
