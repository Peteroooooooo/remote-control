# Android Controller Guide

<p><a href="android.md">简体中文</a> · <strong>English</strong></p>

Control a Windows PC from an Android phone or tablet. The PC runs `remote-control.exe` as usual; the phone only controls, it cannot be controlled.

- Install: download the latest `remote-control.apk` from [GitHub Releases](https://github.com/Peteroooooooo/remote-control/releases) or [CNB](https://cnb.cool/peteroooooooo/remote-control/-/releases). Requires Android 10+ on 64-bit ARM (arm64).
- Update: gear icon (top right) → **Check for updates**. The app only checks when you tap it and never shows prompts on its own. Some systems (e.g. ColorOS) show risk warnings and ask for the lock-screen password during install; that is system behavior.

> The app UI is currently Chinese only.

---

## 1. Connecting

- **Same LAN**: PCs appear under 可用电脑 (available PCs) automatically. Tap one to connect; the PC asks for permission.
- **Across networks**: on the PC, click **Copy connection code** and send it to the phone. On the phone tap **+**, paste the `rc1…` code and tap 添加 (Add).
- **Self-hosted relay**: tap **+** and paste an `rcr1…` relay code. PCs on the same relay become reachable.
- **Remember**: tap 记住 (Remember) on a device card; once the PC agrees, both sides keep each other. From then on, tap to connect whenever the PC is online (also with the lobby window closed, if the background service is installed). The `···` button on a card re-checks the connection or forgets the device.
- **Device name**: gear → 设备名称 (device name). This is the name the PC shows for this phone; leave it empty to use the phone model.

## 2. After connecting

You land on the PC's home page (hub): the top bar shows the PC name with connection route and latency (`Direct / Relay · ms`), with a live desktop preview and running windows below.

- Tap the desktop preview to control the whole desktop full screen.
- Tap a window card once to bring it to the front in the preview; tap it again (or double-tap) to open it full screen.
- The system back gesture returns from full screen to the home page. On the home page, back once more shows a hint; swipe again to disconnect and return to the PC list.

## 3. The capsule at the top of full screen

| Button | Action |
|---|---|
| ← | Back to the home page |
| 触控板 / 直触 | Switch between trackpad mode and direct-touch mode (below) |
| `1.0×` | Current zoom; appears when zoomed, tap to reset |
| ⌨ | Top text input bar, shortcuts, and phone keyboard |
| ⟳ | Rotate between landscape and portrait |
| `…` | Connection stats (latency, fps, Direct/Relay), quality (resolution / fps / bitrate cap), relay-only, and "Show FPS & latency" (persistent corner HUD) |

## 4. Trackpad mode

The phone screen works like a laptop trackpad and moves a mouse cursor. Best for precise clicks and dragging windows.

| Gesture | Result |
|---|---|
| One-finger slide | Move the cursor; a quick flick keeps gliding |
| One-finger tap | Left click |
| Hold ~0.4 s (phone buzzes), then lift | Right click |
| Hold until buzz, then drag | Left-button drag (move windows, select, drag files) |
| Tap, then immediately press again and drag | Left-button drag as well |
| Two-finger tap | Right click |
| Two-finger pinch / drag | Zoom and pan the picture locally (the PC resolution does not change) |
| Three-finger tap | Show / hide the keyboard |

- **Bottom-right scroll wheel**: a hollow click-wheel ring handles vertical and horizontal scrolling—tap for one notch, hold for continuous scroll—without conflicting with two-finger zoom/pan.
- **Bottom-left joystick (when zoomed in)**: pushing it pans the desktop while the cursor stays fixed on screen, helping hit small buttons on a small screen.

## 5. Direct-touch mode

Your finger lands exactly where it touches on the PC screen, like a touchscreen laptop. Best for large buttons and browsing.

| Gesture | Result |
|---|---|
| One-finger tap | Click at that spot |
| One-finger drag | Pan the zoomed picture |
| Hold (buzz), then lift | Right click |
| Hold (buzz), then drag | Left-button drag; the picture follows when you reach the screen edge |
| Two-finger pinch / drag | Zoom and pan the picture locally |
| Three-finger tap | Show / hide the keyboard |

Scrolling uses the same **bottom-right scroll wheel** (aligned to your last tap position). Styluses work, including the eraser end.

## 6. What the circles mean

Each tap or drag shows a circle under your finger telling you what the PC received:

| Circle | Meaning |
|---|---|
| White ripple | Left click |
| Yellow ripple | Right click |
| Blue ring that follows your finger | The left button is held down (dragging). Lift your finger to release it |

If a blue ring appears unexpectedly, you started a drag (hold-until-buzz then move, or tap and press again right away). Just lift your finger.

## 7. Keyboard and shortcuts

Tap ⌨ on the capsule or tap with three fingers. A translucent top input bar appears along with the phone keyboard:

- **Text input**: type in the top draft box and tap `Send` (or the keyboard send action) to send text to the PC; tap `↵` to send Enter directly. When the box is empty, Backspace and Enter pass through to the PC immediately.
- **Quick shortcuts**: `Copy`, `Paste`, `Undo`, `All`.
- **Full PC keyboard**: expand for `Esc`, `Tab`, F1–F12, arrows, `Home` / `End`, `Del`, etc.
- **`Ctrl` / `Alt` / `Shift` / `Win`**: tap once (light blue) to apply to the next key only, e.g. `Ctrl` then `C`. Double-tap (solid blue) to keep it held; tap again to release.
