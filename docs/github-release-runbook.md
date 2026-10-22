# 把 macOS 发版交给 GitHub Actions

2026-10-15 在 `misswell/Sevqo` 上做完的一次切换整理成的清单，可直接交给其他 agent 照着做。
本文不含任何密码、私钥、P12 内容或 GitHub token；需要秘密的地方只写"从哪儿来、怎么交进去"。

## 0. 一条原则

**签名和公证所需的一切只存在于仓库 secrets，任何一台机器只要能推 tag 就能发版。**
如果发版依赖某台电脑上的钥匙串、某个 `.p8` 文件、某个未进仓库的脚本，那它就不是"在仓库发"，
只是"在那台机器发，顺便把产物传到 GitHub"。这两种模式别混着跑。

## 1. 先看这些文件和网址

仓库是公开的：<https://github.com/misswell/Sevqo>。下面路径都相对仓库根，行号以 `main` 当前内容为准。

### 发版链路上的文件

| 文件 | 谁在用 | 移植到别的项目要改什么 |
| --- | --- | --- |
| `.github/workflows/release.yml` | 整条云端流水线：`verify` / `probe-credentials` / `build-macos`（矩阵 aarch64 + x86_64）/ `release` | 改 `on.push.tags`、matrix 里的 `platform`、任务名；`:37-38` 的 `RELEASE_TAG`/`SOURCE_REF` 是两条触发路线共用变量的写法，照抄 |
| `.github/scripts/import_signing_identity.sh` | Runner 上建一次性钥匙串、导 P12、放行 `/usr/bin/codesign`、**真试签一个文件** | 身份名是从钥匙串 grep 出来的，不写死，一般不用改；改 `SIGNING_KEYCHAIN` 命名即可 |
| `.github/scripts/sign_notarize_macos.sh` | 签 `.app` → 公证并装订 `.app` → 从带票据的 `.app` 出 dmg → 签 dmg → 公证并装订 dmg → `spctl` | `:15-30` 决定走哪条公证路线（见 §2），`:57` 是 API key 分支；dmg 的窗口美化/命名规则在 `tools/make_dmg.sh` |
| `.github/scripts/pack_updater.sh` | **先** `stapler validate` 再 `tar` 成更新包，然后 `cargo tauri signer sign --app-version $(jq -r .version …)` | `--app-version` 读的那个配置路径必须和 §4 的守卫读同一个文件 |
| `apps/desktop/src-tauri/tauri.conf.json` | `:4` 版本号；`:49` 更新公钥；`:51` 更新清单地址 | 换项目要换 `pubkey`（新生成密钥对）和 `endpoints`（指向自己的 repo） |
| `RELEASE_NOTES.md` | 说明文字的唯一来源：本机脚本写进 `latest.json.notes`，CI 当 Release 正文开头 | 换项目照样只留这一份，别在 workflow 里另写模板 |
| `tools/release_macos.sh` / `tools/publish_release.sh` | 本机兜底路线：构建+签名+公证+装订，再用 `gh` 发布 | 只在云端不可用时用；`publish_release.sh` 从配置反推标签、发现同名 Release 就退出，不覆盖 |

### 版本号到底写在哪几处

只改标签不改配置，客户端会因为 minisign trusted comment 里的 `app-version` 对不上而**拒绝安装更新**。
这三处是源头，两个 lockfile 跟着动（v0.1.2 那次实际提交 `eb7c991` 改的就是这 6 个文件）：

```
Cargo.toml:6                                  version = "<x.y.z>"        ← workspace 版本
apps/desktop/src-tauri/Cargo.toml:3           version.workspace = true   ← 继承，不用改
apps/desktop/src-tauri/tauri.conf.json:4      "version": "<x.y.z>"       ← CI 守卫读的就是它
apps/desktop/package.json:4                   "version": "<x.y.z>"
Cargo.lock, apps/desktop/package-lock.json    跟着刷新：cargo update -w --offline
                                              && (cd apps/desktop && npm install --package-lock-only --no-audit --no-fund)
```

### 要在浏览器里打开的页面

| 用途 | 地址 |
| --- | --- |
| 填 secrets（§2 那批名字） | <https://github.com/misswell/Sevqo/settings/secrets/actions> |
| 填 `RELEASE_PROVIDER` 变量（§7） | <https://github.com/misswell/Sevqo/settings/variables/actions> |
| 看流水线 / 手动触发探针 | <https://github.com/misswell/Sevqo/actions/workflows/release.yml> |
| 应用内更新实际读的清单 | <https://github.com/misswell/Sevqo/releases/latest/download/latest.json> |
| 生成 App Store Connect API key（`.p8`） | <https://appstoreconnect.apple.com/access/integrations/api>；`.p8` 私钥只在下载那一刻显示一次，下下来直接 `base64` 喂给 `gh secret set`，别落进仓库 |
| App 专用密码（只有走 Apple ID 路线才需要） | 用户自己在 Apple ID 账户页 →「登录与安全」→「App 专用密码」生成，**由用户本人填进 GitHub，不发进聊天** |

命令行等价物（不用开网页）：

```bash
gh secret list   -R misswell/Sevqo      # 只列名字，读不回值
gh variable list -R misswell/Sevqo      # 看 RELEASE_PROVIDER
gh workflow run release.yml -R misswell/Sevqo -f tag=v9.9.9 -f probeCredentials=true
```

## 2. 需要配哪些 secrets

命名不必照抄，但**每一项都要能被 workflow 里的显式检查覆盖**——脚本必须"缺哪项就报哪项名"，
而不是静默跳过构建。Sevqo 用的是：

| 用途 | 名称 | 来源与处理 |
| --- | --- | --- |
| Developer ID 签名 | `APPLE_DEVELOPER_ID_P12_BASE64` | 本机已有身份导出为**只含该身份**的 P12，转 Base64，经标准输入交给 `gh secret set` |
| P12 保护口令 | `APPLE_DEVELOPER_ID_P12_PASSWORD` | 导出时新生成的随机口令，与 P12 成对写入；**重新生成 P12 必须两项一起更新** |
| 公证（路线 A，优先） | `APPLE_API_KEY_BASE64` + `APPLE_API_KEY_ID` + `APPLE_API_ISSUER` | App Store Connect API key（`.p8`） |
| 公证（路线 B，兜底） | `APPLE_ID` + `APPLE_TEAM_ID` + `APPLE_APP_SPECIFIC_PASSWORD` | Apple ID 与 App 专用密码；专用密码由用户自己在网页填，不进聊天 |
| 更新包签名 | `TAURI_SIGNING_PRIVATE_KEY` + `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | minisign 私钥；**公钥要提交进仓库配置**，换密钥两边同步 |

两条公证路线只需配一条。**先读自己 workflow 的分支顺序再决定要不要配第二条**：
`sign_notarize_macos.sh:15-30` 先算 `api_key_ready`（三项齐）再算 `apple_id_ready`，只有两条都不齐才报错——
所以 Sevqo 在三条 API key 齐全时永远走路线 A，路线 B 是死代码，此时让用户去生成 App 专用密码是白折腾。
反过来，只有 Apple ID 路线的项目里，App 专用密码就是必需项。别的项目结论照搬过来会错。

写 secrets 的纪律：值只经标准输入或管道交给 `gh secret set --repo …`；不进命令参数、不进聊天、
不进日志、不进仓库；不要 `set -x`。`gh` 会用仓库公钥加密后提交。

## 3. 顺序：先花十几秒验证凭据，再花几十分钟构建

Secret **名字存在不等于值能用**（可能填了占位符、可能已被吊销）。所以先做一个只验证凭据的入口，
再跑完整构建：

- Sevqo 的做法是 `workflow_dispatch` 上开一个 `probeCredentials` 输入（`.github/workflows/release.yml` 的 `probe-credentials` 任务），勾上时只跑：
  导入证书 → `security find-identity` 取到身份名 → **真签一个文件** → `notarytool history` 真问一次 →
  解一遍 minisign 私钥看格式。全程不构建、不发布。
- 注意 `history` 证明的是"这把凭据能读"，`submit` 才是"能提交公证"。两者用同一份凭据，
  但严格说 `submit` 要等第一次真公证才算证明。别把前者说成后者。

## 4. Runner 上必须真的做这五件事

缺一个就会发出"看起来成功、用户装不上"的包：

1. **装证书**：一次性钥匙串 + `security import` + `security set-key-partition-list -S apple-tool:,apple:,codesign:`。
   身份名不要写死在 workflow 里，从钥匙串里 grep 出来（换证书不用改配置）。
2. **真试签**：签一个无关文件并 `codesign --verify --strict`。只有证书列表里有名字是不够的。
3. **真提交公证并等结果**：`notarytool submit … --wait`。签名带 `--options runtime --timestamp`（hardened runtime）。
4. **顺序：先公证并装订 `.app` → 再从这份带票据的 `.app` 造 dmg → 最后才生成更新包**。
   装订发生在造镜像之后，镜像里那份副本就没有票据（v0.1.3 真实翻车点，见 §9）；
   更新包早于装订，走应用内更新装到的包里就没有公证票据。
5. **断言要能被这条缺陷弄红**：`xcrun stapler validate` 必须跑在**挂载后的包内 app** 上
   （`hdiutil attach` → `$mnt/Sevqo.app`），不能只对 dmg 自己跑。`spctl` 联网时对无票据的包也报 accepted，抓不到这条。

`spctl -a -t exec -vv` 和 `xcrun stapler validate` 都放进 CI 断言里，别只打印。

## 5. 版本一致性要有守卫

`latest.json` 的版本常常取自 **git 标签**，而 minisign 签名里的 `app-version` 取自 **应用配置文件**。
只改标签不改配置，客户端会因为签名里的版本对不上而**拒绝安装更新**——CI 全绿也照样坏。

本机发版时这个坑通常被脚本顺手挡了（`tools/publish_release.sh` 从配置反推标签）；换到云端就没人跑那个脚本了，
必须在 `verify` 任务里显式检查（`.github/workflows/release.yml:70`）：

```bash
conf="$(jq -r .version apps/desktop/src-tauri/tauri.conf.json)"
[ "v${conf}" = "${RELEASE_TAG}" ] || { echo "标签 ${RELEASE_TAG} 对不上配置的 ${conf}" >&2; exit 1; }
```

发布事件（推标签，或手动触发且勾了 `publish`）上强制；只有不发布的手动自检允许用任意命名标签，否则测试流水线自己被卡住。

客户端侧也开了 `plugins.updater.requireSignedVersion`：清单里的版本必须与更新包签名 trusted comment 里的 `version:` 一致。
`build-macos` 在打包后跑 `release_packages_verify_with_the_configured_pubkey -- --ignored`，同时验公钥和这个版本，
漏传 `--app-version` 的包会在 CI 里直接变红，而不是等用户安装时才被拒。

## 6. 版本说明只能有一个来源

Sevqo 的说明文字唯一来源是仓库根的 `RELEASE_NOTES.md`，本机脚本和 CI 都读它：
前者写进 `latest.json.notes`（更新卡片上唯一的文字），后者同时当 Release 正文开头。
CI 里**不要自带一套模板文案**——那样两条路线的说明会各说各话，而且更新卡片会突然只剩一行。
缺文件就失败，别退化成空字符串。

## 7. 一个变量决定谁出包

仓库变量 `RELEASE_PROVIDER`（去 <https://github.com/misswell/Sevqo/settings/variables/actions> 看当前值）：
`github` = 云端签名+公证+发布；`local` = 标签触发的 Actions 只跑验证，构建与发布任务跳过，由本机脚本发。
切换的代价是**别两边同时发**：同名 Release 会被拒绝或覆盖。

本机脚本降级为兜底（runner 排队、凭据轮换、断网），并把它依赖的本机前置写清楚：
Sevqo 的 Developer ID 不在登录钥匙串里，光 `security create-keychain` 不够，
必须**追加**进用户搜索列表并设默认，再放行 `/usr/bin/codesign`，跑完在 EXIT trap 里删除并还原。

## 8. 验收：不查文档，查产物

README 里写"已公证"不算验收。按这三步实测：

```bash
# 1) Apple 侧：这个账号的 history 混着其他 app，必须按名字过滤
xcrun notarytool history --key <.p8> --key-id <ID> --issuer <ISSUER> \
  | grep -i -B3 -A2 '<AppName>'          # 期望每次发布四条 Accepted：每架构各一份 .notarization.zip + 一份 dmg，且 zip 的 createdDate 早于 dmg。只剩 dmg 记录说明 .app 没提交过公证

# 2) 线上产物：拉下来跟本机产物逐字节比，并验票据
gh release download <tag> -R <owner>/<repo> --pattern '*.dmg' -D /tmp/x
shasum -a 256 /tmp/x/*.dmg               # 与本机 target/.../<App>_<tag>_macos_<arch>.dmg 一致
xcrun stapler validate /tmp/x/*.dmg

# 3) 包内：挂载后验签名链、hardened runtime、Gatekeeper；更新包同样查一遍
hdiutil attach -readonly -nobrowse -mountpoint /tmp/m /tmp/x/*.dmg
xcrun stapler validate /tmp/m/<App>.app        # ← 这条才是"票据真进得去镜像"的断言，别只对 dmg 跑
codesign --verify --deep --strict -vvv /tmp/m/<App>.app
codesign -dv --verbose=4 /tmp/m/<App>.app | grep -E 'Authority|Timestamp|flags'   # 要看到 0x10000(runtime)
spctl --assess --type open -vvv /tmp/m/<App>.app
tar -xzf <App>_<tag>_macos_<arch>.app.tar.gz -C /tmp/u && xcrun stapler validate /tmp/u/<App>.app
lipo -archs /tmp/u/<App>.app/Contents/MacOS/*   # 架构要和文件名里那个 arch 一致
```

从 GitHub 直连下载可能很慢，挂后台跑，别因此跳过第 2 步。
dry run 没有 Release 可拉，就验 Runner 上传的 artifact：`gh run download <run-id> -D /tmp/x`，
里面是 `.dmg` / `.app.tar.gz` / `.sig`，同样跑 `stapler validate` 和 `codesign -dv --verbose=4`；
`.sig` 是 base64 套 base64，解开后拿它的 minisign key id 跟配置里的 `pubkey` 对一遍，
再看 trusted comment 里的 `version:` 是不是配置文件那个版本——这两条对上，应用内更新才会认这个包。
构建目录里那个 `notarization.json` 常常是上一版残留，不能当证据。

## 9. 这次真踩到的坑

- **干净 runner 会暴露漏掉的步骤**：`build-macos` 只装了 Rust 工具链，没跑 `npm ci`，于是 `cargo tauri build` 的 beforeBuildCommand 找不到 `tsc`，两个架构一起 `exit 127`。本机永远撞不到——你的 `node_modules` 一直在。verify 任务装过依赖也不算，那是另一台 runner。
- **CI 自带一套说明文案**：切到云端发版后更新卡片会突然只剩一行，两条路线的话术还会分叉。已改成同读一个文件。
- **`--tags` 一次推一堆**：推标签即发布，且没有确认环节。批量推旧标签会连着发多个 Release。以后只单推 `git push origin v1.2.3`。
- **凭据"配了"≠"能用"≠"会走到"**：三件事要分开证明——名字在列表里、值在 runner 上认证通过、分支真的会选它。
- **部分失败别当全失败**：网络 EOF 可能只写进去几项。先 `gh secret list` 核名字，再单独补缺的那项。
- **未验证的架构照样被矩阵发出去**：矩阵出 `x86_64` 但从来没人真机打开过，而说明文字写着"Intel 未验证"——
  产物和文案自相矛盾。要么摘掉矩阵，要么别在文案里承诺支持。
- **别跳过只验证凭据的探针**：它十几秒，完整构建几十分钟。第一次上云就直接跑完整构建，等于用 40 分钟换一个 `tsc: command not found`。
- **dry run 别用真实版本号命名**：手动触发时 `RELEASE_TAG` 直接决定产物文件名，用 `v0.1.2` 自检就产出一堆
  `Sevqo_v0.1.2_macos_aarch64.dmg`，和线上那个 Release 的资产同名。这次就留下了实证：
  云端 dry run 的 aarch64 dmg 是 `c00e1f2d022e493e…`，线上已发布的 v0.1.2 同名 dmg 是 `2153c31ddf06c239…`，
  **文件名一模一样、字节完全不同**。隔几天回头看极易误当"已经发过了"。
  版本守卫在 `workflow_dispatch` 上是跳过的，所以自检尽管用 `v9.9.9-dryrun` 这类测试标签。
- **"已修"不等于"用户能装上"**：两个界面缺陷当天修完提交在 main，用户跑的还是前一天装的旧包，于是看起来等于没修，
  还搭上一句"你这一堆 bug 怎么测的"。修完就发一版让他装得到；发不了就先说清"这处在下一个版本里，你现在这个包没有"。
  验证也别只停在 mock 出来的界面状态——至少证明改动真的进了发出去的那个二进制（见 §10 最后一条）。
- **只公证 dmg，包里的 .app 是没票据的**：CI 那条路线原来是"造 dmg → 签 dmg → 公证 dmg → 给 app 和 dmg 都装订"，
  装订发生在 dmg 已经造完之后，于是镜像里那份副本永远追不上票据。`stapler validate` 对 dmg 本身是过的，
  看起来一切正常；挂上镜像再验包内那个 app 才露馅：`Sevqo.app does not have a ticket stapled to it.`。
  按 Apple 的设计，没票据的那份第一次打开要联网查 cdhash，离线或被代理挡住时就可能被判"已损坏"（这一条本机没法实测，
  属于人工验收；能实测的是下一条）。正确顺序照搬本机路线：**先公证并装订 .app，再从带票据的 .app 造 dmg**
  （`sign_notarize_macos.sh`）。验收也必须挂在挂载之后，而且要跑 `stapler validate`：
  `hdiutil attach -readonly -nobrowse -mountpoint … "$dmg"` → `xcrun stapler validate "$mnt/Sevqo.app"`。
  **别拿 `spctl` 当这条的判据**——这台机器联网时对 v0.1.3（无票据）和 v0.1.4（有票据）的包内 app 都报 `accepted`，
  在线查询会替票据补上上下文，只有 `stapler validate` 分得开。
- **下载被限速截断会伪装成签名失败**：`gh run download` / `gh release download` 并发或断流时能写出一个尺寸不对的
  `.app.tar.gz`，用它验 minisign 就报不匹配，看着像私钥或公钥配错了。先核 sha256 与文件大小再核签名；
  重下时用单独的 `gh run download`（别并发写同一目录）、必要时 `--retry`，截断的那份直接删掉重下。

## 10. 当前状态

- `RELEASE_PROVIDER=github`。**v0.1.3 是第一个完全由云端发出的版本**（run [`38762869057`](https://github.com/misswell/Sevqo/actions/runs/38762869057)，2026-10-15T15:58:47Z 发布）：
  verify → 双架构 build → Developer ID 签名 → Apple 公证 → 装订 → minisign 更新包 → `latest.json` → Release，全绿。
  此前只在本机跑过的 `release` 任务（ubuntu runner 拼清单 + 上传资产）也在这次第一次实测通过。
- 宣布"已发布"要看这四条，不看 CI 的绿灯：
  1. `releases/latest/download/latest.json` 返回新版本（这是产品自己读的更新通道），两个平台的 `signature` 非空、`notes` 非空；
  2. Apple 侧按名字过滤能看到本版**四条**记录都 `Accepted`——每个架构一条 `.notarization.zip`（`.app` 本体）加一条 dmg，
     且 zip 的 `createdDate` 早于 dmg。只剩两条 dmg 就等于 `.app` 从没提交过公证；
  3. Release 资产数量与命名齐（`.dmg` / `.app.tar.gz` / `.sig` × 两架构 + `latest.json`），本地下来的字节数与
     `gh release view --json assets` 里的 `size` 逐个对上，对不上先怀疑截断；
  4. 线上产物拉到本机复核（§8 第 3 步）：**挂载后对包内 app 跑 `stapler validate`**、`flags=0x10000(runtime)`、
     签名链、`lipo -archs` 与包名一致、`.sig` 的 key id 对得上仓库公钥、trusted comment 的 `version:` 等于配置版本，
     再用应用自己的 `release_packages_verify_with_the_configured_pubkey` 验一遍更新包。
- **v0.1.4 才是当前该装的那一版**（run [`38769213489`](https://github.com/misswell/Sevqo/actions/runs/38769213489)，2026-10-15T16:42:53Z 发布）。
  它只改发布脚本，前端与引擎相对 v0.1.3 零改动（`git diff --stat v0.1.3..v0.1.4 -- apps/desktop/src crates` 为空），
  修的是"镜像里那份 `.app` 没有公证票据"。四个包本机逐字节复核过：
  aarch64 dmg `786dd9d812cf51f5…` / 更新包 `676a566c7abab79c…`，x86_64 dmg `a90fa9d15763119e…` / 更新包 `04b7528573a937e6…`；
  **挂载 dmg 后对包内 `Sevqo.app` 跑 `stapler validate` 两侧都 `The validate action worked!`**（v0.1.3 这里是 `does not have a ticket`），
  `lipo -archs` 分别 `arm64` / `x86_64`，`Info.plist` 都是 0.1.4，签名链到 Developer ID Application → Certification Authority → Apple Root CA，
  `flags=0x10000(runtime)`，两份 `.sig` key id 均 `CEE9A3B0F135A93F` 等于仓库公钥、trusted comment `version:0.1.4` 等于配置版本，
  `SEVQO_UPDATE_DIR=/tmp/sevqo-v014 cargo test -p sevqo-desktop --lib -- --ignored release_packages_verify` 一次过两个平台。
  Apple 侧本版四条记录全 `Accepted`，且两条 `.notarization.zip`（16:41:15 / 16:41:34）**早于**两条 dmg（16:41:49 / 16:42:08）；
  v0.1.3 只有两条 dmg 记录——`.app` 本体从没提交给 Apple，这就是包内没票据的根因。
- **"票据在不在"只有两条路能证明，`spctl` 不在此列**：这台机器联网时，对 v0.1.3（无票据）和 v0.1.4（有票据）的包内 app
  加 `com.apple.quarantine` 后跑 `spctl --assess` 两边都是 `accepted`——有网络就能在线查到 cdhash。
  能分辨的只有 `xcrun stapler validate <挂载点>/Sevqo.app`，或者真断网点一次（后者归人工验收，见 README 清单）。
  别把 CI 日志里的 `accepted` 当成包里有票据。
- v0.1.3 的复核结果（保留，因为它是那条坑的物证）：aarch64 dmg `8806f502c1a2ecc0…`、x86_64 dmg `dbc5c52cfd8d6d7b…`、
  更新包 `84d9050112c46c34…` / `6a2b400d06e4341d…`。dmg 自身和更新包里的 app 都带票据，**唯独镜像里那份 `.app` 两份都报
  `does not have a ticket stapled to it`**——`spctl` 当时也报 accepted，所以这个缺陷只在挂载后验包内才露出来。
- **怎么证明"修的那处真进了发出去的包"**：前端资源是编译进二进制的，包里没有 `.js` 文件，
  但 `strings <App>.app/Contents/MacOS/<bin> | grep -oE 'index-[A-Za-z0-9_-]{8}\.(js|css)'` 能把 Vite 的内容哈希文件名抠出来，
  与本机在同一提交上 `npm run build` 产出的名字对比——哈希由内容决定，名字对上就等于内容对上。
  v0.1.4 的包内是 `index-CDMap3bl.js` / `index-BMIx_VAr.css`，与本机一致；主二进制叫 `sevqo-desktop`（crate 名），不是 `Sevqo`。
  这比"我看日志里构建成功了"硬得多，也是回答"你到底修没修"的最省事办法。
- 仍然没证明的：`x86_64` 包被公证过、也被本机验过票据与架构，但**没有任何人在 Intel 真机上打开过**，
  所以 `RELEASE_NOTES.md` 里只写"Intel 包尚未完成真机验证"，不写"支持 Intel"。
