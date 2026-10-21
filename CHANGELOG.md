# Changelog

本项目所有值得注意的变更都记录在此文件。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## 未发布

### 新增

- 导航播报经车机的语音播报通道（`MSG_NAVI_TTS_INIT` / `_DATA` / `_END`）送出，播报期间的 duck 由车机完成。
- 无线会话的导航播报按 Opus 解码，并 3:1 抽取到 16 kHz 单声道。
- 播报静音 700 ms 即补发结束消息，单轨车机的音乐不会停在暂停态。
- 车机的位置在会话建立后就开始订阅，不必等导航开始（`dda9853`）。
- 状态页新增「车载数据 · GPS」一节，车机第一次报出位置之后才显示（`96fba5a`）。
- 车机的位置交给手机当作它自己的定位，坐标按车机所说的坐标系换算（`b3a80ac`）。
- 车机失锁或拔线时把位置收回，手机不会停在最后那个点上（`7ef0cdb`）。
- 日志在终端之外同时写一份到 `/tmp/cp2cl.log`，不带颜色，写满 4 MiB 滚动一份、最多保留两份（`47ece93`）。
- 状态页改为「状态 / 日志 / 设置」三个标签页，日志页从上次读到的位置接着读（`707cc34`）。
- 日志页的每一行按终端的配色画出，时间灰、级别各有颜色（`de194a2`）。

### 修复

- 音频流的身份改用 `streamConnectionID`：同一类型的新流建立后，旧流的 `TEARDOWN` 不再把它一起关掉，"导航一开媒体就断"由此消除。
- 日志页只有一个滚动区域，页面不再跟着一起滚（`42a4b8d`）。

### 变更

- 状态页的「音频编码」改为一路一行。
- 日志副本记录所有级别，终端仍只看 `CP2CL_LOG_LEVEL` 要的那几级（`39f7f8f`）。

## v0.8.0 alpha — 2026-10-20

### 新增

- 车机图标与车型名称（`23b2843`）
- 点击图标回到车机界面（`bc62411`）
- 状态页：80 端口单页，每 2 秒轮询（`7512259`）
- 正在播放桥接到车机面板（`2e8d4db`）
- 封面超过 30 KB 时重编码后再发（`4ebb08f`）
- 状态页新增「正在播放」一节（`2e8d4db`）
- 路线引导桥接到车机诱导条（`2e8d4db`）
- 状态页显示视频的档次与级别、音频的采样率与声道（`c672853`）
- 状态页的车机一侧也显示视频与音频编码（`c672853`）

### 变更

- 资源统一收到 `assets/`（`5ab3f50`）
- 新增车型图标不再需要改代码（`5ab3f50`、`23b2843`）
- 日志变量更名为 `CP2CL_LOG_LEVEL`，可选值收窄为四级（`2f10b36`）
- 日志时间戳改为开机秒数，精确到小数点后 3 位（`a4f166f`）
- 链路断开时日志给出消息名、id 与重传情况（`1923594`）
- 只打印不认识的命令与 `SETUP`、`TEARDOWN` 的请求体（`ce12a91`、`bc62411`）
- 重新引入 axum，只开 `http1` 与 `tokio`（`7512259`）
- 状态页两段各自以会话状态打头（`da20fe7`）
- 状态页的视频编码格式改由后端返回（`da20fe7`）
- 状态页的物理尺寸改由分辨率与 `CP2CL_PPI` 算出（`da20fe7`）
- 状态页「视频编码格式」「音频编码格式」去掉「格式」（`c672853`）
- 状态页不再有左右滑动的回弹（`c672853`）
- 单包上限由 65535 字节改为 4096 字节（`f4d74d5`）
- 媒体信息的日志两端都在 `debug`（`2e8d4db`）
- 依赖的 LIVI 升到 9.4.0（`85a7aed`）
- Wi-Fi 信道与安全类型改用 `livi-wifi` 的类型，安全类型读自 hostapd 的 `wpa_key_mgmt`（`379927b`）
- WPA2 网络不再在 `0x4301` 里携带安全类型，WPA3 与过渡模式仍带（`379927b`）
- 蓝牙服务退出时把控制器改回原来的名字与可发现性（`379927b`）
- 链路前 20 个 chunk 的十六进制日志在 `debug` 级别保留（`379927b`）
- 凭据改为编入二进制，运行时凭据目录与 `CP2CL_AUTH_DIR` 一并去掉（`2ea501f`）
- 没有凭据的构建照常启动，只是无法通过认证（`2ea501f`）

### 修复

- 超过通道上限的消息在发送前丢弃（`064a09a`）
- 早于歌曲到达的封面不再被丢掉（`26dc88d`）
- 主动连接已配对但未连接的手机（`a87ecd7`）
- 手机离开网络后会话在 10 秒无响应时结束，并由蓝牙重新邀请（`5637364`）
- 已结束的会话不再关掉新会话的事件通道（`4e2fdde`）

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
