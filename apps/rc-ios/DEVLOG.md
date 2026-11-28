# iOS 控制端开发日志

记录 `apps/rc-ios` 和 `crates/rc-ios` 从无到有的过程：每一步做了什么、遇到什么问题、为什么这样解决。交接说明见同目录的 [HANDOFF.md](HANDOFF.md)。

## 2026-11-28

### 目标与范围

- 参照 Android 端（`apps/rc-android`），做一个 iPhone / iPad 用的控制端，用来控制运行 `remote-control.exe` 的 Windows 电脑。
- 测试设备只有 iPad，没有 Mac，也没有付费的 Apple 开发者账号。所以编译和打包都在 GitHub Actions 的 macOS 机器上做，安装靠用免费 Apple ID 侧载。
- 第一版只做核心功能：设备列表、全屏控制、触控板和直触两种触控模式、滚轮、键盘条。导播台、窗口卡片、虚拟显示器、画质档位、声音以后再做。

### 架构选择

Android 端的网络部分全部在 Rust 里（`crates/rc-android` 的 `engine.rs`、`session.rs`、`store.rs`），JNI 只是外面薄薄一层。这部分和平台无关，iOS 直接复用：

- 新增 `crates/rc-ios`，编成静态库 `librc_ios.a`，对外提供 C 接口。接口和 Android 的 `RcNative` 一一对应，对应的头文件是 `apps/rc-ios/RcCore/include/rc_core.h`。
- `crates/rc-android/src/lib.rs` 里只把 `engine`、`session` 两个模块改成 `pub`，其余没有改动。
- 握手时 iOS 端自报的平台仍是 `PLATFORM_ANDROID`。电脑端把它当成一部手机，所以现有的 exe 不用改。

另一个方案是在 Swift 里重写网络层。没有选它，因为要重新实现 iroh、QUIC 和 FEC，工作量大，还容易和 Android 的行为对不上。

### App 结构（SwiftUI + UIKit，XcodeGen）

| 目录 | 内容 |
|---|---|
| `Core/` | `RcBridge`：调用 C 接口，把回调转回主线程。视频帧直接送解码，不经主线程。`LanTargets`：读取本机 Wi‑Fi 网段 |
| `State/AppModel.swift` | 对应 Android 的 `RcViewModel`：会话状态、设备列表、偏好设置（存在 UserDefaults） |
| `Video/VideoPipeline.swift` | 把 Annex B 码流转成 AVCC 格式，交给 `AVSampleBufferDisplayLayer` 硬件解码。每帧都标记为立即显示，不做缓冲 |
| `Session/RemoteScreenView.swift` | 一个 UIView，处理全部输入：手指、Apple Pencil、iPad 触控板和鼠标、外接键盘 |
| `Session/SessionView.swift`、`KeyBar.swift` | 顶部胶囊菜单、连接中的提示卡片、键盘条、滚轮 |
| `Devices/DevicesView.swift` | 设备列表、添加连接码、设置 |
| `Logic/` | 不依赖 UIKit 的纯逻辑（坐标映射、触控板加速曲线、Annex B 解析、扫描码、JSON 模型、网段计算），单元测试只编译这部分 |

手势逻辑逐条照搬 `FullscreenScreen.kt`，包括阈值：长按 380 ms，双击间隔 260 ms，单击判定 8 pt 内，捏合阈值 18 pt。加速曲线也照搬，慢速 0.45×，快速最高 2.45×。为了让两端的手感一致，阈值没有按 iOS 的习惯重新调。

iPad 上 Android 没有的部分：外接键盘按键先由 HID usage 转成 set 1 扫描码再发给电脑；触控板和鼠标按绝对坐标移动电脑指针，支持左右键，双指滑动转成滚轮；Apple Pencil 按压感笔发送。

### 局域网发现

Android 端向子网广播地址和 `255.255.255.255` 发 UDP 查询包（端口 47821）。iOS 没有 `com.apple.developer.networking.multicast` 权限就会丢掉广播包，而这个权限要向苹果申请，只发给付费开发者账号。

解决办法：App 把同一网段内的每个主机地址都通过 `rc_set_broadcasts` 交给引擎，引擎逐个单播发查询包。单播只需要用户在系统弹窗里允许「本地网络」。网段大于 /23 时只扫本机所在的 /24。引擎代码不用改，原来的 `set_broadcast_addrs` 本来就接受任意 IPv4 地址列表。

### CI：`.github/workflows/ios.yml`

在 macos-15 上依次运行 `build-core.sh`（为真机和模拟器两个目标编 Rust，打包成 xcframework）、`xcodegen generate`、模拟器上的单元测试，最后在不签名的情况下构建并打包成 `remote-control.ipa`，作为 artifact 上传。

### 排错记录

一共 7 次推送，前 6 次失败，第 7 次通过。

| 提交 | CI 结果 | 问题 | 处理 |
|---|---|---|---|
| 469005e | 失败 | `build-core.sh: Permission denied` | 8b9f346：在 git 里给脚本加执行位，workflow 改成用 `bash` 调用 |
| 8b9f346 | 失败 | `netdev 0.46.3`：`cannot find type in6_ifreq in crate libc` | 17ed902，见下 |
| fbc7c7b | 取消 | 修了换行符和几处 SwiftUI API，没等 CI 跑完就接着推了下一个修复 | — |
| 17ed902 | 失败 | `netwatch 0.19.3`：libc 中缺少 `RTM_VERSION`、`RTAX_DST` 等 22 个常量 | 6599ae6，见下 |
| 6599ae6 | 失败 | 链接阶段找不到 `_nw_path_monitor_*` 等符号 | 782eb49：`project.yml` 里加上 `Network.framework` |
| 782eb49 | 失败 | `LogicTests.swift:13` 类型检查超时 | d607dbf：把一长串数组 `+` 拆开 |
| d607dbf | **通过** | — | run 46158194847，产出 4.8 MB 的 IPA |

**netdev（17ed902）**：iroh 依赖的 `netwatch` 用 `netdev` 读取网卡信息。`netdev` 在所有苹果平台上都会用 `libc::in6_ifreq`，但 libc 0.2.191 只在 `target_os = "macos"` 下定义这个类型。做法和仓库里已有的 `vendor/iroh` 一样：把 netdev 0.46.3 原样放进 `vendor/netdev`，iOS 下用本地定义的同布局结构体，并用编译期断言保证大小是 288 字节，最后用 `[patch.crates-io]` 指过去。详见 `vendor/netdev/PATCHES.md`。`remote-control` 和 `rc-relay` 用的是 netdev 0.45，不受影响。

**libc（6599ae6）**：libc 0.2.190 起，把 `net/route.h` 里的路由常量（`RTM_*`、`RTAX_*`、`RTF_*`）挪进了 `cfg(target_os = "macos")`，netwatch 在 iOS 上就编不过了。我下载 0.2.187 到 0.2.191 的源码逐个核对，0.2.189 是最后一个对 iOS 也提供这些常量的版本。于是在 `crates/rc-ios/Cargo.toml` 里写 `libc = "=0.2.189"`，只对 iOS 目标生效。Cargo 对同一个依赖只选一个版本，所以整个工作区的 libc 都从 0.2.191 退到了 0.2.189。

### 其他坑

- 这台 Windows 机器上，用编辑工具写出的文件是 CRLF 换行，而仓库关闭了 `core.autocrlf`，第一次提交就把 CRLF 带进了仓库。fbc7c7b 把新文件全部改回了 LF。以后在这台机器上提交前要检查换行符。
- `Character(ch.lowercased())` 在小写结果多于一个字符时会直接崩溃，改成了更稳妥的 `ch.lowercased().first ?? ch`。
- `ToolbarItemGroup(placement: .topBarTrailing)` 要 iOS 17 才有，部署目标是 iOS 16，所以改用 `.navigationBarTrailing`。
- 计时器统一改用 target/selector 写法，预防 Swift 并发检查对闭包报 actor 隔离错误（这条是预防性的，CI 没报过）。

### 截至当天的状态

- 分支 `feat/ios` 只推到了 fork Leonhart999/remote-control，原仓库没有推送，也没有提 PR。
- CI 通过，IPA 本地副本在 `D:\AI\scratch\rc-ios-ipa\remote-control.ipa`。
- **还没有在真机上运行过。** 视频解码、局域网单播发现、手势手感都要在 iPad 上验证。
