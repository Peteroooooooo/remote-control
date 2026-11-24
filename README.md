<div align="center">
  <img src="apps/remote-control/assets/icon.svg" width="76" alt="Remote Control" />
  <h1>REMOTE CONTROL</h1>
  <p><strong>简体中文</strong> · <a href="README.en.md">English</a></p>
  <p><strong>单 exe、P2P 直连优先的 Windows 远程桌面、终端与 AI Agent 跨设备控制工具</strong></p>
  <p>
    基于 Rust 与 iroh，免账号、默认打洞直连；<br />
    支持 <strong>Claude Code / Codex</strong> 跨设备跑命令与传文件，受限网络可 Docker 自建中转。
  </p>
  <p>
    <a href="https://peteroooooooo.github.io/remote-control/?lang=zh"><strong>在线试一试 →</strong></a>（网页交互预览）
  </p>
  <img src="docs/images/lobby-devices.png" width="760" alt="Remote Control" />
</div>

---

## 核心功能

- **AI Agent 跨设备控制**：一键安装 Skill（`~/.claude/skills`、`~/.agents/skills`），供 Claude Code / Codex 调用 `devices`、`exec`、`cp` 操作其它 Windows 电脑，直接复用大厅连接与权限。
- **远程桌面**：GPU 硬件编解码与 DXGI 渲染（最高 144 fps、4:4:4 文字清晰模式、FEC 抗丢包）；支持多显示器切换、剪贴板同步（文字/图片/文件/文件夹）、`Ctrl+Alt+Del` 及锁屏控制。
- **远程声音**：支持 Opus 与 24-bit PCM 无损两档，可独立调节音量或一键静音被控端扬声器。
- **远程终端与文件传输**：无需开启桌面画面，直接通过 CLI 进入远程 PowerShell、执行命令或互传文件。
- **私有中转（可选）**：单 Docker 镜像，内置自签证书，无需域名，仅转发自有设备的端到端加密流量。

---

## 与同类工具对比

|                    | Remote Control                                          | RustDesk                                             | ToDesk / 向日葵                        |
| ------------------ | ------------------------------------------------------- | ---------------------------------------------------- | -------------------------------------- |
| **AI Agent / CLI** | 原生内置 `devices` / `shell` / `exec` / `cp` 与 Skill   | 无内置远程终端 / AI Skill                            | 无内置远程终端 / AI Skill              |
| **账号**           | 不需要，本地密钥 + 连接码                               | 不需要，ID + 密码                                    | 主控 / 无人值守需注册登录              |
| **形态**           | 单 `.exe` 免装即用（可选装后台服务）                    | 安装包 / 便携版（可选装后台服务）                    | 安装包 / 绿色版                        |
| **连接与中转**     | 优先直连，公共节点仅协助打洞不转画面；打不通时走自建中转 | 优先直连，打不通时走免费公共中转或自建 `hbbs`+`hbbr` | 厂商服务器调度与中转，免费版有限制     |
| **平台**           | 仅 Windows 10/11                                        | 全平台（桌面 + 移动）                                | 全平台（桌面 + 移动）                  |

---

## 网络连通能力

| 网络环境                                | 连接方式                   |
| --------------------------------------- | -------------------------- |
| 同一局域网 / Wi-Fi                      | 局域网直连（1~5 ms）       |
| 两端均有 IPv6                           | IPv6 直连                  |
| 任一端有公网 IP                         | 直连                       |
| 普通家用宽带 ↔ 普通家用宽带             | NAT 打洞直连               |
| EasyTier / Tailscale 等虚拟局域网       | 经虚拟网卡直连             |
| 两端均为严格公司 / 校园网（对称 NAT）   | **需自建中转**             |
| 防火墙屏蔽所有 UDP                      | **需自建中转**（自动走 TCP） |

- **公网辅助连接**：默认使用 iroh 官方免费节点交换地址协助打洞，**不转发画面与键鼠**（可在「设置 → 网络与发现 → 公网辅助连接」关闭）。
- **私有中转**：仅在无法直连时负责转发端到端加密流量；部署前可先尝试开启路由器 IPv6。

---

## 快速上手

### 1. 远程桌面

两台电脑运行 `remote-control.exe`：

- **局域网**：在「临时设备」中点 **「控制」**，对方允许后建立连接。
- **跨网络**：对方点 **「复制本机连接码」**，主控端粘贴到顶部输入框（或直接 `Ctrl+V`）。连接码 1 小时内单次有效。
- **常用设备**：点临时设备上的 **「记住」** 互加为「信任设备」；在 `···` 菜单中设为 **「我的电脑」** 后双击即可免确认直连。

### 2. 后台服务

在 **「设置 → 后台服务 → 安装」** 中启用（需管理员权限）：

- 支持开机自启、关闭窗口后台常驻，以及锁屏 / 登录界面 / UAC 弹窗控制（锁屏控制仅对「我的电脑」生效）。
- 服务直接运行当前 `.exe`，配置与密钥保存在 `%APPDATA%\remote-control`；托盘右键「退出」可停止服务并下线。

### 3. AI Agent 与 CLI

在主控端打开 **「设置 → AI 集成 → 配置 AI Skills → 安装」**，即可让 Claude Code / Codex 自动调用 CLI 跨设备执行命令与传输文件（「我的电脑」免确认，「信任设备」首次弹窗确认，已装后台服务时以管理员权限运行）。

```powershell
remote-control devices                          # 查看在线设备
remote-control shell laptop                     # 进入远程交互式 PowerShell
remote-control exec laptop "git status"         # 执行远程命令
remote-control exec laptop --cwd D:\proj "git status"
remote-control cp .\report.pdf laptop:Desktop   # 上传文件或文件夹（参数对调即为下载）
```

### 4. 部署私有中转（可选）

```yaml
services:
  rc-relay:
    image: ghcr.io/peteroooooooo/rc-relay:latest
    container_name: rc-relay
    restart: unless-stopped
    network_mode: host
    volumes:
      - rc-relay:/var/lib/rc-relay

volumes:
  rc-relay:
```

1. 服务器防火墙与云厂商安全组放行 `7843/TCP` 和 `7842/UDP`。
2. 启动后通过 `docker logs rc-relay` 或 `docker exec -it rc-relay ui` 复制中转码，粘贴到各客户端的 **「设置 → 自建中转」**。
3. 执行 `docker exec -it rc-relay ui` 可打开终端管理界面管理分组与在线设备：

   ![中转管理界面](docs/images/relay-tui.png)

动态 IP 绑定域名、自定义 TLS 证书及更多配置见 [apps/rc-relay/README.md](apps/rc-relay/README.md)。

### 5. 命令行配置与批量部署

`remote-control set` 改的就是设置页里的各项，适合写进 `.bat` 或交给批量下发工具：

```powershell
remote-control set relay rcr1… public off host-only on   # 加入自建中转、关闭公网辅助、只被控
remote-control install-service                           # 安装后台服务（需管理员）
```

- 所有机器用同一个中转码即可，中转面板里每台单独列出；电脑名不好认时加 `name 财务-01`。
- 只改写出的项，其余不变；软件开着时立即生效。任何一项写错都不做改动并返回非 0 退出码。
- 配置属于运行命令的那个 Windows 用户，请在实际使用这台电脑的用户下运行。
- 全部项目见 `remote-control set --help`。「只被控」会隐藏粘贴连接码、控制、终端和 AI 集成；它只能用命令开关，设置页里没有，关掉用 `remote-control set host-only off`。

批量部署时，把下面的 `部署.bat` 和 `remote-control.exe` 放在同一个文件夹里（打成 zip 发出去最方便），改好中转码，让对方双击 `部署.bat`：

```bat
@echo off
"%~dp0remote-control.exe" set relay rcr1… public off host-only on
"%~dp0remote-control.exe" install-service
pause
```

- `%~dp0` 是 `.bat` 所在的文件夹，解压到哪里都能找到 exe。
- 第二行安装后台服务（锁屏、登录界面、UAC 弹窗时也能连），需要右键「以管理员身份运行」，且要用这台电脑平时登录的账号。不需要后台服务就删掉这一行，直接双击即可。

---

Apache-2.0 许可证，见 [LICENSE](LICENSE)。社区交流：[LINUX DO](https://linux.do)。
