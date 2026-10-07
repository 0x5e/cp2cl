# Changelog

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## v0.6.0 alpha — 2026-10-06

调试界面换成 PlayPort 的 web 界面，ffplay 与 stdin 两条旧通路删除。

### 新增

- 引入 submodule `vendor/playport`（main，`9a0882dd`）与 `vendor/LIVI`（feature/local-credential，`2083da3b`），
  cargo 里 git+branch 形式的依赖改走这两条路径
- 内置 HTTPS web 服务，监听地址由 `CP2AA_WEB_ADDR` 指定（默认 `0.0.0.0:8080`），
  access token 由 `CP2AA_WEB_TOKEN` 指定（默认 16 字节的 0），完整访问路径打印到输出
- 麦克风上行：浏览器采集并编码后，把音频回传给 iPhone

### 变更

- `CP2AA_SINK` 默认值由 `ffplay` 改为 `web`，不再单独提供 webserver 开关
- 后端按 PlayPort 前端的既有接口完整实现，Display 面板支持改分辨率并重启 CarPlay 会话

### 修复

- 改分辨率改为拆掉整条会话再重建，不再重发 `CarPlayStartSession`（`1b8d3c0`）
- 流的端口交给内核去选（`cb9e8d7`）
- 固定 token，打印出来的访问 URL 重启后仍然可用（`611d3b4`）
- 无线音频是 buffered 流、按 18 包（384ms）成块到达，撞上前端 120–350ms 的播放窗口，
  导致听感上的卡顿；服务端按帧时间戳重排后再下发（`4c00f8d`）
- 触摸抬起时报告按下时的原坐标，左侧菜单栏按钮与二级页返回按钮的点击随之生效（`3ce6cba`）
- 日志分 error / warn / info / debug / trace 五档，`CP2AA_LOG` 是唯一旋钮（`0926fe0`）

### 删除

- ffplay 输出与 stdin 输入整体删除

## v0.5.0 alpha — 2026-10-06

CarPlay 部分完成，并在 macOS 上调试通过。macOS 与 Linux 均可构建。

### 新增

- 有线 / 无线 CarPlay 连接，`CP2AA_WIRELESS=1` 配 `CP2AA_WIFI_SSID` 与 `CP2AA_WIFI_PASS`，默认有线
- 输出源开关 `CP2AA_SINK=ffplay|aa`，本版本默认 `ffplay`；`aa` 尚未实现，但代码里预留了协商参数透传
- ffplay 视为恒在线的车机：优先 HEVC、其次 H264，分辨率 1280x720，屏幕尺寸参考 MacPlay，
  窗口大小在 macOS 上宽高减半
- stdin 控制指令，键位对齐 playport 前端：媒体 `play | pause | playpause | next | prev`，
  按键 `home | back | select | left | right | up | down`，会话 `siri`，本地 `quit`

### 变更

- 协商改为同步式：车机在线才开启 CarPlay，建联时透传双方音视频参数，车机离线即断开

<!--
## v0.9.0 alpha — 待发布

完成 AA 车机部分。因 Linux 开发板还未到，先只完成代码开发和编译通过即可；调试相关的实现不打到 release 包里。
-->
