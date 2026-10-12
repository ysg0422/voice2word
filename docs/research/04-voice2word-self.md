# Voice2Word 现状盘点

> 盘点对象：本仓库自身（`A:\Linux\Voice2Word`）。
> 方法：只读 `README.md` / `docs/` / `installer/` / `.github/workflows/` / `Cargo.toml`，并遍历 `src/**`（app / core / engines / storage / subtitle / ui / utils）逐个模块核对实现，未改动任何源码。
> 证据口径：`文件:行号` / 函数名 / 结构体名。
> 目的：为对标 VideoCaptioner、VideoLingo、SmartSub 等同类开源项目建立「自身基线」，供后续合并调研结论。

---

## 1. 产品定位与技术栈

### 1.1 定位

- **本地优先（local-first）的音视频字幕工作台**，覆盖「转写 → 标点/润色 → 翻译 → 校对 → 导出」全流程；除选用在线翻译 API 外全程不联网（`README.md` 首段）。
- **形态**：Windows 桌面原生 GUI（Rust + GPUI），**无 CLI、无 Web、无插件/API**（`Cargo.toml:1-6`、`src/main.rs:29`）。
- **版本**：0.1.7，MIT 许可；仓库内 `legacy_python/` 是已废弃的 PySide6 原型（`开发文档.md` 顶部已声明其不代表当前实现）。
- **两个二进制**：`voice2word`（主 GUI，`src/main.rs`）、`voice2word-setup`（自研 Windows 安装器，`src/bin/setup.rs`）。

### 1.2 关键依赖与技术栈（`Cargo.toml:15-40`）

| 领域 | 选型 | 说明 |
| --- | --- | --- |
| GUI | `gpui 0.2.2` + `wgpu 0.20` + `raw-window-handle 0.6` + `pollster 0.3` | Zed 的 GPUI，Windows/D3D12 优先 |
| 异步/并行 | `tokio 1.43`（full）+ `rayon 1.10` | 管线异步 + 切块进程级并行 |
| 数据库 | `rusqlite 0.33`（bundled） | 单文件 SQLite `voice2word.db` |
| HTTP | `ureq 2.10`（rustls，仅出站） | 模型下载 / 在线翻译 / 更新检查 |
| 其它 | `serde`+`toml`、`tracing`(+subscriber)、`anyhow`、`rfd 0.15`、`chrono`、`sysinfo 0.31`、`image 0.25`、`encoding_rs`、`zhconv 0.3`、`uuid`、`flate2`(rust_backend)、`sha2`、`regex` | — |

**推理/音视频一律以「子进程 + 文本/JSON 协议」集成，不进 Rust 进程：**

- ASR-Whisper：whisper.cpp `whisper-cli.exe` 子进程（`src/engines/whisper.rs:588,736`）。
- ASR-SenseVoice：Python runner `tools/sensevoice_runner.py` + sherpa-onnx INT8 ONNX（`src/engines/sensevoice.rs:329-343`）。
- 标点：Python runner `tools/punc_runner.py` + CT-Transformer INT8 ONNX（`src/engines/punc.rs:190-200`）。
- LLM（润色/翻译）：本地 llama.cpp（`llama-server` HTTP / `llama-completion` CLI，`src/engines/llm.rs:259-346`）。
- 音视频：FFmpeg / FFplay 子进程（`src/engines/ffmpeg.rs`、`src/engines/video_player.rs`）。
- **Python 是运行期依赖**（仅 runner 胶水脚本），配置项 `paths.python`。
- 渲染：自研 wgpu NV12→RGBA 着色器（`src/engines/media_pipeline.rs:836-862`）。

---

## 2. 已实现能力清单（按模块）

### 2.1 语音识别 `src/engines/`

- **多档 Whisper 引擎**：base / small-q5_0（默认）/ turbo-q5 / turbo-q8；界面内切换、下载、删除（`README.md` 表格；模型清单 `src/utils/model_download.rs:156` `ITEMS`）。
- **SenseVoice 非自回归档位**：单次输出、最快（`src/engines/sensevoice.rs`）。
- **Silero VAD 静音加速**：Whisper 走 `--vad -vm ... -vt ...`（`whisper.rs:602-618`）；SenseVoice 走 `--vad-model`（`sensevoice.rs:334`）。
- **流式识别**：Whisper 用 stdin 内存管道 + 128KB 泵线程（`whisper.rs:596-599,767-783`）；SenseVoice 用 JSON 行协议 `event: segment|finished|provider`（`sensevoice.rs:491-552`），界面实时出字。
- **置信度与救场重解码**：token avg logprob（`whisper.rs:1069`）；低置信度两段式重解码 `transcribe_stream_with_fallback`（`whisper.rs:448`；管线阶段 2.5 `src/core/pipeline.rs:928-1057`）。
- **长音频分块并行**：`src/engines/chunked_whisper.rs`（`CHUNK_SEC=360` / `OVERLAP_SEC=1.5` / workers≤4、SenseVoice≤6；`plan_chunks`、`stitch_chunks`、rayon `par_iter`）。
- **说话人分离**：纯 Rust 声学特征（RMS/ZCR/自相关基频/谱质心）+ 确定性 k-means，最多 4 人（`src/engines/diarization.rs:19,71,341`），可选开关 `enable_diarization`。
- **繁转简**：`normalize_zh_text`（zhconv，`whisper.rs:1213`）。

### 2.2 音频预处理 `src/engines/audio_prep.rs`

- 统一 16kHz 单声道；滤镜链 **highpass → afftdn（谱减降噪）→ dynaudnorm（响度归一）→ atempo（变速）→ aresample(soxr)**（`audio_prep.rs:77-124`）。
- **静音压实** `plan_compaction`（能量 VAD + 双门限滞回 + 合并策略）与时间轴回映射 `to_original` / `to_original_span`（`audio_prep.rs:132-435`）。
- 手写 WAV 输出 `write_wav_mono16`（`audio_prep.rs:440`）。

### 2.3 标点与润色

- **CT-Transformer 标点恢复**（`src/engines/punc.rs:61,151`），默认启用、毫秒级。
- **Qwen 大模型润色**：纠错别字与口语碎词、智能断句 `polish_text` / `polish` / `polish_batch`（`src/engines/llm.rs:501,628,1044`）；`polish_mode = punc | qwen`。
- 收尾规整 `optimize_segments`（`src/subtitle/segment.rs:580`）。

### 2.4 字幕翻译 `src/engines/translate.rs` + `llm.rs`

- **两种模式**：`offline_qwen`（本地 llama.cpp + Qwen2.5-0.5B GGUF，可指向自定义 `.gguf`）/ `online_api`（OpenAI 兼容 `/chat/completions`）。
- **在线可配** `api_base` / `api_key` / `api_model`，兼容 OpenAI / DeepSeek 等；`api_key` 留空读环境变量 `VOICE2WORD_API_KEY`（`src/utils/config.rs:250-257`）。
- **8 种目标语言**：简体中文、繁体中文、English、日本語、한국어、Русский、Français、Deutsch。
- **健壮性**：增量翻译（只补未译句，`translate.rs:267-288`）、分批 + 字符预算（`ONLINE_CHAR_CAP=3500`）、`MAX_BATCH_RETRIES=3`、漏译折半重试、429/5xx 重试、401/403/404 fail-fast、`reasoning_content` 处理（不把思考文本当译文，`translate.rs:952-984`）。
- **术语表**：`TranslateConfig.glossary/glossary_entries` + `glossary_prompt`，默认上限 80 条（`config.rs:182,213,234`），注入翻译提示；UI 可编辑。
- **手工订正译文**：在对照表中直接改单句译文。

### 2.5 字幕模型与读写 `src/subtitle/`

- **导出 11 类格式**（`src/ui/mod.rs:11` `EditorExportFormat`；分派 `writer.rs:106`）：
  SRT、ASS、VTT、TXT、JSON、FCPXML(1.9)、Premiere XML(xmeml v4)、剪映草稿（导出到文件夹 / 注入本机草稿库）、EBU-TT-D、Netflix TTAL、剪映文件夹。
- **导入**：仅 **SRT / VTT / TXT**（`reader.rs:36,47`）；编码兜底 UTF-16 BOM → UTF-8 → GBK（`reader.rs:173`）；TXT 时间轴按 0.3s/字估算。
- **ASS 能力**：PlayRes 1920×1080、样式表、说话人分色 8 色（`writer.rs:793,812`）、6 种视觉预设（`config.rs:678`），可调字号/字距/行距/单行字数/底边距。
- **双语**：`ExportMode::{RawOnly, TranslationOnly, Bilingual}`（`segment.rs:40`），SRT 双语为两行；导出栏可选「仅原文 / 仅译文 / 双语对照」。
- **自动折行** `wrap_text`（标点优先 → 空格 → 硬切，`writer.rs:707`）。
- **阅读速度约束**：CJK 9/12 CPS、拉丁 17/25 CPS 分档（`src/subtitle/stats.rs:24-40`）；`SubtitleStats` 统计句数/字数/有语音占比/平均与最快语速/过长句（`stats.rs:77,124`）。
- **时间轴工具** `src/subtitle/timing.rs`：`ShiftAll/ShiftAfter/Scale/FitToDuration`、`resolve_overlaps`、`suggest_fit`。
- **质检**：`QualityCategory{LowConfidence, GlossaryViolation, Untranslated, EmptyOrShort}` + `quality_report`（`segment.rs:361,456`）；校对表导出 CSV/Markdown 7 列（`qc.rs:25,66`）；编辑日志 `EditKind` 8 种 + `render_audit`（`audit.rs:25,145,260`）。
- **原子写** `writer.rs:55`；文件名模板 `{name}/{ext}/{date}`（`writer.rs:611`）。

### 2.6 应用与核心 `src/core/`、`src/app/`

- **主管线** `TaskPipeline`（`core/pipeline.rs:170,326`）：音频抽取/时长 → 预处理 → ASR（Whisper/SenseVoice）→ 低置信度救场 → 标点/润色 → 说话人分离 → 字幕生成 → 指标归档；**翻译在管线之外单独触发**。
- **事件流** `PipelineEvent{StageChanged, Progress, SegmentStream, Finished, Error}`，支持取消（`cancel()`/`is_cancelled()`）。
- **性能体系**：`core/metrics.rs`（17 字段指标）、`core/performance.rs`（硬件分级 Low/Medium/High/Ultra、CPU 基准 GFLOPS、`InferenceProfile`、`UserStrategy`）。
- **应用状态**：`src/app/state.rs`（约 193KB）——undo/redo、`edit_log`、批量队列、播放器/帧缓存、GPU 模式、模型存在性缓存等。

### 2.7 UI `src/ui/`

- **四个工作台**：剪辑校对 / 语音转写 / 视频库 / 性能设置（`ui/components/navigation.rs:24`）。
- **剪辑校对**：内嵌播放器（`engines/video_player.rs`）+ 音画同步监视器 + 多轨时间轴（字幕轨 + 波形轨，`ui/theme.rs:515 TIMELINE_H=134`）+ 波形（`editor.rs:4279`）+ 查找替换 Ctrl+H（`subtitle/edit.rs:76`）+ 撤销/重做 + 整轨平移/缩放/铺满（`editor.rs:2663-2868`）+ 单句起止 ±0.1s/±0.5s + 长句拆分/短句合并 + 样式实时预览。
- **快捷键** 14 条统一表（`ui/shortcuts.rs:69`），含命令面板 Ctrl+K、主题切换 Ctrl+T；**注意** `Theme::set_light(false)` 固定深色（`main.rs:65`），浅色主题实际不可达。
- **批量队列**：`batch_queue.json` 持久化（`utils/queue_store.rs`），支持导入文件/文件夹、开始全部、终止、完成汇总（`ui/dialogs/completion.rs`）。
- **视频库**：卡片列表 + 缩略图 + 关键字/翻译状态筛选 + 排序 + 批量导出 + 查重报告（`ui/views/library.rs`、`utils/library_query.rs`、`utils/duplicates.rs`）。
- **模型与组件管理**：一键补齐缺失、单项下载/删除、打开目录、备份/恢复、导出诊断、检查更新（`ui/components/model_manager.rs`；`utils/model_download.rs` `ITEMS` 14 项）。
- **硬件向导与基准弹窗**（`ui/dialogs/hardware_setup.rs`、`ui/dialogs/benchmark.rs`）。

### 2.8 工程化 / 平台 `src/utils/`、`installer/`、`.github/`

- **配置分层**：`config.toml` + `config.local.toml`（机器专属绝对路径自动拆分、逐项覆盖、原子写；`config.rs:1332,1520`）。
- **持久化**：SQLite `tasks` 表 + WAL + 版本化迁移（`SCHEMA_VERSION=2`）+ 损坏自愈（改名留底 `.corrupt-<ts>.db`）+ 退出 `optimize`/`wal_checkpoint(TRUNCATE)`（`storage/db.rs:133,181-230,311-388,611-639`）。
- **内容指纹缓存去重**：`sha256(size‖首256KB‖尾256KB)`（`utils/fingerprint.rs:40-46`），`find_cached_task` 指纹优先（`db.rs:525`）——改名/复制后秒命中。
- **模型下载**：hf-mirror / ModelScope / gh-proxy 多源（`utils/model_download.rs:49-64`）、尺寸校验、`.part` 清理（`:1076`）。
- **更新检查**：GitHub Releases API 检查 + 下载（`utils/update_check.rs`），**无自动安装/替换**。
- **诊断/崩溃/日志/备份**：启动自检 10 项（`utils/diagnostics.rs`）、上次崩溃检测（`crash_report.rs`）、按日期日志 + `latest.txt` + panic hook（`logger.rs`）、db+config 的 STORED zip 备份（`backup.rs`）。
- **子进程管理**：Windows Job Object 进程树回收（`utils/child_registry.rs`）、GPU 占空比限速 `NtSuspendProcess`（`engines/media_pipeline.rs:660-820`）、子进程 `BELOW_NORMAL` 让路。
- **打包**：Inno Setup（`installer/voice2word.iss`）+ 自研 Rust 安装器（`src/bin/setup.rs`，装到 `%LOCALAPPDATA%`，写 HKCU 卸载项）。
- **CI**：`.github/workflows/ci.yml` —— **仅 windows-latest**：`fmt --check`（硬门禁）、版本同步校验、`clippy -D warnings`（硬门禁）、`test --lib`（单线程规避 wgpu flake）、以及非阻塞的 all-targets / release build；`release.yml` —— 打 tag `v*` 构建两个 bin，发布 `Voice2Word-vX.Y.Z-Setup.exe` + 便携 `voice2word.exe`。
---

## 3. 明显缺失 / 未覆盖的能力（对照关注点逐条）

| 关注点 | 有/无/部分 | 结论与证据 |
| --- | --- | --- |
| 字幕烧录 / 合成视频（burn-in） | **无** | 全仓无 `drawtext`/`subtitles`/`overlay` 烧录逻辑；`subtitles=` 仅出现在 **ffplay 预览**（`src/ui/actions.rs:2147-2148`），导出只写字幕文件、不改视频。 |
| TTS / 配音 / 声音克隆 | **无** | 引擎层无任何集成（`tools/llama-tts.exe` 只是 llama.cpp 自带二进制，未被调用）。 |
| 硬字幕提取（视频 OCR） | **无** | 全仓无 OCR / 视频字幕区域检测代码。 |
| 批量处理 | **部分** | 有队列 + 顺序批量（`utils/queue_store.rs`、`state.batch_running`）；无多文件并发调度，并行只发生在单文件切块内。 |
| GPU 加速 | **部分** | Whisper：Vulkan（自编译 `scripts/build_whisper_msvc_vulkan.bat`）/ 预编译 CUDA 12.4 包（`whisper-cublas`）；解码：D3D11VA 硬解（`D3d12va` 已定义但从不被选中，`media_pipeline.rs:97-209`）；ONNX：DirectML（sherpa-onnx 期望字面 `directml`，当前绑定为 CPU-only 构建，实测静默回落 CPU）。无 CUDA-ONNX / CoreML / Metal / ROCm。 |
| 跨平台 | **无**（Windows only） | `#[cfg(unix)]` 0 处、`#[cfg(windows)]` 32 处；GPUI/D3D12、Job Object、PE 导入解析、`%LOCALAPPDATA%` 等均 Windows 专有；CI 仅 windows-latest。仓库路径名含 `Linux` 但代码不支持 Linux。 |
| 命令行 / 无头模式 | **无** | 全仓 `std::env::args` 0 命中；两个 bin 均为纯 GUI / 交互式安装器。 |
| 插件 / 对外 API | **无** | 无插件机制；无 HTTP 服务端（`TcpListener` 仅用于探测本地空闲端口与测试 mock）。 |
| Web UI | **无** | — |
| 导出格式覆盖面 | **强** | 11 类，含剪映草稿、FCPXML、Premiere XML、EBU-TT-D、Netflix TTAL；但**导入仅 SRT/VTT/TXT**，且无 LRC。 |
| LLM 断句 / 校正 | **有** | 本地 Qwen 润色/断句/纠错；在线 API 通路存在（当前 UI 主要暴露在翻译侧，润色侧为本地模型）。 |
| 术语表 | **有** | 术语表配置 + 术语违规质检徽标 + 编辑入口。 |
| 双语 | **有** | `ExportMode::Bilingual`；SRT/ASS/TTML 支持双语；导出栏「仅原文/仅译文/双语对照」。 |
| 安装包 | **有** | Inno Setup + 自研安装器；**无代码签名**（SmartScreen 告警风险）。 |
| 自动更新 | **部分** | 仅「检查 + 下载」，无静默安装 / 原子替换 / 签名校验。 |

**补充细节缺口（已核实）：**

- 导入缺 ASS / TTML / JSON / FCPXML / LRC；无 karaoke 逐字计时标签（`\k` 系列 0 命中）。
- 时间轴无 zoom、无 clip 拖拽 trim（片段仅可点选）。
- 无项目管理（project CRUD / 标签 / 集合 / 文件夹）。
- 无遥测/匿名统计（可视为隐私优点，但也削弱产品迭代数据）。
- 模型完整性仅**尺寸校验**，无 sha256；下载**无断点续传**。
- schema 迁移版本号不一致：`SCHEMA_VERSION = 2`（`storage/db.rs:133`）但存在 `from < 3` 的 `content_hash` 迁移块（`db.rs:366-379`），v2 库不会触发该块。
- 悬空 UI 条目：`ui/components/model_manager.rs:371` 列出的 `"yt-dlp"` 不在 `model_download.rs` 的 `ITEMS` 中，永不显示。
- 若干 `#[allow(dead_code)]`：`ui/components/config_drawer.rs:320,768`、`ui/views/performance.rs:816`、`ui/editor.rs` 多处、未绑定的 `shortcuts.rs:39 ToggleTheme`。
- 生产代码中 `todo!`/`unimplemented!`/`FIXME` **0 处**（说明当前实现较完整，但也意味着缺口是「未规划」而非「半成品」）。

---

## 4. 与同类工具的初步差距假设（待与调研结果合并）

> 下表为**假设**，需用 01~03 号调研文档的实际结论校准；Voice2Word 的已知差异化优势集中在「全本地 + 剪映生态导出 + 质检/编辑工作台深度」。

| 维度 | VideoCaptioner | VideoLingo | SmartSub | Voice2Word（现状） |
| --- | --- | --- | --- | --- |
| 形态 | GUI(Windows) + 命令行 | GUI + CLI | 桌面/批量 | **仅 GUI** |
| 字幕烧录/合成视频 | 有 | 有 | 有 | **无** |
| TTS / 配音 | 部分 | 有 | — | **无** |
| 硬字幕 OCR 提取 | 部分 | — | 有 | **无** |
| 翻译引擎 | LLM 多 provider | LLM + 术语 | 多引擎 | LLM（本地 Qwen / OpenAI 兼容），**单一在线链路** |
| 术语表 / 双语 | 有 | 有 | 有 | 有（强） |
| 批量 / 队列 | 有 | 有 | 强 | 有（**顺序**） |
| CLI / Web / API | 部分 | 部分 | — | **无** |
| 跨平台 | Win/Linux | Win/Linux/mac | Win | **Win only** |
| 剪辑软件导出 | 部分 | — | — | **强**（剪映 / FCPXML / Premiere / TTML 广播） |
| 本地化/离线 | 部分依赖云端 | 部分 | 部分 | **全本地（强）** |

**初步判断**：Voice2Word 在「字幕编辑与质检工作台、剪辑软件交付格式、全本地隐私」三项上具备相对优势；在「输入侧（硬字幕 OCR）、输出侧（烧录成片/配音）、自动化（CLI/批量并发/API）、平台覆盖」四项上落后。

---

## 5. 值得优先补齐的候选方向（按投入产出粗排）

| 优先级 | 方向 | 理由 | 预估投入 |
| --- | --- | --- | --- |
| **高** | 1. 字幕烧录 / 合成视频（burn-in，可选硬件编码） | 同类产品标配；FFmpeg 与样式预设、硬件探测均已就位，只需接 `ass`/`subtitles` 滤镜与导出选项 | 低-中 |
| **高** | 2. 命令行 / 无头模式（批量转写 + 导出） | 打开自动化、服务器、CI 场景；管线已是库（`src/lib.rs` + `core/pipeline.rs`），加参数解析与参数映射即可 | 低 |
| **高** | 3. 模型下载断点续传 + sha256 校验 | 5GB 级模型下载体验与安全；`utils/model_download.rs` 已有 `.part` 骨架 | 低 |
| 中 | 4. GPU 加速闭环（ONNX DirectML 真生效 + 硬解启用 D3D12VA + 后端可选） | 直接决定速度体验；当前 DML 静默回落 CPU、`D3d12va` 定义了却不用 | 中 |
| 中 | 5. 批量队列升级为并发任务调度 + 任务持久化续跑 | 路线图 P0-6；当前仅顺序执行、崩溃需重跑 | 中 |
| 中 | 6. 自动更新落地（下载 → 校验 → 原子替换 exe） | 检查/下载骨架已有，缺安装与签名 | 中 |
| 中 | 7. TTS / 配音（本地或在线可选） | 补齐「配音」链路，与现有字幕时间轴天然复用 | 中-高 |
| 中 | 8. 硬字幕 OCR 提取（视频→字幕入口） | 补齐输入侧能力，可接 ONNX OCR | 中-高 |
| 低 | 9. 导入格式补齐（ASS / TTML / JSON / FCPXML / LRC）+ karaoke | 提升互操作性 | 低 |
| 低 | 10. 代码签名 + 遥测（可选开关） | 分发体验与迭代数据 | 低-中 |
| 低 | 11. 时间轴 zoom / clip trim、项目管理 | 编辑器与库的打磨项 | 中 |
| 低 | 12. 跨平台（Linux / macOS） | 需抽象 GPUI/D3D12、Job Object、PE 解析等，投入大，收益取决于目标用户 | 高 |

---

## 附：口径说明

- 本文只覆盖 **本仓库自身**；与 VideoCaptioner / VideoLingo / SmartSub 的对比结论为**初步假设**，最终结论以 `docs/research/01~03` 的调研结果合并为准。
- 所有「有/无/部分」判断均来自当前工作区源码现场核对（行号以当前文件为准）；`legacy_python/` 与 `开发文档.md` 中的需求清单不代表现行实现。
- 本文件由只读盘点产出，未修改任何源码、未执行 git 提交。