#!/usr/bin/env bash
# CI 专用：签名 .app → 出镜像 → 签名镜像 → 公证 → 装订。
# 需要环境变量：ARCH（aarch64|x86_64）、TARGET（Rust 三元组）、RELEASE_TAG，
# 以及 SIGNING_IDENTITY（由 import_signing_identity.sh 写入）和 APPLE_* 公证凭据。
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
: "${ARCH:?}"
: "${TARGET:?}"
: "${RELEASE_TAG:?}"
: "${SIGNING_IDENTITY:?}"
# 显式逐个判断：这些写法在 macOS 自带的 bash 3.2 上验证过
for v in APPLE_ID APPLE_TEAM_ID APPLE_APP_SPECIFIC_PASSWORD; do
  if [ -z "${!v:-}" ]; then
    echo "缺少 $v" >&2
    exit 1
  fi
done

app="$root/target/$TARGET/release/bundle/macos/Sevqo.app"
release="$root/release"
[ -d "$app" ] || { echo "找不到 $app" >&2; exit 1; }
mkdir -p "$release"
log() { echo "==> $*"; }

log "签名 .app"
sign=(--force --options runtime --timestamp --sign "$SIGNING_IDENTITY")
while IFS= read -r -d '' f; do
  file "$f" | grep -q 'Mach-O' && codesign "${sign[@]}" "$f"
done < <(find "$app/Contents" -type f -print0)
codesign "${sign[@]}" "$app"
codesign --verify --deep --strict --verbose=2 "$app"
# 主程序名由 Info.plist 决定（tauri 用的是 crate 名，不是产品名）
exe="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$app/Contents/Info.plist")"
lipo -archs "$app/Contents/MacOS/$exe"

log "制作并签名镜像"
out="$(ARCH="$ARCH" bash "$root/tools/make_dmg.sh" "$app" "$root/target/dmg-ci" | tail -1)"
dmg="$release/Sevqo_${RELEASE_TAG}_macos_${ARCH}.dmg"
mv -f "$out" "$dmg"
codesign --force --timestamp --sign "$SIGNING_IDENTITY" "$dmg"
codesign --verify --verbose=2 "$dmg"

log "提交公证"
xcrun notarytool submit "$dmg" \
  --apple-id "$APPLE_ID" \
  --team-id "$APPLE_TEAM_ID" \
  --password "$APPLE_APP_SPECIFIC_PASSWORD" \
  --wait

log "装订并验收"
xcrun stapler staple "$app"
xcrun stapler staple "$dmg"
xcrun stapler validate "$app"
xcrun stapler validate "$dmg"
spctl -a -vvv -t exec "$app"
