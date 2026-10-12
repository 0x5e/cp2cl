# CarLife 车机对接 — 开发计划与技术方案

> Linux 设备（NanoPi Duo2）通过 micro-USB **扮演 CarLife 手机端（MD）**接入车机（HU = 奔驰 MBUX NG6）；
> iPhone 通过 **无线 CarPlay** 连同一台设备。两侧媒体不做编解码，只做重封装。
> 主要工作量在协议：CarLife 的手机端协议栈**不存在开源实现**，必须自己写——本文档的一半篇幅就是在把这份协议钉死。

状态：2026-10-13 起草。**尚未写代码**；本文件是 Phase 0 的核对结论 + 后续阶段的落点与验收。
参考材料：

| 材料 | 位置 | 用途 |
|---|---|---|
| 官方接入指南（174 页） | `../CarLifePort/docs/百度CarLife车机手机互联方案接入详细指南new.pdf` | 协议条款、消息顺序、键值表、鉴权流程 |
| 车机端开源库 CarLifeVehicleLib | `../CarLifePort/vendor/carlife`（Apache-2.0） | **协议字典**：全部 `MSG_CMD_*` 与 protobuf 定义 |
| 车机端整合实现 CarLifePort | `../CarLifePort`（`link/` C+C++、`server/` Go） | 真机跑通过的 HU 侧 wire 行为、AOA 主机侧握手、实测坑 |
| 参考录音工具 | `../CarLifePort/tools/aoa-probe` | 主机侧探针：发 51/52/53、发首帧、校验帧格式 |

---

## 0. 角色与拓扑

```
iPhone ──无线 CarPlay(AirPlay)──► cp2cl ──USB(AOA)──► MBUX NG6
        视频/音频下行、HID 上行              扮演 MD：CarLife 协议栈 + 视频/音频/触摸
```

| 参与者 | CarLife 角色 | USB 角色 | 谁先说话 |
|---|---|---|---|
| MBUX NG6 | HU（车机端） | **HOST**，供电并枚举 | **HU 先发** `MSG_CMD_HU_PROTOCOL_VERSION` |
| cp2cl / Duo2 | MD（手机端） | **DEVICE**（gadget） | 被动应答 |
| iPhone | CarPlay phone | —（无线） | AirPlay 侧由我方 `/info` 应答 |

三点容易搞反的地方，先钉住：

1. **AOA 规范里的 "accessory" 是 USB 主机**（即车机）。设备侧（我们）扮演的是「Android 设备处于 accessory 模式」，
   下面一律写 **MD 侧**，不写 accessory。
2. **AOA 模式下没有 TCP**。指南 §6.1.5 给的那套 `7200/7240…` 端口是 ADB 方案的；AOA 下六条通道被多路复用在
   同一条 bulk 管道上，每帧带 8 字节外头（§2.6）。CarLifePort 的 `link/bridge.c` 就是这么做的。
3. **USB 角色是 device 侧**：Duo2 的 micro-USB（`1c19000.usb`，OTG）切到 device 模式接入车机，与 CarPlay 侧的
   无线通路互不干扰（CarPlay 侧不经 USB）。角色切换命令与供电注意事项见 `WIRED.md`。

---

## 1. 现状（cp2cl 侧）

已有、且这次要复用的：

| 现有能力 | 位置 | 这次怎么用 |
|---|---|---|
| AirPlay 会话几何参数（宽/高/帧率/ui_scale/hevc） | `src/airplay/server.rs:206` `ScreenParams`、`:263` `SharedScreen` | 由 CarLife 的 `VIDEO_ENCODER_INIT` 决定，写进去再重启会话 |
| 会话重启 | `src/web/mod.rs:430` `Hub::restart_session()`（`:816` 由显示面板调用） | CarLife 侧发起几何变更时走同一条路 |
| `/info` 的音频宣示 | `src/airplay/info.rs:178` `audio_formats()`（条目在 `:187-197`） | 把媒体音频锁到 LPCM，省掉解码（§4.2） |
| 视频帧（**已是 annex-B**） | `src/airplay/screen.rs` → `StreamEvent::VideoFrame` | 直接塞进 CarLife 视频通道（§4.1） |
| 音频帧与格式 | `src/sink.rs` `AudioCodec`（含 `Pcm{rate,channels}`） | LPCM 时直通 |
| 输入上行（HID + 会话命令） | `src/airplay/event.rs`、`src/commands.rs` | CarLife 触摸/按键接到这套词汇上 |
| 麦克风上行 | `src/airplay/mic.rs` | CarLife VR 通道的 PCM 接过来 |
| 浏览器客户端 | `src/web/` | 保留：既是本机调试窗口，也是唯一能独立验证 CarPlay 侧的手段 |

缺的（本文档要补的）：

1. **USB gadget 管理**：起停 gadget、描述符、重枚举、与现有 ECM 管理链路共存。
2. **AOA MD 侧**：设备侧应答 51/52/53（如果需要）。
3. **CarLife MD 协议栈**：帧编解码 + protobuf + 握手状态机。
4. **桥接**：第二个媒体消费者（现在是「输出只有浏览器客户端一处」，`src/sink.rs` 的注释就写着这句）。

---

## 2. USB 层：设备侧 AOA

### 2.1 两条接入路径

**路径 A — 直接以 accessory 形态枚举（先试这条）**

设备一上来就报 `VID 0x18D1` + `PID 0x2D00..0x2D05`，接口 vendor-specific + 两个 bulk 端点，不做握手。
指南 §6.1.1.2.1 的 AOA 流程第 ② 步就是「按 VID/PID 判断是否已在 Accessory Mode」，CarLifePort 的
`link/bridge.c:634-638` 在真机上也确实走了「已在 accessory，跳过 51/52/53」这一支。若 MBUX 同样有这一步，
路径 A 成立，**一个控制请求都不用实现**。

**路径 B — 完整 AOA 握手**

先以「手机」形态枚举（VID/PID 取一台常见 Android 机，`iProduct` 含 `android`）；收到 51 回版本（2 字节**小端**，
取值必须 ∈ {1,2}）→ 收到 52 六次（`wIndex = 0..5`）存字符串 → 收到 53 后**逻辑插拔、重新枚举**为
`18D1:2D00..2D05`。六条字符串（指南表 6.1-2，CarLifePort `link/aoa.c:43-50` 同值）：

| wIndex | 字符串 |
|---|---|
| 0 | `Baidu` |
| 1 | `CarLife` |
| 2 | `Baidu CarLife` |
| 3 | `1.0.0` |
| 4 | `http://carlife.baidu.com/` |
| 5 | `0720SerialNo.` |

控制请求的实际取值（CarLifePort 主机侧 `link/aoa.c:379-425`）：51 = `bmRequestType 0xC0` 读 2 字节；
52/53 = `0x40`；`wValue/wIndex = 0`（52 的 wIndex 是字符串序号）；超时 2 s。

### 2.2 板子能力（2026-10-13 在 Duo2 上核实）

```
Linux NanoPi-Duo2 4.14.111 #193 SMP armv7l
CONFIG_USB_GADGET=y  CONFIG_USB_LIBCOMPOSITE=y  CONFIG_USB_CONFIGFS=y
CONFIG_USB_F_FS=y    CONFIG_USB_CONFIGFS_F_FS=y
/lib/modules/4.14.111/…/gadget/function/ 只有 usb_f_phonet.ko
/sys/class/udc/ → musb-hdrc.4.auto
```

⇒ **没有 `USB_F_ACC`（Android 的 `f_accessory`），没有 `CONFIG_USB_RAW_GADGET`，板子上也没有内核源码**。
但 **FunctionFS 在**，这一条就够了：

### 2.3 为什么 FunctionFS 足够（不需要写内核模块）

- uapi 里 `FUNCTIONFS_ALL_CTRL_RECIP = 64`（`include/uapi/linux/usb/functionfs.h`，v4.14 起就在）。
- ffs 对「收件人不是本函数的接口/端点」的请求，用 `ffs_func_req_match()` 主动认领，并把请求作为
  `FUNCTIONFS_SETUP` 事件交给用户态（`f_fs.c:3281`、`ffs_func_setup()` 的 `default:` 分支 `:3247-3252`）。
- composite 层在把非标准请求交给函数前，先逐个问 `f->req_match`（`composite.c:1917-1923`，
  `try_fun_setup` 在 `:1948`）。⇒ **设置该 flag 的 ffs 实例能收到 device 收件人的 vendor 控制请求**，
  也就是 51/52/53，且**在同 config 里还有别的函数时也成立**（ECM 可以同 config 共存，§2.5）。

应答方式就是 ffs 的标准 ep0 协议：读 `/dev/ffs-*/ep0` 拿到 `usb_functionfs_event`，IN 请求往 ep0 写数据，
OUT 请求从 ep0 读数据。这正是 Android 17 里 `aoad` 走的路（把 AOA 从内核 `f_accessory` 迁到用户态）。

v4.14 **没有** `FUNCTIONFS_CONFIG0_SETUP` / configfs 的 `control_config`（那是后来的），但本用途不需要它。

### 2.4 描述符与重枚举

- 描述符由用户态在 ffs 的 ep0 上写（`usb_functionfs_descs_head_v2`，magic=3，flags 里带 `ALL_CTRL_RECIP`）。
  在 `FUNCTIONFS_DISABLE` 之后可以重写，所以「先手机形态、后 accessory 形态」不需要两套 ffs 实例。
- `idVendor` / `idProduct` / `bcdDevice` / 字符串是 **configfs 属性**（`/sys/kernel/config/usb_gadget/<g>/…`），
  不归 ffs 管；改完靠 `echo "" > UDC; echo musb-hdrc.4.auto > UDC` 触发真正的重新枚举。
- accessory 形态的接口取值：CarLifePort 的主机侧**只认 `class 0xff && subclass 0xff`**（`link/aoa.c:496-502`，
  并注释说明 adb 是 subclass `0x42`）。我们不知道 MBUX 要哪一组，三个候选按顺序试：
  `0xff/0xff/0x00`、`0xff/0x00/0x00`、`0xff/0x42/0x01`。端点地址也按 CarLifePort 实测的
  IN `0x81` / OUT `0x01`（与 adb 共存时是 `0x02`）。

### 2.5 与 ECM 管理链路共存

板子只有一个 OTG 口。目前它跑 ECM（`usb0 = 172.16.0.1/24`，Mac 侧 `en15 = 172.16.0.2`），是我们的唯一管理链路
（`BUILD.md`）。设计上**把 ECM 与 ffs 放进同一个 configuration**：车机/主机只认那个 vendor 接口，多出来的
CDC-ECM 不理它；我们就不会因为插上车而丢掉 SSH。代价是描述符里多一个接口，需要实测车机是否介意
（列入 §8 风险 6）。备选是给 ECM 单独一个 configuration，但那要求车机自己选配置，不可控。

### 2.6 USB 帧格式

```
[4B channelId][4B totalLen][CarLife 消息]      全部大端
channelId: 1=cmd 2=video 3=media 4=tts 5=vr 6=touch
totalLen : CarLife 消息头 + 消息体，不含这 8 字节
```

单帧上限 640 KB（CarLifePort `link/aoa.h:26,29`）；写侧头与体分两次 bulk、单次 **≤16 KB** 分片
（`aoa.c:563-609`）。读侧**请求长度必须是端点 max packet size 的整数倍**——CarLifePort 在这上面踩过一个
「请求 8 字节 → 设备回满包 → macOS `kIOReturnOverrun` → `LIBUSB_ERROR_OVERFLOW` → 链路拆了重建也无效」的坑
（`../CarLifePort/.workbuddy/memory/2026-10-13.md`，修复 `5d090e4`）。我们作为设备侧不直接面对这个问题，但它告诉我们**对端会要求字节流严格
对齐**，我们的写侧同样要按 max packet 分片、不要留半包。

---

## 3. 协议层：MD 侧要做什么

### 3.1 两种包头

| 通道 | 头长 | 布局（大端） | 体上限 |
|---|---|---|---|
| cmd / touch | 8 B | `u16 bodyLen`、`u16 reserved`、`u32 serviceType` | cmd 40 KB / touch 10 KB |
| video / media / tts / vr | 12 B | `u32 bodyLen`、`u32 timestamp(ms)`、`u32 serviceType` | video 600 KB / media 100 KB / tts 50 KB / vr 50 KB |

（上限取自 CarLifeVehicleLib 的 `CMD_DATA_SIZE` 等常量，`CTranRecvPackageProcess.h:26-45`；CarLifePort 侧
同值，`link/bridge.c:111-144`）。`reserved` 对端可能不填，**解析时忽略、回包填 0**。

### 3.2 方向位

`serviceType` 低 16 位的 **bit15 = 1 表示 HU→MD，= 0 表示 MD→HU**（指南 §10.1）。前两字节是通道
（`0x0001`=cmd、`0x0002`=video、`0x0003`=media、`0x0004`=tts、`0x0005`=vr、`0x0006`=touch）。
特例：`0x00010029`（档位）与 `0x00010032`（车数据订阅完成）**值上没有 0x8000 却是 HU 发的**，
是库的历史遗留，按 HU→MD 解析（`CTranRecvPackageProcess.cpp:2234,2273`）。

### 3.3 握手与会话（HU 先说话）

| # | 方向 | serviceType | 体 |
|---|---|---|---|
| 1 | HU→MD | `0x00018001` `HU_PROTOCOL_VERSION` | `major=1, minor=0` ⇒ 线上 `08 01 10 00`（实测） |
| 2 | MD→HU | `0x00010002` `PROTOCOL_VERSION_MATCH_STATUS` | `matchStatus=1`（MATCH） |
| 3 | HU→MD | `0x00018003` `HU_INFO` | `CarlifeDeviceInfo`（全字段 optional） |
| 4 | MD→HU | `0x00010004` `MD_INFO` | 同结构；`os` 填 `Android` |
| 5 | HU→MD | `0x00018027` `STATISTIC_INFO` | `cuid/versionName/versionCode/channel/connect*` |
| 6 | HU→MD | `0x00018048` `HU_AUTHEN_REQUEST` | `randomValue = "A_B"`（见 §3.5） |
| 7 | MD→HU | `0x00010049` `MD_AUTHEN_RESPONSE` | `encryptValue` |
| 8 | HU→MD | `0x0001804A` `HU_AUTHEN_RESULT` | `result` |
| 9 | MD→HU | `0x0001004B` `MD_AUTHEN_RESULT` | `result` |
| 10 | MD→HU | `0x00010051` `MD_FEATURE_CONFIG_REQUEST` | **空体** |
| 11 | HU→MD | `0x00018052` `HU_FEATURE_CONFIG_RESPONSE` | key-value 列表 |
| 12 | HU→MD | `0x00018007` `VIDEO_ENCODER_INIT` | `{width, height, frameRate}` |
| 13 | MD→HU | `0x00010008` `VIDEO_ENCODER_INIT_DONE` | 同上，填**我方实际能给的值** |
| 14 | HU→MD | `0x00018009` `VIDEO_ENCODER_START` | 空 |
| 15 | MD→HU | 通道 2 上的 `0x00020001` `VIDEO_DATA` | annex-B H.264 |

指南的硬性时限：`STATISTIC_INFO` 必须**连接成功后 2 分钟内**到（不到则 MD 主动断开，§19.2）；
MD 收到 `GO_TO_FOREGROUND` 后 5 秒没切前台要回 `0x0001004C`。**都是对 HU 的要求，我们做 MD 侧只需容忍
对方不做**——但 §3.3 的 1→2 和 4→13 是我们必须回答的，不回答链路走不下去
（CarLifePort `CarLifeLibTest.cpp:277` 明确写了「必须先发 0x00018001」）。

### 3.4 MD 侧必须发出的消息（最小集）

| serviceType | 何时 | 体 |
|---|---|---|
| `0x00010002` | 收到 1 后 | `{matchStatus=1}` |
| `0x00010004` | 紧跟其后 | `CarlifeDeviceInfo`（`os/brand/model/release/sdk_int/btaddress/btname`） |
| `0x00010008` | 收到 `0x00018007` 后 | `{width, height, frameRate}`（**实际值**） |
| `0x00010049` / `0x0001004B` | 见 §3.5 | 视策略 |
| `0x00010051` | 认证流程走完后 | 空体 |
| `0x0001000D` | 收到帧率变更 | `{frameRate}` |
| `0x00020001` | 有画面时 | annex-B 裸流 + 12 字节头 |
| `0x00020002` | 无画面时（替代视频包） | 空；协议只要求 MD 切后台时这么发，这里用来在没接 iPhone 时保活 |
| `0x00030001` / `0x00040001` / `0x00050002` | 相应音频开始前 | `CarlifeMusicInit{sampleRate, channelConfig, sampleFormat}` |
| `0x00030006` / `0x00040003` / `0x00050003` | 有音频时 | 裸 PCM |
| `0x00060009` | 可选 | UI 音效；CarLifePort 的接收侧是死代码，可以不发 |
| `0x00010059` | 退出时 | 空 |

**注意 `MSG_TTS_INIT` 用的也是 `CarlifeMusicInit`**，不是 `CarlifeTTSInit`（`CTranRecvPackageProcess.cpp:1535`）。

### 3.5 鉴权：这一侧的真正风险

两条流程，方向对我们相反：

- **17.1（HU 认证 MD）**：HU 发 `randomValue = "A_B"`（A 是随机串，B 是车机要求的最低手机版本），
  MD 用**自己的私钥**做 `RSA/ECB/PKCS1Padding` 加密回 `encryptValue`，HU 用百度发给车厂的公钥解密比对；
  不一致 ⇒ **HU 主动断开**。私钥在**手机端**（百度持有并只给手机端），开源车机库里没有，合法取值也拿不到。
- **17.2（MD 认证 HU）**：MD 用 `STATISTIC_INFO` 里的渠道号判断车机合法性，非法则主动断开。
  **我们是 MD，这一侧的判定权在我们手里**，可以对渠道号放行。

⇒ 风险集中在 17.1。观察到的先例：CarLifePort 反向做 HU 时，真机弹出「未认证的车机端」但**音频照旧在推**
（`../CarLifePort/.workbuddy/memory/2026-10-12.md:381-385`），说明手机的处置是「提示」而非「立即断开」。我们的对策按代价从低到高：

1. **先什么都不做**：不回 `0x00010049`，看 MBUX 是否继续（Phase 1 的观察项）。
2. **回一个失败值**：按指南「版本低于 B 就把该字段全置 0」，看车机是断开还是只提示。
3. **逆向手机端 APK 取私钥**（`com.baidu.carlife`，用户手上有 Pixel 7a 与该 App）：私钥必须在端上才能签名，
   所以它一定在包里。可行性未知，列入 Phase 4 前的可选项。
4. 走百度官方立项拿证书——只有真的要做产品才值得。

**绝不做**：伪造一张证书、或把 `encryptValue` 猜成某个固定值。车机校验不过会直接断开，浪费时间。

### 3.6 视频

- 编码：指南 §10.3 规定 MD 用 **H.264 Baseline Profile Level 3.1**；性能指标「800×480@60、720p@30」。
- 分辨率：HU 发 `VIDEO_ENCODER_INIT(w,h,fps)` → MD 按自身能力回 `INIT_DONE(w',h',f',)`，**之后双方以 MD 的值为准**，
  不等时由 HU 自行缩放（§7.3）。
- 帧率：HU 下发的帧率参数**只在 [3,30] 生效**；MD 产帧速度不得超过它（§7.2）。
- 关键帧：**协议里没有「请求关键帧」这条**（CarLifePort 把前端的 keyframe 直接丢弃，`session.go:655-657`）。
  编码器要自己按 GOP 出 IDR。
- JPEG：`0x00018056` 可要求出一帧 JPEG 代替视频，支持才回 `0x00010057`——**我们不支持，不回**。

### 3.7 音频

| 流 | 通道 | 格式 | 方向 |
|---|---|---|---|
| Media | 3 | PCM 44.1 kHz / 双声道 / 16 bit 小端 | MD→HU |
| Navi TTS | 4 | PCM 16 kHz / 单声道 / 16 bit 小端 | MD→HU |
| VR TTS | 5 | PCM 16 kHz / 单声道 / 16 bit | MD→HU |
| VR MIC | 5 | PCM 16 kHz / 单声道 / 16 bit，**每包 1 KB** | HU→MD |

`AUDIO_TRANSMISSION_MODE`（feature config 下发，默认 `0` = 专用音频通道，`1` = 走蓝牙）决定 HU 是听通道还是
听蓝牙；我们要的是 `0`，但那是 **HU 发给我们**的，不接受也没得选。

### 3.8 触摸与按键

- 触摸是 **HU→MD**，`0x00068001` `TOUCH_ACTION{action,x,y}`（action：0=DOWN 1=UP 2=MOVE）或
  `0x00068002/3/4` 三个单点消息。**坐标由 HU 换算到 MD 坐标系**（指南 §10.6.1.1.3：
  `x2 = x1 * w2 / w1`，`(w2,h2)` 是 MD 分辨率），所以我们收到的坐标可以直接当「视频像素坐标」用。
- **单点**：协议里没有多点结构，`CarlifeTouchAction`/`CarLifeTouchSinglePoint` 只有一组 x,y。
- 硬按键 `0x00068008` `CAR_HARD_KEY_CODE{keycode}`，键值表 30 项（附录 A.3）。常用：
  `OK=0x14`、`MAIN=0x1E`、`BACK=0x0E`、`Move_Left/Right=0x15/0x16`、
  `Selector_Next/Previous=0x06/0x07`、`Media_Start/Stop=0x1F/0x20`、`Seek_Add/Sub=0x10/0x0F`、
  `VR_Start/Stop=0x21/0x22`、`Phone_Call/End=0x02/0x03`、`Mute=0x13`、`Navi=0x0B`、`TEL=0x1D`、`Media=0x09`。
  实测注意（CarLifePort 反过来映射时踩到的）：`KEYCODE_Media(0x09)` 是「进音乐界面」不是播放/暂停；
  `Move_Up/Down(0x17/0x18)` 在列表界面不响应，纵轴要用 selector。

### 3.9 功能定制项

`0x00010051` 空体请求 → HU 回 `0x00018052` 的 key-value 列表。默认值（指南表 10.2-2）：
`VOICE_MIC=0`（用车机麦）、`VOICE_WAKEUP=1`、`BLUETOOTH_AUTO_PAIR=0`、`BLUETOOTH_INTERNAL_UI=0`、
`FOCUS_UI=0`、`MEDIA_SAMPLE_RATE=0`、`AUDIO_TRANSMISSION_MODE=0`。我们只关心：
`VOICE_MIC`（决定麦克风走车机还是走我们）、`AUDIO_TRANSMISSION_MODE`（决定音频是否走通道）、
`MEDIA_SAMPLE_RATE`（`1` = 48 kHz，会改变 Media 通道的采样率）。

---

## 4. 桥接：CarPlay → CarLife

### 4.1 视频：重封装，不重编码

AirPlay 侧交出来的是 **annex-B** 访问单元（`src/airplay/screen.rs` 已在内部转好），CarLife 视频通道吃的是
「编码后视频数据」并配 12 字节头。所以只要能满足下面三条，就是**逐帧搬运**：

1. **编码家族一致**：CarPlay 侧关掉 HEVC（`CP2CL_HEVC=0`，或者由 CarLife 会话决定 `ScreenParams.hevc=false`），
   两端都 H.264。
2. **profile 兼容**：指南只保证 baseline L3.1。CarPlay 侧实际多为 Main/High，可能带 B 帧。**先直通试，
   车机不接受再想办法**（§8 风险 3）。
3. **单帧 ≤ 600 KB**：1280×720 / 30 fps 的 IDR 一般在 100–300 KB，安全；1080p 大关键帧可能越界。
   由 `INIT_DONE` 把分辨率压在我们能保证的档位。

几何对齐靠已有的显示参数通道：CarLife 的 `VIDEO_ENCODER_INIT` 到达时，把 `width/height/fps` 写进
`SharedScreen`（`server.rs:263`）并调 `Hub::restart_session()`（`web/mod.rs:430`）——和显示面板按 Apply 做的是
同一件事，代价也一样：**几何只在「新会话」生效，所以要拆掉当前 AirPlay 会话重建**。未连接的手机不受影响。

### 4.2 音频

首选**直通**：`/info` 的 `audio_formats()`（`info.rs:178-197`）现在给媒体流开了两条路——`100/media` 的 LPCM
（`0x800` = 44.1 kHz 立体声）与 `102/media` 的 AAC-LC。**把 `102` 那条去掉、只留 PCM 位**，手机就只能给 PCM，
而 44.1 kHz/立体声/16 bit 正好是 CarLife Media 通道要的格式，直接搬。

无线 CarPlay 下手机是否乖乖选 LPCM 要实测（`info.rs:175` 的注释说「只有有线 CarPlay 会出现 PCM」——那是指
Apple 的实际行为）。若手机坚持 AAC，退一步在本地解码：`symphonia` 能解 AAC-LC 与 ALAC，44.1 kHz 立体声在
Cortex-A7 上可行；**这是整个方案里唯一可能引入实时解码的地方**，所以它是后备而不是首选。

TTS/VR 通道是 16 kHz 单声道——CarPlay 侧对应的是 `speechRecognition`/`telephony` 那两个低带宽流，
必要时做一次重采样。

### 4.3 反向输入

CarLife 触摸（设备像素坐标）→ AirPlay 事件通道的 HID 触摸；CarLife 硬按键 → `src/commands.rs` 的词汇：

| CarLife 键值 | cp2cl 输入 |
|---|---|
| `0x14` OK | `knob.select` |
| `0x1E` MAIN | `home` |
| `0x0E` BACK | `back` |
| `0x15/0x16` Move_Left/Right | `knob.x` 方向 |
| `0x06/0x07` Selector_Next/Previous | `knob.y` 方向 |
| `0x1F/0x20` Media_Start/Stop | `media 3`（播放/暂停） |
| `0x10/0x0F` Seek_Add/Sub | `media 4` / `media 5` |
| `0x21/0x22` VR_Start/Stop | `siri` |
| `0x02/0x03` Phone_Call/End | `telephony 1` / `2`（按通话状态选，与显示面板一致） |
| `0x13` Mute | `telephony 2` 的静音语义 |
| `0x0B` Navi / `0x1D` TEL / `0x09` Media | 无对应，丢弃 |

`night`、`keyframe` 在 CarLife 侧没有对应，丢弃。

### 4.4 麦克风

CarLife `VOICE_MIC=0` 时麦克风在车机侧，HU 会把 16 kHz 单声道 PCM 从 VR 通道（`0x00058001`，每包 1 KB）
推给我们；这正是 `src/airplay/mic.rs` 要的上行格式。`VOICE_MIC=1` 时反过来——由我们推 VR TTS 到通道 5。

### 4.5 会话生命周期

- CarLife 会话由车机发起（用户在 MBUX 里点 CarLife）。**与 AirPlay 会话互相独立**：没有 iPhone 时我们照样
  完成 CarLife 握手，只是没有视频可发（发 `MSG_VIDEO_HEARTBEAT` 保活；指南说 MD 切后台时就是这么做的）。
- iPhone 后来接上：AirPlay 会话建立 → 有帧就转发。
- 车机重插：USB 读失败 ⇒ 重启 gadget 与协议栈（进程内重建，不重启进程）；CarLifePort 的教训是**会话必须在
  新的 accessory 附着上重建**，不能原地重连（`README.md:233-242`）。
- 拔掉 iPhone：继续发心跳，不发视频。

---

## 5. 实验装置与验证

### 5.1 假车机（**最重要的一台设备**）

`../CarLifePort` 是 macOS 上能跑的 HU 实现。角色摆好就是我们要的测试台：

```
Duo2(micro-USB, gadget) ──USB──► Mac(CarLifePort: carlife-link + carlifed + 浏览器)
```

Mac 是 host、Duo2 是 device，拓扑与「车机 ↔ 我们的设备」完全一致。可用的抓手：

- `dist/carlife-link --list` 枚举；`--vid/--pid` 强制指定目标；`aoa-probe --handshake-only / --force-handshake /
  --send-proto-version` 单独验控制阶段。
- `carlifed` 带浏览器前端：CarLife 会话一旦建立，**Mac 上就能看到我们推过去的画面**，这是端到端的第一道验收。
- 日志级别 `CL_LOG_LEVEL=debug` 能看到逐通道转发统计与手机侧播放进度。
- 我们的 ECM 管理链路可以留在同一个 config 里，测试期间 SSH 不断（§2.5）。

一台机器就能把 Phase 1–3 全部验完，**不需要车**。

### 5.2 参考录音（MD 侧的真值）

反过来角色：Mac 跑 `carlife-link` 当车机，插上装了 CarLife 的 **Pixel 7a**，把手机端在完整会话里的
报文逐条录下来（`carLifeLibTest` 的回调、`aoa-probe` 的收发、或 `stonexu1986/carlife-protocol-analysis-tool`
的解析器）。我们写 MD 侧时，**手机的行为才是标准答案**：消息顺序、`MD_INFO` 里填了什么、
`encryptValue` 的长度与形态、视频包的时间戳步进、`MEDIA_INIT` 的 `channelConfig/sampleFormat` 取值。
这份录音应当作为 Phase 0 的产出归档（`docs/carlife-md-capture.md` 之类）。

### 5.3 离线验证

- protobuf 编解码：对我们用到的十几条消息做**表驱动单测**（用 5.2 的录音当输入）。
- AOA 帧编解码：外头/内头切分的往返测试，含跨包、半包、超长帧。
- 状态机：把握手序列喂进去，断言发出的消息与顺序。

### 5.4 上真车（MBUX）验收

1. 插上后 MBUX 出现 CarLife 入口，点击后进入 CarLife 全屏。
2. 画面出现且触摸有效；旋钮/方向键有效。
3. 手机（iPhone）接上后画面切进来，触摸控的是 CarPlay 界面。
4. 拔插车机侧 USB 能自动恢复；拔插 iPhone 不影响 CarLife 会话。
5. `dmesg` 无 USB 复位风暴；连续跑 30 分钟不冻帧。

---

## 6. 分阶段

### Phase 0 — 事实与设施（只读 + 搭台子）

- `0.1` 板子侧把 §2.2 之外的三件事验掉：**ffs 是否接受 `ALL_CTRL_RECIP` flag**（写描述符头看是否 `EINVAL`）、
  **UDC 解绑/绑定能否稳定重枚举**、**ECM 与 ffs 同 config 是否都正常**。
- `0.2` Mac 侧把假车机跑起来，确认它能挑到 Duo2 的 gadget（先用一个最简单的 ffs gadget 当靶子）。
- `0.3` 用 Pixel 7a 录一份 MD 侧参考报文（§5.2），归纳出「必须发的消息」清单与字段取值。

验收：板子上的三条命令有明确结论；Mac 能对 Duo2 完整做一次 51/52/53 并读到重枚举后的设备；
参考录音里能看到 `0x00010002` / `0x00010004` / `0x00010008` / `0x00010051` 这几条。

### Phase 1 — USB gadget 与 AOA MD 侧

新建 gadget 管理模块：按配置组装 configfs（idVendor/idProduct/描述符/ecm+ffs）、写 ffs 描述符、
处理 ep0 事件与 51/52/53、必要时重枚举、退出时清理。先按**路径 A**（直接 accessory 形态）跑；
只有当假车机明确说「没找到 accessory 设备」时，才补**路径 B** 的握手。

验收：Mac 的 `aoa-probe --send-proto-version` 能收到我们的回包；`carlife-link --list` 能看到设备。

### Phase 2 — CarLife MD 协议栈

帧编解码、protobuf 编解码（用哪套见 D5）、握手状态机、`MD_INFO`/`STATISTIC_INFO` 解析、feature config、
视频初始化应答、心跳。**不接 CarPlay**，先用假的帧源（一张固定图编出来的 IDR 或静态 annex-B 样本）。

验收：Mac 的 `carlifed` 日志出现会话建立、`VIDEO_ENCODER_INIT_DONE` 的协商值、并收到帧；浏览器出画。

### Phase 3 — 桥接

视频直通、音频、触摸/按键反向上行、麦克风、几何对齐与「按需求重启 AirPlay 会话」、无手机时的行为。

验收：Mac 浏览器看到的画面就是 iPhone 的 CarPlay 界面；触摸能操作；媒体音频出声。

### Phase 4 — 上真车（MBUX）

按 §5.4 走；认证（§3.5）在这一阶段才有真实答案。

---

## 7. 风险

| # | 风险 | 影响 | 对策 |
|---|---|---|---|
| 1 | **17.1 鉴权**：拿不到 RSA 私钥 | 车机可能直接断开 | 先不回/回失败观察（§3.5）；其次逆向 APK；不做假证书 |
| 2 | 车机是否接受「不是真手机」的 USB 设备 | 完全接不上 | 路径 A/B 都试；VID/PID 与字符串按真机形态来 |
| 3 | CarPlay 侧 H.264 不是 baseline（可能带 B 帧） | 车机解码失败 | 先直通；不行再用 H3 的 VPU 重编码（BSP 是否可用待确认） |
| 4 | 单帧 > 600 KB | 车机侧解析失败或丢帧 | 分辨率控制在 720p，IDR 体积实测；必要时限制码率 |
| 5 | ffs + musb 在 4.14 上的吞吐与稳定性 | 花屏/断流 | Phase 1 就先压测：600 KB/帧 × 30 fps ≈ 18 MB/s |
| 6 | ECM 与 ffs 同 config，车机是否介意多一个接口 | 车机不认 | 备选：给 ECM 单独 configuration，或插车时放弃管理链路 |
| 7 | 无线 CarPlay 不给媒体 LPCM | 需要实时 AAC/ALAC 解码 | `symphonia`；分辨率/帧率已经直通，解码只占音频 |
| 8 | CarLife 会话先到、iPhone 后到（或反之） | 时序错乱 | §4.5 的两条独立生命周期；缺一边时按心跳/空画面处理 |
| 9 | 车机 USB 口供电不足 | 掉电重启 | 已知取舍：车机供电，不足时扩展板独立供电 |
| 10 | 单 USB 口的角色冲突（管理链路 vs 车机） | 场景互斥 | 同 config 共存（§2.5）；实测确认 |

---

## 8. 决策记录

| # | 决策 | 理由 |
|---|---|---|
| D1 | 目标仍是「CarPlay 进来，CarLife 出去」 | MBUX 只有 CarLife 全屏 |
| D2 | 我们扮演 **MD**，USB 侧是 device | 车机是 host，AOA 的 accessory 命名与我们相反 |
| D3 | AOA 用 **FunctionFS + `ALL_CTRL_RECIP`**，不写内核模块 | 板子内核无 `f_accessory`/`raw-gadget`，ffs 在；Google 自己也走这条路 |
| D4 | 先试**路径 A**（直接 accessory 枚举），后补路径 B | 少写一半代码，且真机先例支持 |
| D5 | protobuf **手写最小编解码**，不引入 codegen | 只用到十几条消息、字段全是定长标量/字符串；`proto/` 里放参考 `.proto` 作文档与单测依据 |
| D6 | 视频**不重编码**，annex-B 直通 | H3 没有重编码余量；两端都是 H.264 annex-B |
| D7 | 音频**优先锁 LPCM** | `info.rs:187-197` 可控；避免实时解码 |
| D8 | 几何由 HU 的 `VIDEO_ENCODER_INIT` 决定，用 `SharedScreen` + `restart_session` 对齐 | 复用显示面板那条已验证的路 |
| D9 | 浏览器客户端**保留** | 唯一能单独验证 CarPlay 侧的手段，且是 Phase 3 的对照窗口 |
| D10 | 管理链路 ECM 与 CarLife 同 configuration | 插车时也要能 SSH；风险 6 兜底 |
| D11 | 不在协议栈里做「请求关键帧」 | CarLife 没这条消息 |

---

## 附录 A — 常量速查

### A.1 通道

| # | 名称 | serviceType 前缀 | USB channelId | 方向 |
|---|---|---|---|---|
| 1 | cmd | `0x0001` | 1 | 双向 |
| 2 | video | `0x0002` | 2 | MD→HU |
| 3 | media | `0x0003` | 3 | MD→HU |
| 4 | tts | `0x0004` | 4 | MD→HU |
| 5 | vr | `0x0005` | 5 | 双向 |
| 6 | touch | `0x0006` | 6 | HU→MD |

### A.2 MD 侧消息 ID

`0x00010002` 版本匹配 · `0x00010004` MD_INFO · `0x00010008` VIDEO_ENCODER_INIT_DONE ·
`0x0001000D` FRAME_RATE_CHANGE_DONE · `0x00010014..1C` 电话/屏幕状态 · `0x00010021` GO_TO_DESKTOP ·
`0x00010022..24` 语音唤醒/结束/识别 · `0x00010026` MODULE_STATUS · `0x00010035` MEDIA_INFO ·
`0x00010036` MEDIA_PROGRESS_BAR · `0x00010037` CONNECT_EXCEPTION · `0x00010044` 订阅回复 ·
`0x00010049` MD_AUTHEN_RESPONSE · `0x0001004B` MD_AUTHEN_RESULT · `0x0001004C` GO_TO_FOREGROUND_RESPONSE ·
`0x0001004D` START_BT_AUTOPAIR_REQUEST · `0x00010051` FEATURE_CONFIG_REQUEST · `0x00010054` BT_IDENTIFY_RESULT ·
`0x00010059` MD_EXIT · `0x00020001` VIDEO_DATA · `0x00020002` VIDEO_HEARTBEAT · `0x00030001/3/4/6` MEDIA_* ·
`0x00040001/2/3` TTS_* · `0x00050002/3/4` VR_* · `0x00060009` UI_ACTION_SOUND

### A.3 硬按键（完整 30 项见指南表 10.6-1）

`01` Home · `02` Phone_Call · `03` Phone_End · `04` Phone_End_Mute(废弃) · `05` HFP · `06` Selector_Next ·
`07` Selector_Previous · `08` Setting(废弃) · `09` Media · `0A` Radio(废弃) · `0B` Navi · `0C` SRC(废弃) ·
`0D` Mode(废弃) · `0E` Back · `0F` Seek_Sub · `10` Seek_Add · `11` Volume_Sub(废弃) · `12` Volume_Add(废弃) ·
`13` Mute · `14` OK · `15` Move_Left · `16` Move_Right · `17` Move_Up · `18` Move_Down · `19..1C` 四角 ·
`1D` TEL · `1E` MAIN · `1F` Media_Start · `20` Media_Stop · `21` VR_Start · `22` VR_Stop ·
`23..2C` 数字 0–9 · `2D` `*` · `2E` `#` · `2F` DEL · `30` CLEAR · `31` `+`

### A.4 触摸

`0x00068001` TOUCH_ACTION `{action,x,y}`（0/1/2 = DOWN/UP/MOVE）· `0x00068002/3/4` DOWN/UP/MOVE `{x,y}` ·
`0x00068005/6/7` 单击/双击/长按 `{x,y}` · `0x00068008` HARD_KEY `{keycode}`

---

## 附录 B — 待确认清单

**板子上（Phase 0）**

```sh
# ffs 是否接受 ALL_CTRL_RECIP（flags=64）：写描述符头，看是否 EINVAL
# UDC 解绑/绑定是否稳定重枚举
echo "" > /sys/kernel/config/usb_gadget/<g>/UDC && echo musb-hdrc.4.auto > /sys/kernel/config/usb_gadget/<g>/UDC
# ECM 与 ffs 同 config：Mac 侧 ifconfig 看 en15 是否照旧拿到 172.16.0.2
```

**假车机上（Phase 1–3）**

```sh
cd ../CarLifePort && make all
dist/carlife-link --list                    # 能否看到 Duo2 的 gadget
dist/carlife-link --socket /tmp/carlife-link.sock --channel 20000000 --cuid CarLifePort -v
dist/carlifed                               # 浏览器出画即 Phase 2/3 通过
```

**真车上（Phase 4）**：车机是否发 51/52/53、是否发 `0x00018048`、`VIDEO_ENCODER_INIT` 请求的分辨率与帧率、
六条通道里哪几条有数据 —— 全部靠设备侧日志，日志从 Phase 1 起就按「每个控制请求一行、每通道首帧一行」打。
