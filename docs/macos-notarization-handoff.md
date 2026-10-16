# 给 macOS 应用做完公证（可直接转交给其他 agent）

这份文档只回答一个问题：**怎么把一个 `.app` 做到"别人拖进应用程序、断网也能双击打开"**。
不含任何密码、私钥、P12 内容或 token；需要秘密的地方只写"从哪儿来、怎么交进去"。

它**不**回答"功能对不对"。公证只解决能不能装，界面与业务正确性要另行人工验收。

来源：2026-10-15 在 `misswell/Sevqo` 上把签名+公证整条搬到 GitHub Actions 时实测整理。
文中标 **[实测]** 的是那次真跑出来的，标 **[Apple 文档]** 的是 Apple 的既定行为、我们没在断网环境复现过。

---

## 1. 前置条件（少一条，公证一定做不成）

| 需要 | 怎么确认 |
| --- | --- |
| 付费 Apple Developer Program 成员资格，且你的 Apple ID 在那个 Team 里 | 能登录 <https://developer.apple.com/account> 并看到该 Team |
| 一张 **Developer ID Application** 证书 + 它的私钥（不是 Apple Development，也不是 App Store 用的 Apple Distribution） | `security find-identity -v -p codesigning` 输出里有一行 `Developer ID Application: 你的名字 (TEAMID)` |
| 公证凭据，二选一：<br>A. App Store Connect API key（`.p8` + Key ID + Issuer UUID）<br>B. Apple ID + Team ID + App 专用密码 | A 在 <https://appstoreconnect.apple.com/access/integrations/api> 生成，角色要能访问该 Team 的资源；`.p8` 私钥只在下载那一刻显示一次 |
| 应用能构建出**自包含**的 `.app`（内嵌的二进制、framework、sidecar 都在包里） | 公证是对提交的整个包做检查，跑不起来的东西会被拒 |

只有 Apple Development 证书就能停在这儿了——公证不认它，报 `not signed with a valid Developer ID certificate`。

**别只凭一条命令就下结论说"没有 Developer ID 证书"**：`security find-identity -v -p codesigning` 只查登录钥匙串，
而 Developer ID 常常装在**另一个钥匙串文件**里（自建签名钥匙串、或干脆只在 `.p12` 里）。
判断依据是"有没有一张 Developer ID 证书 + 它的私钥"，不是"登录钥匙串里能不能列出来"——
走 CI 的话只要 `.p12` 就够了，本机钥匙串装着与否根本不影响。先三处都找一遍：

```bash
security find-identity -v -p codesigning                          # 只看到 Apple Development / Distribution 别急着停
ls ~/Library/Keychains/*.keychain-db 2>/dev/null                  # 逐个再查一遍
security find-identity -v -p codesigning ~/Library/Keychains/<那个>.keychain-db
ls ~/Desktop/*signing*/*.p12 ~/Desktop/*signing*/*.p12password 2>/dev/null   # 有 p12 就等于有证书+私钥
```

### 上面这些，哪一步必须用户、哪一步 agent 自己就能做完

| 步骤 | 谁能做 |
| --- | --- |
| 登录 Apple 账号、生成 API key、导出 P12、生成 App 专用密码 | **只有用户本人**（这是账号登录态，agent 不该代持） |
| 在本机找到现成凭据材料，用管道写进仓库 secrets | **agent 自己就能做完**，不需要用户动手 |
| 改 workflow、跑探针、构建、签名、公证、复核产物 | agent |

所以"把方案发给 agent，他自己能不能做完"取决于**这台机器上有没有现成材料**。
**先找，别先问用户**：

```bash
ls ~/Desktop/*signing*/*.p12 ~/Desktop/*signing*/.p12password 2>/dev/null
ls ~/.appstoreconnect/private_keys/*.p8 2>/dev/null
security find-identity -v -p codesigning        # 看有没有 Developer ID Application 那一行
```

三样都在，就照 §4 的命令自己 `gh secret set` 完，用户一步都不用动（Sevqo 那 9 项就是这么进去的）。
缺哪样就**只要那一样**，而且要的是"文件在哪"或"你在哪个网页点哪一下"，不是把值贴进聊天。
一上来就让用户手敲 `gh secret set` 而不去本机找材料，是把可自动化的步骤推回给人。

---

## 2. 本机最小闭环：从 `.app` 到"包里有票据"

不需要 Tauri、不需要 GitHub Actions，六步跑通就说明你这套公证是好的。
**顺序不能变**（§3 解释为什么）。

```bash
APP=/绝对路径/MyApp.app
IDENT='Developer ID Application: 你的名字 (TEAMID)'   # 从 find-identity 那行原样复制
P8=/绝对路径/AuthKey_XXXXXXXXXX.p8
KEYID=XXXXXXXXXX
ISSUER=<Issuer UUID>

# ① 由内向外签：所有 Mach-O（含内嵌可执行文件、dylib、framework、打包进去的 python 等）逐个签
while IFS= read -r -d '' f; do
  file "$f" | grep -q 'Mach-O' && codesign --force --options runtime --timestamp \
    --sign "$IDENT" "$f"
done < <(find "$APP" -print0)
codesign --verify --deep --strict --verbose=2 "$APP"        # 这一步红了就别往下走

# ② 打成公证用的 zip：--keepParent 是关键，少了它 zip 里没有 .app 这一层
ditto -c -k --keepParent "$APP" /tmp/MyApp.zip

# ③ 提交并等结果（--wait 会一直等到 Accepted / Invalid）
xcrun notarytool submit /tmp/MyApp.zip --key "$P8" --key-id "$KEYID" --issuer "$ISSUER" --wait

# ④ 装订：票据钉在 .app 目录树上，不是钉在 zip 上
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"

# ⑤ 从"已经带票据的那份 .app"造 dmg，再对 dmg 走一遍公证+装订
hdiutil create -volname MyApp -srcfolder "$APP" -ov -format UDZO /tmp/MyApp.dmg
codesign --force --timestamp --sign "$IDENT" /tmp/MyApp.dmg
xcrun notarytool submit /tmp/MyApp.dmg --key "$P8" --key-id "$KEYID" --issuer "$ISSUER" --wait
xcrun stapler staple /tmp/MyApp.dmg

# ⑥ 唯一能证明顺序没错的断言：挂起来，验包内那份
hdiutil attach -readonly -nobrowse -mountpoint /tmp/m /tmp/MyApp.dmg
xcrun stapler validate /tmp/m/MyApp.app      # 期望 The validate action worked!
hdiutil detach /tmp/m
```

要点：

- `--options runtime`（hardened runtime）和 `--timestamp` 两个都不能省，缺哪个 Apple 就退回来告诉你缺哪个。
- **票据钉不进 zip**：`stapler` 只支持目录树和 dmg 这类能放扩展属性的载体。所以正确做法是"装订 `.app` → 用装订过的 `.app` 造 dmg"，
  而不是"造 dmg → 公证 dmg → 再回头装订 `.app`"。
- 如果应用需要 entitlements（网络、文件访问、JIT 等），每个 `codesign` 都带上同一份 `--entitlements`，
  且 `com.apple.security.get-task-allow` 必须是 `false`（Debug 构建默认是 true，会被拒）。

---

## 3. 唯一的真坑：装订发生在造镜像之后，镜像里就没有票据 **[实测]**

我们在 GitHub Actions 上就这么翻过车（Sevqo v0.1.3，两个架构都中了）：

- 当时的顺序是"造 dmg → 签 dmg → 公证 dmg → 最后给 `.app` 和 dmg 都 staple"。staple 时 dmg 已经造完了，
  **镜像里那份副本永远追不上票据**。
- 所有"看起来正常"的检查都过了：dmg 自己 `stapler validate` 通过、Apple 侧 dmg `Accepted`、CI 全绿。
- 露馅只有一条路：`hdiutil attach` 挂起来，对 `$mnt/MyApp.app` 跑 `stapler validate` →
  `MyApp.app does not have a ticket stapled to it.`
- **`spctl` 抓不到这条**：同一台联网机器上，它对无票据的包和有票据的包都报 `accepted`——有网络就能在线查到 cdhash，
  在线查询替票据补上了上下文。所以别拿 `spctl` 当这条的判据。
- 后果：走 dmg 首装的人，在离线或验证服务不通时第一次打开会被 Gatekeeper 拦下 **[Apple 文档]**
  （具体文案随系统版本不同，我们没实测过哪一种）。走"应用内更新"的人不受影响，因为更新归档是在装订之后才打的。

**不变式**：`公证 .app → 装订 .app → 用这份 .app 造 dmg → 公证 dmg → 装订 dmg → 挂载复核包内`。
任何一步把"造 dmg"放到"装订 .app"前面，都是错的。

---

## 4. 凭据怎么进 GitHub（如果要在 CI 上做）

secret 名字随你起，但要能被 workflow 里**显式的缺项检查**覆盖——缺哪项就报哪项名，别静默跳过。
Sevqo 用的是这套名字：

| 用途 | 名称 |
| --- | --- |
| 签名 | `APPLE_DEVELOPER_ID_P12_BASE64` + `APPLE_DEVELOPER_ID_P12_PASSWORD` |
| 公证路线 A（优先） | `APPLE_API_KEY_BASE64` + `APPLE_API_KEY_ID` + `APPLE_API_ISSUER` |
| 公证路线 B（兜底） | `APPLE_ID` + `APPLE_TEAM_ID` + `APPLE_APP_SPECIFIC_PASSWORD` |

纪律：值只经标准输入或管道交给 `gh secret set`；不进命令参数、不进聊天、不进日志、不进仓库；不要 `set -x`。

```bash
base64 -i AuthKey_XXXXXXXXXX.p8 | gh secret set APPLE_API_KEY_BASE64 -R <owner>/<repo>
gh secret set APPLE_API_KEY_ID      -R <owner>/<repo>   # 回车后粘贴，别写在参数里
gh secret set APPLE_API_ISSUER      -R <owner>/<repo>
gh secret list -R <owner>/<repo>                        # 只列名字和更新时间，读不回值
```

**两条公证路线只需配一条，但要先读自己脚本的分支顺序**：Sevqo 的脚本先算"API key 三项齐不齐"，
齐了永远走 A，只有 A 不齐才落到 B——这时 B 是死代码，让用户去生成 App 专用密码纯属白折腾。
反过来，只配了 Apple ID 路线的项目，App 专用密码就是必需项。**别的项目的结论照搬过来会错。**

`gh secret list` 里有名字 **不等于值能用**（可能是占位符、可能已吊销）。

---

## 5. CI 上必须先跑十几秒的探针，再跑几十分钟构建

第一次把公证搬到云上，别直接跑完整构建——那等于用 40 分钟换一个"证书没导进来"。
做一个只验证凭据的 `workflow_dispatch` 入口，它应该依次做四件事，**任何一步红就报出缺的键名**：

1. 一次性钥匙串 + `security import` P12 + `security set-key-partition-list -S apple-tool:,apple:,codesign:`
   （只 `create-keychain` 不够，`codesign` 会报 `no identity found`）；
2. **真签一个无关文件**并 `codesign --verify --strict`——身份列表里有名字不算证明；
3. `xcrun notarytool history …` 真问一次 Apple（证明这把凭据能读）；
4. 解一遍更新包私钥（如果这个项目有应用内更新）。

注意 `history` 证明的是"这把凭据能读"，`submit` 才是"能提交公证"，别把前者说成后者。

Sevqo 那次探针红的样子（另一个仓库 `misswell/Vistile`，run `38774694133`）：
`import_signing_identity.sh: line 6: DEVELOPER_ID_P12_BASE64: 缺少 APPLE_DEVELOPER_ID_P12_BASE64`
——这就是"缺哪项报哪项名"该有的样子，十几秒就定位到。

CI 里另外四件必做（缺一个就会发出"看起来成功、用户装不上"的包）：装证书、真试签、
`notarytool submit --wait` 并等结果、**按 §3 的不变式排序并在挂载后断言**。

---

## 6. 公证被拒时先查这张表

| Apple 回你的字样 | 真正的原因 | 怎么办 |
| --- | --- | --- |
| `not signed with a valid Developer ID certificate` | 用了 Apple Development / Apple Distribution，或 p12 里根本没有 Developer ID Application | `security find-identity -v -p codesigning` 核对身份名前缀 |
| `does not include a secure timestamp` | `codesign` 少了 `--timestamp`；或时间戳服务被代理挡住 | 补上参数；确认能访问 `timestamp.apple.com` |
| `does not have the hardened runtime enabled` | 少了 `--options runtime`，**或嵌套的那份漏签** | 按 §2 ① 从内向外逐个签 |
| `Invalid File Format` / zip 里找不到 app | `ditto` 漏了 `--keepParent`，或用 `zip -r` 压了目录内容而不是包本身 | 用 `ditto -c -k --keepParent` |
| `not all code has been signed` / nested code 相关 | 内嵌 framework、helper、`.xpc`、打包进去的解释器/二进制没签 | 同 §2 ①；`codesign --verify --deep --strict` 会指出是哪一个 |
| `get-task-allow` 相关 | Debug 构建默认带这个 entitlement | 用 Release 构建，或显式置 `false` |
| `Getting API information from Apple Services Failed` / 403 | `.p8` / Key ID / Issuer 三者不配对，key 被吊销，或 base64 里混进了换行 | 三条一起重填；`base64 -D` 后 `head -1` 应是 `-----BEGIN PRIVATE KEY-----` |
| 429 / `unexpected error`，稍后再试 | Apple 侧限流或抖动 | 隔几分钟重试，**别当成配置错** |
| `does not have a ticket stapled to it` **[实测]** | staple 的对象不是最终分发的那一份（顺序错），或试图把票据钉到 zip 上 | 照 §3 的不变式重排 |

---

## 7. 验收：三条"缺陷存在时会红"的断言

README 里写"已公证"不算验收。

1. **挂载后验包内**：`xcrun stapler validate /tmp/m/MyApp.app` → `The validate action worked!`。
   这条能红，是 §3 那个坑的唯一自动化探针。
2. **Apple 侧条数与顺序**：`xcrun notarytool history …` 按自己的 app 名过滤（账号下往往混着别的项目）。
   一次正确的发布该有 **两条**：`.app` 的 `*.notarization.zip` 和 dmg，且 **zip 的 `createdDate` 早于 dmg**。
   只剩 dmg 一条 = `.app` 本体从没提交公证。
3. **断网双击**：把 dmg 拖进「应用程序」，断网，双击。这条只能人做。

**不要用 `spctl` 当第 1 条的替代**（联网时它对两种包都报 accepted）。也别把 CI 日志里的 `Accepted` 时间戳
当成"包里有票据"——顺序只有产物能证明。

---

## 8. 一次成功长什么样（Sevqo v0.1.4，run `38769213489`）

```
Apple notarytool history（按名字过滤，四条全 Accepted）
  16:41:15  Sevqo_v0.1.4_macos_x86_64.notarization.zip   Accepted   ← .app 本体
  16:41:34  Sevqo_v0.1.4_macos_aarch64.notarization.zip  Accepted   ← .app 本体
  16:41:49  Sevqo_v0.1.4_macos_x86_64.dmg                Accepted
  16:42:08  Sevqo_v0.1.4_macos_aarch64.dmg               Accepted
                                                              zip 全部早于 dmg = 顺序对
本机复核（两个架构）
  挂载 dmg 后包内 Sevqo.app     The validate action worked!
  dmg 自身 / 更新包解出的 app   The validate action worked!
  lipo -archs                   arm64 / x86_64，与包名一致
  codesign -dv --verbose=4      Developer ID Application → Certification Authority → Apple Root CA
                                flags=0x10000(runtime)
```

对照 v0.1.3（坏的那版）：Apple 侧**只有两条 dmg 记录**，包内 app 报 `does not have a ticket stapled to it`，
而 CI 日志、`spctl`、dmg 自身的 `stapler validate` 全部正常。

---

## 9. 别照抄的部分

从 Sevqo 搬这套时，下面这些是**它项目特有的**，换仓库要各自重定：

- **secret 的值永远不复制**，只复制名字；每个项目用自己的证书与凭据。
- 文件路径与配置位置：`apps/desktop/src-tauri/…`、`tauri.conf.json`、`Cargo.toml:6`、`apps/desktop/package.json:4`
  （Tauri 项目才用得上；非 Tauri 项目版本号只有一处的，守卫照写但路径要换）。
- 仓库变量 `RELEASE_PROVIDER`（Sevqo 用来在"云端发"和"本机发"之间切换的开关）。
- minisign 更新包那一整块（`TAURI_SIGNING_PRIVATE_KEY*`、`latest.json`、`pubkey`/`endpoints`）——
  **不做应用内更新就不需要**；要用的话公钥必须提交进仓库配置，且更新归档必须在装订**之后**才打。
- Team ID、证书身份名：从 `security find-identity` 现读，别硬编码进 workflow。
