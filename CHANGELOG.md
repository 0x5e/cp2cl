# Changelog

本项目所有值得注意的变更都记录在此文件。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## v0.8.0 alpha — 待发布

### 新增

- 手机端 CarPlay 主屏上多出一格车机图标，下方是车型名称：`GET /info` 应答新增 `oemIconVisible`、`oemIconLabel` 与 `oemIcons`，256 与 512 两档、`prerendered`，PNG 内联在应答里而不是另起一次下载。车型由车机在 `MSG_CMD_HU_STATISTIC_INFO` 中报的渠道号决定：`assets/carlife_channel_map.json` 查出标签，图片按 `<标签>-<尺寸>.png` 取；没有标签的渠道用 `App` 与 `Default-*`（`23b2843`）。
- 点击那一格回到车机自己的界面：手机在控制通道发 `requestUI`，回 200 后向车机发 `MSG_CMD_GO_TO_DESKTOP`（`0x00010021`，无 body）；`url` 为 `videoplayback:` 的不处理，`suggestUI` 只记一行日志（`bc62411`）。
- 状态页：80 端口提供单页，分「CarLife 连接」与「CarPlay 连接」两段，每 2 秒轮询 `GET /api/status` 重绘；页面与 WeUI 样式表内置进二进制，不依赖外部文件、CDN 或环境变量（`7512259`）。
- 桥接手机的正在播放到车机面板：`NowPlayingUpdate` → `MSG_CMD_MEDIA_INFO`，每条更新即时发送；`playlistNum`、`songId`、`mode` 留空，无对应源。封面随歌曲走：带时长的更新即清空，只改标题的不清除；大小为 0 的封面视为这首歌没有封面，消息里不写 `albumArt`；超过 30 KB 时先重编码（baseline JPEG、4:2:0、去元数据），仍放不下才不发（`2e8d4db`）。
- 状态页新增「正在播放」一节：来源、歌曲、艺人、专辑、时长（分:秒）与封面，仅在车机与手机两端都连接时显示；封面由 `GET /api/cover?id=<哈希>` 单独取，只回答当前那张，新的 CarLife 会话开始时清空（`2e8d4db`）。
- 桥接手机的路线引导到车机简易诱导条：`RouteGuidanceUpdate` / `RouteGuidanceManeuverUpdate` → `MSG_CMD_NAVI_NEXT_TURN_INFO` + `MSG_CMD_MODULE_STATUS`；`totalDistance`、`time` 留空，无对应源；`turn_kind` 仅映射语义一致的类型（`2e8d4db`）。

### 变更

- 可执行文件携带的资源统一收到 `assets/`：图标、渠道映射表、状态页与其样式表。`build.rs` 遍历该目录生成「名字 → `include_bytes!`」的表，`src/assets.rs` 按相对路径取；内容仍在只读段，运行时不解包到临时目录，板子上也不需要额外文件（`5ab3f50`）。
- 新增一套车型图标不再需要改代码：放两张 PNG 进 `assets/icons/`，再在映射表里给对应渠道号加一个 `label`（`5ab3f50`、`23b2843`）。
- 日志级别变量由 `CP2CL_LOG` 更名为 `CP2CL_LOG_LEVEL`，可选值收窄为 `error`、`warn`、`info`、`debug`（`2f10b36`）。
- 日志行时间戳改为本地时间 `HH:MM:SS.mmm`，不再输出日期与时区（`e41c37c`）。
- 链路断开时的日志写明未被应答的消息名与 id、所在会话，以及重传次数与等待时长，不再只有包序号（`1923594`）。
- `POST /command` 中不认识的类型、以及 `SETUP`、`TEARDOWN` 的请求体整包打印，单行 JSON；`Data` 只取前 32 字节 hex，键按字母序（`ce12a91`、`bc62411`）。
- axum 重新引入，只开 `http1` 与 `tokio` 两个 feature，仅服务于状态页（`7512259`）。
- 状态页两段各自以「会话状态」打头，未连接时该段其余各项留空。
- 状态页的视频编码格式不再由页面写死，改由后端返回，常量定义在 `airplay::screen::CODEC`。
- 状态页的物理尺寸由车机协商的分辨率与 `CP2CL_PPI` 算出，并挪到车机端的「分辨率」之后。
- 链路同步参数里的单包上限由 65535 字节改为 4096 字节，对端据此分片。
- 媒体信息的日志两端都在 `debug`：CarPlay 侧每收到一条 `NowPlayingUpdate` 记一条（只列该条带了的字段，一个都不带的不记），解不出的记 `warn`；CarLife 侧每发出一条记一条，`cover` 记大小（kB）（`2e8d4db`）。

### 修复

- 发送前校验消息是否超过所在通道的 body 上限，超出则告警并丢弃，避免车机端因长度不符断开数据通道（`2e8d4db`）。

## v0.7.0 alpha — 2026-10-16

### 新增

- CarLife 手机端协议栈。本设备以手机身份接入车机：AOA 的控制请求、车机枚举出的接口与两个 bulk 端点、消息的两层承载，以及由版本、设备信息、功能配置、编码器协商与心跳组成的会话（`a1b0a3d`）。
- CarLife 视频通道，把 CarPlay 的访问单元原样交给车机，SPS/PPS 置于所属画面之前，通道静默一秒后改为发送心跳（`dcff363`）。
- CarLife 媒体通道。无线 CarPlay 只提供 AAC-LC，先在本地解码为 16 位 PCM 再交给车机（`src/aac.rs`，`symphonia` 只取 `aac` feature）（`c856b7d`）。
- CarLife 输入通道，把车机的触摸与面板按键转成 CarPlay 侧的事件：触摸抬起上报按下时的位置，面板按键按车机的语义映射为方向与媒体命令（`51cb000`）。
- CarLife 双向认证：用手机应用里的私钥应答车机的挑战 `0x00018048`，并在车机随统计信息报上渠道号时回 `0x0001004B`（`456236e`）。
- 设备自己组装 USB gadget：清除残留、驱逐厂商的 `g_mass_storage`、建立 configfs gadget、写入描述符与字符串、挂载 FunctionFS、绑定控制器。设备先以 Android 手机的身份上总线，车机在启动请求中给出身份后再解绑、改写厂商与产品标识及字符串并重新绑定（`7254e1f`）。
- `board/install.sh` 与接入点所需的 `hostapd.conf`、两个 systemd 单元（`ffa8c61`）。
- `docker/`：armv7 构建镜像的 Dockerfile 与 CMake 工具链文件收进仓库，交叉编译变为构建该目录再运行（`c2e8269`）。
- `CP2CL_INPUT_FILE`：视频通道改从文件按时序送出访问单元，用于在没有手机时验证通道与车机解码器（`a690723`）。

### 变更

- 项目更名为 cp2cl：包名与二进制名、环境变量前缀、日志前缀、配件与 HID 名称、配对目录（`6ddbc27`）。
- 目标平台收拢为一块开发板上的 Linux，目标三元组改为 `armv7-unknown-linux-musleabihf` 并静态链接，二进制自带 C 库；macOS 不再是目标，交叉编译只经 `docker/` 里的镜像（`22b4edb`、`3c9d205`）。
- 车机是唯一的消费端：浏览器客户端与选择消费端的开关一并移除，CarPlay 侧只在对端会话进行时开启（`68a4336`）。
- 视频通道只宣示 H.264：`/info` 不再发 `hevcInfo`，随之去掉 hvcC 解析、H.265 关键帧判定与 `VideoCodec`（`8e426bc`）。
- 屏幕几何由车机决定：车机在 `VIDEO_ENCODER_INIT` 谈定后回报，只在值变化时重启 CarPlay 会话；帧率改为接受车机要求的 3–60（`68a4336`、`4548432`）。
- 物理尺寸改由 `CP2CL_PPI` 推出：像素数与面板密度决定毫米数，取代两个需要互相照顾的变量（`e15cb04`）。
- 设备名从主机名读取，`name`、`model`、`manufacturer` 与枚举时的序列号都用它（`0c3777d`）。
- 无线参数（SSID、口令、信道）改从 `/etc/hostapd` 中 `CP2CL_WIFI_IFACE` 所指的那份配置读取，不会再与接口实际在空的网络出现分歧（`6240430`）。
- 接入点不再宣示网关，租约地址从 `.2` 起顺序分配（`b0d14f3`、`dd537d9`）。
- 部署改为把二进制拷上板、与 `authentication/` 放在同一目录后前台运行（`ffa8c61`、`1710848`）。

### 移除

- Android Auto 的一半：`src/aa-client`、`src/bridge.rs` 与 `livi-aa` 依赖。车机对 CarPlay 与 Android Auto 都只给窗口，只有 CarLife 全屏（`417dc1e`）。
- 浏览器客户端：`src/web/`、`vendor/playport` 子模块，以及 axum、axum-server、rcgen、rustls、rustls-pemfile、serde 等只服务于它的依赖（`68a4336`）。
- macOS：`wireless/macos.rs`、蓝牙 helper（`native/`）、`build.rs`，以及维持两个平台并行的 `#[cfg]` 分支（`22b4edb`）。
- 环境变量 `CP2AA_AA_CERT_DIR`、`CP2AA_AA_TCP_PORT`、`CP2AA_AA_USB`、`CP2AA_SINK`、`CP2AA_WEB_ADDR`、`CP2AA_WEB_TOKEN`，随 Android Auto 与浏览器客户端一并移除（`417dc1e`、`68a4336`）。
- 环境变量 `CP2AA_DEVICE_ID`、`CP2AA_BT_MAC`、`CP2AA_AP_MAC`、`CP2AA_TARGET_BT`、`CP2AA_NAME`、`CP2AA_BT_HELPER`、`CP2AA_HEVC`、`CP2AA_WIDTH`、`CP2AA_HEIGHT`、`CP2AA_FPS`、`CP2AA_WIDTH_MM`、`CP2AA_HEIGHT_MM`、`CP2AA_WIFI_SSID`、`CP2AA_WIFI_PASS`、`CP2AA_WIFI_CHANNEL`，分别由控制器、主机名、`CP2CL_PPI` 与 `/etc/hostapd` 取代（`a91c903`、`0c3777d`、`6240430`）。
- 设置只剩 `CP2CL_AUTH_DIR`、`CP2CL_PPI`、`CP2CL_LOG`、`CP2CL_WIRELESS`、`CP2CL_WIFI_IFACE`、`CP2CL_BT_ADAPTER`、`CP2CL_INPUT_FILE`。

### 修复

- USB gadget 的解绑写的是空值，而 `write_all` 对空切片不发起调用，内核收不到，gadget 留在控制器上致使后续绑定报 `EBUSY`；属性改以换行结尾写入（`aac2742`）。
- 不带数据的控制传输必须以一次读来结束状态阶段，否则主机的控制传输一直挂着，设备一消失即被报为请求失败（`5c588af`）。
- 两种形态之间保持 500 ms 的断开，否则主机看不见设备离开、不会重新枚举（`5c588af`）。
- 清理上一次运行留下的 gadget 时，对 configfs 自建的组做 rmdir 被拒不再算失败（`7283fd6`）。
- 无线音频的采样率被读成 44.1 kHz（实际 48 kHz），重排、`MSG_MEDIA_INIT` 与播放位置因此都慢 8.8%，队列持续增长而车机缺料；两个 AAC 位都按 48 kHz 读（`b259942`）。
- 无线音频的帧不总按时间戳到达，队列改为按时间戳插入，并丢弃重复与已经过去的帧（`b259942`）。
- 媒体帧改在到期那一刻解码，不再于一到达就整批解码，避免单个循环里做上百毫秒的工作（`fa6074c`）。
- CarLife 停止时打印完整错误链，而不是只有最外层消息（`5780acb`）。
- 无线视频补上首个视频帧的日志与「消息、帧」分开的计数；此前一路无线跑完不出一张图，日志里没有任何线索（`8e942db`）。

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
