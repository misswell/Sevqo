#!/usr/bin/env bash
# Sevqo macOS 发布链路：构建 → Developer ID 签名 → 公证 → 装订 → 镜像与更新包 → minisign → latest.json
#
# 只负责产出可发布的文件，不碰 GitHub；推送与发版用 tools/publish_release.sh。
#
# 用法：
#   bash tools/release_macos.sh                 # 全流程
#   SKIP_BUILD=1 bash tools/release_macos.sh    # 复用已构建好的 .app
#   SIGN_ONLY=1  bash tools/release_macos.sh    # 只签名，不公证（自检用，别拿这种产物发版）
#
# 环境变量：
#   SIGNING_IDENTITY          默认自动探测 "Developer ID Application: …"
#   NOTARY_PROFILE            xcrun notarytool 的钥匙串凭据 profile 名
#   APPLE_ID / APPLE_APP_SPECIFIC_PASSWORD / APPLE_TEAM_ID   notarytool 的直接凭据
#   UPDATER_KEY               默认 apps/desktop/src-tauri/.tauri/sevqo-updater.key
#   UPDATER_KEY_PASSWORD      默认读同名的 .password 文件
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tauri_dir="$root/apps/desktop/src-tauri"
conf="$tauri_dir/tauri.conf.json"
bundle="$root/target/release/bundle"
app="$bundle/macos/Sevqo.app"
dmg_dir="$bundle/dmg"
up_dir="$bundle/updater"

name="$(jq -r '.productName' "$conf")"
version="$(jq -r '.version' "$conf")"
tag="${TAG:-v$version}"
# tauri 用 aarch64 / x86_64 命名产物，跟 uname -m 不一样
case "$(uname -m)" in
  arm64) arch=aarch64 ;;
  *) arch="$(uname -m)" ;;
esac

dmg="$dmg_dir/${name}_${tag}_macos_${arch}.dmg"
archive="$up_dir/${name}_${tag}_macos_${arch}.app.tar.gz"
updater_key="${UPDATER_KEY:-$tauri_dir/.tauri/sevqo-updater.key}"
updater_pw_file="$tauri_dir/.tauri/sevqo-updater.password"

log() { echo "==> $*"; }
fail() { echo "✗ $*" >&2; exit 1; }

# ------------------------------------------------------------------ 0. 前置检查
signing_identity="${SIGNING_IDENTITY:-}"
if [[ -z "$signing_identity" ]]; then
  signing_identity="$(security find-identity -v -p codesigning 2>/dev/null \
    | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)"
fi
[[ -n "$signing_identity" ]] || fail "没有 Developer ID Application 证书：${name} 的发布包必须签名，请先在 Keychain 里安装该证书"
log "签名身份：$signing_identity"

[[ -f "$updater_key" ]] || fail "找不到更新包签名私钥 $updater_key（用 cargo tauri signer generate 生成）"
if [[ -z "${UPDATER_KEY_PASSWORD:-}" ]]; then
  [[ -f "$updater_pw_file" ]] || fail "私钥有口令，但 $updater_pw_file 不存在，也没给 UPDATER_KEY_PASSWORD"
  UPDATER_KEY_PASSWORD="$(cat "$updater_pw_file")"
fi

# ------------------------------------------------------------------ 1. 构建
if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
  log "构建 .app（$version）"
  # tauri 自带的 bundle_dmg.sh 会让 Finder 美化窗口，每次都要授权 AppleScript；只出 .app，镜像自己做
  (cd "$root/apps/desktop" && npm run tauri -- build --bundles app)
fi
[[ -d "$app" ]] || fail "找不到 $app —— 先跑 cargo tauri build 或去掉 SKIP_BUILD=1"

# ------------------------------------------------------------------ 2. 签名
log "签名 .app"
sign_opts=(--force --options runtime --timestamp --sign "$signing_identity")
mach_o=0
while IFS= read -r -d '' f; do
  if file "$f" | grep -q 'Mach-O'; then
    codesign "${sign_opts[@]}" "$f"
    mach_o=$((mach_o + 1))
  fi
done < <(find "$app/Contents" -type f -print0)
codesign "${sign_opts[@]}" "$app"
codesign --verify --deep --strict --verbose=2 "$app"
codesign -dv --verbose=2 "$app" 2>&1 | grep -E 'Authority=|Timestamp=|TeamIdentifier='
log "已签名 $mach_o 个 Mach-O 文件"

# ------------------------------------------------------------------ 3. 镜像
log "制作安装镜像"
out="$(bash "$root/tools/make_dmg.sh" "$app" "$dmg_dir" | tail -1)"
[[ -f "$out" ]] || fail "make_dmg.sh 没有产出镜像"
# make_dmg.sh 按 tauri 的命名规则取名，统一到带 tag 的发布名
mv -f "$out" "$dmg"
codesign --force --timestamp --sign "$signing_identity" "$dmg"
codesign --verify --verbose=2 "$dmg"

# ------------------------------------------------------------------ 4. 公证
notarized=0
notarize() {
  local file="$1"
  if [[ "${SIGN_ONLY:-0}" == 1 ]]; then
    log "SIGN_ONLY=1：跳过公证"
    return 0
  fi
  if command -v asc >/dev/null 2>&1 && asc auth status >/dev/null 2>&1; then
    log "用 asc 提交公证：$(basename "$file")"
    asc notarization submit --file "$file" --wait --output json >"$bundle/notarize-$(basename "$file").json"
    notarized=1
    return 0
  fi
  if [[ -n "${NOTARY_PROFILE:-}" ]]; then
    log "用 notarytool（profile：$NOTARY_PROFILE）提交公证"
    xcrun notarytool submit "$file" --keychain-profile "$NOTARY_PROFILE" --wait
    notarized=1
    return 0
  fi
  if [[ -n "${APPLE_ID:-}" && -n "${APPLE_APP_SPECIFIC_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]]; then
    log "用 notarytool（Apple ID）提交公证"
    xcrun notarytool submit "$file" --apple-id "$APPLE_ID" \
      --password "$APPLE_APP_SPECIFIC_PASSWORD" --team-id "$APPLE_TEAM_ID" --wait
    notarized=1
    return 0
  fi
  fail "没有可用的公证凭据：asc / NOTARY_PROFILE / APPLE_ID 三者都没有。确实只想签名自检时加 SIGN_ONLY=1"
}
notarize "$dmg"

# ------------------------------------------------------------------ 5. 装订
if [[ "$notarized" == 1 ]]; then
  log "装订公证凭据"
  xcrun stapler staple "$app"
  xcrun stapler staple "$dmg"
  xcrun stapler validate "$app"
  xcrun stapler validate "$dmg"
  spctl -a -vvv -t exec "$app"
else
  log "未装订：这台机器只做了签名"
fi

# ------------------------------------------------------------------ 6. 更新包
log "打包 updater 归档"
mkdir -p "$up_dir"
rm -f "$archive" "$archive.sig"
# 归档里放 .app 本体，安装时由 updater 插件解开替换
tar -czf "$archive" -C "$(dirname "$app")" "$(basename "$app")"
tar -tzf "$archive" | head -3

log "用 minisign 私钥签名更新包"
# --app-version 把版本写进签名的 trusted comment，清单里换版本号也配不上这份签名
(cd "$root/apps/desktop" && npm run tauri -- signer sign \
  --private-key-path "$updater_key" --password "$UPDATER_KEY_PASSWORD" \
  --app-version "$version" "$archive" >/dev/null)
[[ -s "$archive.sig" ]] || fail "没有生成 $archive.sig"

# ------------------------------------------------------------------ 7. 清单
log "生成 latest.json"
repo="${GH_REPO:-misswell/Sevqo}"
download="https://github.com/$repo/releases/download/$tag/$(basename "$archive")"
jq -n \
  --arg version "$version" \
  --arg notes "$(RELEASE_NOTES:-"${name} ${version}")" \
  --arg pub_date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg signature "$(cat "$archive.sig")" \
  --arg url "$download" \
  --arg platform "darwin-$arch" \
  '{version:$version, notes:$notes, pub_date:$pub_date, platforms:{($platform):{signature:$signature, url:$url}}}' \
  >"$up_dir/latest.json"

log "产物清单"
ls -lh "$dmg" "$archive" "$archive.sig" "$up_dir/latest.json"
echo
echo "公证：$([[ "$notarized" == 1 ]] && echo 已完成并装订 || echo 未做（SIGN_ONLY）)"
echo "更新源：$download"
