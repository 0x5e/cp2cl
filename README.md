# cp2cl

CarPlay转CarLife桥接服务，运行于Linux嵌入式设备。

## 背景

本项目主要用于**奔驰MBUX NTG6.0车机**，此车机CarPlay、Android Auto被限制到只能显示半个屏幕（1300x720），而CarLife支持全屏（1920x720）。
其他车机未经测试。

## 特性

- 纯视频流转发，无编解码
- 支持有线/无线CarPlay
- 后台管理页（状态、日志、设置）
- 开机启动时间优化（30s降至约7s）

## 支持情况
### 软件完成度
- [x] 连接方式
  - [x] 有线
  - [x] 无线
- [x] 视频流
- [x] 音频流
  - [x] 媒体
  - [x] 导航
  - [ ] 通话（双向）
  - [ ] Siri（双向）
- [x] 控制流
  - [x] 触摸
  - [x] 按键
- [x] 仪表盘
  - [x] 媒体信息（含封面）
  - [x] 方向键切歌
- [x] 车载数据
  - [x] 车速
- [x] Web后台

### 硬件适配列表（BuildRoot）
- [x] 随身WiFi-UFI001C
- [x] NanoPi-Duo2
- [x] OrangePi-Zero2W

## 运行环境

### 硬件要求

- 1个USB OTG口
- 1个USB Host口（可选，用于有线CarPlay）
- WiFi + BLE 网卡（可选，用于无线CarPlay）
- 存储空间大于128MB

## 构建

在项目根目录运行：

```shell
# 可从 com.baidu.carlife_8.9.0_base.apk/classes2.dex 中获取。
# com/baidu/carlife/authorization/PrivateKeyManager.java:37
export CARLIFE_PRIVATE_KEY="MIICdwIBAD..........gHdvNpcqA="
make all
```
应用和固件产物位于`dist/`目录.

## 配置

配置文件目录位于`/etc/cp2cl/`，配置文件`config.json`及MFi证书、私钥均置于此目录

| 字段名 | 默认值 | 备注 |
|---|---|---|
| `log_level` | `debug` | 日志级别：`error`, `warn`, `info`, `debug` |
| `wifi_iface` | `wlan0` | Wi-Fi接口名称 |
| `bt_adapter` | `hci0` | 蓝牙适配器名称 |
| `addr` | `0.0.0.0:80` | Web后台监听地址与端口 |
| `screen` | — | 屏幕自定义参数 |

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

## 硬件测试数据

以1920x720@30分辨率，停留在正在播放页面5分钟，AirPlay RTT 最小值/平均值/最大值数据如下：
- msm8916-ufi001c 4/60/1059 ms
- nanopi-duo2 3/14/183 ms
- orangepi-zero2w 3/3/12 ms
结论：2.4GHz网卡延迟跳动过大，音乐界面勉强够用，导航页来回拖动延迟会越来越高。建议选择5GHz网卡的设备。
设备选择建议参考：[DashPlay - 硬件与实测](https://github.com/yizhixiaoheigou/DashPlay/blob/main/README.md#硬件与实测)

## 调试工具

CarLife车机 - macOS版：[CarLifePort](https://github.com/0x5e/CarLifePort)

## 已知问题
- 初次无线CarPlay配对可能会超时（约15秒），等超时后第二次才能成功配上，暂未找到原因。
- 方向盘右侧方向键无法完整支持。无论按上下还是左右，MBUX NTG6.0车机都只会发送`KEYCODE_SELECTOR_NEXT`/`KEYCODE_SELECTOR_PREVIOUS`，无法辨别，只能按照滚轮“前/后”进行映射。

## 来源与许可
参考项目：
- [LIVI](https://github.com/f-io/LIVI)
- [MacPlay](https://github.com/Roylyl/MacPlay)
- [PlayPort](https://github.com/youcci/playport)

项目沿用GPL-3.0-or-later。
