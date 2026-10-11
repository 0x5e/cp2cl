# AA 车机对接 — 开发计划与技术方案

> **状态变更（2026-10-10）：AA 方向终止，本文档的实现部分描述的是已被删除的代码。**
> 车机是奔驰 MBUX NG6，有线支持 CarPlay / CarLife / Android Auto，但**只有 CarLife 全屏**——CarPlay 与 Android
> Auto 都被限制在非全屏窗口，桥到 AA 没有收益 ⇒ 目标改为 **CarLife**，项目更名为 **`cp2cl`**。
> 本轮已删除 `src/aa-client/`、`src/bridge.rs`、`sink.rs` 的 AA 出口、`SINK` 开关、`Control` 会话门控与
> `/info` 的 `audioFormats` 裁剪。
>
> **本文档保留作 CL 方案的技术底稿**：§0.1（角色与传输）、§0.2（硬件前提）、§3（协议核对方法）与 §4 的
> 「仍待硬件确认」项仍可复用。已过时的部分：§1/§4/§5/附录 A 里「已实现」的位置标注（代码已不存在）、
> 环境变量名（当时是 `CP2AA_*`，现在是 `CP2CL_*`）。§0.2 的系统结论已按当前平台更新（FriendlyElec 官方
> Ubuntu，4.14 BSP 内核，交叉编译 `armv7-unknown-linux-gnueabihf`）。

> Linux 设备通过 USB 连接 Android Auto 车机，扮演 Android 手机角色；iPhone 通过 CarPlay 连接同一台设备。  
> 音视频参数由车机侧上报的能力决定，可协商项不硬编码，且不得在单次会话中途变更。

状态：2026-10-08 定稿，**Phase 0 已结清、Phase 1–2 的代码与 Phase 3 的代码侧改动已实现，等待上板验证**。范围：
AA 仅走 USB（TCP 入口删除）；不支持 `CP2AA_SINK=web,aa` 同时启用；验收包含音频；车机会话决定 CarPlay 会话的
生命周期，车机断开立即终止，不加宽限。验证节奏：现有板分两次单独验证（host 模式测 CarPlay、gadget 模式测 AA），
NanoPi Duo2 到货后做端到端。

实现落点：`§4` 的每一项都标了「已实现」并给出代码位置，`§4` 末尾的悬置项已逐条结清；`§5` 按 Phase 0–4 给出
每阶段的落点、验收与**尚未完成的部分**；`附录 A` 是 Phase 0 的核对表（文件:行）。仍未验证的是硬件本身：
`usb.rs` 的 UDC/端点顺序（D20）、车机等待推迟 Setup 的容忍度（D19）、以及真车是否接受直接枚举为 accessory
（§6 风险 5）。

---

## 0. 硬件平台与对端

### 0.1 现有板（NanoPi Duo2 到货前）

高通 410 随身 Wi-Fi（msm8916，postmarketOS）只有一个可切换角色的 USB 控制器  
（`/sys/class/usb_role/ci_hdrc.0-role-switch/role`），两个角色互斥，因此分两次单独验证：

| 模式     | 接什么                   | 验证内容                | 媒体源   |
| ------ | --------------------- | ------------------- | ----- |
| host   | iPhone                | CarPlay 链路（经浏览器客户端） | 手机    |
| gadget | 车机（真车，或跑 LIVI 车机端的机器） | AA 链路               | 测试源注入 |

切换是运行期写 `usb_role` 节点，重启失效；切到 gadget 前先向 `UDC` 写入空值解绑板子自带的 gadget。gadget  
模式下没有 iPhone，AA 侧的媒体由测试源提供（Phase 3 第 3 步）。

### 0.2 NanoPi Duo2 + 扩展板（到货后）

**NanoPi Duo2 + 扩展板**（SoC Allwinner H3，4×Cortex-A7 @1.2GHz，512MB DDR3，板载 AP6212 WiFi/BT4.0）。  
USB 侧有两个相互独立的控制器，这是端到端的前提：

| 用途           | 本设备角色  | 对端             | 控制器                           |
| ------------ | ------ | -------------- | ----------------------------- |
| 有线 CarPlay   | host   | iPhone（device） | H3 USB Host（EHCI/OHCI），经扩展板引出 |
| Android Auto | gadget | 车机（host）       | USB0 的 musb OTG 控制器           |

两者不共用控制器 ⇒ iPhone 与车机可同时接入，端到端在同一块板上完成。

**系统用 FriendlyElec 官方 Ubuntu（4.14 BSP 内核，systemd）。** H3 = Cortex-A7，**只有 32 位** ⇒ 交叉编译
目标是 `armv7-unknown-linux-gnueabihf`。Armbian 没有这块板的现成镜像。功能全部调通后再用 Buildroot
自行打包固件。

**供电取自车机 USB 口（2026-10-08 定）。** Duo2 的 USB-C 是 DRP 兼电源输入，厂商标称 5V/2A。实测不足时
改由扩展板独立供电，OTG 只走数据。

**上板第一步先确认 gadget 能力。** 以下三项缺任何一项，USB gadget 都无从谈起，不要留到上板最后再查：

- `ls /sys/class/udc/` —— 有 UDC 节点，且名称就是 FunctionFS 要写入 `UDC` 的那个。
- `zcat /proc/config.gz | grep -E "CONFIG_USB_CONFIGFS|USB_CONFIGFS_F_FS|CONFIG_USB_MUSB"` —— 内核的  
  configfs、FunctionFS 与 musb OTG 驱动都已启用。
- `mount | grep functionfs` —— `usb.rs` 依赖的挂载点存在。

### 0.3 对端车机

车机有两个来源：

- **真车**：最终验收对象。
- **LIVI 车机端**：同一份 `livi-aa` 也实现车机侧，其 USB watcher 在 mac 与 Linux 两个入口都无条件启动  
  （`mac_main.rs:388`、`linux_main.rs:404`），经 `nusb` 访问总线上的 AOA 设备。把 `livi-helperd` 跑起来  
  就是一台上位对端，不需要额外硬件。它的 `ACCESSORY_PIDS` 含 `0x2d00`（`livi-aa/src/usb.rs:25`），正是本  
  设备枚举出的 PID ⇒ 本设备被直接当作 accessory 并打开 bulk 管道，即「跳过 AOA 协商」在这个对端上成立。

两个对端的用途不同：LIVI 用于把本设备一侧的问题与车机差异分开（它同时是字段用法的参考实现），真车用于最终  
验收。

---

## 1. 现状

| 部分                                            | 状态                                                                              |
| --------------------------------------------- | ------------------------------------------------------------------------------- |
| `aa-client/{frame,proto,control,av,input}.rs` | 移植自 LIVI `livi-aa`（车机侧实现），已改写路径，有单元测试，**未在真实硬件上运行**                             |
| `aa-client/tls.rs`                            | TLS 1.2 **server**，使用 AACS 的 `android_auto.crt/key`，两端均不校验证书                    |
| `aa-client/session.rs`                        | 状态机已走通：版本 → TLS → ServiceDiscovery → ChannelOpen → 延迟 Setup → focus → 媒体；多音频通道各自独立 |
| `bridge.rs`                                   | 桥接：车机 ServiceDiscovery → `screen` → iPhone `/info`；车机输入 → CarPlay HID；观测码流后定码            |
| `aa-client/usb.rs`                            | configfs + FunctionFS 的 **gadget**，枚举为 AOA accessory（0x18D1/0x2D00），**未在硬件上运行** |
| CarPlay 侧                                     | 有线链路已跑通（`v0.6.1`）；会话由车机会话启动与终止（`session::Control`），浏览器显示面板仍用同一机制             |

**协议栈骨架已具备，Phase 0–3 的改动已落地（见 §4），未完成的是硬件验证。**

---

## 2. 会话生命周期：由车机会话驱动

### 2.1 三条规则

1. **CarPlay 会话以车机会话为前提。** 不允许车机未连接时先建立 CarPlay 会话。
2. **车机会话结束 ⇒ CarPlay 会话立即终止**，不等待、不重连；车机重新接入时重新建立。
3. 车机已连接、iPhone 未连接 ⇒ 继续等待 iPhone（现有实现为每秒轮询），车机会话不受影响。

### 2.2 时序

```
车机接入
  └─ AA 会话：version → TLS → ServiceDiscovery        ← 由车机侧发起
        ├─ 解析车机上报的能力 → 排序得出「最佳参数」       §3.1
        ├─ 写入 screen（/info 的响应内容在此确定，之后不再变更）
        ├─ 打开 AA 通道（ChannelOpen，暂不 Setup）        §3.3
        └─ 建立 CarPlay 会话 ──▶ iPhone 按该参数编码
                                 │
        AA 通道 Setup → Start ◀──┘   待 iPhone 实际编码确定后再发
              └─ 媒体开始传输（不转码）

车机断开
  └─ 终止 CarPlay 会话（不重启、不等待）
```

### 2.3 改动位置

- **`restart` 广播的载荷改为枚举。** `Ended` 现有 `Finished`/`Restart`/`Stop`，`run_bringup(…, changed)` 接收
  `broadcast::Receiver<Signal>`；`Signal::{Restart, Stop}`（`session.rs:71`）经 `Ended::Stop` 从 `attach` 返回。
  该路径已被验证（浏览器显示面板切换分辨率即使用此机制），未新增机制。`wireless/mod.rs::run_session` 使用同一个
  `run_bringup`，已同步改为 `broadcast::Sender<Signal>`。
- **CarPlay 侧增加启动条件判断。** `session::watch(cfg, auth, airplay_port, control: Arc<Control>)`
  （`session.rs:220`）在 `control.live()` 为假时清空 `busy` 并只 sleep，不扫描 iPhone；由关转开时若会话正
  在运行，`Control::stop()`（`session.rs:110`）广播 `Signal::Stop`，会话走 `close_streams()` 后结束。
- **`Bridge` 持有该控制句柄。** `Bridge.control: Arc<Control>`（`bridge.rs:112`），`Bridge::new` 接收它；
  `discovered()` 末尾 `self.control.start()`（`bridge.rs:347`），`serve()` 结束时 `self.control.stop()`
  （`bridge.rs:238`）。
- **`main.rs`**：`CP2AA_SINK=aa` 时由桥驱动（`Control::new()` 不预启动）；`SINK=web` 时无条件
  `control.start()`，车机不参与，浏览器面板仍是唯一触发方。

### 2.4 车机断开的判定

- **物理断开／车机断电**：bulk IN 读到 EOF ⇒ `session::serve` 返回 Err ⇒ 现有路径即可终止  
  （`bridge.rs:124` 的 `events.recv() → None → break`）。
- **链路静默但未断开**（车机侧无响应）：需增加空闲超时检测（连续若干秒未收到任何控制消息与输入即判定断开）。  
  是否必要待实测，见 D19。
- **重新接入后能否复用 `connect()`**：`usb.rs:153` 重新打开 `ep1`/`ep2`。FunctionFS 在 host 重新枚举后是否
  需要重写 `ep0` 描述符**尚未验证**；稳妥做法是在每次车机会话结束时解绑并重新绑定 gadget。已按稳妥做法实现：
  `Gadget::rebind()`（`usb.rs:178`）先向 `UDC` 写空值、睡 200ms、写回原名、再睡 300ms，`watch()`
  （`usb.rs:63`）在每轮会话结束后调用。见 D20。
- **不加抖动宽限**（2026-10-08 确认）：车机自身重启 USB 时同样按断开处理，CarPlay 会话立即终止，待车机重新  
  枚举后重建，不做延迟等待。宽限需要额外的定时器与状态保持，立即终止的实现更简单，且与规则 2 一致。

---

## 3. 协商：车机侧决定取值，iPhone 侧决定交集

### 3.1 第一步：车机能力 → `/info`

排序规则：**视频编码 H.265 > H.264；分辨率 1080p > 720p > 480p；帧率 60 > 30；音频 PCM > AAC**。

| 项    | 车机侧上报字段                                          | 影响 CarPlay 侧                           | 影响 AA 侧                                |
| ---- | ------------------------------------------------ | -------------------------------------- | -------------------------------------- |
| 视频编码 | `available_type`：3=H.264、7=H.265                 | `/info` 是否携带 `hevcInfo`（`info.rs:82`）  | `AVChannelSetupRequest` 的 codec        |
| 分辨率  | `VideoConfig.resolution` 1/2/3                   | `displays[0].widthPixels/heightPixels` | `av_start_indication` 的 config 下标、触摸换算 |
| 帧率   | `VideoConfig.fps` 1/2 = 30/60                    | `displays[0].maxFPS`                   | —                                      |
| 物理尺寸 | `VideoConfig.dpi`（含 margin）                      | `widthPhysical/heightPhysical`         | —                                      |
| 音频编码 | audio `available_type`：1=PCM、2=AAC-LC、4=AAC-ADTS | `audioFormats` 位图**仅保留该编码族**           | 音频 `AVChannelSetupRequest`             |
| 音频参数 | `AudioConfig{sample_rate,bits,channel_count}`    | 同上                                     | 校验，见 §3.3                              |
| 音频类型 | `AudioType`（3=media）+ 通道号                        | —                                      | 每路音频写入哪条通道                             |
| 按键   | `input.buttons[]`                                | HID 不变                                 | `InputChannelHandshakeRequest` 过滤      |
| 触摸面  | `InputService.TouchConfig` 尺寸                    | —                                      | 触摸换算的除数                                |

两点需要说明：

- **音频是「只声明一种」而非「交由 iPhone 选择」**：`info::audio_formats()` 当前同时给出 PCM 与 AAC-LC  
  （`info.rs:176-201`），由 iPhone 选择。应改为按车机接受的编码族裁剪该位图——车机优先 PCM 时只保留 PCM。
- **`dpi` 需进入 `/info`**：当前 `widthPhysical/heightPhysical` 仅来自 `CP2AA_WIDTH_MM`（`server.rs:658`），  
  车机上报的 dpi 未被使用。`ScreenParams` 需携带毫米尺寸（D9）。

音频通道号可参考 LIVI 车机侧的取值：`CH_MEDIA_AUDIO=4`、`CH_SPEECH_AUDIO=5`、`CH_SYSTEM_AUDIO=6`  
（`livi-aa/src/consts.rs:17-19`）。这是 LIVI 的分配，真车按它 ServiceDiscovery 上报的通道号为准。

### 3.2 第二步：iPhone 的实际取值

| 项       | 观测点                                                                                                       |
| ------- | --------------------------------------------------------------------------------------------------------- |
| 视频编码    | 首个 `StreamEvent::VideoConfig`：`codec_data` 为 avcC ⇒ H.264，hvcC ⇒ H.265（`screen.rs:135`）                   |
| 音频编码与参数 | AirPlay 流 SETUP 的 `audioFormat` → `AudioCodec::from_format`（`server.rs:1024`）→ `StreamEvent::AudioConfig` |
| 分辨率     | **不校验**。`VideoConfig` 事件不含尺寸（需解析 SPS 才能得到），而 `/info` 属于请求项，iPhone 会按该响应执行；即使不符也只是车机侧拉伸画面，不会黑屏。            |

**这两项观测经 `AaOutlet` 的广播通道送到桥。** `AaSink::push`（`sink.rs:419-509`）在转发 parameter sets 与
帧之外，把 `VideoConfig`/`AudioConfig` 一并包成 `Observed`（`sink.rs:193`）发出；`AaOutlet::observe()`
（`sink.rs:265`）是订阅端，桥在 `bridge.rs:203-221` 收集进 `Room`（`bridge.rs:65`）。广播而非队列：桥重启后
不需要清理上一轮的积压。见 D10。

### 3.3 关键改动：推迟 AA 通道 Setup

AA 的通道流程为：

```
ChannelOpenRequest → ChannelOpenResponse → AVChannelSetupRequest(codec) → AVChannelSetupResponse
   → 焦点 → AVChannelStartIndication → 媒体
```

**第二条消息由本设备主动发送，因此可以延后。** 顺序改为：

1. ServiceDiscovery 之后打开通道（此步不带 codec，无信息量）；
2. 等待 §3.2 的两项观测；
3. 观测到后 Setup（codec 与 config 下标均与实际一致）→ 焦点到达后 start → 媒体开始传输。

不采用「先按车机最佳参数 Setup、iPhone 不匹配时重启车机会话」的原因：需要重建整条 AA 会话——在 USB 上等同  
于一次物理重新枚举，车机会提示断开，恢复需要数秒。推迟 Setup 则无需重建，代价仅为车机侧等待若干秒（此期间  
车机显示自身的等待界面）。

**降级路径**：观测等待超时（iPhone 未接入或未开始推流）时，按 N 秒（暂定 5s）以车机最佳参数 Setup——此时  
无媒体可发，声明与实际不符不产生影响。

状态机注意事项：`opened` 与 `set_up` 之间没有挂起状态——`start_av`（`session.rs:761`）要求 `opened &&
set_up` 同时成立才发 `AV_START_INDICATION`，因此 Setup 迟到不会误启动。原 `SETUP_TIMEOUT` 已改为**进展超时**
（`session.rs:30`，30 秒）：每收到一条消息或一帧媒体都重置 `deadline`（`session.rs:248`、`session.rs:255`），
只有车机彻底静默才判定失败；`Phase::Running` 在**视频通道真正 start 时**置位（`session.rs:783`），而不是在 focus
指示到达时——focus 与 SetupResponse 是两条独立消息，车机可以任意顺序发出，先到的那个不算完成。见 D3。

### 3.4 残留情况：何时需要真正重建

iPhone 在单次车机会话中途更换编码（更换手机、CarPlay 重连）时，车机侧的 codec 已经声明过。需要确认 AA 是否  
存在「停止后重新 Setup」的轻量路径（`AVChannelStopIndication` 后能否重新 Setup，见 D22），否则只能解绑/重绑  
`UDC`（等同于物理重新枚举）。**本轮不实现，仅记录日志**，待 D22 有结论。

### 3.5 不做的事

- **不转码、不重采样。** 参数不匹配时降级或直接丢弃该路数据，不在本设备转换。
- **不在单次会话内变更参数。** 车机侧通过推迟 Setup 实现，iPhone 侧通过会话建立时确定实现。
- **不支持 `CP2AA_SINK=web,aa` 同时启用。**
- **AA 只有 USB gadget 一条传输路径**：无线 Android Auto 不做；TCP 入口已删除（`Config::aa_tcp_port`、
  `CP2AA_AA_TCP_PORT`、原 `bridge.rs` 的 accept 循环全部移除），`Bridge::run` 在非 Linux 目标上返回错误
  （`bridge.rs:145-158`），见 D15。

---

## 4. 缺陷清单

### 必须改（全部已实现）

- **D1 CarPlay 会话由车机会话控制。已实现。** `session::Control`（`session.rs:85`，`new`/`start`/`stop`/`live`/
  `sender`/`subscribe`）与 `Signal::{Restart,Stop}`（`session.rs:71`）；`Ended::{Restart,Stop}` 经
  `run_bringup`（`session.rs:406`）→ `attach`（`session.rs:273`）→ `watch`（`session.rs:220`）传递；
  `Bridge.control` 接线见 §2.3。
- **D2 车机断开即终止 iPhone 会话。已实现。** 物理断开：bulk IN 读到 EOF ⇒ `serve` 返回 Err ⇒
  `bridge.rs:195-202` 的 `events.recv()` 得到 `None` 而 break ⇒ `control.stop()`。静默断开的容忍度见 D19——
  不另加空闲超时，由 `session.rs:30` 的 30 秒进展超时承担；重枚举由 `Gadget::rebind()` 承担（D20）。
- **D3 推迟 AA 通道 Setup。已实现。** Setup 由 `apply_choice`（`session.rs:553`）在收到桥的选择后才发，
  不是「新增一个挂起状态」而是「`channels_open` 与 `setup_sent` 两个布尔 + `CodecChoice` 的等待」；
  `Choice`（`session.rs:42`）是桥与会话之间的唯一载荷。`SETUP_TIMEOUT` 改为进展超时，`Phase::Running` 在
  视频通道真正 start 时置位。见 §3.3。
- **D4 音频编码不再硬编码为 PCM。已实现。** `control::setup_request_audio(codec)`（`control.rs:87`）接收
  codec；由 `choose`（`bridge.rs:511-533`）按 `AudioOffer.codec` 与观测值的交集决定；`audio_codec`
  （`bridge.rs:541`）对 Opus 返回 `None` ⇒ 该通道不 Setup（AA 无 Opus 编号）。
- **D5 `/info` 的 `audioFormats` 按车机能力裁剪。已实现。** `info::AudioFormats{pcm, aac_lc, opus}`
  （`info.rs:183`）三族掩码（`mask()`，`info.rs:210`，PCM `0x0000_ffff`、AAC-LC `0x00c0_0000`、
  Opus `0x7000_0000`）；`audio_formats(rate, allow)`（`info.rs:225`）改为 9 条表驱动，输出与输入位图都被裁，
  两处都为零的条目整条丢弃（不发空 offer）；桥由 `offered_audio`（`bridge.rs:557`）从车机实际列出的 codec
  推出该掩码。单测三条在 `info.rs`，一条在 `bridge.rs`。
- **D6 视频编码取交集。已实现。** `choose`（`bridge.rs:511-523`）：有观测用观测值，无观测用车机最优（与
  `/info` 已请求的一致，因此两者在手机就绪时自然吻合）；交集为空 ⇒ `Choice.video = None` ⇒ `apply_choice`
  不 Setup 该通道并 `error!`（`session.rs:570-574`），不发送无效声明。
- **D7 视频时间戳保留本地单调时钟（2026-10-08 定）。** 原计划改用 CarPlay 侧时间戳，核对后不可行：
  `AirPlayScreenHeader` 的 presentation timestamp 在参考实现中没有任何读取点，没有可对照的取值语义。
  音频继续用手机 RTP（`sink.rs:497`，`StreamEvent::AudioPacket.timestamp_us`），视频用进程内单调时钟
  （`sink.rs:530` 的 `now_us`）。单位是微秒，与 AACS 的 `buffer->pts / 1000`
  （`../AACS/AAServer/src/VideoChannelHandler.cpp:38`）一致。后果：音视频不同源，存在固定偏差，属已知现象
  （风险 7），不是新增缺陷。
- **D8 `av_start_indication` 的 config 下标。已实现。** `AvState.config` 记下
  `AVChannelSetupResponse.configs[0]`（`session.rs:635`），`start_av` 用它填
  `control::av_start_indication(0, config)` 的 field 2（`session.rs:790`）。
- **D9 `ScreenParams` 增加毫米尺寸。已实现。** `ScreenParams.width_mm/height_mm`；`bridge.rs:288-291` 用车机
  `VideoConfig.density()`（优先 field 9 `real_density`，`control.rs:223`）换算，`millimetres`
  （`bridge.rs:570`）按 `px*254/(dpi*10)`；`info.rs::display_entry` 优先用它，缺失时回退 200mm 等比。
  单测 `millimetres_come_from_pixels_and_density`（D21 的换算式即此）。
- **D10 观测通道。已实现。** `Observed`（`sink.rs:193`）+ `AaShared.observed` 广播（`sink.rs:220`）+
  `AaOutlet::observe()`（`sink.rs:265`）；桥在 `bridge.rs:203-221` 用 `Room::record`（`bridge.rs:78`）收集，
  中途换编码只 `warn!` 一次（D22）。
- **D11 流控。已实现。** `AvState{window, outstanding, ack_seen}`（`session.rs:173-200`）；`full()`
  （`session.rs:197`）只在 `ack_seen` 为真时生效——从不 ack 的车机不会把通道永久卡死；`media_blocked()`
  （`session.rs:274`）只阻塞媒体分支，ack 与控制消息继续流动；`AVMediaAckIndication` 在 `on_ack`
  （`session.rs:670`）里扣减 `outstanding`。
- **D14 音频按 `AudioType` 分派到对应通道。已实现。** `aa_audio_type(name)`（`sink.rs:201`）把 CarPlay 的
  `media`/`speechRecognition`/`telephony`/`alert` 映到 AA 的 3/1/4/2；`Negotiated.audio`（`sink.rs:163`）
  只记真正上线的通道；`AudioRoute`（`sink.rs:310`）记住每个 stream 的路由，`channel_of`（`sink.rs:380`）
  按 `AudioType` 取通道。四个 `AUDIO_TYPE_*` 常量在 `control.rs:129-132`，`Session.audio` 是每路一个
  `AvState`（`session.rs:214`）。
- **D15 删除 AA 的 TCP 入口。已实现。** `Config::aa_tcp_port` 与 `CP2AA_AA_TCP_PORT` 已删；原 accept 循环
  删除；`CP2AA_AA_USB` 一并删除；非 Linux 目标 `Bridge::run` 返回错误（`bridge.rs:145-158`），该目标上的
  测试直接调用 `Bridge::serve`（泛型 `RW`，用 `tokio::io::duplex` 喂假车机）。新增
  `CP2AA_AA_TEST_SOURCE` 作为 gadget 模式下没有手机时的媒体注入开关（`sink::test_source`，`sink.rs:554`）。
- **D16 音频采样参数校验。已实现。** `AaSink::format_matches`（`sink.rs:393`）：rate/channels 与车机
  `AudioConfig` 不一致即丢弃该路，不重采样；结果记进 `AudioRoute.matches`，每个 stream 只报一次
  （`AudioPacket` 分支在 `sink.rs:485` 静默返回）。

### 后续（不在 Phase 0–3 范围）

- **D12 input handshake 过滤按键。** 已按车机上报的 `buttons` 发起 handshake（`session.rs:692`），但车机发来的
  仍可能超出该集合；`key_command`（`bridge.rs:579`）只翻译认识的键，其余丢弃。够用，不再收紧。
- **D13 触摸 Clamp 参照错误。已实现。** `touch_command`（`bridge.rs:428`）的 clamp 依次取车机 input service
  上报的 `TouchConfig` 尺寸 → `screen` → `cfg`（`bridge.rs:436-448`）；`input.screen` 在 `discovered()`
  里被记下（`bridge.rs:325-327`）。

### 已核实（2026-10-08 对照参考实现逐条结清）

参考实现：`../AACS/AAServer/src`（**手机/MD 侧**，`sendToHeadunit`，即本设备扮演的一方）、
`../AACS/AAClient/src`（**车机侧**，`sendToMobile`）、`../AACS/include/enums.h`（消息号）、
`vendor/LIVI/native/livi-helperd/crates/livi-aa/src`（Rust 车机侧实现，且是 §0.3 的对端）。

- **D17 video focus 方向：原判断方向正确，接收即可。** AACS 手机侧的
  `VideoChannelHandler::handleMessageFromHeadunit` 在收到 `MediaMessageType::VideoFocusIndication`
  （`= 0x8008`，`enums.h:46`）时直接 `sendStartIndication()`（`AAServer/src/VideoChannelHandler.cpp:181`）；
  车机侧则在收到 `SetupRequest` 后 `sendSetupResponse()` → `sendVideoFocusIndication()`
  （`AAClient/src/VideoChannelHandler.cpp:32-33`）。⇒ focus 由车机发出，手机收到后才发 Start，与
  `session.rs:654` 的写法一致；**不存在也必须不存在**「本设备先发 VideoFocusIndication」这一步。
  附带的顺序结论：焦点正常情况下跟在 SetupResponse 之后，但两者分属独立消息，实现仍按「任一方完成配对即
  start」处理（`session.rs:783`）。
- **D18 视频 media 载荷：一条消息 = 一个 access unit，整段附带，不按 NAL 拆分。** 手机侧
  `AAServer/src/VideoChannelHandler.cpp:33-46` 每个 appsink buffer 就是一条消息——`pushBackInt16` 消息号
  （`MediaIndication`，有 pts 时用 `MediaWithTimestampIndication` 并 `pushBackInt64`）之后
  `copy(map.data, map.data + map.size, ...)` 整段原始字节流直接追加；车机侧 `AAClient` 收到后
  `pushDataToPipeline(0, vector(data.begin() + 2, data.end()))`（同文件 `:39`）即剥掉 2 字节消息号后整段送
  解码器。⇒ 起始码不剥离也不重加，SPS/PPS 由手机侧自己保证在需要时出现；本设备单独先发 parameter sets
  与之一致，`sink.rs:434-437` 保留。
- **D23 ack 方向：由车机（接收方）发送，手机不必回复。** `AAClient/src/VideoChannelHandler.cpp:40`、`:50`
  在收到 `MediaIndication` / `MediaWithTimestampIndication` 后调用 `sendAck()`，其载荷是
  `MediaAckIndication` + `0x08 0x00 0x10 0x01`（field 2 = 1 ⇒ `ack_count = 1`，同文件 `:161-167`）；
  手机侧只接收（`AAServer` 对 `MediaAckIndication` 无动作）。`receive_timestamps` 在 AACS 中**没有任何
  使用点**（全仓 grep 无匹配）⇒ 不必回复，当前「收到即扣减窗口」正确。
- **D21 尺寸换算式：`millimetres(px, dpi) = px*254/(dpi*10)`，已并入 D9 实现。** 1280px@160dpi = 203mm，
  与 AACS 手机侧经 `VideoConfig` 上报 dpi 的语义一致（`control.rs:223` 的 `density()` 优先取 field 9
  `real_density`，回退量化过的 field 5）。`margin_width/margin_height` 未参与换算：`/info` 的
  `widthPhysical` 是整屏物理尺寸，margin 是画面在屏上的留边，两者不同层。
- **D7 判据补充（结论见「必须改」D7）：** 参考实现的时间戳来自 GStreamer buffer 的 ns 级 pts，除以 1000
  得微秒（`AAServer/src/VideoChannelHandler.cpp:38`）——这与本设备 `AV_MEDIA_WITH_TIMESTAMP` 使用的微秒
  单位一致，但本设备手上没有等价的源（AirPlay 画面头的 timestamp 无参考读取点），因此保留本地单调时钟。

### 仍待硬件确认

- **D19** 推迟 Setup 是否会被车机判定超时：AACS 车机侧未发现有看门狗（`gotSetupResponse` 只是一个布尔），
  但真车可能不同，且 5 秒兜底（`bridge.rs:53`）是否合适需实测。
- **D20** FunctionFS 在 host 重新枚举后能否继续使用同一组 `ep1`/`ep2` 文件；已按稳妥做法实现
  `Gadget::rebind()`（`usb.rs:178`），但 200ms/300ms 的等待是否够需上板量。
- **D22** 中途更换 codec 是否存在轻量路径（`AVChannelStopIndication` 后能否重新 Setup）：AACS 未见此用法，
  暂按「不可」处理，只记日志（`bridge.rs:205-211`）。


## 5. 分阶段与执行顺序

每阶段结束都能编译与测试；Phase 0–3 的代码已落地，Phase 3 的**上板动作**与 Phase 4 待硬件。

### Phase 0 — 协议核对（只读，不改动代码）· 已完成

将 D17/D18/D19/D20/D21/D22/D23 连同 §3.1 的全部字段编号，逐条对照 `../AACS/proto` + `../AACS/{AAServer,AAClient}/src`
+ `livi-aa/src` 确认，产出核对表（每条注明 文件:行），结论并入本文件的「附录 A」。

对端不必另找模拟器：`livi-aa` 自身就是车机侧实现，且 mac 入口无条件启动 USB watcher（§0.3）。Phase 0 只核对
它的字段用法与消息方向，不评估第三方模拟器。

验收：每条都能指出出处；D17–D19 有明确结论。

**结果：** D17/D18/D21/D23 已结清（结论见 §4「已核实」，出处见附录 A.4）；**D19/D20/D22 依赖硬件，未能在
Phase 0 得出结论**，已转为 §4「仍待硬件确认」，其中 D20 按稳妥做法先行实现（`Gadget::rebind()`）。

### Phase 1 — 生命周期改造（不涉及协商）· 已实现

按 D1/D2 修改：由车机会话建立/终止 CarPlay 会话、断开即停、启动条件判断。**不依赖硬件**：假车机使用内存
duplex，CarPlay 侧使用假会话，验证目标是「会话在何时被建立与终止」。

落点：`Ended::{Finished,Restart,Stop}` 与 `Signal::{Restart,Stop}`（`session.rs:58`/`:71`）；`Control`
（`session.rs:85`）；`watch` 的 `live` 门控（`session.rs:232`）；`Bridge.control` 与 `main.rs` 接线（§2.3）。

验收：单元测试中车机会话建立 ⇒ CarPlay 会话建立；车机断开 ⇒ CarPlay 会话终止且不再自动重建；车机再次接入
⇒ 重新建立。

**当前状态：** 上述三段的**接线已完成且编译通过**，但「建立/终止」的时序断言尚无单测——`session::watch` 依赖
真实 USB 枚举（`attached()`），在无手机的机器上无法构造。因此这一段留到 Phase 3a 用真设备观察日志确认
（`[cp] … found over usb` / `[aa] … asking the phone for …` / `… session over`）。

### Phase 2 — 协商与观测 · 已实现

D3–D10。排序函数、能力交集、观测通道、推迟 Setup 与超时降级、`/info` 收敛。

落点：`sink.rs`（`Negotiated`/`Observed`/`AudioRoute`/`format_matches`）、`info.rs`（`AudioFormats` 与表驱动
`audio_formats`）、`control.rs`（`setup_request_video`/`setup_request_audio`、`AUDIO_TYPE_*`、`density()`）、
`session.rs`（`Choice`/`CodecChoice`/`apply_choice`/流控）、`bridge.rs`（`Room`/`choose`/`offered_audio`/
`millimetres`/`CHOICE_TIMEOUT`）。

验收：`cargo test` 全部通过，覆盖六种输入（仅 H.264／仅 H.265／无视频／仅 PCM／仅 AAC／交集为空）、观测超时
的降级路径，以及音频分派（媒体与语音各归其通道）；交集为空一项断言的是**报错并拒绝**，而非任选其一。

**当前状态：** `cargo test` 53 passed。已覆盖的是「手机的自选编码压过车机偏好」「车机解不了的编码被拒绝」
「音频逐通道匹配、Opus 无 AA 编号故不 Setup」「`/info` 的音频族跟随车机列出」「毫米换算」「空
`Discovered` 什么都不协商」六条 + `info.rs` 三条 + `control.rs` 的表驱动与 `real_density` 两条。**尚未单测的
是**：推迟 Setup 的 5 秒降级路径（需构造 `CodecChoice` 不返回的会话）与「音频分派到不同通道」的端到端断言
（`sink.rs` 侧目前只有路由函数，没有喂帧的测试）——两项都记在这里，不在 Phase 3 之前补。

### Phase 3 — USB gadget 上板 · 代码已实现，上板待做

**3a 现有板（gadget 模式）**

1. 切到 gadget 模式：先向 `UDC` 写入空值解绑板子自带的 gadget，再把 `usb_role` 写成 `gadget`。
2. **对端用 LIVI 车机端**：在 mac 上跑 `livi-helperd`，用 USB 连接板子。期望它认到设备并打开 bulk 管道，
   随后 `tls up` → `service discovery` → `channels open` → `video set up`。先用 `lsusb -v` 核对枚举结果：
   `idVendor 0x18d1 / idProduct 0x2d00`、vendor-specific 接口 `class 0xff / subclass 0x42 / proto 0x01`、
   两个 bulk endpoint。（描述符由 `usb.rs::interface_descriptors` 写死，`usb.rs:228`。）
3. **媒体源用测试源**（`CP2AA_AA_TEST_SOURCE=1`）：注入一段已知的 H.264 + PCM，验证 AA 侧的画面与声音。
   此模式下本设备不能同时接 iPhone。
4. 接真车机，重复第 2–3 步。

**3b NanoPi Duo2（到货后）**

5. 先跑 §0.2 的三项 gadget 能力检查，再重复 3a 的第 2–4 步（UDC 名称与内核配置可能不同）。

**代码侧已就位的部分**（相当于 3a 第 1 步与底盘）：删除 `Config::aa_tcp_port`/`CP2AA_AA_USB` 与 TCP accept
循环；`Bridge::run` 非 Linux 返回错误；`usb.rs::Gadget::rebind()` 每轮会话结束后重绑 `UDC`；`sink::test_source()`
与 `CP2AA_AA_TEST_SOURCE`；README 与环境变量表同步。

### Phase 4 — 端到端（NanoPi Duo2）

iPhone 与车机同时接入：iPhone 画面显示在车机上，车机触摸与旋钮操作 iPhone，车机喇叭出声且各路音频不混。


---

## 6. 风险

1. **新板的 gadget 支持需先确认**（§0.2 的三条命令）。系统用 FriendlyElec 官方 Ubuntu（32 位，
   `armv7-unknown-linux-gnueabihf`，4.14 BSP 内核），它的内核仍必须按 `usb.rs` 的用法启用 configfs、
   FunctionFS 与 musb OTG 三件套；缺项只能换内核或换发行版。
2. **OTG 口兼作供电**：Duo2 由 OTG 口取电，作 gadget 时供电来自车机 USB 口。车机口的供电能力与 H3 板（含
   WiFi）的峰值功耗是否匹配需实测；不足时改由扩展板独立供电，让 OTG 只走数据——已按此作为既定退路（§0.2）。
3. **推迟 Setup 可能被车机判定超时**（D19）。LIVI 对端与真车都要验；若不可行，退回「先按最佳参数 Setup、
   不匹配时重启车机会话」，§3.3 的结构无需推倒。
4. `usb.rs` 未在硬件上验证：UDC 名称、endpoint 顺序、configfs 残留目录、`functionfs` 挂载点，均需首次运行时
   确认。现有板第一次跑通这段之后，NanoPi Duo2 上仍要重跑（内核与 UDC 不同）。
5. **车机未必接受直接枚举为 accessory**：`usb.rs` 跳过了 AOA 协商。LIVI 车机端把 `0x2d00` 直接列为
   accessory（§0.3），对它成立；真车需实测，不识别时补回 AOA 步骤。
6. 车机仅支持 H.264 的情况很常见，降级路径必须可用。
7. 视频用本地单调时钟、音频用手机 RTP（D7 的决策）⇒ 音视频不同源，存在固定偏差，属预期现象，不是新增缺陷。

---

## 7. 决策与遗留问题

2026-10-08 已确定：

1. **TCP 入口删除**，AA 只保留 USB gadget（D15）。
2. **车机来源有两个**：真车，以及跑 LIVI 车机端的机器（§0.3）。
3. **验收包含音频**，D11/D14/D16 落在必须改范围内。
4. **先分别在现有板上验证**（host 模式测 CarPlay、gadget 模式测 AA），NanoPi Duo2 到货后做端到端。
5. **不支持 `CP2AA_SINK=web,aa` 同时启用。**
6. **抖动宽限不做**：立即终止的实现更简单，与 §2.1 规则 2 一致。
7. **NanoPi Duo2 上运行 FriendlyElec 官方 Ubuntu**：H3 是 32 位 Cortex-A7，交叉编译目标
   `armv7-unknown-linux-gnueabihf`；Armbian 没有这块板的现成镜像（§0.2）。
8. **OTG 口供电取自车机 USB 口**（厂商标称 5V/2A），实测不足时改由扩展板独立供电（§0.2、风险 2）。
9. **D7 保留本地单调时钟**：AirPlay 画面头的时间戳在参考实现中没有读取点，取错反而引入无法排查的偏移（§4 D7）。
10. **本轮不发布**：按仓库约定「版本号与 tag 一一对应，未打 tag 不写 CHANGELOG」，`Cargo.toml` 停在 `0.6.1`。
    上板验证通过、决定发布时，再按 §5 的三个 Phase 汇总 CHANGELOG 段并打 tag。

仍需确认：

1. **A17 上板实测项**：`usb.rs` 的 UDC 名称与端点顺序（D20）、车机对推迟 Setup 的容忍度（D19）、真车是否接受
   直接枚举为 accessory（风险 5）。三项都只能在硬件上得出结论，因此 §0.2 的第一步就是查 gadget 能力。

---

## 附录 A — Phase 0 核对表（文件:行）

行号以 2026-10-08 的实现为准；改动后如偏移，以符号名为准（每个条目都给了符号名）。

### A.1 硬件前提（§0.2）

| 要确认的事                | 命令 / 位置                                                                   |
| -------------------- | ------------------------------------------------------------------------ |
| 有几个可当 device 的控制器     | `ls /sys/class/udc/`（NanoPi Duo2 上应非空）                                  |
| configfs / FunctionFS / musb | `zcat /proc/config.gz \| grep -E "CONFIG_USB_CONFIGFS\|USB_CONFIGFS_F_FS\|CONFIG_USB_MUSB"` |
| FunctionFS 挂载点        | `mount \| grep functionfs`                                                |
| 现有板的角色切换节点           | `/sys/class/usb_role/ci_hdrc.0-role-switch/role`（§0.1）                     |
| gadget 目录与挂载点          | `usb.rs:38` `GADGET_DIR`、`usb.rs:42` `FFS_DIR`                             |

### A.2 CarPlay 侧会话控制（§2.3、D1）

| 项                | 位置                                                                |
| ---------------- | ----------------------------------------------------------------- |
| `Signal::{Restart,Stop}` | `session.rs:71`（`pub(crate) enum Signal`）                        |
| `Ended` 三个变体      | `session.rs:58` 附近（`enum Ended`）                                  |
| `Control`（门控 + 广播） | `session.rs:85`；`new`/`start`/`stop`/`live`/`sender`/`subscribe` |
| `watch` 的启动条件      | `session.rs:220`                                                  |
| `attach` 的返回值       | `session.rs:273`                                                  |
| `run_bringup` 的 `changed` | `session.rs:406`（无线共用）                                          |
| 无线侧同步            | `wireless/mod.rs::run_session`、`wireless/{linux,macos}.rs`           |

### A.3 AA 协商与观测（§3、D3–D11、D14、D16）

| 项                                  | 位置                                                                       |
| ---------------------------------- | ------------------------------------------------------------------------ |
| `Choice` / `CodecChoice`            | `aa-client/session.rs:42` / `:55`                                        |
| 多音频通道 `Vec<AvState>`               | `aa-client/session.rs:214`；`AvState` 在 `:173`                          |
| 延迟 Setup `apply_choice`             | `aa-client/session.rs:553`                                               |
| 进展超时 `SETUP_TIMEOUT`               | `aa-client/session.rs:30`（deadline 重置在 `:248`、`:255`）                   |
| 视频通道置 `Running`                     | `aa-client/session.rs:783`（`start_av` 内）                                 |
| 流控 `window/outstanding/ack_seen`    | `aa-client/session.rs:172-200`；`on_ack` 在 `:670`；`media_blocked` 在 `:274`  |
| `Observed` / 观测广播                   | `sink.rs:193` / `sink.rs:220`；`AaOutlet::observe` 在 `:265`                |
| `Negotiated`                       | `sink.rs:163`（`audio` 与 `audio_format`）                                   |
| 音频路由 `AudioRoute` / `aa_audio_type` | `sink.rs:310` / `:201`；`channel_of` 在 `:380`                              |
| 采样参数校验 `format_matches`            | `sink.rs:393`                                                             |
| `/info` 音频族掩码 `AudioFormats`        | `airplay/info.rs:183`、`:210`（`mask`）、`:225`（`audio_formats`）              |
| 多路 Setup 构造                        | `aa-client/control.rs:81` / `:87`                                          |
| `AUDIO_TYPE_*`                      | `aa-client/control.rs:129-132`                                             |
| `VideoConfig::density()`            | `aa-client/control.rs:223`                                                 |
| 桥侧交集                            | `bridge.rs:511`（`choose`）、`:541`（`audio_codec`）、`:557`（`offered_audio`） |
| 毫米换算                            | `bridge.rs:570`（`millimetres`）                                             |
| 观测 → 选择                          | `bridge.rs:203-221`（`Room::record` 在 `:78`，`decide` 在 `:355`）              |
| 5 秒兜底                            | `bridge.rs:53`（`CHOICE_TIMEOUT`）、`:181`                                    |
| gadget 重绑                           | `aa-client/usb.rs:178`（`Gadget::rebind`）、`:63`（`watch`）                   |
| 测试源                              | `sink.rs:554`（`test_source`）；`CP2AA_AA_TEST_SOURCE` 在 `config.rs`          |

### A.4 参考实现对照（§4「已核实」）

| 结论           | 参考实现位置                                                                                  |
| ------------ | --------------------------------------------------------------------------------------- |
| D17 focus 方向 | `../AACS/AAServer/src/VideoChannelHandler.cpp:181`（手机侧收到即 Start）、`../AACS/AAClient/src/VideoChannelHandler.cpp:32-35`（车机侧 Setup 后发 focus） |
| D18 载荷切分    | `../AACS/AAServer/src/VideoChannelHandler.cpp:33-46`（一条消息一个 buffer）、`../AACS/AAClient/src/VideoChannelHandler.cpp:39`（剥 2 字节整段送解码） |
| D23 ack 方向   | `../AACS/AAClient/src/VideoChannelHandler.cpp:40`、`:50`、`:161-167`（车机侧发）、`:184`（手机侧只接收） |
| D7 时间戳单位    | `../AACS/AAServer/src/VideoChannelHandler.cpp:38`（`pts / 1000` ⇒ 微秒）                      |
| 消息号          | `../AACS/include/enums.h:46`（`VideoFocusIndication = 0x8008`）                            |
| 对端车机        | `vendor/LIVI/native/livi-helperd/crates/livi-aa/src/usb.rs:25`（`ACCESSORY_PIDS` 含 `0x2d00`） |
