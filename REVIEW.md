# Sevqo 代码审查与修复

审查日期：2026-10-14。依据用户提供的 Agent 记录与当前本地源码。

## 判断

这是一个具备实际推理、索引和检索链路的桌面原型。Rust 引擎、SQLite、Python sidecar 与 Tauri 前端的职责划分较清楚，也已有真实模型评测和冒烟测试。但此前的验证主要覆盖引擎，遗漏了桌面任务生命周期、媒体文件名检索、筛选与增量更新等路径，不能据此判定完整应用已完成。

## 已修复的问题

| 优先级 | 原问题与触发条件 | 本轮修复 |
| --- | --- | --- |
| P1 | 桌面全量和增量索引处理完文件后，仍持有 Indexer 的进度发送端；`join()` 永远等待转发线程退出，任务一直占用运行标志。 | 在等待转发线程前释放 Indexer 和所有发送端；任务运行标志用 RAII guard 管理，错误和线程启动失败也会释放。 |
| P1 | 推理请求失败或批次取消后，文件停在 `indexing`，本次运行无法再次取到它；连续错误还可能被报告为成功结束。 | 未完成的文件自动返回 `pending`；连续推理失败返回错误；向量写入并刷新后才发布 Done 状态。 |
| P1 | 文本模式扫描只标记文档/代码，却对整个目录执行清理，会删除已有媒体索引。遍历错误也可能被当成文件删除。 | 扫描标记所有已启用类型，处理模式仅决定本轮处理哪些类型；扫描不完整时保留未见记录。 |
| P1 | 图片、视频和无正文文档没有 FTS 文件名记录，输入准确文件名也可能搜不到。 | Done 时补元数据 FTS；打开旧索引时补缺失记录，保留现有正文和向量。 |
| P1 | 停用目录仅停止监听，关键词、语义及最近文件仍可返回该目录内容，待处理队列也未排除它。 | 所有检索通道与工作队列统一限制为已启用目录。 |
| P1 | 语义候选未应用目录、大小和时间筛选；文档和代码共用 chunk 类型导致类型筛选混淆。空输入与媒体查询在前端还会丢掉筛选。 | 在语义打分前应用完整文件筛选；空输入和媒体查询传递同一筛选条件。 |
| P1 | 监听器收到重命名/目录变动的 NeedsRescan 却不扫描；索引忙时丢掉 dirty 状态；一轮 64 个文件后不继续；排除目录规则也未用于监听。 | 暂存变更，空闲后统一处理；执行必要重扫；分批排空；扫描与监听共用排除规则。 |
| P1 | 保存 Python 路径、线程数或维度只修改设置 JSON，仍使用旧 Client；首启配置解释器后无法立即使用。 | 相关配置变化时重建推理 Client 并刷新状态；维度变更保护现有向量；运行中的索引/维护操作互斥。 |
| P1 | 视频采样允许约 0.6 秒间隔，却按采样数量提前退出，默认 16 秒窗口只覆盖前约 10 秒；非零 start 的隐式结束时间也计算错误。 | 根据时间戳按目标 fps 采样，遍历到窗口结束后统一限帧；隐式 end 使用绝对时间。 |
| P1 | 压缩向量原地截断文件，与正在读取的 mmap 冲突，可触发 SIGBUS；视频重建还会删除向量映射并复用尾部行号。 | 压缩写到临时文件后替换；应用内维护与搜索互斥；数据库重映射使用事务；旧视频向量保留墓碑。 |
| P1 | 视频 Range 的大后缀请求不受 8 MiB 限制，可能一次分配整部视频；倒序范围可能触发长度下溢。 | 后缀与普通请求统一限流，拒绝零长度/倒序/非法结束值，并使用饱和加法。 |
| P1 | 手动模型校验会删除校验失败的权重，包含用户通过环境变量指定的目录；同尺寸损坏文件又会被下载器当缓存跳过。 | 校验只读并保留文件；校验失败后清除就绪状态；下载时核对已有记录的校验和，成功下载后替换损坏副本。 |
| P2 | 默认自动线程数 0 被设置表单拒绝；轮询每隔数秒用新对象覆盖未保存的草稿；模型仓库字段可编辑但实现固定使用一个仓库。 | 接受 0；草稿只在打开设置时初始化；固定仓库字段只读，后端拒绝不支持的仓库。 |
| P2 | 关闭图片/视频后，自动索引永远走文本模式，文档无法获得语义向量；模型到齐后旧文本记录也不自动补向量。 | 模型可用就执行 Full 模式；完整索引前将缺失向量的 Done 文件重新排队。 |
| P2 | 拖入媒体后，之前排队的文本防抖查询仍会执行并覆盖媒体结果。 | 主窗口与悬浮框按当前查询意图决定是否执行延迟文本查询。 |

## 验证

新增 20 项回归测试。最终验证通过，完整输出保存在 [.research/review-2026-10-14-xu7bxik8](/Users/guofeng/Code/solo/Sevqo/.research/review-2026-10-14-xu7bxik8/summary.txt)。

| 验证 | 结果 |
| --- | --- |
| Rust 引擎 `cargo test -p sevqo-engine --lib` | 92 通过，0 失败 |
| 桌面壳 `cargo test -p sevqo-desktop --release` | 11 通过，0 失败，1 忽略；主程序及文档测试退出 0 |
| 前端 `npm run typecheck` 与 `npm run build` | 通过；使用 bundled Node v24.19.0，Vite 成功构建 55 个模块 |
| Python 视频采样回归 | 4 通过，0 失败 |
| 真实模型端到端 smoke | 15 通过，0 失败，`SMOKE OK`；MPS/bfloat16/512 维，17 个素材、21 个向量、3 个视频切片 |
| 补丁格式 `git diff --check` | 通过 |

被忽略的测试需要正式发布脚本生成的签名更新包，本轮没有把它计入通过项。第一次桌面文档测试遇到共享 target 的 rlib 缺失，停止并发编译后完整复验通过。系统默认 Node 14 不满足项目要求，其 Vite 错误即使退出码为 0 也不算成功；使用 bundled Node 后确认实际生成了 HTML、CSS 和 JS 产物。

## 尚未完成的产品范围

- 打包版仍依赖用户自行配置装有 torch 等依赖的 Python；内嵌 sidecar 源码并不等于独立运行时。
- 移动端尚未实现。CPython sidecar 无法直接作为 iOS 的交付方案。
- 本轮没有完成 Windows/Linux 真机验证、十万级素材压力测试或打包后 GUI 全流程验收。
- Sevqo v0.1.2 已于 2026-10-14 21:27（北京时间）发布到 [GitHub Release](https://github.com/misswell/Sevqo/releases/tag/v0.1.2)。tag 指向发布提交；GitHub Actions 前端和 Rust 验证成功，本机负责签名、公证、打包与发布。
- 2026-10-15 起发布路径切到 GitHub Actions：仓库变量 `RELEASE_PROVIDER=github`，签名、公证、装订、更新包与 Release 全在 runner 上完成，推 `v*` 标签即发版，不再依赖本机的钥匙串；本机两个脚本降级为离线兜底。CI 的 `latest.json.notes` 与 Release 正文改读 `RELEASE_NOTES.md`，与本机路线同一个来源。
- Sevqo v0.1.3 已于 2026-10-15 23:58（北京时间）发布到 [GitHub Release](https://github.com/misswell/Sevqo/releases/tag/v0.1.3)，是第一个完全由云端发出的版本（run `38762869057`：双架构构建 → Developer ID 签名 → Apple 公证 → 装订 → minisign 更新包 → `latest.json` → Release）。Apple 侧两条 `Sevqo_v0.1.3_macos_*.dmg` 都 `Accepted`；`releases/latest/download/latest.json` 回读为 `0.1.3`、两个平台各带签名、`notes` 非空。本机复核：dmg 自身与更新包解出的 `Sevqo.app` 带票据，`flags=0x10000(runtime)` + Developer ID 链。**但这一版有缺陷**：挂载 dmg 后验包内那份 `Sevqo.app`，两个架构都报 `does not have a ticket stapled to it`——CI 先造镜像、后给 `.app` 装订，镜像里的副本追不上票据；Apple 侧也只收过两条 dmg，`.app` 本体从没提交公证。走应用内更新的用户不受影响，走 dmg 首装的有离线风险。
- Sevqo v0.1.4 已于 2026-10-16 00:42（北京时间）发布到 [GitHub Release](https://github.com/misswell/Sevqo/releases/tag/v0.1.4)（run `38769213489`），修的就是上面那条：`.github/scripts/sign_notarize_macos.sh` 改成先公证并装订 `.app`、再从带票据的 `.app` 造 dmg，并在挂载后复核包内票据；本机路线 `tools/release_macos.sh` 补上同一断言。前端与引擎相对 v0.1.3 零改动。复核四个包全部通过：挂载 dmg 后包内 `Sevqo.app` 两侧都 `The validate action worked!`，`lipo -archs` 与包名一致，`flags=0x10000(runtime)` + Developer ID 链，`.sig` key id `CEE9A3B0F135A93F` 对得上仓库公钥、trusted comment `version:0.1.4`，`release_packages_verify_with_the_configured_pubkey` 一次过两平台；Apple 侧本版四条记录（2 个 `.notarization.zip` + 2 个 dmg）全 `Accepted` 且 zip 早于 dmg。同一台联网机器上 `spctl` 对无票据的 v0.1.3 包内 app 也报 `accepted`，所以这条缺陷只能靠 `stapler validate` 或断网双击发现——已作为人工验收项写进 README。Intel 真机仍未验证。
- Sevqo v0.1.5 已于 2026-10-22 08:47（北京时间）发布到 [GitHub Release](https://github.com/misswell/Sevqo/releases/tag/v0.1.5)（run `39699807513`，提交 `26ac31b`）。本版是整体代码审查后的修复版：不可用的根目录不再拖死整轮扫描与监听重试；退出不再等待在途嵌入请求；单文件上限的下限收到 1MB；被丢弃的向量会记录原因；前端修掉清空/ Esc 后的防抖回填、暂停态按钮、设置保存失败不可见与 `alive` 引用不重置。更新通道启用 `requireSignedVersion`，并在每个构建矩阵任务里用 `release_packages_verify_with_the_configured_pubkey` 断言更新包能用仓库公钥验签、签名内版本等于 `tauri.conf.json`；手动 `workflow_dispatch` 勾选 publish 也必须通过标签版本检查。复核 7 个产物：两个 dmg 挂载后包内 `Sevqo.app` 与两个更新归档解出的 `.app` 都 `The validate action worked!`，`lipo -archs` 分别为 arm64 / x86_64，`CFBundleShortVersionString` 为 `0.1.5`，`flags=0x10000(runtime)` + Developer ID 链；`.sig` key id `CEE9A3B0F135A93F`、trusted comment `version:0.1.5`，两个 `.sig` 与 `latest.json.platforms[*].signature` 逐字一致；本机跑同一 ignored 测试通过；CI 两个构建任务该步骤均 `1 passed`。Apple 侧本版四条记录（2 个 `.notarization.zip` + 2 个 dmg）全 `Accepted` 且 zip 早于 dmg。CI 日志里的 Vite 产物 `index-CF3MEWzm.js` / `index-BMIx_VAr.css` 与本地构建一致。Intel 真机仍未验证。

## 2026-10-25 复审：v0.1.5 之后的存量缺陷

审查日期：2026-10-25。通读引擎、桌面壳、前端与 Python sidecar 全部源码，聚焦 v0.1.2–v0.1.5 未覆盖的逻辑与操作漏洞。

### 本轮修复

| 优先级 | 原问题与触发条件 | 本轮修复 |
| --- | --- | --- |
| P1 | `invalidate_stale_rows` 定义后从未被调用：嵌入失败只预留了向量行、字节从未落盘，这些行 alive=1 一直被当作活行。后果有二：`compact` 把它们拷贝成零向量，永远收不回；当向量文件尚未建立（首个文件就失败）时，`Store::compact` 读到越界行直接报错，**压缩功能整体不可用**。 | `compact` 先调用 `invalidate_stale_rows` 再计算活行映射；新增两条回归钉住「失败预留行被回收」与「无字节时压缩不报错」。 |
| P1 | 停用再启用目录（或对既有目录重复 `add_root`）把 done 文件全部重置 pending，整个目录**全部重新嵌入**——即使 size/mtime 未变、向量仍有效；几千张图的目录一次误操作就是数小时 GPU 时间。 | 启用目录时：done 且仍有活向量的文件保持 done；done 无向量的置 stale（与 `mark_missing_vectors` 同一套补齐语义）；failed 置 stale 重试并清旧错误。`add_root` 冲突分支不再动文件状态。 |
| P1 | 添加（或重新启用）目录后不触发任何扫描：新目录里的既有文件不进库，用户必须自己找到「开始索引」按钮，首用体验是加完目录后索引一直为 0。 | 从 `index_start` 抽出 `spawn_index_thread`；`add_root` 与 `set_root_enabled(true)` 在许可释放后自动开跑一轮（扫描+索引，模式走 `auto_mode`）。拿不到许可（监听增量在跑）只记日志不算失败。 |
| P2 | `purge_files` 删索引行但不删缩略图文件，`thumbs/`、`clip-thumbs/` 随库的增删只增不减。 | 删除行前收集 `file.thumb` 与 `clip.thumb` 指向的文件并尽力删除；缺文件不阻断。 |
| P2 | `mark_missing_vectors` 不过滤停用目录：停用目录的 done 文件被标 stale，进度里永远挂着处理不完的队列（`pending()` 只取启用目录，它们永远不会被消费）。 | SQL 限定 `root_id IN (SELECT id FROM root WHERE enabled=1)`。 |
| P2 | `update.rs install` 在下载完成后、换包前就 `end_update()`，安装窗口期可触发第二次安装，竞争替换同一 bundle。 | `updating` 标志保持到 `install` 结束（成功、失败、取消三条路径各自释放）。 |
| P2 | 前端 `invalidateFileCache` 只清缓存 Map，已挂载的行组件不会重新拉取，重建/重索引后界面继续显示旧缩略图，直到滚动移出再移入。 | 引入模块级缓存版本号 + `useSyncExternalStore`，`useThumb`/`useClipThumb`/`useMoments` 订阅版本变化后重取。 |
| P3 | `.mts/.m2ts` 在 `media.rs` 有 MIME 映射却不在 `Kind::from_ext` 的视频扩展名表里，被归为 Other 永不索引。`.ts` 与 TypeScript 冲突，维持归为代码。 | 视频表补 `mts`/`m2ts`（不含 `ts`），前端 `VIDEO_EXTS` 同步。 |
| P3 | `search.rs run()` 残留一行 `* 0.0` 死代码。 | 删除。 |

### 明确不修（说明理由）

- `ram_budget_mb` 设置项全链路无人读取：属预留字段，删除会动设置序列化，留待真正实现内存预算时接上。
- 嵌套目录（父/子同时入库）会重复遍历但行唯一、不会误删：只浪费遍历，收益不大，暂不加重叠检测。
- `compact` 与 DB 重映射跨文件无原子性（崩溃在两者之间会留下错位映射）：桌面单进程下由许可与写锁串行化，崩溃窗口极窄，重建索引即可恢复，维持现状。

### 验证

新增 6 条回归测试（db.rs 3 条、jobs.rs 2 条、scan.rs 扩展名断言并入既有用例）。

| 验证 | 结果 |
| --- | --- |
| Rust 引擎 `cargo test -p sevqo-engine --lib` | 99 通过，0 失败 |
| 桌面壳 `cargo test -p sevqo-desktop --release` | 13 通过，0 失败，1 忽略；主程序及文档测试退出 0 |
| 前端 `npm run typecheck` 与 `npm run build` | 通过；Node v22.13.0（volta），Vite 构建成功 |
| 补丁格式 `git diff --check` | 通过 |

被忽略的测试仍需正式发布脚本产出的签名更新包。Python sidecar 本轮零改动，`inference/.venv` 不在本 checkout，视频采样回归未重跑。本轮改动未发版；发版走既有 `v*` 标签流水线。
