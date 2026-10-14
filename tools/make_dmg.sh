#!/usr/bin/env bash
# 用 hdiutil 制作 macOS 安装镜像，替代 Tauri 的 bundle_dmg.sh。
# 上游脚本会让 Finder 美化窗口（排图标、设背景），每次无人值守构建都要授权一次
# AppleScript 自动化权限；这里产出同样可拖到「应用程序」的镜像，且不碰桌面。
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
app="${1:-$root/target/release/bundle/macos/Sevqo.app}"
out_dir="${2:-$root/target/release/bundle/dmg}"

if [[ ! -d "$app" ]]; then
  echo "找不到 $app —— 先构建 .app：(cd apps/desktop && npm run tauri build -- --bundles app)" >&2
  exit 1
fi

version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")"
# tauri names its artifacts aarch64 / x86_64, so match that instead of `uname -m`.
# Cross-built artifacts must say what they are, hence the ARCH override for CI.
if [[ -n "${ARCH:-}" ]]; then
  arch="$ARCH"
else
  case "$(uname -m)" in
    arm64) arch=aarch64 ;;
    *) arch="$(uname -m)" ;;
  esac
fi
out="$out_dir/Sevqo_${version}_${arch}.dmg"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"

mkdir -p "$out_dir"
rm -f "$out"
hdiutil create -volname Sevqo -srcfolder "$stage" -ov -format UDZO "$out"
hdiutil verify "$out"
echo "$out"
