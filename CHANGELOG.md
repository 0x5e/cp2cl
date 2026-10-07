# Changelog

本项目所有值得注意的变更都记录在此文件。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## v0.6.1 alpha — 2026-10-08

### 新增

- Linux 上不再依赖系统 `usbmuxd`。改由 `iap2-usbmux` 自行把 iPhone 切到 CarPlay 的 USB 配置，并经 lockdown 打开 `com.apple.carkit.service`（`9b64f7d`）。该句柄需持有至会话结束，否则手机会退回默认配置。
- 新增 `NcmBridge`，接手手机暴露的 NCM 控制与数据接口，在其上建立 TAP 并负责 `up`，会话使用它返回的接口名（`fad789b`）。

### 变更

- 移除 `CP2AA_AV_IFACE`。接口名改由 `NcmBridge` 返回，`av_interface()` 只作兜底（`d514498`）。
- 各平台构建产物统一收进 `target/<triple>/`，交叉编译命令补进 README（`62417d3`）。

### 修复

- 修复连接成功后 CPU 占满一核。`Vehicle` 被当作临时值立即析构，其持有的 `watch` 发送端一并丢弃，事件循环不再挂起（`78d3932`）。
- `vendor/LIVI` 更新至 `9f829f0a`，使 `iap2-usbmux` 能在 musl 下编译（`b9c442e`）。

## v0.6.0 alpha — 2026-10-06

### 新增

- 引入子模块 `vendor/playport`（main，`9a0882dd`）与 `vendor/LIVI`（feature/local-credential，`2083da3b`），原先 git+branch 形式的依赖改为本地路径。
- 内置 HTTPS 服务。监听地址由 `CP2AA_WEB_ADDR` 指定（默认 `0.0.0.0:8080`），访问 token 由 `CP2AA_WEB_TOKEN` 指定（默认 32 个 `0`），启动时打印完整访问地址。
- 麦克风上行：浏览器采集并编码音频后回传至 iPhone。

### 变更

- `CP2AA_SINK` 默认值由 `ffplay` 改为 `web`，不再单独提供 webserver 开关。
- 输出侧换成 PlayPort 的 Web 界面，后端按其既有接口实现；Display 面板支持改分辨率并重启 CarPlay 会话。

### 移除

- 删除 ffplay 输出与 stdin 控制指令。

### 修复

- 改分辨率改为拆掉整条会话再重建，不再重发 `CarPlayStartSession`（`1b8d3c0`）。
- 音视频流的端口交由内核分配（`cb9e8d7`）。
- 固定 Web token，打印出的访问地址在重启后仍然可用（`611d3b4`）。
- 无线音频按帧时间戳重排后再下发，消除手机成块发送造成的卡顿（`4c00f8d`）。
- 触摸抬起时上报按下时的原坐标，侧边栏与二级页返回按钮的点击随之生效（`3ce6cba`）。
- 日志分 error / warn / info / debug / trace 五档，`CP2AA_LOG` 是唯一开关（`0926fe0`）。

## v0.5.0 alpha — 2026-10-06

### 新增

- 有线 / 无线 CarPlay 连接。`CP2AA_WIRELESS=1` 配 `CP2AA_WIFI_SSID` 与 `CP2AA_WIFI_PASS`，默认有线。
- 输出源开关 `CP2AA_SINK=ffplay|aa`，默认 `ffplay`；`aa` 尚未实现，只在代码里预留了协商参数透传。
- ffplay 视为恒在线的车机：优先 HEVC、其次 H264，分辨率 1280x720；窗口尺寸参考 MacPlay，macOS 上宽高减半。
- stdin 控制指令，键位对齐 PlayPort 前端：媒体 `play | pause | playpause | next | prev`，按键 `home | back | select | left | right | up | down`，会话 `siri`，本地 `quit`。

### 变更

- 协商改为同步式：车机在线才开启 CarPlay，建联时透传双方音视频参数，车机离线即断开。

<!--
## v0.9.0 alpha — 待发布

完成 AA 车机部分。因 Linux 开发板还未到，先只完成代码开发和编译通过即可；调试相关的实现不打到 release 包里。
-->
