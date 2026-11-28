#!/usr/bin/env bash
# Builds the Rust engine (crates/rc-ios) for iOS devices and the arm64 simulator and packs
# both into apps/rc-ios/build/RcCore.xcframework. Run on macOS with Xcode and rustup.
#
#   apps/rc-ios/scripts/build-core.sh            # release build
#   PROFILE=dev apps/rc-ios/scripts/build-core.sh  # faster debug build
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
APP="$ROOT/apps/rc-ios"
PROFILE="${PROFILE:-ios-dist}"
OUT_DIR="$([ "$PROFILE" = dev ] && echo debug || echo "$PROFILE")"
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-16.0}"

LOCKED="${CARGO_LOCKED:---locked}"
cd "$ROOT"
for target in aarch64-apple-ios aarch64-apple-ios-sim; do
  rustup target add "$target" >/dev/null
  cargo build $LOCKED --profile "$PROFILE" -p rc-ios --target "$target"
done

rm -rf "$APP/build/RcCore.xcframework"
mkdir -p "$APP/build"
xcodebuild -create-xcframework \
  -library "target/aarch64-apple-ios/$OUT_DIR/librc_ios.a" -headers "$APP/RcCore/include" \
  -library "target/aarch64-apple-ios-sim/$OUT_DIR/librc_ios.a" -headers "$APP/RcCore/include" \
  -output "$APP/build/RcCore.xcframework"
ls -l "target/aarch64-apple-ios/$OUT_DIR/librc_ios.a"
