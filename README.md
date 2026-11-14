# cp2cl

[![license](https://img.shields.io/github/license/0x5e/cp2cl)](LICENSE) [![build](https://github.com/0x5e/cp2cl/actions/workflows/build.yml/badge.svg)](https://github.com/0x5e/cp2cl/actions/workflows/build.yml) [![release](https://img.shields.io/github/v/release/0x5e/cp2cl)](https://github.com/0x5e/cp2cl/releases/latest) ![arch](https://img.shields.io/badge/arch-aarch64%20%7C%20armv7-informational)

CarPlay to CarLife. 让 iPhone CarPlay 能够连接到 CarLife 车机。运行于 Linux 嵌入式设备。

![preview](./screenshots/IMG_7155.png)

## 背景

本项目主要用于**奔驰 MBUX NTG 6.0 车机**，通过转接 CarLife 通道实现 CarPlay 的全屏显示。其他车机未经测试。

> NTG 6.0 的 CarLife 窗口能全屏显示，而 CarPlay 窗口只能显示一半。

## 特性

- 支持有线、无线 CarPlay
- 视频流纯转发，无编解码环节，低性能开销
- 支持所有音频流：媒体、导航、通话（双向）、Siri（双向）
- 支持智能缩放显示，开启后可显示3个小组件
- 支持仪表盘媒体信息显示（含封面）
- 适配车机图标与名称
- 后台管理页，可查看状态、日志、设置

## 硬件适配列表

固件基于 Buildroot 2025.02 构建，开机启动时间相对 Ubuntu 30s，提升到 7s。

- [x] OrangePi-Zero2W
- [x] 随身WiFi-UFI001C（很卡，不推荐）
- [x] NanoPi-Duo2（略卡，不推荐）

其他适合的设备参考：[DashPlay - 硬件](https://github.com/yizhixiaoheigou/DashPlay/blob/main/README.md#硬件)
后续有时间我会尝试适配，欢迎小伙伴帮忙一起测试。

## 运行环境

### 硬件要求

- 1个 USB OTG 口
- 1个 USB Host 口（可选，用于有线 CarPlay）
- WiFi + BLE 网卡（可选，用于无线 CarPlay）
- 存储空间大于 128MB

## 使用方式
### 使用固件
[Github Release](https://github.com/0x5e/cp2cl/releases)可下载对应固件或可执行文件。

### 使用安装包（暂无）
Ubuntu、Debian、OpenWrt 安装包暂未提供。
[Github Release](https://github.com/0x5e/cp2cl/releases)可下载 armv7、aarch64 可执行文件，需要自行准备启动脚本，安装相关依赖。

依赖列表（待补充）：
dbus, bluez, hostapd, dnsmasq-base, iw, kmod

## 运行截图

![guide](./screenshots/guide.png)
![widget](./screenshots/IMG_7151.png)
![apps](./screenshots/IMG_7155.png)

<p align="left">
  <img src="./screenshots/status1.png" width="30%" alt="status1">
  <img src="./screenshots/status2.png" width="30%" alt="status2">
</p>

## 开发调试

### 编译
在项目根目录运行：

```shell
# 可从 com.baidu.carlife_8.9.0_base.apk/classes2.dex 中获取。
# com/baidu/carlife/authorization/PrivateKeyManager.java:37
export CARLIFE_PRIVATE_KEY="MIICdwIBAD..........gHdvNpcqA="
make all
```
应用和固件产物位于`dist/`目录.

### 调试工具

CarLife车机 - macOS 版：[CarLifePort](https://github.com/0x5e/CarLifePort)

## 配置

配置文件目录位于`/etc/cp2cl/`，配置文件`config.json`及MFi证书、私钥均置于此目录

| 字段名 | 默认值 | 备注 |
|---|---|---|
| `log_level` | `debug` | 日志级别：`error`, `warn`, `info`, `debug` |
| `wifi_iface` | `wlan0` | Wi-Fi接口名称 |
| `bt_adapter` | `hci0` | 蓝牙适配器名称 |
| `addr` | `0.0.0.0:80` | Web后台监听地址与端口 |
| `screen` | — | 屏幕自定义参数：`width`, `height`, `fps` |

例子：
```json
{
  "log_level": "debug",
  "wifi_iface": "wlan0",
  "bt_adapter": "hci0",
  "addr": "0.0.0.0:80",
  "screen": { "width": 1920, "height": 720, "fps": 60 }
}
```

MFi证书（`certificate.p7b`）、密钥（`identity.pk8`）文件可以在构建前放置于项目`authentication/`目录下，或是运行前拷贝至配置文件目录。

## 已知问题
- 初次无线 CarPlay 配对可能会超时（约15秒），也可能会失败多次，原因暂不明。
- 方向盘右侧方向键无法完整支持。无论按上下还是左右，MBUX NTG 6.0 车机都只会发送`KEYCODE_SELECTOR_NEXT`/`KEYCODE_SELECTOR_PREVIOUS`，无法辨别，只能按照滚轮“前/后”进行映射。
- CarLife 协议不支持夜间模式，因此 CarPlay 界面无法根据车外环境光自动变化浅色、深色模式。

## 来源与许可
参考项目：
- [LIVI](https://github.com/f-io/LIVI)
- [MacPlay](https://github.com/Roylyl/MacPlay)
- [playport](https://github.com/youcci/playport)

项目沿用 GPL-3.0-or-later。
