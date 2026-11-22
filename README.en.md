<div align="center">
  <img src="apps/remote-control/assets/icon.svg" width="76" alt="Remote Control" />
  <h1>REMOTE CONTROL</h1>
  <p><a href="README.md">简体中文</a> · <strong>English</strong></p>
  <p><strong>Single-exe, P2P-first Windows remote desktop, terminal &amp; AI Agent cross-device control tool</strong></p>
  <p>
    Built with Rust &amp; iroh. No accounts, no SSH setup, no port forwarding — direct hole-punching by default without touching third-party servers;<br />
    One-click integration for <strong>Claude Code / Codex</strong> to check logs, run commands, and transfer files across PCs, with optional self-hosted Docker relay for restricted networks.
  </p>
  <p>
    <a href="https://peteroooooooo.github.io/remote-control/?lang=en"><strong>Try Online Demo →</strong></a> (Interactive web mockup of remote control, terminal &amp; connection codes with mock data)
  </p>
  <img src="docs/images/lobby-devices-en.png" width="760" alt="Remote Control" />
</div>

---

## Key Features

- **Cross-Device Control for AI Agents**: One-click Skill install (`~/.claude/skills`, `~/.agents/skills`) lets Claude Code and Codex call `devices` / `exec` / `cp` on your other Windows PCs — no OpenSSH, public IP, or port forwarding required, reusing the lobby's P2P/relay connections and device permissions.
- **Remote Desktop**: Automatic LAN discovery, cross-network pairing via connection codes, and full GPU hardware acceleration (up to 144 fps, low-latency DXGI rendering, FEC packet-loss recovery, automatic Wi-Fi low-latency mode, configurable resolution / frame rate / bitrate, and 4:4:4 crisp text mode) for smooth high-refresh office work and video playback. Supports multi-monitor switching, clipboard sync (text, images, files, and folders), `Ctrl+Alt+Del`, and lock-screen control when the background service is installed.
- **Remote Audio**: Real-time audio streaming with standard (Opus) and lossless (24-bit PCM, auto-matched sample rate) modes plus a low-latency jitter buffer. Includes independent volume control and one-click local speaker mute (automatically restored on disconnect).
- **Remote Terminal & File Transfer**: Drop straight into the remote PowerShell, run single commands, or transfer files and folders via CLI without opening a desktop stream.
- **Private Relay (Optional)**: A single Docker image with built-in self-signed TLS that only forwards encrypted traffic for your own devices — no domain or external certificate required.

---

## Comparison with RustDesk, ToDesk, etc.

|                             | Remote Control                                                                   | RustDesk                                                                      | ToDesk / Sunlogin                                                   |
| --------------------------- | -------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- | ------------------------------------------------------------------- |
| **AI Agent / CLI**          | Native `devices` / `shell` / `exec` / `cp` and one-click Skill                   | No built-in remote shell / AI Skill                                           | No built-in remote shell / AI Skill                                 |
| **Account**                 | None required; local keys + connection codes                                     | None required; ID + password                                                  | Account required for controller / unattended access                 |
| **Distribution**            | Single portable `.exe` (optional background service)                             | Installer / portable version (optional background service)                    | Installer / lightweight portable edition                            |
| **Connection & Relay**      | Direct P2P first; public nodes never relay video; self-host relay if P2P fails   | Direct P2P first; falls back to free public relays (may be slow at peak)      | Vendor-routed relays with high NAT traversal; free tier has limits  |
| **Self-Hosted Relay**       | Single image, auto-generated cert, forwards only your devices                    | Supported; requires deploying `hbbs` + `hbbr` and configuring address &amp; key | Not supported                                                     |
| **Platform & Ecosystem**    | Windows 10/11 only; supports remote audio (including lossless mode)              | Cross-platform (desktop + mobile), mature ecosystem, audio &amp; file manager | Cross-platform, full-featured (mobile-to-PC, privacy screen, etc.)  |

**Best for**: Windows users who want Claude Code / Codex to work directly across PCs, prefer a lightweight zero-account tool, and don't want screen traffic passing through third-party servers.

**Not for**: Mobile / Mac / Linux control, strictly firewalled networks where you don't plan to host a relay, or pro game streaming requiring controller passthrough / HDR (use Moonlight + Sunshine for pure game streaming).

---

## Network Connectivity

**Most networks connect directly without any server seeing your screen. A private relay is only needed when both ends are behind strict networks that block direct P2P.**

| Network Environment                              | How It Connects                           |
| ------------------------------------------------ | ----------------------------------------- |
| Same LAN / Wi-Fi                                 | Direct LAN connection, 1–5 ms latency     |
| Both ends have IPv6                              | Direct IPv6 connection                    |
| At least one end has a public IP                 | Direct connection                         |
| Home broadband ↔ Home broadband                  | Direct NAT hole-punching                  |
| EasyTier / Tailscale / virtual LAN               | Direct over virtual adapter               |
| Both ends on strict corporate / campus symmetric NAT | **Requires private relay**            |
| Firewall blocks all UDP                          | **Requires private relay** (over TCP)     |

Without a private relay, those last two cases cannot stream video and the lobby will show "Unable to connect". Try enabling IPv6 on your router first; if that's not an option, deploy a private relay.

### Public Relay Assist vs. Private Relay

- **Public Relay Assist**: Free public iroh nodes used solely to exchange addresses and establish the connection — they **never forward screen video or input**. Can be disabled in **Settings → Network & Discovery → Public Relay Assist**.
- **Private Relay**: Your own self-hosted server (see "4. Deploy a Private Relay" below). When direct hole-punching fails, it forwards end-to-end encrypted traffic.

---

## Quick Start

### 1. Remote Desktop Control

Run `remote-control.exe` on both PCs and open the main window:

- **Same LAN**: The other PC appears under **Temporary Devices**. Click **Control** and wait for the other side to accept.
- **Across Networks**: Have the other side click **Copy Connection Code** and send it to you. Paste it into the top input bar (or press `Ctrl+V` anywhere in the window). Connection codes expire in 1 hour and are single-use.
- **Future Connections**: Click **Remember** on a temporary device. Once accepted, both PCs become **Trusted Devices** and can find each other even after restarts or network changes.
- **Your Own PCs**: Open the `···` menu on a trusted device and mark it as **My Devices**. It can then connect directly without confirmation prompts on double-click.

Temporary devices are only visible while both windows are open, so keep both windows open for the first connection.

### 2. Background Service & System Tray

Go to **Settings → Background Service → Install** (requires administrator privileges). Once installed:

- The PC stays online after closing the window and starts automatically at boot;
- Works on the lock screen, sign-in screen, and UAC prompts (**Allow Lock Screen Control** is available only for **My Devices**).

A few helpful notes:

- **Tray**: Closing the window minimizes it to the system tray (on Windows 11, check the `^` overflow menu and drag it onto the taskbar if desired). Left-click opens the window; right-click → **Exit** takes the PC offline and stops the background service.
- **File Locations**: The service runs the exact `.exe` you launched without copying it elsewhere. Device keys, device lists, and settings are stored in `%APPDATA%\remote-control`.
- **Upgrading**: Right-click the tray icon → **Exit**, replace the `.exe`, and launch it again — the service restarts automatically. If you moved the `.exe` to a new path, the main window will prompt you to switch the service path with one click.

### 3. AI Agent & CLI

**Connect Claude Code / Codex:**

1. On your controller PC, go to **Settings → AI Integration → AI Skills → Install** (writes to `~/.claude/skills` and `~/.agents/skills`; if you move or upgrade the `.exe`, the UI will prompt to update, and you can uninstall anytime).
2. Start a new chat and give cross-device tasks directly, e.g. *"Check the service logs on my laptop"* or *"Copy `dist/` to the desktop PC's Desktop and restart the service"*.

Permissions and networking follow your lobby settings: **My Devices** execute without prompts; **Trusted Devices** prompt once (with a 60-second grace window after approval); and commands run elevated if the target has the background service installed. A top banner stays visible on the target screen with a **Disconnect** button at all times.

**CLI Commands (manual or invoked by AI):**

```powershell
remote-control devices                          # List online devices and device codes
remote-control shell laptop                     # Open an interactive PowerShell on laptop
remote-control exec laptop "git status"         # Run a command (UTF-8, split stdout/stderr, exit code preserved)
remote-control exec laptop --cwd D:\proj "git status"
remote-control cp .\report.pdf laptop:Desktop   # Push file or folder (reverse args to pull)
```

See `remote-control --help`, `remote-control exec --help`, and `remote-control cp --help` for more options.

### 4. Deploy a Private Relay (Optional)

Only needed when both ends are behind strict corporate/campus NATs or UDP-blocking firewalls and direct P2P fails (the lobby shows "Unable to connect").

**How it works:**

- On startup, the relay generates a **relay code**. Paste this same code on each of your PCs to make them members of the relay.
- Members sharing the same code can relay traffic with each other. Each member's **Trusted Devices**, **My Devices**, and **Temporary Devices** can only relay to that specific member PC. Anyone else connecting to the relay cannot forward anything.
- The relay only sees encrypted bytes — never screen video, input, or commands.
- Removing a device in the relay TUI takes effect within 1 second, and the member PC that brought it in removes it as well.

**Deploy:** Create a new Stack in Portainer, 1Panel, etc. and paste:

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

1. Open `7843/TCP` and `7842/UDP` in your server firewall and cloud security group.
2. Start the container and copy the relay code from `docker logs rc-relay` or `docker exec -it rc-relay ui`.
3. On each of your PCs, paste it into **Settings → Private Relays**.
4. To manage groups, check online devices, or kick a device, run `ui` in the container console (or `docker exec -it rc-relay ui`):

   ![Relay TUI](docs/images/relay-tui.png)

**Addresses & Certificates:** No domain or external certificate is needed by default. The relay generates a self-signed certificate and embeds its fingerprint into the relay code for verification. Since the code contains the server IP, **if the server IP changes, you must re-paste the code on each PC**. If your IP is dynamic (home broadband or a cloud VM without a static IP), attach a domain name instead. See [apps/rc-relay/README.en.md](apps/rc-relay/README.en.md) for IPv4/IPv6-only setups, custom domains, trusted TLS certificates, and home-router port forwarding.

**When multiple relays are configured:**

- Device lists and connection codes sync to every relay; kicking a device on any relay removes it across the others.
- Video traffic only uses the first relay in the list. Click **Set as Primary** on any row to move it to the top; newly pasted relay codes automatically become primary.
- If the primary relay is unreachable for 10 seconds, the next reachable relay takes over temporarily and switches back once the primary recovers.
