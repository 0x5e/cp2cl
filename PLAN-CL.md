# CarLife 车机对接 — 开发计划与技术方案

> Linux 设备（NanoPi Duo2）通过 micro-USB **扮演 CarLife 手机端（MD）**&#x63A5;入车机（HU = 奔驰 MBUX NG6）；  
> iPhone 通过 **无线 CarPlay** 连同一台设备。视频重封装不重编码；**音频必须本地解 AAC-LC**（§4.2，无线实测只给 AAC）。  
> 主要工作量在协议：CarLife 的手机端协议栈**不存在开源实现**，必须自己写——本文档的一半篇幅就是在把这份协议钉死。

状态：2026-10-13 起草。**Phase 1–3 已实现**（`src/carlife/`，见 §6），**Phase 4 的鉴权一项已过**（§5.4 第 6 条）。  
参考材料：

| 材料                       | 位置                                                   | 用途                                    |
| ------------------------ | ---------------------------------------------------- | ------------------------------------- |
| 官方接入指南（174 页）            | `../CarLifePort/docs/百度CarLife车机手机互联方案接入详细指南new.pdf` | 协议条款、消息顺序、键值表、鉴权流程                    |
| 车机端开源库 CarLifeVehicleLib | `../CarLifePort/vendor/carlife`（Apache-2.0）          | **协议字典**：全部 `MSG_CMD_*` 与 protobuf 定义 |
| 车机端整合实现 CarLifePort      | `../CarLifePort`（`link/` C+C++、`server/` Go）         | 真机跑通过的 HU 侧 wire 行为、AOA 主机侧握手、实测坑     |
| 参考录音工具                   | `../CarLifePort/tools/aoa-probe`                     | 主机侧探针：发 51/52/53、发首帧、校验帧格式            |

---

## 0. 角色与拓扑

```
iPhone ──无线 CarPlay(AirPlay)──► cp2cl ──USB(AOA)──► MBUX NG6
        视频/音频下行、HID 上行              扮演 MD：CarLife 协议栈 + 视频/音频/触摸
```

| 参与者          | CarLife 角色    | USB 角色             | 谁先说话                                    |
| ------------ | ------------- | ------------------ | --------------------------------------- |
| MBUX NG6     | HU（车机端）       | **HOST**，供电并枚举     | **HU 先发** `MSG_CMD_HU_PROTOCOL_VERSION` |
| cp2cl / Duo2 | MD（手机端）       | **DEVICE**（gadget） | 被动应答                                    |
| iPhone       | CarPlay phone | —（无线）              | AirPlay 侧由我方 `/info` 应答                 |

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

| 现有能力                                 | 位置                                                               | 这次怎么用                                        |
| ------------------------------------ | ---------------------------------------------------------------- | -------------------------------------------- |
| AirPlay 会话几何参数（宽/高/帧率/ui_scale） | `src/airplay/server.rs:206` `ScreenParams`、`:263` `SharedScreen` | 由 CarLife 的 `VIDEO_ENCODER_INIT` 决定，写进去再重启会话 |
| 会话重启                                 | `src/main.rs` 的 `restart`（`broadcast`），`airplay::serve` 与两个 watch 各自订阅    | CarLife 侧发起几何变更时走同一条路                        |
| `/info` 的音频宣示                        | `src/airplay/info.rs:178` `audio_formats()`（条目在 `:187-197`）      | 音频走 AAC-LC，`102/media` 那条留着（§4.2）             |
| 视频帧（**已是 annex-B**）                  | `src/airplay/screen.rs` → `StreamEvent::VideoFrame`              | 直接塞进 CarLife 视频通道（§4.1）                      |
| 音频帧与格式                               | `src/airplay/audio.rs` → `StreamEvent::AudioPacket`（解码后是 PCM）      | 直通：AAC-LC 在这条路的起点就解成 PCM，进 CarLife Media 通道（§4.2） |
| 输入上行（HID + 会话命令）                     | `src/airplay/event.rs`、`src/commands.rs`                         | CarLife 触摸/按键接到这套词汇上                         |
| 麦克风上行                                | `src/airplay/mic.rs`                                             | CarLife VR 通道的 PCM 接过来                       |

这次要写的东西，以及它们现在的落点：

| 待补能力              | 落点                              | 状态                              |
| ----------------- | ------------------------------- | ------------------------------- |
| USB 设备（gadget / ffs / 重枚举） | `src/carlife/usb.rs`            | 已实现：configfs 组装、挂 ffs、写描述符、绑 UDC、换身份重枚举（只在 Linux 编） |
| 设备形态与 AOA 请求字节      | `src/carlife/desc.rs`           | 已实现：描述符与 strings blob、51/52/53 的识别、身份字符串；9 条单测，纯字节、跨平台 |
| AOA 控制请求的应答          | `src/carlife/gadget.rs`         | 已实现：ep0 事件循环、51/52/53 应答、触发重枚举、把两个 bulk 端点交给会话（只在 Linux 编） |
| CarLife MD 协议栈    | `src/carlife/{frame,proto,session}.rs` | 已实现：帧编解码、protobuf、握手状态机、心跳      |
| 桥接：视频             | `src/sink.rs` `ClOutlet`         | 已实现：`VideoFrame` 直通视频通道，参数集在下一帧前带出；`CP2CL_INPUT_FILE` 则改为重复一个文件 |
| 桥接：音频             | `src/aac.rs` + `src/sink.rs` `Media` | 已实现：AAC-LC 在 `airplay::audio` 里解成 PCM，经第二条队列进 Media 通道；`MSG_MEDIA_INIT` 在第一批采样之前发（§4.2）。**CPU 余量与两个取值未在真机确认** |
| 桥接：媒体信息           | `src/playing.rs` + `src/carlife/session.rs` | 已实现：`NowPlayingUpdate` → `MSG_CMD_MEDIA_INFO`，六个字段；`playlistNum`/`songId`/`mode` 留空（§4.6） |
| 桥接：导航信息           | `src/navigation.rs` + `src/carlife/session.rs` | 已实现：`RouteGuidanceUpdate` / `RouteGuidanceManeuverUpdate` → `MSG_CMD_NAVI_NEXT_TURN_INFO` + `MSG_CMD_MODULE_STATUS`；`totalDistance`/`time` 留空，`turn_kind` 仅映射语义一致的类型（§4.7） |

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

| wIndex | 字符串                         |
| ------ | --------------------------- |
| 0      | `Baidu`                     |
| 1      | `CarLife`                   |
| 2      | `Baidu CarLife`             |
| 3      | `1.0.0`                     |
| 4      | `http://carlife.baidu.com/` |
| 5      | `0720SerialNo.`             |

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
  也就是 51/52/53，且**在同 config 里还有别的函数时也成立**（`req_match` 是逐函数问的）。

应答方式就是 ffs 的标准 ep0 协议：读 `/dev/ffs-*/ep0` 拿到 `usb_functionfs_event`，IN 请求往 ep0 写数据，  
OUT 请求从 ep0 读数据。这正是 Android 17 里 `aoad` 走的路（把 AOA 从内核 `f_accessory` 迁到用户态）。

v4.14 **没有** `FUNCTIONFS_CONFIG0_SETUP` / configfs 的 `control_config`（那是后来的），但本用途不需要它。

### 2.4 描述符与重枚举

- 描述符由用户态在 ffs 的 ep0 上写（`usb_functionfs_descs_head_v2`，magic=3，flags 里带 `ALL_CTRL_RECIP`）。  
  在 `FUNCTIONFS_DISABLE` 之后可以重写，所以「先手机形态、后 accessory 形态」不需要两套 ffs 实例。
- `idVendor` / `idProduct` / `bcdDevice` / 字符串是 **configfs 属性**（`/sys/kernel/config/usb_gadget/<g>/…`），  
  不归 ffs 管，**由程序在运行期写**：出厂形态是 `18d1:4ee1` + `Google`/`Android`（Android 手机在 MTP 模式下的号码，  
  按号码或按厂商串判断的主机会认成手机），收到 53 之后改成 `18d1:2d00` + 车机发来的三条字符串。改属性前必须先解绑  
  UDC，所以「换身份」就是「解绑 → 写属性 → 重新绑」，端点 fd 跨这三步保持有效（§2.5）。
- accessory 形态的接口取值：CarLifePort 的主机侧**只认 `class 0xff && subclass 0xff`**（`link/aoa.c:496-502`，  
  并注释说明 adb 是 subclass `0x42`）。我们取 `0xff/0xff/0x00`。CarLife 的 HU 侧实现（`AOAHostSetup.java`）是  
  **按位置取端点**（`getEndpoint(0)` = IN 读、`getEndpoint(1)` = OUT 写），所以描述符里 **IN `0x81` 必须排在  
  OUT `0x02` 前面** —— 两个地址本身只是占位，FunctionFS 会通过 `usb_ep_autoconfig` 重写，方向与顺序才有意义。  
  `desc.rs` 的单测锁住了这个顺序。

### 2.5 UDC 只有一个，设备整个由程序组

板子只有一个 OTG 口。管理链路走 **AP**（`192.168.4.1`），USB 口整个交给 CarLife —— 设备以手机形态出现、  
被切成 accessory，中间还要重枚举，里面没有网络可放。**gadget、FunctionFS 实例、描述符、绑定全在程序里**  
（`src/carlife/usb.rs`），开机脚本不再碰 USB（`board/` 只剩 AP，见 `README.md`「Setting the board up」）。

- `Usb::open()`：清掉上次残留（解绑 + 卸载 ffs + 删 configfs 树）→ 驱逐厂商镜像预置的 `g_mass_storage`（它占着  
  UDC）→ 建 `cp2cl` gadget、写身份、建 `configs/c.1`、**链接 `functions/ffs.carlife`** →  
  `mount("carlife", "/dev/usb-ffs/carlife", "functionfs")` → 写描述符 blob、**再写 strings blob**（两次写都落地  
  实例才 ACTIVE）→ 开 ep0/ep1/ep2 → 绑 UDC。
- `Usb::reidentify()`：**解绑 → 改 `idVendor`/`idProduct`/strings → 重新绑**。configfs 不允许改一个已绑定到 UDC  
  的 gadget，所以换身份只能是这三步；端点 fd 跨它保持有效，描述符也不用重写。
- 解绑写 `"\n"` 而不是零长度写：长度为 0 的 `write` 到不了内核（syscall 直接返回）。本来没绑定时返回 `ENODEV`，忽略。
- 绑定遇到 `EBUSY` 时读 `holder()` 指出是谁占着 UDC —— 通常是上一套安排留下的 gadget —— 并提示重启。

⇒ **程序的存活期就是设备的存活期**：起进程 = 设备上总线，Ctrl-C = 设备下总线、主机重新枚举。管理链路不受影响（走 AP）。

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

| 通道                       | 头长   | 布局（大端）                                              | 体上限                                                |
| ------------------------ | ---- | --------------------------------------------------- | -------------------------------------------------- |
| cmd / touch              | 8 B  | `u16 bodyLen`、`u16 reserved`、`u32 serviceType`      | cmd 40 KB / touch 10 KB                            |
| video / media / tts / vr | 12 B | `u32 bodyLen`、`u32 timestamp(ms)`、`u32 serviceType` | video 600 KB / media 100 KB / tts 50 KB / vr 50 KB |

（上限取自 CarLifeVehicleLib 的 `CMD_DATA_SIZE` 等常量，`CTranRecvPackageProcess.h:26-45`；CarLifePort 侧  
同值，`link/bridge.c:111-144`）。`reserved` 对端可能不填，**解析时忽略、回包填 0**。

### 3.2 方向位

`serviceType` 低 16 位的 **bit15 = 1 表示 HU→MD，= 0 表示 MD→HU**（指南 §10.1）。前两字节是通道  
（`0x0001`=cmd、`0x0002`=video、`0x0003`=media、`0x0004`=tts、`0x0005`=vr、`0x0006`=touch）。  
特例：`0x00010029`（档位）与 `0x00010032`（车数据订阅完成）**值上没有 0x8000 却是 HU 发的**，  
是库的历史遗留，按 HU→MD 解析（`CTranRecvPackageProcess.cpp:2234,2273`）。

### 3.3 握手与会话（HU 先说话）

| #  | 方向    | serviceType                                  | 体                                               |
| -- | ----- | -------------------------------------------- | ----------------------------------------------- |
| 1  | HU→MD | `0x00018001` `HU_PROTOCOL_VERSION`           | `major=1, minor=0` ⇒ 线上 `08 01 10 00`（实测）       |
| 2  | MD→HU | `0x00010002` `PROTOCOL_VERSION_MATCH_STATUS` | `matchStatus=1`（MATCH）                          |
| 3  | HU→MD | `0x00018003` `HU_INFO`                       | `CarlifeDeviceInfo`（全字段 optional）               |
| 4  | MD→HU | `0x00010004` `MD_INFO`                       | 同结构；`os` 填 `Android`                            |
| 5  | HU→MD | `0x00018027` `STATISTIC_INFO`                | `cuid/versionName/versionCode/channel/connect*` |
| 6  | HU→MD | `0x00018048` `HU_AUTHEN_REQUEST`             | `randomValue = "A_B"`（见 §3.5）                   |
| 7  | MD→HU | `0x00010049` `MD_AUTHEN_RESPONSE`            | `encryptValue`                                  |
| 8  | HU→MD | `0x0001804A` `HU_AUTHEN_RESULT`              | `result`                                        |
| 9  | MD→HU | `0x0001004B` `MD_AUTHEN_RESULT`              | `result`                                        |
| 10 | MD→HU | `0x00010051` `MD_FEATURE_CONFIG_REQUEST`     | **空体**                                          |
| 11 | HU→MD | `0x00018052` `HU_FEATURE_CONFIG_RESPONSE`    | key-value 列表                                    |
| 12 | HU→MD | `0x00018007` `VIDEO_ENCODER_INIT`            | `{width, height, frameRate}`                    |
| 13 | MD→HU | `0x00010008` `VIDEO_ENCODER_INIT_DONE`       | 同上，填**我方实际能给的值**                                |
| 14 | HU→MD | `0x00018009` `VIDEO_ENCODER_START`           | 空                                               |
| 15 | MD→HU | 通道 2 上的 `0x00020001` `VIDEO_DATA`            | annex-B H.264                                   |

指南的硬性时限：`STATISTIC_INFO` 必须**连接成功后 2 分钟内**到（不到则 MD 主动断开，§19.2）；  
MD 收到 `GO_TO_FOREGROUND` 后 5 秒没切前台要回 `0x0001004C`。**都是对 HU 的要求，我们做 MD 侧只需容忍  
对方不做**——但 §3.3 的 1→2 和 4→13 是我们必须回答的，不回答链路走不下去  
（CarLifePort `CarLifeLibTest.cpp:277` 明确写了「必须先发 0x00018001」）。

### 3.4 MD 侧必须发出的消息（最小集）

| serviceType                                | 何时                | 体                                                                      |
| ------------------------------------------ | ----------------- | ---------------------------------------------------------------------- |
| `0x00010002`                               | 收到 1 后            | `{matchStatus=1}`                                                      |
| `0x00010004`                               | 紧跟其后              | `CarlifeDeviceInfo`（`os/brand/model/release/sdk_int/btaddress/btname`） |
| `0x00010008`                               | 收到 `0x00018007` 后 | `{width, height, frameRate}`（**实际值**）                                  |
| `0x00010049` / `0x0001004B`                | 见 §3.5            | 视策略                                                                    |
| `0x00010051`                               | 认证流程走完后           | 空体                                                                     |
| `0x0001000D`                               | 收到帧率变更            | `{frameRate}`                                                          |
| `0x00020001`                               | 有画面时              | annex-B 裸流 + 12 字节头                                                    |
| `0x00020002`                               | 无画面时（替代视频包）       | 空；协议只要求 MD 切后台时这么发，这里用来在没接 iPhone 时保活                                  |
| `0x00030001` / `0x00040001` / `0x00050002` | 相应音频开始前           | `CarlifeMusicInit{sampleRate, channelConfig, sampleFormat}`            |
| `0x00030006` / `0x00040003` / `0x00050003` | 有音频时              | 裸 PCM                                                                  |
| `0x00060009`                               | 可选                | UI 音效；CarLifePort 的接收侧是死代码，可以不发                                        |
| `0x00010059`                               | 退出时               | 空                                                                      |

**注意 `MSG_TTS_INIT` 用的也是 `CarlifeMusicInit`**，不是 `CarlifeTTSInit`（`CTranRecvPackageProcess.cpp:1535`）。

### 3.5 鉴权：两条流程，17.1 是唯一有密码学的一侧

**17.1（HU 认证 MD）**：HU 发 `randomValue = "A_B"`（A 是随机串，B 是车机要求的最低手机版本），  
MD 用**自己的私钥**做 `RSA/ECB/PKCS1Padding`，把结果的 Base64 放进 `encryptValue` 回给 HU；  
HU 用百度发给车厂的公钥还原并与自己发出的串比对，不一致 ⇒ **HU 主动断开**。

私钥在手机端，官方不给，但**已从 `com.baidu.carlife` 8.9.0 的 dex 里取出**（过程见  
`../reverse-carplaybox/carlife_app/CARLIFE_AUTH.md`），放在 `authentication/carlife_md_rsa_private.pem`  
（`authentication/` 在 `.gitignore` 里）。实现在 `src/carlife/auth.rs`：

- **版本规则照抄手机端**：把 `B` 逐段与自称的版本（`CLAIMED_VERSION = 8.9.0`，即取到密钥的那个版本）比较，  
  **第一处不同就决定**；车机要的更新 ⇒ 回 `A_00.00.00`（手机端表达「我太旧」的方式），否则原样回。  
  没有 `_`、或段数不是 3 ⇒ 原样回 —— 手机端会抛异常因而什么都不回，我们选能通过车机比对的那条。
- **填充用 `Pkcs1v15Sign::new_unprefixed`**（块首 `00 01 FF…FF 00 00` 后跟明文），即 Java  
  `RSA/ECB/PKCS1Padding` 在 SunJCE 下的输出形状，也是指南写下的那个 cipher 名。块首是 `01` 而不是  
  加密填充的 `02` —— **车机接受这个形状**（2026-10-16 真机验证）。
- **没有密钥就不发**（与手机端拿不到密钥时的结果一致），日志 warn 一行指出缺哪个文件。

**17.2（MD 认证 HU）**：MD 用 `STATISTIC_INFO` 里的渠道号判断车机合法性，非法则主动断开。  
**我们是 MD，这一侧的判定权在我们手里**。规则从手机端 dex 读出：**8 位十进制、首位 `2`，且不是  
`20000000`（后装）/`20000001`（预装）** —— 是形状不是白名单，别的 `2xxxxxxx` 一律放行。  
实现上收到 `STATISTIC_INFO` 即回 `MSG_CMD_MD_AUTHEN_RESULT`，**真车渠道号是什么都回**：不合规则也说，  
但不因此断开（车机是画面唯一的去处，一个不合规则的渠道号是「说一句」的理由，不是拿掉画面的理由）。

**已验（2026-10-16 真机）**：车机发 `0x00018048`，并接受上面那个块（`[cl] the car accepted us`）—— 两个方向都通了。

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

| 流        | 通道 | 格式                                    | 方向    |
| -------- | -- | ------------------------------------- | ----- |
| Media    | 3  | PCM 48 kHz / 双声道 / 16 bit 小端          | MD→HU |
| Navi TTS | 4  | PCM 16 kHz / 单声道 / 16 bit 小端          | MD→HU |
| VR TTS   | 5  | PCM 16 kHz / 单声道 / 16 bit             | MD→HU |
| VR MIC   | 5  | PCM 16 kHz / 单声道 / 16 bit，**每包 1 KB** | HU→MD |

`AUDIO_TRANSMISSION_MODE`（feature config 下发，默认 `0` = 专用音频通道，`1` = 走蓝牙）决定 HU 是听通道还是  
听蓝牙；我们要的是 `0`，但那是 **HU 发给我们**的，不接受也没得选。

表里的格式是**我们报给车机的**（`MSG_MEDIA_INIT`），不是它要求我们给的：采样率报的是手机上 AAC-LC 的采样率
（解码不改采样率与声道数），**实测无线下手机给的是 48 kHz**（§4.2 有据）。车机自己期望哪一档由 feature config 的
`MEDIA_SAMPLE_RATE` 说明（`1` = 48 kHz，默认 `0`），那只是车机的期望，我们不去满足它——我们只报手机给的那个。

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

1. **编码家族一致**：`/info` 只宣示 H.264（CarLife 这边没有可协商的编解码字段），两端都 H.264。
2. **profile 兼容**：指南只保证 baseline L3.1。CarPlay 侧实际多为 Main/High，可能带 B 帧。**先直通试，  
   车机不接受再想办法**（§8 风险 3）。
3. **单帧 ≤ 600 KB**：1280×720 / 30 fps 的 IDR 一般在 100–300 KB，安全；1080p 大关键帧可能越界。  
   由 `INIT_DONE` 把分辨率压在我们能保证的档位。

几何对齐走的是同一个 `SharedScreen`（`server.rs:259`）：CarLife 的 `VIDEO_ENCODER_INIT` 谈定后，`carlife::session`  
把 `width/height/fps` 报给 `main`（`run` 的 `screen` 回调），`main` 写进 `SharedScreen`，并在**变了**的时候发一次  
`restart`——就是「改了几何要重开会话」这条规则本身，代价也一样：**几何只在「新会话」生效，所以要拆掉当前 AirPlay  
会话重建**。未连接的手机不受影响。

### 4.2 音频

**实测结论（2026-10-13，无线 CarPlay 真机）：手机不给 PCM，媒体流只给 AAC-LC。** `/info` 里开出的 `100/media`  
PCM 与 `102/media` AAC-LC 两条路，手机选的是后者；`info.rs:173-177` 的注释原本假设「只有有线会出现 PCM」，而这一侧  
就是无线，所以 PCM 从来不会来。⇒ **音频必须在本地解 AAC-LC**，不能直通。

已落地的做法（2026-10-16）：

- `/info` 的 `audio_formats()`（`info.rs:178-197`）保持现状——`100/media` 的 PCM 位留着无用但无害，`102/media` 的  
  `aac_lc` 是手机真正会用的那条。**不要试图删掉 AAC 条目来把手机逼回 PCM**：实测无线下手机不产生 PCM，  
  删掉这一条只会让媒体流无从选择，不会换来 PCM。
- **解码在 `airplay::audio`（`AudioReceiver`），不在 CarLife 侧**：流是什么编解码器只有那里知道，而且那是
  每条流一个实例、生命周期与流一致的地方。`new` 时按 `AudioCodec` 建 `aac::Decoder`，`decode()` 出来就已经是
  PCM，`sink.rs` 收到的 `AudioPacket` 是 PCM。
- **`symphonia` 只开 `aac`**（`default-features = false, features = ["aac"]`，落 0.5.5）：只要解器，不要
  demuxer 与其它编解码器。二进制增加约 230 KB（12.36 → 12.59 MB）。
- **没有重采样**：`MSG_MEDIA_INIT` 里的 `sampleRate` 就报解出来的采样率，车机按它开 AudioTrack。解码不改变
  采样率与声道数，所以报的就是手机上 SETUP 谈定那两个数。
- **AAC-LC 一律按 48 kHz 读**（2026-10-16 实测）：手机在 SETUP 里把 `audioFormat` 回成 `0x400000`，而这个位按
  LIVI 的表念是 44.1 k——**念错了**。同一次会话 20.71 s 收到 972 个包 = 46.93 包/s，一个包一个 1024 样本的
  AAC 帧 ⇒ **48.06 kHz**，而 48000 ÷ 1024 = 46.875。所以 `from_format` 现在把两个 AAC 位都当 48 k。
  **错一处的代价是三处**：重排按 1024/44100 = 23.22 ms/帧 走，比声音慢 8.8% ⇒ `pump_audio` 的队列一直涨
  （5 秒涨 18 帧、20 秒攒到 90 帧 ≈ 2 s 音频），车机那头随之一直缺料；报给车机的 `MSG_MEDIA_INIT` 与
  `/feedback` 里回报的播放位置也都跟着偏 8.8%。
- **只有 media 进这条通道**：`sink.rs` 按 `AudioConfig` 的 `audioType` 认流（`media` / `telephony` /
  `speechrecognition` 是同一条事件流上的三样东西），只有 `media` 会被记住并转发；`AudioStopped` 转成
  `MSG_MEDIA_STOP`。格式只在变化时发一次 `MSG_MEDIA_INIT`。
- `MSG_MEDIA_INIT` 的三个字段：`sampleRate` = 解出来的采样率，`channelConfig` = 声道数，`sampleFormat` = 16。
  车机侧 `AudioTrackManagerSingle` 把 `channelConfig` 读成「1=单声道，其余=立体声」、把 `sampleFormat` 读成
  「8=8 bit，其余=16 bit」，所以这两个值是按位宽与声道数本身给的，不是编码值。
- **TTS/VR（通道 4/5）仍然留空**：那两个低带宽流是车机自己的语音通道，而且 `symphonia` **没有** Opus 解器
  （早先记的「`symphonia` 的 `opus` feature」是错的），要做得另找库或移植。车机不会因为这两个通道没数据而拒会话。
- **重排在上游，不在 CarLife 侧**（2026-10-16 订正）：`media` 那条流在 `102` 上是**成块**到达的（约 384 ms 一批，
  18 包），接收侧 `airplay::server::pump_audio` 里一直有按手机时间戳的重新计时（`paced = stream_type == BUFFERED_AUDIO`、
  `wait_due`/`PACE_SLACK` 8 ms/`PACE_RESET` 5 s），CarLife 侧只是把交到手上的 PCM 写出去。
  **随浏览器一起删掉的只有前端自己的播放窗口（0.12/0.35），不是这段重排。**
  现在**解码也挪到了「每帧到期的那一刻」**：收包时只存密文帧与（明文里的）时间戳，到期才解——一批 18 帧在
  收到时连着解，会让这一轮循环错过它自己的排期（回来时已经有几帧逾期，`PACE_SLACK` 一到就被一起发出去，
  正是重排想避免的那种 burst）。
- **队列按时间戳排，不按到达顺序**（2026-10-16）：`hold()` 把每帧插到它时间戳该在的位置，并丢掉「比已经发出去
  的那一帧还早」的帧。Wi-Fi 会偶发乱序（本次实测约 0.6 次/s，在日志里表现为 `missing` 以 65536 的整数倍往上
  跳——那是序号倒退，不是丢包），乱序帧直接喂给解器，它自己就会说 `previous window is invalid`
  （symphonia 的 `debug!`；日志里**没有 `[tag]` 前缀**的行都是依赖里的），21 ms 的错位听感上就是一声咔。
- **判据在日志里**（`CP2CL_LOG=debug`，每 5 秒一行 `[airplay] audio: …`）：
  - `missing` / `out of order` / `duplicates`：线上的事（丢包 / 乱序 / 重包），累计；
  - `worst gap` / `bursts of N`：到达的块状程度（成块是手机的常态，一批 18 帧 ≈ 384 ms）；
  - `N held`：堆在队列里的帧数——**它必须不涨**，涨就是收得比发得快；
  - `X frames/s arrived (Y Hz), Z sent/s`：窗口内的收发速率，以及「到达速率换算成的采样率」。
    `X` 与 `Z` 对不上就是排期与声音不同步；`Y` 落在 44.1 k 还是 48 k 一眼可辨；
  - `N ms spent (worst M ms)`：解码侧。**一帧的预算**是 21.33 ms（1024 样本 ÷ 48 k），
    `worst` 逼近它就是余量不够；`N` 对 5 秒窗口的占比就是单核占用。
- **待验**：① Cortex-A7 单核实时解 AAC-LC 的余量——H3 四个核、视频侧只做搬运，理论上够，但这是整个方案里
  **唯一**引入实时解码的地方，上板后用上面那行日志与 `top` 量；② `channelConfig`/`sampleFormat` 在真车上的取值。
  ③ 已确认：一个 RTP 包就是一个 AAC 帧（`packets = decoded + held` 一直精确成立）。

### 4.3 反向输入

CarLife 触摸（设备像素坐标）→ AirPlay 事件通道的 HID 触摸；CarLife 硬按键 → `src/commands.rs` 的词汇：

| CarLife 键值                              | cp2cl 输入                            |
| --------------------------------------- | ----------------------------------- |
| `0x14` OK                               | `knob.select`                       |
| `0x1E` MAIN                             | `home`                              |
| `0x0E` BACK                             | `back`                              |
| `0x15/0x16` Move_Left/Right             | `knob.x` 方向                         |
| `0x06/0x07` Selector_Next/Previous      | `knob.y` 方向                         |
| `0x1F/0x20` Media_Start/Stop            | `media 3`（播放/暂停）                    |
| `0x10/0x0F` Seek_Add/Sub                | `media 4` / `media 5`               |
| `0x21/0x22` VR_Start/Stop               | `siri`                              |
| `0x02/0x03` Phone_Call/End              | `telephony 1` / `2`（按通话状态选，与显示面板一致） |
| `0x13` Mute                             | `telephony 2` 的静音语义                 |
| `0x0B` Navi / `0x1D` TEL / `0x09` Media | 无对应，丢弃                              |

`night`、`keyframe` 在 CarLife 侧没有对应，丢弃。

### 4.4 麦克风

CarLife `VOICE_MIC=0` 时麦克风在车机侧，HU 会把 16 kHz 单声道 PCM 从 VR 通道（`0x00058001`，每包 1 KB）  
推给我们；这正是 `src/airplay/mic.rs` 要的上行格式。`VOICE_MIC=1` 时反过来——由我们推 VR TTS 到通道 5。

### 4.5 会话生命周期

**消费者只有车机，它就是总开关。** CarLife 会话没起来时，CarPlay 侧不接受连接、不扫描也不认证手机、不发布 iAP  
服务；车机断开时，正在跑的 AirPlay 会话被主动拆掉。手机何时连、连不连都不影响这条规则。浏览器客户端已经去掉，  
`CP2CL_SINK` 随之消失，所以闸门没有「恒开」的那一种情形：CarPlay 侧只有在车机在的时候才是活的。

几何也不再靠环境变量决定：`carlife::session` 在 `VIDEO_ENCODER_INIT` 谈定后把 `width/height/fps` 报给 `main`  
（见 §4.1），`main` 写进 `SharedScreen` 并只在值变了时发一次 `restart`。车机什么都没说时，两侧用同一份默认  
（1280×720 / 30，即 `ScreenParams::default()`）。

（那个 TCP 端口仍然在启动时就 bind——它是 `/info` 要告诉手机的端口，必须在车机出现之前就定下来；受闸门控制的是  
**是否 accept**，所以闸门关着时没有 CarPlay 会话能建立起来。）

机制是一个 `tokio::sync::watch::channel(false)` 当闸门（起手关着，只有车机会开），加一个 `broadcast` 当断开信号（`src/main.rs`）：

| 位置                       | 对闸门的处置                                                              |
| ------------------------ | ------------------------------------------------------------------ |
| `airplay::serve`         | 关门时不 `accept`，只在 `gate.changed()` 上等；开门时才进 `select!` 的 accept 分支 |
| `session::watch`         | 开门前不枚举 USB、不 attach                                                |
| `wireless::watch`        | `ready()` 之后、`bt::start()` 之前等闸门——iAP 服务是「这里有 CarPlay」的宣告，不发在没有的时候   |
| `carlife::session::run`  | 收到 `HU_INFO` 时把闸门打开（只开一次）；会话结束时关上；`VIDEO_ENCODER_INIT` 谈定后另外报一次几何        |

开门时同时发一次 `restart`：`CarPlayStartSession` 是 accessory 对手机 `CarPlayAvailability` 的回答，手机不会自己  
再问一遍，所以只能丢掉当前 iAP2 会话重开一轮。断连走另一条 `shutdown`，与 `restart` 分开——`restart` 的意思是  
「重来」，`shutdown` 的意思是「没什么可重来的」。

其余时序：

- 车机重插：USB 读失败 ⇒ 重新组一次设备（**解绑 → 重新绑 UDC**，§2.5）并重建协议栈；CarLifePort 的教训是  
  **会话必须在新的 accessory 附着上重建**，不能原地重连（`README.md:233-242`）。
- 车机在、iPhone 没来：CarLife 握手照走完，视频通道发 `VIDEO_HEARTBEAT` 保活（指南说 MD 切后台时就是这么做的），  
  AirPlay 侧只是没有人连进来。
- 拔掉 iPhone：CarLife 会话不受影响，退回发心跳。

### 4.6 媒体信息

车机面板要显示「当前歌曲播放信息」，对应 `MSG_CMD_MEDIA_INFO`（`0x00010035`，指南 §13.2，刷新时机是「开始播放或者切换歌曲时」）。
CarPlay 侧对应的是 iAP2 的 `NowPlayingUpdate`（`0x5001`），在 `bringup::subscriptions` 里随其余订阅一起订上，作为一条普通 incoming 消息到达。

两侧形状不同：手机每次只描述一个属性（改了标题就只发标题），封面另起一个文件传输、且不指名属于哪首歌；车机的消息则一次给出整首歌。
`src/playing.rs` 是两者对齐的地方——`Track::from_iap2` 读一条更新，`merge` 把「只说了变动」的更新折进已知内容，`carlife::session` 在折出来的结果变化时才发一条 `MEDIA_INFO`。

发之前还要等一个停顿：手机描述一首歌是好几条消息（标题、歌手、专辑、时长），一条一发会让面板同一首歌重画好几遍。
所以每条更新都把定时器推后 `CP2CL_MEDIA_DELAY_MS`（默认 1000 ms），到点才把折好的整首歌发出去；期间到达的消息只并进已知内容，不额外发。
设为 0 就退化成「每条更新立即发」。

`CarlifeMediaInfo` 九个字段，指南称都是必填，这里写六个（`source` / `song` / `artist` / `album` / `albumArt` / `duration`，`duration` 是秒）。剩下三个留空而不是写 0：

- `playlistNum`、`songId`：手机没订 `persistent_id` 与 `app_bundle_id`（`bringup::subscriptions` 里这两个 flag 是 false），拿不到值；
- `mode`：iAP2 根本没有描述播放模式的消息。

留空与 0 不是同一件事：车机自己的解析器读到提前结束的 body 会用默认值回调，与「如实说没有」一致；而 `mode=0` 等于告诉面板单曲循环。

封面文件不指名歌曲，按「属于最新那首」处理；标题一变就把它放下，免得新歌名下面挂着旧封面。

封面还有大小限制：它和歌曲同处一条命令通道消息，而命令通道 body 上限 40 KB，参考实现（`carlife_app` 的 `MusicInfoSender`）把封面压到 30 KB 以内再发。
超过 `proto::MAX_ART`（30 KB）的封面由 `src/artwork.rs` 重编码后再发：解出 RGB、按 EXIF 方向转正、去掉全部元数据，输出 baseline JPEG + 4:2:0；
尺寸先降到长边 400px，质量 90/80/70/60 逐档试，仍不行再把长边降到 350、300（只配 60 质量），最多 6 次编码，还不行才丢弃并告警，歌照发。
本来就够小的封面按原字节直传，不做二次压缩；解码上限 2048×2048，超过的不解。
重编码发生在 CarPlay 侧的 `carry_artwork` 任务里（`spawn_blocking`），不占写流的线程；同一张封面重复到达时按长度 + FNV-1a 指纹认出来，不重复解码。
另有一道通用防护：`send_frame` 在写之前比对通道上限（命令 40 KB / 媒体 100 KB / 视频 600 KB），超了告警并丢弃——车机读不了这种消息，且长度不符会让整条通道断开，丢一条比丢会话划算。

`MSG_CMD_MEDIA_PROGRESS_BAR`（`0x00010036`，播放进度）没有发：手机那边有 `elapsed_ms`，但这条消息不在本次范围内。

### 4.7 导航信息

车机简易诱导条对应 `MSG_CMD_NAVI_NEXT_TURN_INFO`（`0x00010030`，指南 §13.1），刷新时机是「有新的诱导信息时」；
导航模块状态对应 `MSG_CMD_MODULE_STATUS`（`0x00010026`，指南 §10.2）。
CarPlay 侧对应的是 iAP2 的 `RouteGuidanceUpdate`（`0x5201`）和 `RouteGuidanceManeuverUpdate`（`0x5202`），
前者随其余订阅一起订上，在 `bringup::subscriptions` 里作为普通 incoming 消息到达。

两侧形状不同：手机把整条路线拆成两份——一份说「路线整体状态 + 当前是哪几步」（`0x5201`），
另一份按索引逐个描述每一步的转向类型与进入道路名（`0x5202`）；
车机则只要「下一步是什么、多远、在哪个路口」。
`src/navigation.rs` 是两者对齐的地方——`Guidance::from_iap2` 读一条更新，`merge` 把「只说了变动」的更新折进已知内容，
`carlife::session` 在折出来的结果变化时才发一条 `NAVI_NEXT_TURN_INFO`，并在路线起停时发 `MODULE_STATUS`。

`CarlifeNaviNextTurnInfo` 六个字段，这里写四个（`action` / `nextTurn` / `roadName` / `remaindistance`）。剩下两个留空：

- `totalDistance`：iAP2 对「到下一步」只有一个距离（`distance_to_maneuver`），没有「总距离 vs 剩余距离」的区分；
- `time`：iAP2 有到目的地的 `time_remaining`，但没有到下一步的秒数；把到目的地的时间填进「到下一步」的字段会误导司机。

`turn_kind` 仅映射语义一致的类型。手机有 54 种 `EManeuverType`，车机有 58 种 `TurnKind`；
 ramps（匝道上下）、motorway changes（换高速）、arrivals（到达）在车机列表里没有对应箭头，
 宁可留空也不近似成「左转」或「直行」。环岛出口 1–19 全部映射为 `RING`（`0x0E`），因为车机没有细分到第几个出口。

诱导条只在车机订阅后才发（`MSG_CMD_CARLIFE_DATA_SUBSCRIBE_START`，`0x00018045`）。
未订阅前只发 `MODULE_STATUS`——那是手机自身状态，不是订阅数据。
车机停止订阅（`0x00018046`）时把诱导条收起来（`ACTION_HIDE=3`），但导航模块状态不受影响。

`MSG_CMD_NAVI_ASSITANTGUIDE_INFO`（`0x00010047`，电子眼/限速相机）不在本次范围内：iAP2 没有对应消息。

---

## 5. 实验装置与验证

### 5.1 假车机（**最重要的一台设备**）

`../CarLifePort` 是 macOS 上能跑的 HU 实现。角色摆好就是我们要的测试台：

```
Duo2(micro-USB, gadget) ──USB──► Mac(CarLifePort: carlife-link + carlifed + 浏览器)
```

Mac 是 host、Duo2 是 device，拓扑与「车机 ↔ 我们的设备」完全一致。可用的抓手：

- `dist/carlife-link --list` 枚举；`--vid/--pid` 强制指定目标；`aoa-probe --handshake-only / --force-handshake / --send-proto-version` 单独验控制阶段。
- `carlifed` 带浏览器前端：CarLife 会话一旦建立，**Mac 上就能看到我们推过去的画面**，这是端到端的第一道验收。
- 日志级别 `CL_LOG_LEVEL=debug` 能看到逐通道转发统计与手机侧播放进度。
- 板子的管理链路走 AP，**插车时也不掉**：USB 口上跑的是 CarLife，与登录用的链路无关（§2.5）。板子上要看日志  
  就是 `journalctl -u cp2cl`，站在 AP 那一侧。

一台机器就能把 Phase 1–3 全部验完，**不需要车**。

### 5.2 参考录音（MD 侧的真值）

反过来角色：Mac 跑 `carlife-link` 当车机，插上装了 CarLife 的 **Pixel 7a**，把手机端在完整会话里的  
报文逐条录下来（`carLifeLibTest` 的回调、`aoa-probe` 的收发、或 `stonexu1986/carlife-protocol-analysis-tool`  
的解析器）。我们写 MD 侧时，**手机的行为才是标准答案**：消息顺序、`MD_INFO` 里填了什么、  
`encryptValue` 的长度与形态、视频包的时间戳步进、`MEDIA_INIT` 的 `channelConfig/sampleFormat` 取值（我们现在按
声道数 / 16 位给，见 §4.2）。  
这份录音应当作为 Phase 0 的产出归档（`docs/carlife-md-capture.md` 之类）。

### 5.3 离线验证

已落地的单测（`cargo test`，全部不需要 USB）：

- **帧编解码**（`frame.rs`，10 条）：头长与长度字段、带/不带时间戳的往返、`reserved` 非零不影响、方向位含例外、  
  逐字节分片仍能拼整、一次 write 两个块分别读出、截断报错、未知通道拒绝、空管道当结束。
- **protobuf**（`proto.rs`，18 条）：逐条消息的字节级断言（含 `1280` 在线上是 `08 80 0A` 这类 varint 细节）、  
  未设字段不写 0、负 int32 十字节符号扩展、截断体之前仍可读、`MSG_MEDIA_INIT` 三个字段的字节序、  
  鉴权应答是单个字符串字段；**新增**导航相关的 `MODULE_STATUS` 列表体、`NAVI_NEXT_TURN_INFO` 字段选择性写入、订阅应答双向编解码。
- **状态机**（`session.rs`，19 条）：自带一个 `tokio::io::duplex` 假车机，喂完整握手序列并断言发出的消息与顺序；  
  含触摸/按键上行的映射、fps 上限、两条**否定式**断言（未 `START` 前视频通道静默、没有密钥时鉴权请求不回），  
  以及 Media 通道（格式先于采样、同格式不重发、电话流不进来、`AudioStopped` 转 `MSG_MEDIA_STOP`）、  
  渠道号四种取值下的 `MSG_CMD_MD_AUTHEN_RESULT`、用临时生成的密钥应答一次挑战并用公钥验回，  
  **以及导航通道**（订阅前只发模块状态、订阅后发出完整诱导、车机停止订阅时收起诱导条、无法映射的转向类型留空）。
- **导航**（`navigation.rs`，7 条）：LIVI 捕获帧的完整往返（状态、距离、转向列表、单步描述）、  
  `distance_to_maneuver` 与 `distance_remaining` 的区分、`START_ROUTE` 占位步被跳过、`turn_kind` 映射表断言。
- **鉴权**（`auth.rs`，6 条）：块形状（`00 01 FF…FF 00` 后跟明文）与公钥验回、版本规则的两个方向、
  不是版本的挑战原样回、渠道号的合法与非法取值、两种 PEM 形状都能读；
  另有 1 条**只在 `authentication/` 里真有密钥时断言**的测试：用它应答并用随密钥一起来的公钥验回 ——  
  本机（取到密钥的机器）会跑，别的机器直接跳过。
- **音频解码**（`aac.rs`，3 条）：一帧真 AAC-LC（ffmpeg 编的 440 Hz 正弦，去 ADTS 头）解成 1024 帧 × 2 声道 ×  
  2 字节且不是静音；截断帧与空帧被拒；不支持的采样率/声道数在建解器时就拒。
- **流向**（`sink.rs`，1 条）：只有 `media` 那条流会进 Media 通道，并且格式先于采样。
- **描述符**（`desc.rs`，9 条，跨平台）：把 blob 当字节读，核对 magic/flags/两个 count、接口的  
  class/subclass/protocol、**IN 在 OUT 之前**、两段的 `wMaxPacketSize`（FS 64 / HS 512）、strings blob 的  
  magic/length；51/52/53 被识别而别的请求不被识别、逐字段解包、短读返回 `None`、六条身份落位与越界索引被丢弃、  
  版本答复 `[2,0]`、accessory 形态的号码与字符串。

仍缺的：用 §5.2 的录音当输入做表驱动对账（字段取值对不对，只能靠真手机的行为判）。

### 5.4 上真车（MBUX）验收

1. 插上后 MBUX 出现 CarLife 入口，点击后进入 CarLife 全屏。
2. 画面出现且触摸有效；旋钮/方向键有效。
3. 手机（iPhone）接上后画面切进来，触摸控的是 CarPlay 界面。
4. 拔插车机侧 USB 能自动恢复；拔插 iPhone 不影响 CarLife 会话。
5. `dmesg` 无 USB 复位风暴；连续跑 30 分钟不冻帧。
6. **鉴权两问**（§3.5）：**已通过**（2026-10-16 真机）—— 车机发 `0x00018048` 并接受我们的应答，  
   我们按它的渠道号回了判定。

---

## 6. 分阶段

### Phase 0 — 事实与设施（只读 + 搭台子）

- `0.1` 板子侧把 §2.2 之外的两件事验掉：**ffs 是否接受 `ALL_CTRL_RECIP` flag**〔**已验**：66 B 的 v2 头被接受〕、  
  **UDC 解绑/绑定能否稳定重枚举**〔**未做**：现在由程序自己做（§2.5），验收要真机〕
- `0.2` Mac 侧把假车机跑起来，确认它能挑到 Duo2 的 gadget（先用一个最简单的 ffs gadget 当靶子）。〔**未做**〕
- `0.3` 用 Pixel 7a 录一份 MD 侧参考报文（§5.2），归纳出「必须发的消息」清单与字段取值。〔**未做**——Phase 2 是  
  按指南 + `CarLifeVehicleLib` 的常量写的，不依赖录音；但录音对 Phase 3/4 逐字段对账仍是最快的路〕

验收：板子上的两条命令有明确结论；Mac 能对 Duo2 完整做一次 51/52/53 并读到重枚举后的设备；  
参考录音里能看到 `0x00010002` / `0x00010004` / `0x00010008` / `0x00010051` 这几条。

### Phase 1 — USB 设备与 AOA MD 侧 ✅

`src/carlife/usb.rs` 与 `src/carlife/gadget.rs`（两个模块整块 `#[cfg(target_os = "linux")]`），设备形态的字节  
在 `src/carlife/desc.rs`（跨平台）：

- **设备是程序的**：`usb.rs` 在 configfs 里组 gadget、挂 FunctionFS、写描述符、绑 UDC（§2.5）；`gadget.rs` 只有  
  ep0 上发生的事 —— 一次读 12 字节的 `usb_functionfs_event`，按 type 分派 ENABLE / DISABLE / SETUP。没有  
  `LISTEN_FDS`，也没有 `readlink /proc/self/fd/N`：三个 fd 都是自己开的。
- **注册与绑定是两步**（`functionfs_bind` 要求 `FFS_ACTIVE`）：描述符 blob 与 strings blob 都写进 ep0 之后  
  实例才 ACTIVE，绑 UDC 才能成功。v2 头 `magic/length/flags/fs_count/hs_count`，flags 含  
  `FUNCTIONFS_ALL_CTRL_RECIP`（64），FS 与 HS 各三个描述符；字节在 `README.md`「The accessory interface」里  
  逐偏移给出。端点文件按描述符顺序命名：`ep1` = 我们→车机（描述符里第一个，主机按位置读的那个）、  
  `ep2` = 车机→我们。
- **51/52/53 全部应答**（路径 A 用不到也照答），每个控制请求先打一行 info，字符串内容一并打印。**别的请求也  
  应答**：带数据的 OUT 请求把数据读光丢掉，IN 请求对 ep0 做一次 read 触发 stall —— 不回话会让主机一直等。
- **53 触发重枚举**：等 100 ms 让应答的控制传输走完，再 `reidentify()` 换身份（§2.5）。
- 会话在两种形态下都能起来：ENABLE 事件即开一次 `session::run`，车机没 START 之前会话是静默的，多开一次不花代价。

**验收**：`dist/aoa-probe --send-proto-version` 能收到我们的回包；`dist/carlife-link --list` 能看到设备。

### Phase 2 — CarLife MD 协议栈 ✅

`src/carlife/{frame,proto,session}.rs`，**不含 USB，所以能跑在任何机器上**：

- `frame.rs`：8 字节外头（`[u32 channelId][u32 totalLen]`，全大端，`totalLen` 不含外头）+ 消息头（cmd/touch 8 B、  
  其余 12 B）、方向位（含两个不按位走的例外）、逐通道体上限、`MAX_MESSAGE = 640 KiB`。头与体分两次 write。
- `proto.rs`：手写 varint + 长度分隔字段的最小编解码，不做 codegen（D5）。**上行 6 个编码器**（版本匹配、  
  `MD_INFO`、`INIT_DONE`、鉴权应答、鉴权结果、帧率应答）**+ 下行 9 个解码器**（版本、鉴权请求/结果、  
  `STATISTIC_INFO`、编码器信息、帧率、触摸、硬按键、feature config 列表），空体消息在 `session` 里直接发。
- `auth.rs`：两侧鉴权（§3.5）。**17.1 用 `rsa` crate 的 `Pkcs1v15Sign::new_unprefixed`**（纯 Rust；`rsa`  
  本来就在依赖树里，直接依赖只多 80 KB），私钥在凭据目录里按**固定文件名**读（`KEY_FILE`，不是配置项），  
  **每次会话读一次**；没有密钥时只 warn 一行、什么都不发。**17.2 是纯形状判定**，不需要密钥。
- `session.rs`：握手状态机跑在 `AsyncRead + AsyncWrite` 上。**读线程独立 spawn**——600 KB 的帧不能在 `select!`  
  里被心跳 tick 打断丢掉已读字节；`select!` 等三件事：「收到一块」「CarPlay 侧交出一帧」「该发心跳了」，  
  且只有 tick 分支推进时钟，所以消息洪水不会饿死视频通道。帧的来源是 `Frames`：`Live(ClOutlet)` 时逐帧直写，  
  `Still(文件)` 时按协商帧率重复，两者都在未 START 前静默。
- 鉴权两条都接上了（§3.5）：`STATISTIC_INFO` 一到就回渠道号的判定，`AUTHEN_REQUEST` 到就用密钥应答。

**验收（无需车）**：Mac 上 `dist/carlife-link` + `dist/carlifed`，加
`CP2CL_INPUT_FILE=<单帧 annex-B>`，车机侧出画即通过。若要连手机一起验，  
不设 `CP2CL_INPUT_FILE` 即可。单帧可用：

```sh
ffmpeg -f lavfi -i testsrc=size=1280x720:rate=30 -frames:v 1 -c:v libx264 \
       -profile:v baseline -x264-params keyint=1 -bsf:v h264_mp4toannexb -f h264 frame.h264
```

### Phase 3 — 桥接

视频直通、音频、触摸/按键反向上行、麦克风、几何对齐与「按需求重启 AirPlay 会话」、无手机时的行为。

验收：**`carlifed` 的浏览器**里看到的就是 iPhone 的 CarPlay 界面；触摸能操作；媒体音频出声。

### Phase 4 — 上真车（MBUX）

按 §5.4 走；**鉴权（§3.5）两个方向都已通过（2026-10-16 真机）**。

---

## 7. 风险

| #  | 风险                                   | 影响               | 对策                                       |
| -- | ------------------------------------ | ---------------- | ---------------------------------------- |
| 1  | **17.1 鉴权已解**（2026-10-16 真机：车机接受这个块）                          | —                | 已实现（§3.5）：`rsa` 的 unprefixed PKCS#1 v1.5，密钥是凭据目录里的固定文件名；不做假证书 |
| 2  | **车机接受「不是真手机」的 USB 设备已验**（2026-10-16：车机枚举并起了会话）             | —                | 路径 A/B 都试；VID/PID 与字符串按真机形态来（§2.4）           |
| 3  | CarPlay 侧 H.264 不是 baseline（可能带 B 帧） | 车机解码失败           | 先直通；不行再用 H3 的 VPU 重编码（BSP 是否可用待确认）       |
| 4  | 单帧 > 600 KB                          | 车机侧解析失败或丢帧       | 分辨率控制在 720p，IDR 体积实测；必要时限制码率             |
| 5  | ffs + musb 在 4.14 上的吞吐与稳定性           | 花屏/断流            | Phase 1 就先压测：600 KB/帧 × 30 fps ≈ 18 MB/s |
| 6  | **程序自组设备**：`reidentify()` 假设 4.14 的 ffs + configfs 在解绑/重绑之间端点 fd 仍然有效       | 换身份后会话断，要重插      | 已对着 `gadget_dev_desc_UDC_store` 与 `ffs_ep0_read` 的实现核对（§2.5）；验收就是「Mac 侧完整做一次 51/52/53，然后操作重枚举后的设备」 |
| 7  | **已确认**：无线 CarPlay 不给媒体 LPCM，只给 AAC-LC | 音频必须本地解码        | 已实现：`symphonia`（§4.2）；**风险仍在 CPU**：H3 实时解 AAC-LC 的余量等上板量 |
| 8  | CarLife 会话建立前后，CarPlay 侧是否在监听                 | 手机连上一个没人应答的会话          | 已解：闸门式（§4.5），车机没到时 CarPlay 侧什么都不开                    |
| 9  | 车机 USB 口供电不足                         | 掉电重启             | 已知取舍：车机供电，不足时扩展板独立供电                     |
| 10 | 单 USB 口的角色冲突（管理链路 vs 车机）             | 场景互斥             | **已解**：USB 口整个交给 CarLife，管理链路走 AP（§2.5）                   |

---

## 8. 决策记录

| #   | 决策                                                                       | 理由                                                    |
| --- | ------------------------------------------------------------------------ | ----------------------------------------------------- |
| D1  | 目标仍是「CarPlay 进来，CarLife 出去」                                              | MBUX 只有 CarLife 全屏                                    |
| D2  | 我们扮演 **MD**，USB 侧是 device                                                | 车机是 host，AOA 的 accessory 命名与我们相反                      |
| D3  | AOA 用 **FunctionFS + `ALL_CTRL_RECIP`**，不写内核模块                           | 板子内核无 `f_accessory`/`raw-gadget`，ffs 在；Google 自己也走这条路 |
| D4  | 先试**路径 A**（直接 accessory 枚举），后补路径 B                                       | 少写一半代码，且真机先例支持                                        |
| D5  | protobuf **手写最小编解码**，不引入 codegen                                         | 只用到十几条消息、字段全是定长标量/字符串；`proto/` 里放参考 `.proto` 作文档与单测依据 |
| D6  | 视频**不重编码**，annex-B 直通                                                    | H3 没有重编码余量；两端都是 H.264 annex-B                         |
| D7  | 音频**必须本地解 AAC-LC**，不指望 PCM                                         | 无线 CarPlay 实测只给 AAC-LC（§4.2）；`symphonia`（只开 `aac`）是唯一的实时解码点 |
| D8  | 几何由 HU 的 `VIDEO_ENCODER_INIT` 决定，写进 `SharedScreen` 并在变化时发 `restart` | 车机才知道自己屏幕多大；环境变量给不出这个值（已完成）                        |
| D9  | 浏览器客户端**去掉**（`src/web/` 与 `CP2CL_SINK` 一并删除）                            | CarLife 是唯一的消费者；CarPlay 侧改用 CarLifePort 当假车机来验，不再维护一份前端       |
| D10 | USB 口整个交给 CarLife，管理链路改走 AP                                          | 设备要模拟手机、还要在会话中间换身份，接口里放不下网络；AP 与 USB 互不相干             |
| D11 | 不在协议栈里做「请求关键帧」                                                           | CarLife 没这条消息                                         |
| D12 | **消费者是总开关**：`cl` 时 CarLife 会话起 → CarPlay 侧才开；断 → 主动拆                       | 手机不该在车机没来时在后台把 CarPlay 会话建起来（§4.5）                     |
| D13 | 设备由**程序自组**（configfs 组装、ffs 挂载、描述符、绑 UDC 全在进程内）                             | 换身份必须解绑/重绑 UDC，只有拥有设备的一方做得到（§2.5）；代价是重启服务会把设备摘下来再放回去      |
| D14 | 视频通道来源由 `CP2CL_INPUT_FILE` 定：设了就是那个文件，没设就是 CarPlay 会话                  | 让通道与车机解码器能脱离手机单独验（§4.5），不必再多一个开关                       |
| D15 | 17.1 用 **`rsa` crate 的 unprefixed PKCS#1 v1.5** 应答，密钥是凭据目录里的一个固定文件名      | 手机端的 cipher 名是 Java 的 `RSA/ECB/PKCS1Padding`，其输出就是没有摘要前缀的那种块（§3.5）；RSA 不自己写；文件名写死不给配置面；`rsa` 本来就在依赖树里，直接依赖只多 80 KB |
| D16 | 媒体信息**只发有值可填的字段**，`playlistNum`/`songId`/`mode` 留空不写 0                    | 留空与 0 对车机解析器是同一种空值，但 `mode=0` 是「单曲循环」的实义；前两个没订、第三个 iAP2 没有（§4.6） |
| D17 | 导航信息**只发有值可填的字段**，`totalDistance`/`time` 留空；`turn_kind` 仅映射语义一致的类型，不近似 | `totalDistance` 与 `time` 没有对应源，填进去会误导； ramps / motorway changes / arrivals 在车机列表里没有对应箭头，近似反而降低可信度（§4.7） |
| D18 | 封面超过 30 KB 时**在 CarPlay 侧重编码**（`src/artwork.rs`）：长边 400 配 90/80/70/60，再 350、300 配 60，去元数据并按 EXIF 方向转正，输出 baseline + 4:2:0 | 车机是整份替换，丢封面等于面板没图；本来够小的按原字节直传，避免二次压缩；重编码跑在 `carry_artwork` 的独立任务里，不占写流的线程（§4.6） |

---

## 附录 A — 常量速查

### A.1 通道

| # | 名称    | serviceType 前缀 | USB channelId | 方向    |
| - | ----- | -------------- | ------------- | ----- |
| 1 | cmd   | `0x0001`       | 1             | 双向    |
| 2 | video | `0x0002`       | 2             | MD→HU |
| 3 | media | `0x0003`       | 3             | MD→HU |
| 4 | tts   | `0x0004`       | 4             | MD→HU |
| 5 | vr    | `0x0005`       | 5             | 双向    |
| 6 | touch | `0x0006`       | 6             | HU→MD |

### A.2 MD 侧消息 ID

`0x00010002` 版本匹配 · `0x00010004` MD_INFO · `0x00010008` VIDEO_ENCODER_INIT_DONE ·  
`0x0001000D` FRAME_RATE_CHANGE_DONE · `0x00010014..1C` 电话/屏幕状态 · `0x00010021` GO_TO_DESKTOP ·  
`0x00010022..24` 语音唤醒/结束/识别 · `0x00010026` MODULE_STATUS · `0x00010035` MEDIA_INFO ·  
`0x00010036` MEDIA_PROGRESS_BAR · `0x00010037` CONNECT_EXCEPTION · `0x00010044` 订阅回复 ·  
`0x00010049` MD_AUTHEN_RESPONSE · `0x0001004B` MD_AUTHEN_RESULT · `0x0001004C` GO_TO_FOREGROUND_RESPONSE ·  
`0x0001004D` START_BT_AUTOPAIR_REQUEST · `0x00010051` FEATURE_CONFIG_REQUEST · `0x00010054` BT_IDENTIFY_RESULT ·  
`0x00010059` MD_EXIT · `0x00020001` VIDEO_DATA · `0x00020002` VIDEO_HEARTBEAT · `0x00030001/3/4/6` MEDIA\_* ·  
`0x00040001/2/3` TTS\_* · `0x00050002/3/4` VR\_* · `0x00060009` UI_ACTION_SOUND

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
# ffs 是否接受 ALL_CTRL_RECIP（flags=64）：写描述符头，看是否 EINVAL    〔已验：66 B 的 v2 头被接受〕
# 解绑/重绑 UDC 能否把设备稳定地换个身份重新枚举（§2.5）                  〔未做：要真机，程序已经这么做了〕
```

**假车机上（Phase 1–3）**

```sh
cd ../CarLifePort && make all
dist/carlife-link --list                    # 能否看到 Duo2 的 gadget
dist/carlife-link --socket /tmp/carlife-link.sock --channel 20000000 --cuid CarLifePort -v
dist/carlifed                               # 浏览器出画即 Phase 2/3 通过
```

**真车上（Phase 4）**：51/52/53 与 `0x00018048` **已验**（2026-10-16 真机）—— 车机两个都发，且接受鉴权应答。  
剩下的靠设备侧日志读：`VIDEO_ENCODER_INIT` 请求的分辨率与帧率、六条通道里哪几条有数据。
