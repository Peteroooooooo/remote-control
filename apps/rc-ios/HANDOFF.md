# iOS 控制端交接说明

写给接手 `apps/rc-ios` 的开发者。开发过程和每个决定的原因见 [DEVLOG.md](DEVLOG.md)。

## 一句话现状

iOS / iPadOS 控制端第一版的代码已经写完，CI 能编译、能跑单元测试，也能产出未签名的 IPA，**但还没在真机上跑过**。代码在 fork 的 `feat/ios` 分支，还没合进原仓库。

| 项目 | 位置 |
|---|---|
| 分支 | `feat/ios`（基于 upstream/main `94c68bd`），只推到了 `Leonhart999/remote-control` |
| 本地工作目录 | `D:\AI\github\remote-control-ios`（`D:\AI\github\remote-control` 的 git worktree） |
| 最近一次通过的 CI | run 46158194847，提交 `d607dbf` |
| IPA | 每次 CI 都会上传名为 `remote-control-ios-unsigned` 的 artifact；本地副本在 `D:\AI\scratch\rc-ios-ipa\remote-control.ipa` |

> `D:\AI\github\remote-control` 主目录里有和 iOS 无关的 Android 二维码改动，还没提交，不要动。

## 代码地图

```
crates/rc-ios/src/lib.rs          C 接口，包着 rc-android 的 Engine，一个进程一个引擎
apps/rc-ios/RcCore/include/       rc_core.h + module.modulemap（Swift 里 import RcCore）
apps/rc-ios/project.yml           XcodeGen 工程描述（工程文件不进 git）
apps/rc-ios/scripts/build-core.sh Rust 编出 build/RcCore.xcframework（真机和模拟器两个目标）
apps/rc-ios/RemoteControl/
  App/        入口；前后台切换时释放按键、恢复视频
  Core/       RcBridge（C 回调先转主线程，视频帧不经主线程）、LanTargets
  State/      AppModel：会话状态和偏好设置
  Video/      VideoPipeline：Annex B → AVSampleBufferDisplayLayer
  Session/    RemoteScreenView（全部输入处理）、SessionView、KeyBar（含滚轮）
  Devices/    设备列表、添加连接码、设置
  Logic/      纯逻辑，单元测试也编译这一层
apps/rc-ios/RemoteControlTests/   单元测试，不需要宿主 App，也不需要 Rust 库
vendor/netdev/                    补丁版 netdev（见 PATCHES.md）
.github/workflows/ios.yml         iOS 的 CI
```

**改接口时要同步改三处**：`crates/rc-ios/src/lib.rs`、`rc_core.h`、`RcBridge.swift`。Rust 和 C 之间的结构体是按字段顺序对上的（`#[repr(C)]`），编译器不会检查两边是否一致，三处的字段顺序必须完全相同。

## 怎么构建

**有 Mac 的话：**

```sh
apps/rc-ios/scripts/build-core.sh              # 正式构建要几分钟；PROFILE=dev 会快一些
cd apps/rc-ios && xcodegen generate
open RemoteControl.xcodeproj                   # 在 Signing 里选你的 Team，然后运行
```

**没有 Mac 的话**：推到 GitHub 后从 Actions 下载 IPA，用 Sideloadly 或 AltStore 拿自己的 Apple ID 签名后装到设备上。免费账号签出的包 7 天后失效。设备上还要打开开发者模式，并信任对应的开发者证书。

## 下一步该做什么

按优先级排：

1. **真机验证**（最重要，目前完全没测过）：
   - 同一 Wi‑Fi 下，电脑能不能自动出现在列表里。首次启动必须允许「本地网络」权限。
   - 用 `rc1…` 连接码能不能连上。电脑端会弹窗要求同意。
   - 画面能不能出来：H.264 和 HEVC 都要试，切换显示器后看画面比例对不对。
   - 触控板和直触两种模式的手势，iPad 外接键盘、触控板、Apple Pencil。
   - 切到后台再回来，画面能不能恢复。目前的做法是清空解码器缓冲，再向电脑要一个关键帧。
2. 根据真机测试结果修 bug。
3. 整理成 PR 提给原仓库，PR 描述里要说明：
   - libc 被锁到了 `=0.2.189`，整个工作区都受影响，包括 Windows 端和 Android 端（原来是 0.2.191）。
   - 新增了 `vendor/netdev`。
   - `crates/rc-android` 里有两个模块改成了 `pub`。
   - 是否要把 iOS 加进 `release.yml` 和 `publish-release.mjs`，生成的 `latest.json` 要不要带上 iOS 版本。这需要和仓库作者商量。
4. 补功能，可以照着 Android 的实现逐个移植：导播台和窗口卡片（`HubScreen.kt`，回调 `on_windows`、`on_app_icon`、`on_window_thumb` 现在是空实现）、虚拟显示器（`on_virtual_display`）、画质档位（目前固定为「自动」）、完整的电脑键盘面板、声音。

## 需要知道的坑

- **libc 版本锁**：libc 0.2.190 起只在 macOS 上提供 `RTM_*` 等路由常量，netwatch 在 iOS 上编不过。等 netwatch 或 libc 修好后，删掉 `crates/rc-ios/Cargo.toml` 里的锁定，执行 `cargo update -p libc`。
- **netdev 补丁**：升级 iroh 或 netwatch 时，要按 `vendor/netdev/PATCHES.md` 的说明重新打补丁。
- **局域网发现**：没有 multicast 权限，iOS 会丢掉广播包，所以现在是逐个单播扫网段，最多扫 /23。如果以后用付费账号申请到了这个权限，可以只发广播地址。
- **Network.framework** 必须链接（netwatch 用它监控网络路径）。不加的话链接时会报 `_nw_*` 符号缺失。
- **Windows 上的换行符**：这台机器上写出的文件可能是 CRLF，而仓库要求 LF。提交前用 `git show :文件 | grep -c $'\r'` 检查一下。
- **Swift 类型检查超时**：一长串数组 `+` 拼接会让编译器超时，要拆成多行。
- iOS 不允许 App 内自更新，所以 Android 的 `AppUpdater` 没有移植。
- 提交和 PR 都只署用户本人，不要加 Claude 共同作者行。
