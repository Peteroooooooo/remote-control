# Remote Control for iOS / iPadOS

A controller app for iPhone and iPad, ported from the Android app (`apps/rc-android`). It
controls a Windows PC running `remote-control.exe`; the phone or tablet is only the controller.

The network engine is the same Rust code as on Android: `crates/rc-ios` puts a C ABI over the
engine in `crates/rc-android` (LAN discovery, `rc/meet/1` handshake, QUIC session, input). The UI
is SwiftUI + UIKit, video is decoded by VideoToolbox through `AVSampleBufferDisplayLayer`.
To the host the app looks like the Android controller, so existing PCs need no update.

## What works in this first version

- Computer list: LAN computers show up by themselves; `rc1…` connection codes and `rcr1…`
  relay codes can be pasted; remember / agree / refuse / forget, retry.
- Full-screen desktop with H.264 / HEVC hardware decode, monitor picker, connection info,
  relay-only switch, fps / latency badge.
- Touch input in trackpad and direct mode with the same gestures as Android (see
  `RemoteControl/Session/RemoteScreenView.swift`), local pinch zoom, the bottom-right wheel.
- Key bar with draft box (works with Chinese input methods), shortcut keys, one-shot
  Ctrl / Alt / Shift / Win.
- iPad extras: hardware keyboard keys go straight to the PC, the Magic Keyboard trackpad or a
  mouse drives the PC pointer (clicks, right click, two-finger scroll), Apple Pencil is sent as a
  pen with pressure.

Not yet ported: the hub (live preview + window cards), virtual display, quality presets,
audio, the full PC keyboard panel. iOS does not allow in-app updates.

## LAN discovery on iOS

iOS drops UDP broadcasts unless the app holds the multicast entitlement, which Apple grants on
request to paid developer accounts. The app therefore also sends the discovery query to every
host of its Wi-Fi subnet (up to /23), which only needs the local network permission. On first
launch iOS asks for that permission; without it only connection codes work.

## Build

Needs macOS with Xcode 16, [rustup](https://rustup.rs) and
[XcodeGen](https://github.com/yonaskolb/XcodeGen):

```sh
apps/rc-ios/scripts/build-core.sh          # Rust engine -> apps/rc-ios/build/RcCore.xcframework
cd apps/rc-ios && xcodegen generate        # -> RemoteControl.xcodeproj
open RemoteControl.xcodeproj               # pick your team under Signing, then run
```

The `iOS` GitHub Actions workflow does the same, runs the logic tests on a simulator and
uploads an unsigned `remote-control.ipa` as the `remote-control-ios-unsigned` artifact.

## Installing without a Mac

The unsigned IPA has to be signed before iOS installs it. With a free Apple ID, tools such as
Sideloadly or AltStore (Windows or macOS) sign it with that ID and install it over USB; the
signature then lasts 7 days and has to be renewed. A paid Apple Developer account removes the
7-day limit and allows TestFlight.
