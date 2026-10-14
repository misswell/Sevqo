#!/usr/bin/env bash
# 把 tools/release_macos.sh 的产物发成 GitHub Release，并回读 latest.json 确认可用。
#
# 用法：
#   bash tools/publish_release.sh                 # 用 tauri.conf.json 的版本号打 v* 标签
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

notes="$(cat <<EOF
本地离线的跨模态文件搜索：自然语言、以图搜图、以片段搜视频，全部在你这台机器上跑，不上传任何文件，不需要 API Key。

### 下载
| 文件 | 用途 |
|------|------|
| \`${dmg##*/}\` | 拖进「应用程序」安装（Apple Silicon） |
| \`${archive##*/}\` + \`.sig\` | 应用内更新用，别手动下载 |
| \`latest.json\` | 更新清单，签名与下载地址都在这里 |

本版为 Apple Silicon（arm64）构建；Intel 与 Universal 由 CI 产出。

### 首次启动
语义检索的模型权重（约 500 MB）不会打进安装包，启动时按设置里的下载源自动拉取并校验；断点续传、进度可见。推理还需要一个装了 \`torch / sentence-transformers / av / pillow-heif / pypdf\` 的 Python 环境，可在设置里指定路径，留空则自动探测。

$(
  if [[ "$notarized" == 1 ]]; then
    echo '### 签名与公证'
    echo 'Developer ID 签名 + Apple 公证并已装订，双击即可打开。'
  else
    echo '### 签名'
    echo '已用 Developer ID 签名，但这份产物没有公证凭据，首次打开会被 Gatekeeper 拦住。两种放行方式：右键 → 打开 → 再点打开；或执行 \`xattr -dr com.apple.quarantine /Applications/Sevqo.app\`。'
  fi
)

### 更新
设置里的「关于与更新」可手动检查；启动后也会后台检查一次。更新包带 minisign 签名，与 tauri.conf.json 里的公钥不符就不会安装。下载与检查会优先走本机代理，代理不通自动直连。

### 已知限制
- 只索引 POSIX 路径下的常规文件；外置卷休眠时监听会暂停。
- 视频定位精度受切片窗口限制（默认 16 秒窗口 / 12 秒步进）。
EOF
)"

if [[ "${DRY:-0}" == 1 ]]; then
  log "DRY=1：核对通过，不上传"
  ls -lh "$dmg" "$archive" "$archive.sig" "$up_dir/latest.json"
  exit 0
fi

log "发布 $repo 的 $tag"
files=("$dmg" "$archive" "$archive.sig" "$up_dir/latest.json")
if gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
  gh release upload "$tag" --repo "$repo" --clobber "${files[@]}"
  gh release edit "$tag" --repo "$repo" --title "$name $tag" --notes "$notes"
else
  gh release create "$tag" --repo "$repo" --title "$name $tag" --notes "$notes" --latest "${files[@]}"
fi

log "回读更新源"
url="https://github.com/$repo/releases/latest/download/latest.json"
got="$(curl -fsSL "$url" | jq -r '.version')"
[[ "$got" == "$version" ]] || fail "$url 返回版本 $got，期望 $version"
log "更新源可用：$url（$got）"
gh release view "$tag" --repo "$repo" --json tagName,url,assets --jq '{tag:.tagName,url:.url,assets:[.assets[].name]}'
