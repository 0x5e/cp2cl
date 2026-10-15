# CarLife 车机对接 — 开发计划与技术方案

> Linux 设备（NanoPi Duo2）通过 micro-USB **扮演 CarLife 手机端（MD）**&#x63A5;入车机（HU = 奔驰 MBUX NG6）；  
> iPhone 通过 **无线 CarPlay** 连同一台设备。视频重封装不重编码；**音频必须本地解 AAC-LC**（§4.2，无线实测只给 AAC）。  
> 主要工作量在协议：CarLife 的手机端协议栈**不存在开源实现**，必须自己写——本文档的一半篇幅就是在把这份协议钉死。

状态：2026-10-13 起草。**Phase 1 与 Phase 2 已实现**（`src/carlife/`，见 §6）；本文档同时是落点、验收方式与 Phase 3 的设计。  
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
| 会话重启                                 | `src/web/mod.rs:430` `Hub::restart_session()`（`:816` 由显示面板调用）    | CarLife 侧发起几何变更时走同一条路                        |
| `/info` 的音频宣示                        | `src/airplay/info.rs:178` `audio_formats()`（条目在 `:187-197`）      | 音频走 AAC-LC，`102/media` 那条留着（§4.2）             |
| 视频帧（**已是 annex-B**）                  | `src/airplay/screen.rs` → `StreamEvent::VideoFrame`              | 直接塞进 CarLife 视频通道（§4.1）                      |
| 音频帧与格式                               | `src/sink.rs` `AudioCodec`（含 `Pcm{rate,channels}`）               | AAC-LC 需在本地解码后转成 PCM 再进通道（§4.2）               |
| 输入上行（HID + 会话命令）                     | `src/airplay/event.rs`、`src/commands.rs`                         | CarLife 触摸/按键接到这套词汇上                         |
| 麦克风上行                                | `src/airplay/mic.rs`                                             | CarLife VR 通道的 PCM 接过来                       |
| 浏览器客户端                               | `src/web/`                                                       | 与车机互斥，由 `CP2CL_SINK` 选（§4.5）：`web` 时保留，`cl` 时不建      |

这次要写的东西，以及它们现在的落点：

| 待补能力              | 落点                              | 状态                              |
| ----------------- | ------------------------------- | ------------------------------- |
| USB 设备（gadget / ffs / 重枚举） | `src/carlife/usb.rs`            | 已实现：configfs 组装、挂 ffs、写描述符、绑 UDC、换身份重枚举（只在 Linux 编） |
| 设备形态与 AOA 请求字节      | `src/carlife/desc.rs`           | 已实现：描述符与 strings blob、51/52/53 的识别、身份字符串；9 条单测，纯字节、跨平台 |
| AOA 控制请求的应答          | `src/carlife/gadget.rs`         | 已实现：ep0 事件循环、51/52/53 应答、触发重枚举、把两个 bulk 端点交给会话（只在 Linux 编） |
| CarLife MD 协议栈    | `src/carlife/{frame,proto,session}.rs` | 已实现：帧编解码、protobuf、握手状态机、心跳      |
| 桥接：视频             | `src/sink.rs` `ClOutlet`         | 已实现：`VideoFrame` 直通视频通道，参数集在下一帧前带出；`CP2CL_INPUT=file` 则改为重复一个文件 |
| 桥接：音频             | 待定                              | 未实现：Media 通道要 PCM，拿到的却是 AAC-LC，落在 `ClSink` 里被丢弃（§4.2） |

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

### 3.5 鉴权：这一侧的真正风险

两条流程，方向对我们相反：

- **17.1（HU 认证 MD）**：HU 发 `randomValue = "A_B"`（A 是随机串，B 是车机要求的最低手机版本），  
  MD 用**自己的私钥**做 `RSA/ECB/PKCS1Padding` 加密回 `encryptValue`，HU 用百度发给车厂的公钥解密比对；  
  不一致 ⇒ **HU 主动断开**。私钥在**手机端**（百度持有并只给手机端），开源车机库里没有，合法取值也拿不到。
- **17.2（MD 认证 HU）**：MD 用 `STATISTIC_INFO` 里的渠道号判断车机合法性，非法则主动断开。  
  **我们是 MD，这一侧的判定权在我们手里**，可以对渠道号放行。

⇒ 风险集中在 17.1。观察到的先例：CarLifePort 反向做 HU 时，真机弹出「未认证的车机端」但**音频照旧在推**  
（`../CarLifePort/.workbuddy/memory/2026-10-12.md:381-385`），说明手机的处置是「提示」而非「立即断开」。我们的对策按代价从低到高：

1. **什么都不做**（程序现在的做法）：不回 `0x00010049`，看 MBUX 是否继续。
2. **回一个失败值**：按指南「版本低于 B 就把该字段全置 0」，看车机是断开还是只提示。**代码里没有这条**——  
   要试就得把它写回来，`proto` 侧原本有那个编码器，随鉴权开关一起删了。
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

| 流        | 通道 | 格式                                    | 方向    |
| -------- | -- | ------------------------------------- | ----- |
| Media    | 3  | PCM 44.1 kHz / 双声道 / 16 bit 小端        | MD→HU |
| Navi TTS | 4  | PCM 16 kHz / 单声道 / 16 bit 小端          | MD→HU |
| VR TTS   | 5  | PCM 16 kHz / 单声道 / 16 bit             | MD→HU |
| VR MIC   | 5  | PCM 16 kHz / 单声道 / 16 bit，**每包 1 KB** | HU→MD |

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

1. **编码家族一致**：`/info` 只宣示 H.264（CarLife 这边没有可协商的编解码字段），两端都 H.264。
2. **profile 兼容**：指南只保证 baseline L3.1。CarPlay 侧实际多为 Main/High，可能带 B 帧。**先直通试，  
   车机不接受再想办法**（§8 风险 3）。
3. **单帧 ≤ 600 KB**：1280×720 / 30 fps 的 IDR 一般在 100–300 KB，安全；1080p 大关键帧可能越界。  
   由 `INIT_DONE` 把分辨率压在我们能保证的档位。

几何对齐靠已有的显示参数通道：CarLife 的 `VIDEO_ENCODER_INIT` 到达时，把 `width/height/fps` 写进  
`SharedScreen`（`server.rs:263`）并调 `Hub::restart_session()`（`web/mod.rs:430`）——和显示面板按 Apply 做的是  
同一件事，代价也一样：**几何只在「新会话」生效，所以要拆掉当前 AirPlay 会话重建**。未连接的手机不受影响。

### 4.2 音频

**实测结论（2026-10-13，无线 CarPlay 真机）：手机不给 PCM，媒体流只给 AAC-LC。** `/info` 里开出的 `100/media`  
PCM 与 `102/media` AAC-LC 两条路，手机选的是后者；`info.rs:173-177` 的注释原本假设「只有有线会出现 PCM」，而这一侧  
就是无线，所以 PCM 从来不会来。⇒ **音频必须在本地解 AAC-LC**，不能直通。

落点：

- `/info` 的 `audio_formats()`（`info.rs:178-197`）保持现状——`100/media` 的 PCM 位留着无用但无害，`102/media` 的  
  `aac_lc` 是手机真正会用的那条。**不要试图删掉 AAC 条目来把手机逼回 PCM**：实测无线下手机不产生 PCM，  
  删掉这一条只会让媒体流无从选择，不会换来 PCM。
- 解码用 `symphonia`（`aac` feature）：输入 = `sink.rs` 的 `AudioCodec::AacLc` 帧（带 `StreamEvent::AudioConfig` 给的  
  ASC），输出 = CarLife Media 通道要的 **PCM 44.1 kHz / 双声道 / 16 bit 小端**。重采样只在采样率不是 44.1 k 时做。
- **待验**：Cortex-A7 单核实时解 AAC-LC 的余量。H3 四个核，视频侧只做搬运，理论上够；但这是整个方案里**唯一**
  引入实时编解码的地方，所以 Phase 3 一开始就要用 `top`/`pidstat` 量出来，不要等到上车才发现。
  ALAC 也可能出现（无损源），`symphonia` 同样能解，代价一样。

TTS/VR 通道是 16 kHz 单声道——CarPlay 侧对应的是 `speechRecognition`/`telephony` 那两个低带宽流，手机给的是  
**OPUS**（同一次实测），也需要解码（`symphonia` 的 `opus` feature）再做一次重采样。若嫌重，可先只把 Media 通道  
打通，TTS/VR 留空（车机不会因为这两个通道没数据而拒会话）。

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

**消费者选谁，谁就是总开关。** `CP2CL_SINK=cl` 时车机是消费者，于是：CarLife 会话没起来时，CarPlay 侧不接受连接、  
不扫描也不认证手机、不发布 iAP 服务；车机断开时，正在跑的 AirPlay 会话被主动拆掉。手机何时连、连不连都不影响  
这条规则。`CP2CL_SINK=web`（默认）时消费者在本机上，没有可等的一方，闸门恒开。

同一个开关也决定消费的是谁，所以两者互斥：`cl` 不建浏览器客户端，也就没有页面、没有显示面板。**代价是几何只能  
靠 `CP2CL_WEB_WIDTH`/`CP2CL_WEB_HEIGHT`/`CP2CL_WEB_FPS`**——`SharedScreen` 没有人写。§4.4 的「几何由 HU 的 `VIDEO_ENCODER_INIT`  
决定」因此是 Phase 3 要补的路，不是现在这条路。

（那个 TCP 端口仍然在启动时就 bind——它是 `/info` 要告诉手机的端口，必须在车机出现之前就定下来；受闸门控制的是  
**是否 accept**，所以闸门关着时没有 CarPlay 会话能建立起来。）

机制是一个 `tokio::sync::watch::channel(cfg.sink == Sink::Web)` 当闸门，加一个 `broadcast` 当断开信号（`src/main.rs`）：

| 位置                       | 对闸门的处置                                                              |
| ------------------------ | ------------------------------------------------------------------ |
| `airplay::serve`         | 关门时不 `accept`，只在 `gate.changed()` 上等；开门时才进 `select!` 的 accept 分支 |
| `session::watch`         | 开门前不枚举 USB、不 attach                                                |
| `wireless::watch`        | `ready()` 之后、`bt::start()` 之前等闸门——iAP 服务是「这里有 CarPlay」的宣告，不发在没有的时候   |
| `carlife::session::run`  | 收到 `HU_INFO` 时把闸门打开（只开一次）；会话结束时关上                                     |

开门时同时发一次 `restart`：`CarPlayStartSession` 是 accessory 对手机 `CarPlayAvailability` 的回答，手机不会自己  
再问一遍，所以只能丢掉当前 iAP2 会话重开一轮。断连走另一条 `shutdown`，与 `restart` 分开——`restart` 的意思是  
「重来」，`shutdown` 的意思是「没什么可重来的」。

其余时序：

- 车机重插：USB 读失败 ⇒ 重新组一次设备（**解绑 → 重新绑 UDC**，§2.5）并重建协议栈；CarLifePort 的教训是  
  **会话必须在新的 accessory 附着上重建**，不能原地重连（`README.md:233-242`）。
- 车机在、iPhone 没来：CarLife 握手照走完，视频通道发 `VIDEO_HEARTBEAT` 保活（指南说 MD 切后台时就是这么做的），  
  AirPlay 侧只是没有人连进来。
- 拔掉 iPhone：CarLife 会话不受影响，退回发心跳。
- 消费者在本机（`CP2CL_SINK=web`，默认）：闸门恒开，`serve` 每次 `select!` 都走 accept 分支，行为与加闸门之前一致。

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
`encryptValue` 的长度与形态、视频包的时间戳步进、`MEDIA_INIT` 的 `channelConfig/sampleFormat` 取值。  
这份录音应当作为 Phase 0 的产出归档（`docs/carlife-md-capture.md` 之类）。

### 5.3 离线验证

已落地的单测（`cargo test`，全部不需要 USB）：

- **帧编解码**（`frame.rs`，8 条）：头长与长度字段、带/不带时间戳的往返、`reserved` 非零不影响、方向位含例外、  
  逐字节分片仍能拼整、一次 write 两个块分别读出、截断报错、未知通道拒绝、空管道当结束。
- **protobuf**（`proto.rs`，14 条）：逐条消息的字节级断言（含 `1280` 在线上是 `08 80 0A` 这类 varint 细节）、  
  未设字段不写 0、负 int32 十字节符号扩展、截断体之前仍可读。
- **状态机**（`session.rs`，6 条）：自带一个 `tokio::io::duplex` 假车机，喂完整握手序列并断言发出的消息与顺序；  
  另含两条**否定式**断言——未 `START` 前视频通道静默、默认策略下鉴权请求不回。
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
- `proto.rs`：手写 varint + 长度分隔字段的最小编解码，不做 codegen（D5）。**上行 5 个编码器**（版本匹配、  
  `MD_INFO`、`INIT_DONE`、鉴权结果、帧率应答）**+ 下行 9 个解码器**（版本、鉴权请求/结果、  
  `STATISTIC_INFO`、编码器信息、帧率、触摸、硬按键、feature config 列表），空体消息在 `session` 里直接发。  
  鉴权应答的编码器随鉴权开关一起删掉了（§3.5）。
- `session.rs`：握手状态机跑在 `AsyncRead + AsyncWrite` 上。**读线程独立 spawn**——600 KB 的帧不能在 `select!`  
  里被心跳 tick 打断丢掉已读字节；`select!` 等三件事：「收到一块」「CarPlay 侧交出一帧」「该发心跳了」，  
  且只有 tick 分支推进时钟，所以消息洪水不会饿死视频通道。帧的来源是 `Frames`：`Live(ClOutlet)` 时逐帧直写，  
  `Still(文件)` 时按协商帧率重复，两者都在未 START 前静默。
- 鉴权**什么都不回**，程序里没有第二条路（§3.5）。

**验收（无需车）**：Mac 上 `dist/carlife-link` + `dist/carlifed`，`CP2CL_SINK=cl` 加  
`CP2CL_INPUT=file CP2CL_INPUT_FILE=<单帧 annex-B>`，车机侧出画即通过。若要连手机一起验，  
把 `CP2CL_INPUT` 留默认（`cp`）即可。单帧可用：

```sh
ffmpeg -f lavfi -i testsrc=size=1280x720:rate=30 -frames:v 1 -c:v libx264 \
       -profile:v baseline -x264-params keyint=1 -bsf:v h264_mp4toannexb -f h264 frame.h264
```

### Phase 3 — 桥接

视频直通、音频、触摸/按键反向上行、麦克风、几何对齐与「按需求重启 AirPlay 会话」、无手机时的行为。

验收：**`carlifed` 的浏览器**里看到的就是 iPhone 的 CarPlay 界面；触摸能操作；媒体音频出声。  
（`cp2cl` 自己的浏览器客户端仍可开在旁边，作为 CarPlay 侧的独立对照。）

### Phase 4 — 上真车（MBUX）

按 §5.4 走；认证（§3.5）在这一阶段才有真实答案。

---

## 7. 风险

| #  | 风险                                   | 影响               | 对策                                       |
| -- | ------------------------------------ | ---------------- | ---------------------------------------- |
| 1  | **17.1 鉴权**：拿不到 RSA 私钥               | 车机可能直接断开         | 先不回/回失败观察（§3.5）；其次逆向 APK；不做假证书           |
| 2  | 车机是否接受「不是真手机」的 USB 设备                | 完全接不上            | 路径 A/B 都试；VID/PID 与字符串按真机形态来             |
| 3  | CarPlay 侧 H.264 不是 baseline（可能带 B 帧） | 车机解码失败           | 先直通；不行再用 H3 的 VPU 重编码（BSP 是否可用待确认）       |
| 4  | 单帧 > 600 KB                          | 车机侧解析失败或丢帧       | 分辨率控制在 720p，IDR 体积实测；必要时限制码率             |
| 5  | ffs + musb 在 4.14 上的吞吐与稳定性           | 花屏/断流            | Phase 1 就先压测：600 KB/帧 × 30 fps ≈ 18 MB/s |
| 6  | **程序自组设备**：`reidentify()` 假设 4.14 的 ffs + configfs 在解绑/重绑之间端点 fd 仍然有效       | 换身份后会话断，要重插      | 已对着 `gadget_dev_desc_UDC_store` 与 `ffs_ep0_read` 的实现核对（§2.5）；验收就是「Mac 侧完整做一次 51/52/53，然后操作重枚举后的设备」 |
| 7  | **已确认**：无线 CarPlay 不给媒体 LPCM，只给 AAC-LC | 音频必须本地解码        | `symphonia`（§4.2）；**真正的风险转移到 CPU**：H3 实时解 AAC-LC 的余量在 Phase 3 量 |
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
| D7  | 音频**必须本地解 AAC-LC**，不指望 PCM                                         | 无线 CarPlay 实测只给 AAC-LC（§4.2）；`symphonia` 是唯一的实时解码点 |
| D8  | 几何由 HU 的 `VIDEO_ENCODER_INIT` 决定，用 `SharedScreen` + `restart_session` 对齐 | 复用显示面板那条已验证的路                                         |
| D9  | 浏览器客户端**保留**，但只在 `CP2CL_SINK=web` 时建                                     | 唯一能单独验证 CarPlay 侧的手段；`cl` 下没有页面可选                      |
| D10 | USB 口整个交给 CarLife，管理链路改走 AP                                          | 设备要模拟手机、还要在会话中间换身份，接口里放不下网络；AP 与 USB 互不相干             |
| D11 | 不在协议栈里做「请求关键帧」                                                           | CarLife 没这条消息                                         |
| D12 | **消费者是总开关**：`cl` 时 CarLife 会话起 → CarPlay 侧才开；断 → 主动拆                       | 手机不该在车机没来时在后台把 CarPlay 会话建起来（§4.5）                     |
| D13 | 设备由**程序自组**（configfs 组装、ffs 挂载、描述符、绑 UDC 全在进程内）                             | 换身份必须解绑/重绑 UDC，只有拥有设备的一方做得到（§2.5）；代价是重启服务会把设备摘下来再放回去      |
| D14 | 消费者由 `CP2CL_SINK` 二选一（`web`/`cl`），**不同时在线**；视频通道来源由 `CP2CL_INPUT` 选（`cp`/`file`） | 两端是同一份媒体，只有去向不同；`file` 让通道与车机解码器能脱离手机单独验（§4.5）      |

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

**真车上（Phase 4）**：车机是否发 51/52/53、是否发 `0x00018048`、`VIDEO_ENCODER_INIT` 请求的分辨率与帧率、  
六条通道里哪几条有数据 —— 全部靠设备侧日志，日志从 Phase 1 起就按「每个控制请求一行、每通道首帧一行」打。
