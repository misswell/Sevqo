# Sevqo

完全离线的本地多模态搜索引擎：用一句中文、一张图或一段视频，找到电脑里的图片、视频、文档和代码，并直接跳到视频里的那一秒。

不联网检索、不接 API、不上传文件。模型在首次启动时按需下载，之后全程本地运行。

## 能做什么

| 能力 | 说明 |
| --- | --- |
| 自然语言搜图 | 「海边日落 有人骑自行车」→ 命中图片，无需文件名匹配 |
| 以图搜图 | 把图片拖到窗口（含悬浮框）或填本机路径，找相似画面 |
| 以视频片段搜视频 | 拖入或填入一段视频，用它的开头若干秒作为查询，找同类片段 |
| 视频内容搜索 | 每个视频按滑动窗口切片，逐段编码 |
| 视频时刻定位 | 结果里直接给出命中时间点，点击在应用内从该秒开始播放 |
| 文档全文搜索 | PDF / DOCX / RTF / HTML / MD / TXT（含 GBK 老文件），CJK 逐字索引 |
| 精确文件名搜索 | BM25 对文件名/路径加权，找 `invoice_2026.pdf` 这类目标 |
| 混合检索 | 关键词 + 语义双通道，RRF 融合 |
| 自动增量索引 | 目录监听，改动先合并再索引，不重复劳动 |
| 悬浮搜索框 | 全局快捷键唤起的 Spotlight 式输入框，另有完整窗口 |

## 架构

```
crates/sevqo-engine        Rust 引擎：扫描 / 抽取 / FTS5 / 向量文件 / 混合检索 / 索引调度 / 模型下载 / 目录监听
apps/desktop/src-tauri     Tauri 2 桌面壳：IPC 命令、窗口与全局快捷键、后台任务编排
apps/desktop/src           React 19 + TypeScript 前端：悬浮搜索框 + 主窗口
inference/sevqo_embed      Python 推理 sidecar：EmbeddingGemma 2 编码、媒体解码、PDF 文本
models/google/embeddinggemma-2   本地权重（首次启动时下载，sha256 校验）
tools                      质量评测与冒烟数据集生成
```

数据面：

- **精确通道** SQLite WAL + contentless FTS5，BM25 列权重 `bm25(fts, 12.0, 2.0, 6.0, 1.0)`（文件名 > 路径 > 标题 > 正文）；中文按字切索引、按短语查询，摘要在 Rust 侧从 `text_store` 生成。
- **语义通道** 一个扁平 f32 文件（`vectors.f32`，16 字节头 + 行存储），查询时 mmap + rayon 精确余弦；`vec_map` 保存「向量行号 ↔ 文件/分块/切片」映射。删除只打墓碑标记，`compact` 一次性回收。
- **融合** 两通道各取排名，RRF（k=60）合并；视频命中按文件聚合，并把命中的切片展开成时间点。
- **媒体流** `sevqo-media://video/<file_id>` 自定义协议只服务索引内的视频（所属目录停用即 403），按 Range 每次最多给 8 MiB，播放器的 `#t=` 负责跳到那一秒。

为什么不用现成的向量库：语料规模在十万级向量以内时，顺序扫描的精确余弦比任何 ANN 都更快、更准，且零索引维护成本。

## 模型

EmbeddingGemma 2（740M 总参数，文本 + 视觉塔约 439M）输出 768 维统一空间向量，按 MRL 截断到 512（默认）/ 256 / 128 后重新 L2 归一化。约束：

- 只用 bfloat16 或 float32，**绝不使用 float16**（激活溢出会产生 NaN）。
- 查询带 `task: search result | query: ` 前缀，文档带 `title: … | text: …`，媒体不带前缀。
- 单图 280 token；视频每帧 140 token，处理器硬上限 32 帧 ⇒ 单个切片不超过 32 秒（默认窗口 16 秒、步长 12 秒）。
- 权重从 `models.google/embeddinggemma-2` 的镜像端点下载，可在设置里改端点顺序；下载可随时取消，断点续传。

## 快速开始

前置：Rust ≥ 1.85、Node ≥ 20、Python ≥ 3.11（仅 sidecar 需要）。

```bash
# 1. 推理环境
python3 -m venv .venv
.venv/bin/pip install torch sentence-transformers av pillow pillow-heif pypdf psutil numpy

# 2. 权重（也可跳过，交给 App 首启自动下载）
.venv/bin/python tools/fetch_model.py

# 3. 桌面端开发运行
cd apps/desktop && npm install
npm run tauri dev

# 4. 打包
cd apps/desktop && npm run tauri build -- --bundles app   # 产出 Sevqo.app
../../tools/make_dmg.sh                                  # 产出可拖到「应用程序」的 .dmg
```

`tauri build` 默认的 DMG 步骤会调用 `bundle_dmg.sh`，那脚本让 Finder 排版图标，因此每次都要给终端授予「控制 Finder」的自动化权限；在干净或无人值守的环境里它会以 `failed to run bundle_dmg.sh` 结束。`tools/make_dmg.sh` 用 `hdiutil` 直接做同样的镜像（含 `/Applications` 快捷方式），不依赖权限，也不需要打开桌面窗口。

关键环境变量：`SEVQO_MODEL_HOME` 指定权重目录（默认 `~/Library/Caches/Sevqo/models/google/embeddinggemma-2`），`SEVQO_PYTHON` 指定 sidecar 解释器。索引数据在 `~/Library/Application Support/Sevqo/index.sqlite`，向量在缓存目录的 `vectors.f32`，日志在 `logs/`。

`Sevqo.app` 里内嵌了 sidecar 的 7 个 Python 模块（`crates/sevqo-engine/src/sidecar_assets.rs`），首次启动写到缓存目录的 `sidecar/` 并挂到 `PYTHONPATH`，所以装到「应用程序」后不依赖仓库布局；开发时仓库里的 `inference/` 优先，改模块不用重新编译。torch 这类体积巨大的依赖不能内嵌，打包版仍然需要一个装好依赖的 Python：在「设置 → Python 路径」里指到上面那个 `.venv/bin/python`，或让 `SEVQO_PYTHON` 指向它。

## 验证

```bash
cargo test -p sevqo-engine                     # 单元与管线测试
cargo test -p sevqo-desktop --release          # 桌面壳：媒体协议的范围解析
.venv/bin/python tools/make_fixtures.py 12     # 生成可肉眼核对的冒烟语料
cargo run -p sevqo-engine --example smoke -- .research/fixtures   # 端到端：索引 + 关键词/语义/以图搜图/片段搜视频 + 视频切片
.venv/bin/python tools/eval_retrieval.py --images 1000 --queries 2000   # 检索质量与吞吐
```

`eval_retrieval.py` 在 Flickr30k 的中文人工标注子集上测 t2i / i2i 的 R@1/5/10、768/512/256 三种维度、前缀消融，以及单图编码耗时与内存峰值。

<!-- BENCHMARK_TABLE -->

### 检索质量：Flickr30k 中文人工标注子集，1000 图 / 2000 查询

设备 Apple M1（8 核 / 16 GB），MPS + bfloat16，原始数据见 `.research/eval_results.json`。

| 配置 | t2i R@1 | R@5 | R@10 | i2t R@1 | 2000 查询 |
| --- | --- | --- | --- | --- | --- |
| 中文 + 查询前缀 · 768 | 0.790 | 0.943 | 0.970 | 0.975 | 35.0 s |
| 中文 + 查询前缀 · **512（默认）** | 0.783 | 0.937 | 0.967 | 0.973 | 32.0 s |
| 中文 + 查询前缀 · 256 | 0.757 | 0.932 | 0.963 | 0.963 | 31.8 s |
| 中文 · 无前缀 · 768 | 0.783 | 0.933 | 0.965 | 0.983 | 25.1 s |
| 中文 · 无前缀 · 512 | 0.783 | 0.924 | 0.962 | 0.983 | 23.3 s |
| 中文 · 无前缀 · 256 | 0.748 | 0.915 | 0.950 | 0.963 | 23.0 s |
| 英文原文 + 前缀 · 768 | 0.848 | 0.974 | 0.989 | 0.953 | 30.3 s |
| 英文原文 + 前缀 · 512 | 0.845 | 0.975 | 0.989 | 0.949 | 29.1 s |

- **512 是划算的那一档**：比 768 只丢 0.7 个百分点 R@1，向量文件和每次相似度扫描直接砍到 2/3；256 掉 3.3 个百分点，只留给内存极紧的设备。
- **切片是安全的**：截断到前 N 维再重新 L2 归一化，与模型原生低维输出等价（query 向量余弦 0.99997，R@1 0.8594 = 0.8594），所以换维度只是重写向量文件，不需要重新推理。
- **前缀消融在这个规模上几乎无差**（0.790 vs 0.783，R@10 0.970 vs 0.965）；查询前缀仍然保留，因为它对文档侧毫无代价、对长尾查询略有帮助。
- **英文比中文高 5.8 个百分点**（0.848 vs 0.790）：中文口语描述更依赖关键词通道兜底，这正是混合检索存在的理由。
- **吞吐**：单图 1.52→1.74 s（p50 1.67，p95 2.31），约 34 张/分钟；进程常驻峰值 ~1.07 GB。测速期间有 Rust release 构建抢占 CPU，纯索引时按 1.5 s/张 估算更准。

## 实跑验收清单

自动化部分已经绿：`cargo test -p sevqo-engine`（78 项）与端到端 `smoke`（15 项，`SMOKE OK`，含「自然语言只搜图片也确实走语义通道」「关闭后 sidecar 进程确实被回收」）。下面这些只有肉眼能确认，用 `npm run tauri dev` 或打包好的 `Sevqo.app` 逐条勾。

- [ ] **首启下载**：删掉权重目录后启动，模型卡片显示 14 个文件 / 1.49 GB 的进度，能取消、能续传；完成后悬浮框搜索不再提示「模型未就绪」。
- [ ] **装到应用程序后仍可用**：把 `Sevqo_0.1.0_aarch64.dmg` 拖进「应用程序」并从那里启动（不是在仓库里跑），在设置里把 Python 路径指向装好依赖的解释器，语义搜索即可用；启动后缓存目录出现 `sidecar/sevqo_embed/`，日志里没有 `ModuleNotFoundError`。注意产物是 adhoc 签名（没有 Developer ID 证书），本机构建的镜像不带隔离属性所以直接能开；发给别人要签名+公证，否则得右键「打开」或 `xattr -dr com.apple.quarantine`。
- [ ] **索引速度**：添加一个几百张图的目录，进度持续推进，实测约 1.5 s/张；期间搜索仍可用。
- [ ] **自然语言搜图**：用一句图里没出现的中文描述（「黄昏的海滩 有人骑车」）能命中对应图片。
- [ ] **精确文件名**：输入 `invoice_2026` 这类片段，目标排在第一且标为 keyword/both 通道。
- [ ] **文档全文**：PDF 正文里的一句话能搜到并显示摘要；GBK 老 txt、DOCX、HTML 同样命中。
- [ ] **以图搜图**：把一张已索引的图拖进主窗口或悬浮框，松手即出结果，且它自己排第一。
- [ ] **以视频片段搜视频**：拖入一段已索引的视频，标题变成「以视频片段搜视频」，结果里该视频排第一并带时间点；填入 `~/Movies/x.mp4` 这类路径同样可用。
- [ ] **视频时刻定位**：搜视频内容 → 结果里给出时间点；点击后在应用内从那一秒开始播放，进度条可拖（Range 请求）。
- [ ] **悬浮搜索框**：Alt+Space 在任意应用上唤起；空输入显示最近文件；Esc 依次退出「输入内容 → 图片 → 隐藏窗口」。
- [ ] **混合与降级**：关掉混合只剩关键词通道，语义无命中时仍能按文件名找到；停用某目录后其结果与视频流一起消失（403）。
- [ ] **增量索引**：新增 / 改名 / 删除文件后约半分钟内自动反映，重复扫描不会产生新向量行。
- [ ] **退出干净**：关闭应用后 `pgrep -f sevqo_embed` 无残留 Python，活动监视器里 sidecar 常驻约 0.7–1.0 GB。
- [ ] **失败面**：损坏图片 / 加密 PDF 标为失败并给出原因而不是崩溃；缺 HEVC 解码器时播放器提示而不是黑屏。

## 已知边界

- 音频索引未开启（模型有 300M 音频塔，接口已预留）。
- 移动端（Stage 05）尚未开始：引擎侧已按可复用设计，但 sidecar 依赖 CPython，iOS 需要换成 LiteRT-LM 的移动端运行时。
- 语义检索的召回率与图片密度相关：一张图里东西越多，越依赖关键词通道兜底，这也是混合检索存在的原因。
