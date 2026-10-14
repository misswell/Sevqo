#!/usr/bin/env bash
# CI 专用：把装订好的 .app 打成 updater 归档，并用 minisign 私钥签名。
# 归档必须在公证、装订之后生成，否则用户走更新装到的包里没有凭据。
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
: "${ARCH:?}"
: "${TARGET:?}"
: "${RELEASE_TAG:?}"
if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
  echo "缺少 TAURI_SIGNING_PRIVATE_KEY（更新包签名私钥）" >&2
  exit 1
fi

app="$root/target/$TARGET/release/bundle/macos/Sevqo.app"
[ -d "$app" ] || { echo "找不到 $app" >&2; exit 1; }
xcrun stapler validate "$app" >/dev/null || { echo "$app 还没有装订公证凭据" >&2; exit 1; }

version="$(jq -r '.version' "$root/apps/desktop/src-tauri/tauri.conf.json")"
mkdir -p "$root/release"
archive="$root/release/Sevqo_${RELEASE_TAG}_macos_${ARCH}.app.tar.gz"
rm -f "$archive" "$archive.sig"

echo "==> 打包 $archive"
tar -czf "$archive" -C "$(dirname "$app")" "$(basename "$app")"

echo "==> minisign 签名"
cargo tauri signer sign \
  --private-key "$TAURI_SIGNING_PRIVATE_KEY" \
  --password "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}" \
  --app-version "$version" \
  "$archive"
[ -s "$archive.sig" ] || { echo "没有生成 $archive.sig" >&2; exit 1; }
ls -lh "$archive" "$archive.sig"
