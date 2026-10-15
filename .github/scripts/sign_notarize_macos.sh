#!/usr/bin/env bash
# CI 专用：签名 .app → 公证并装订 .app → 用带票据的 .app 出镜像 → 签名镜像 → 公证并装订镜像 → 挂载复核。
# 需要环境变量：ARCH（aarch64|x86_64）、TARGET（Rust 三元组）、RELEASE_TAG，
# 以及 SIGNING_IDENTITY（由 import_signing_identity.sh 写入）和 APPLE_* 公证凭据。
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
: "${ARCH:?}"
: "${TARGET:?}"
: "${RELEASE_TAG:?}"
: "${SIGNING_IDENTITY:?}"
# 公证凭据两条路线任选其一（本机用的就是 API key 那条）：
#   APPLE_API_KEY_BASE64 + APPLE_API_KEY_ID + APPLE_API_ISSUER
#   APPLE_ID + APPLE_TEAM_ID + APPLE_APP_SPECIFIC_PASSWORD
api_key_ready=1
for v in APPLE_API_KEY_BASE64 APPLE_API_KEY_ID APPLE_API_ISSUER; do
  [ -n "${!v:-}" ] || api_key_ready=0
done
apple_id_ready=1
for v in APPLE_ID APPLE_TEAM_ID APPLE_APP_SPECIFIC_PASSWORD; do
  [ -n "${!v:-}" ] || apple_id_ready=0
done
if [ "$api_key_ready" = 0 ] && [ "$apple_id_ready" = 0 ]; then
  {
    echo "缺少公证凭据，二选一："
    echo "  APPLE_API_KEY_BASE64 + APPLE_API_KEY_ID + APPLE_API_ISSUER"
    echo "  APPLE_ID + APPLE_TEAM_ID + APPLE_APP_SPECIFIC_PASSWORD"
  } >&2
  exit 1
fi

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

log "公证并装订（顺序要紧：镜像必须从已经带票据的 .app 造出来）"
notarize() {
  # 两条凭据路线共用；.p8 只落临时文件，用完就删——runner 的 workspace 会进日志
  if [ "$api_key_ready" = 1 ]; then
    key_file="$(mktemp)"
    chmod 600 "$key_file"
    printf '%s' "$APPLE_API_KEY_BASE64" | base64 -D >"$key_file"
    xcrun notarytool submit "$1" --key "$key_file" \
      --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER" --wait
    rm -f "$key_file"
  else
    xcrun notarytool submit "$1" --apple-id "$APPLE_ID" \
      --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_SPECIFIC_PASSWORD" --wait
  fi
}

work="$(mktemp -d)"
mnt="$(mktemp -d)"
# 先卸载再删目录：验收失败时 $mnt 还挂着镜像，直接 rm -rf 等于对活挂载点动手
trap 'hdiutil detach "$mnt" -force >/dev/null 2>&1 || true; rm -rf "$work" "$mnt"' EXIT
app_zip="$work/Sevqo_${RELEASE_TAG}_macos_${ARCH}.notarization.zip"
ditto -c -k --keepParent "$app" "$app_zip"
notarize "$app_zip"
xcrun stapler staple "$app"
xcrun stapler validate "$app"

log "制作并签名镜像"
out="$(ARCH="$ARCH" bash "$root/tools/make_dmg.sh" "$app" "$root/target/dmg-ci" | tail -1)"
dmg="$release/Sevqo_${RELEASE_TAG}_macos_${ARCH}.dmg"
mv -f "$out" "$dmg"
codesign --force --timestamp --sign "$SIGNING_IDENTITY" "$dmg"
codesign --verify --verbose=2 "$dmg"

notarize "$dmg"
xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"

log "验收"
spctl -a -vvv -t exec "$app"
# 挂载后查镜像里那份 app：CI 一度先造镜像、后装订 .app，用户从 dmg 拖出来的是没票据的副本，
# 离线或 ocsp 不通时 Gatekeeper 就会拦。只看 dmg 自己的票据发现不了这件事。
hdiutil attach -readonly -nobrowse -mountpoint "$mnt" "$dmg" >/dev/null
xcrun stapler validate "$mnt/Sevqo.app"
spctl -a -vvv -t exec "$mnt/Sevqo.app"
hdiutil detach "$mnt" >/dev/null
