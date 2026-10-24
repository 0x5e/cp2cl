# Changelog

本项目所有值得注意的变更都记录在此文件。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## 未发布

### 新增

- 无线 CarPlay 起来后手机把 iAP2 控制通道从蓝牙搬到 AirPlay 的数据流上：`SETUP` 里 type 130、`clientTypeUUID` 为 iAP DataStream 的那条流现在会得到一个端口，`RECORD` 之后 iAP2 会话就跑在这条连接上，回程消息走事件通道的 `iAPSendMessage`（`b8f4552`）。
- 隧道在跑期间不再 page 手机，AirPlay 会话结束才恢复；手机发来 `disableBluetooth` 时主动断蓝牙链路（`b8f4552`）。
- 渠道表可按渠道号预配置分辨率、帧率与面板密度：条目的 `video` 字段（`"1920x720@30"`）在车机报上尺寸时直接存为生效尺寸，会话里只有这一份，不再与车机参数并存；`VIDEO_ENCODER_INIT_DONE` 按它应答、`GET /info` 按它渲染、送帧节奏与 `FRAME_RATE_CHANGE` 的应答也跟着它，触摸按它只做边界钳制，状态页显示的是同一个数。`ppi` 字段给出算毫米数用的密度。首批为 `20062104`（Mercedes，1920x720@30、120 dpi）。

### 变更

- 构建拆成 `make app` 与 `make firmware`，`make all` 出程序与卡镜像两样，产物都落在 `dist/`（`1797119`）。
- 卡镜像名为 `buildroot-2026.02.3-cp2cl-v0.10.0-9063c18.img`，依次是钉住的 Buildroot 版本、提交上的 tag（无 tag 即 `dev`）、提交短哈希，脏树在末尾加 `-dirty`（`1797119`）。
- 日志全在内存里：`/var/log` 挂 tmpfs，启动脚本的日志与程序的副本同在一处，卡上不再有日志写入，重启即失（`79efa5a`、`947d719`）。
- 卡镜像的 rootfs 从 64 MB 缩到 24 MB，与 8 MB 的 boot 分区合计 32 MB（`d014bb8`）。
- 触摸事件不再写日志，一次拖动就是每个指针位置一行，按键与滚轮仍在 `debug` 级别各留一行（`8747000`）。

### 移除

- 环境变量 `CP2CL_PPI` 去掉：面板密度改由渠道表的 `ppi` 字段给出，表里没有时为常量 120。
- 车机端删掉「厂商 / 型号」「描述」「序列号」三行，这三项是 CarLife 握手时报上去的，不随会话变化（`231317d`）。

### 修复

- 刷出来的卡不再带上已经删掉的文件，构建前清空 `target` 树并让各包重做安装，此前留在树里的 `S20cp2cl-ap` 让每次开机多等 10 秒（`4a21ad2`）。
- 开机停住、Wi-Fi 一直不出现，原因是卡里的 `boot.scr` 仍是根文件系统为 squashfs 时生成的那份，内核按它挂根失败；现在每次构建都从 `boot.cmd` 现生成（`0a2c803`）。

## v0.10.0 alpha — 2026-10-24

### 新增

- 固件改由本仓库构建：mainline 内核与 U-Boot、板载 Wi-Fi 与蓝牙、可写的 ext4 根，`make firmware` 出卡镜像（`1b42a36`）。
- 状态页「设备端」新增固件版本与内核版本，位于内存占用率与软件版本之间；前者取 `os-release` 的 `NAME` 与 `VERSION_ID`（`Ubuntu 24.04`、`Buildroot 2024.02.1`、`ImmortalWrt 23.05.3`），后者取内核自己的发布串（`da0f8ba`）。
- 镜像里预置了构建它的机器的公钥，进板子不必再输口令（`5e9c2c7`）。

### 变更

- 程序的身份写在 `/var/lib`，重刷镜像后手机认的仍是同一个配件（`1b42a36`）。
- 删除 `board/`、`install.sh` 与两个 systemd 单元，不再往厂商 Ubuntu 上装文件（`1b42a36`）。
- 接入点启动脚本 `S20cp2cl-ap` 改名 `S20ap`（`71077e4`）。
- 车机静默时日志能分清「没打开管道」和「打开了不说话」，`usb.enumerated` 带上 `speed`/`state`，管道开启 2 秒仍无消息就写一帧心跳并记下是否被取走（`662a073`）。
- `CP2CL_LOG_LEVEL=debug` 时能看到车机发来的前 8 帧原始字节，非命令通道与未识别的消息类型也在其中（`662a073`）。
- 前 8 帧改记前 64 字节，日志键 `head=` 改 `body=`，一次运行就能看出消息头与消息体的分界（`483e9c7`）。
- `dr_mode = "otg"` 的板子上 OTG 口能被主机看到了，组装 gadget 前会把控制器的 `mode` 写成 `peripheral`（`0d785ec`）。
- 构建时工作区不干净，版本串在提交短哈希后面带 `-dirty`，如 `v0.9.1 (a12935f-dirty)`（`4f8ecb0`）。
- 状态页标题改为「cp2cl」（`9788192`）。
- 无线会话在等车机开管道期间日志有一行，启动时也先报出运行模式与无线参数，静默不再长得像失败（`066033e`）。
- root 不再设密码，进板子只认那把公钥（`f574b46`）。

### 修复

- 切到 accessory 形态只改 VID/PID，manufacturer、product、serial 保持本机自己的，与真机在同样切换下的表现一致（`dc6bb77`）。
- 组装 gadget 中途失败不再留下注册着的 FunctionFS 函数；此前这一次失败会让之后每次启动都报 `Directory not empty`（`067bf27`）。
- 控制器被别的 gadget 占住时，日志不再断言那是上一次留下的，configfs 里没人认领就写明没人认领（`5f44fe3`）。
- 读车机端点改为常驻 8 个异步读请求，接收 FIFO 里的包不再被下一次入队冲掉（`ef94991`）。
- 什么都没改也整份重编，`build.rs` 现在只监视 `HEAD`、`refs/heads`、`refs/tags` 与 `packed-refs`，`git status` 每次重写的 `.git/index` 不再算作变更（`5a46957`）。

## v0.9.1 alpha — 2026-10-21

### 变更

- 构建改为 fat LTO、单 codegen unit、strip 符号与 panic 直接 abort，可执行文件 14.4 MB → 6.8 MB；日志级别也改为直接解析，非法值回落到 info（`c97551e`）。
- 访问点改用信道 11、DTIM 1、20 MHz 短保护间隔，关掉组密钥轮换与低 ACK 断连（`e1397a2`）。

### 修复

- 手机挂断 AirPlay 连接后主动请求新会话，Wi-Fi 与蓝牙仍连着却没有画面的状态不必再拔线；流结束时也分清「对端关闭」与「连接被打断」（`00fa8bf`）。
- 车机停车时按固定频率重发同一个位置，日志被刷满；现在只在参数变化时打一行（`2af10b4`）。

## v0.9.0 alpha — 2026-10-21

### 新增

- 导航播报经车机的语音播报通道（`MSG_NAVI_TTS_INIT` / `_DATA` / `_END`）送出，无线会话按 Opus 解码后降到 16 kHz 单声道，播报静音 700 ms 即补发结束消息（`40d93d4`）。
- 车机的位置在会话建立后即订阅，按车机所说的坐标系换算后交给手机当作它自己的定位，状态页也单列一节（`dda9853`、`96fba5a`、`b3a80ac`）。
- 车机失锁或拔线时把位置收回，手机不会停在最后那个点上（`7ef0cdb`）。
- 状态页新增「设备端」一节，显示 CPU 占用与内存，每 5 秒采一次（`99ef751`）。
- 日志在终端之外同时写一份到 `/tmp/cp2cl.log`，不带颜色、记录所有级别，写满 4 MiB 滚动一份、最多保留两份（`47ece93`、`39f7f8f`）。
- 状态页改为「状态 / 日志 / 设置」三个标签页，日志页从上次读到的位置接着读，每行按终端的配色画出（`707cc34`、`de194a2`）。
- 日志页可以按级别筛选、关掉自动换行，也能把磁盘上留着的日志整个下载下来（`f3a9853`、`343f421`、`855702e`）。
- 状态页的「设备端」多一行软件版本（tag 加提交短哈希），没有打 tag 的提交显示 `dev`，启动时日志里也打一行（`e4b0e92`、`0ebbafb`）。

### 变更

- 日志改为事件式，每行是 `area.subject_outcome` 加键值字段（`d572f48`）。
- 状态页的「音频编码」改为一路一行（`40d93d4`）。
- 编不进去车机的认证私钥时跳过认证而不是当作失败，车机每半分钟重问一次、流照常（`62694de`）。
- 构建改为 `make all` 与 `make test`，内部仍是原来的交叉编译容器（`e70a8a4`）。

### 修复

- 同一类型的两条音频流不再互相覆盖，导航一开媒体就断的问题随之消除，流的身份改用 `streamConnectionID`（`40d93d4`）。

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
