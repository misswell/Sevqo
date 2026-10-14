#!/usr/bin/env bash
# 把 tools/release_macos.sh 的产物发成 GitHub Release，并回读 latest.json 确认可用。
#
# 用法：
#   bash tools/publish_release.sh                 # 用 tauri.conf.json 的版本号发布
#   bash tools/publish_release.sh v0.1.0          # 指定标签
#   DRY=1 bash tools/publish_release.sh           # 只核对产物和清单，不上传
#
# 依赖：gh（已登录）、jq、tar。仓库地址取 GH_REPO，默认 misswell/Sevqo。
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
conf="$root/apps/desktop/src-tauri/tauri.conf.json"
bundle="$root/target/release/bundle"
app="$bundle/macos/Sevqo.app"
up_dir="$bundle/updater"

name="$(jq -r '.productName' "$conf")"
version="$(jq -r '.version' "$conf")"
tag="${1:-v$version}"
repo="${GH_REPO:-misswell/Sevqo}"
archive="$up_dir/${name}_${tag}_macos_aarch64.app.tar.gz"
dmg="$bundle/dmg/${name}_${tag}_macos_aarch64.dmg"

log() { echo "==> $*"; }
fail() { echo "✗ $*" >&2; exit 1; }

for f in "$dmg" "$archive" "$archive.sig" "$up_dir/latest.json"; do
  [[ -s "$f" ]] || fail "缺少 $f —— 先跑 bash tools/release_macos.sh"
done
gh auth status --hostname github.com >/dev/null || fail "gh 未登录 github.com"

# 已存在的 Release 只能由专门授权的修订流程处理，绝不覆盖附件或说明。
if gh api "repos/$repo/releases/tags/$tag" >/dev/null 2>&1; then
  fail "远端 Release $tag 已存在；拒绝覆盖"
fi

# 公证状态决定发行说明里要不要写「如何绕过 Gatekeeper」
if xcrun stapler validate "$app" >/dev/null 2>&1 && spctl -a -t exec -vv "$app" 2>&1 | grep -q 'accepted'; then
  notarized=1
else
  notarized=0
  log "提示：这份产物没有公证凭据"
fi

jq -e --arg v "$version" '.version == $v and (.platforms | length > 0)' "$up_dir/latest.json" >/dev/null \
  || fail "latest.json 的版本与 tauri.conf.json 不一致，或没有平台条目"
# notes 是更新卡片上唯一的说明文字，空字符串会显示成一片空白
jq -e '.notes | length > 0' "$up_dir/latest.json" >/dev/null \
  || fail "latest.json 的 notes 是空的，客户端的更新卡片会没有说明"
jq -e --arg a "$(basename "$archive")" '[.platforms[].url] | all(endswith($a))' "$up_dir/latest.json" >/dev/null \
  || fail "latest.json 里的下载地址指的不是 $archive"

notes="$(cat "$root/RELEASE_NOTES.md")"

if [[ "${DRY:-0}" == 1 ]]; then
  log "DRY=1：核对通过，不上传"
  ls -lh "$dmg" "$archive" "$archive.sig" "$up_dir/latest.json"
  exit 0
fi

log "发布 $repo 的 $tag"
files=("$dmg" "$archive" "$archive.sig" "$up_dir/latest.json")
notes_file="$bundle/release-notes-$tag.md"
printf '%s\n' "$notes" >"$notes_file"
gh release create "$tag" --repo "$repo" --title "$name $tag" --notes-file "$notes_file" --latest "${files[@]}"

log "回读更新源"
url="https://github.com/$repo/releases/latest/download/latest.json"
got="$(curl -fsSL "$url" | jq -r '.version')"
[[ "$got" == "$version" ]] || fail "$url 返回版本 $got，期望 $version"
log "更新源可用：${url}（${got}）"
gh release view "$tag" --repo "$repo" --json tagName,url,assets --jq '{tag:.tagName,url:.url,assets:[.assets[].name]}'
