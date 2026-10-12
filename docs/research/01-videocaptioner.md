# VideoCaptioner（WEIFENG2333/VideoCaptioner · 16212★ · Python · GPL-3.0）

> 调研日期：2026-10-12。数据来自 GitHub API 与仓库源码（默认分支 master；代码已整体位于 `videocaptioner/` 包下），版本/星标为抓取时的实际值。
> 说明：仓库 `LICENSE` 为 **GPL-3.0**，而官网页脚写作「基于 MIT 许可发布」，两者不一致；引用许可时以仓库为准。

## 一句话定位

「卡卡字幕助手」——基于 LLM 的智能字幕工作台：把 **语音识别 → 字幕断句 → LLM 校正 → 字幕翻译 → 视频合成/配音** 做成一条完整流水线，桌面 GUI 与 CLI 双入口，主打「免费开箱」（必剪识别 + 必应/谷歌翻译零配置可用）与「老电脑也能跑」。

## 核心功能清单

1. **语音识别（5 种引擎）**：faster-whisper（本地）、whisper-cpp（轻量本地）、whisper-api（在线）、必剪 B 接口、剪映 J 接口（后两者免费、快速、限中英）。输出词级时间戳，配 Silero VAD 过滤静音段。
2. **字幕断句**：标点规则切分 + LLM 语义断句（`core/split/split_by_llm.py`，要求模型用 `<br>` 分隔），CJK 每行最大字数可配（默认 18 字）。
3. **LLM 优化**：错别字校正、术语统一、标点规范化；分批 + 上下文 + 输出校验回灌重试。
4. **字幕翻译（4 种引擎）**：LLM（上下文感知 + 反思优化机制）、必应、谷歌、DeepLX；多线程 + 分批。
5. **视频合成**：软字幕（封装进容器）/ 硬字幕（ASS 烧录，支持 libass CUDA 加速）/ 圆角背景样式（PIL 逐帧绘制）；可保留原音或替换。
6. **字幕样式系统**：预设 JSON（`ass-anime.json`、`rounded-default.json` 等），可调字体/字号/颜色/圆角/内边距/行距/字距/底部边距，界面实时预览。
7. **配音 dubbing（v1.4.x 新增）**：Edge-TTS（默认）/ SiliconFlow（含音色克隆）/ Gemini；语速自适应、LLM 自动缩短超长台词、混音模式 replace/mix/duck、可逐角色指定音色、额外产出 `*.dubbing.json` 报告。
8. **视频下载**：内置 yt-dlp，支持 B 站/YouTube 等，可导入浏览器 Cookie。
9. **批量处理**：批量转录、批量字幕处理、批量视频合成。
10. **命令行**：`gui / transcribe / subtitle / synthesize / dub / process / download / style / config / doctor` 十个子命令。
11. **字幕工具链**：SRT / ASS / VTT 互转、样式预设管理、LLM 请求日志查看。
12. **配套能力**：模型自动下载（ModelScope / HuggingFace）、软件内更新检查、日志窗口、界面多语言资源（`resource/translations`）。

## 技术栈 / 架构

- **语言 / 构建**：Python（源码约 1.30 MB），`requires-python >=3.10,<3.13`；hatchling + hatch-vcs（**版本号来自 git tag**）。
- **UI 库**：PyQt5 `5.15.11` + PyQt-Fluent-Widgets `1.8.4`（Fluent Design 观感）；视频播放用 **VLC**（`ui/components/MyVideoWidget.py` 的 `StandardMediaPlayBar` + python-vlc）。
- **关键依赖**：openai>=1.97.1、httpx、requests、diskcache（结果缓存）、yt-dlp、json-repair（LLM 返回容错）、langdetect、pydub、tenacity（重试）、pillow（圆角字幕绘制）、fonttools（字体族名解析）、platformdirs（跨平台数据目录）、modelscope、psutil/GPUtil（硬件监控）、edge-tts。
- **分层**：`videocaptioner/{config.py, cli/, core/, ui/, resources/}`。**core 不依赖 Qt**，CLI 与 GUI 共用同一套 core —— 这是它能「一个 pip 包同时给 CLI 和 GUI」的前提。
- **core 子模块**：`asr/`（base 模板方法、asr_data、chunk_merger、chunked_asr、各引擎）、`split/`（alignment / split / split_by_llm）、`optimize/`、`translate/`（factory + 4 引擎）、`tts/`、`dubbing/`（pipeline / presets / rewriter）、`subtitle/`（ass_renderer / rounded_renderer / style_manager / font_utils）、`prompts/`（Markdown 提示词库）、`llm/`（client / context / request_logger / check_*）、`utils/`（cache / logger / platform_utils / subprocess_helper）。
- **ui 分层**：`view/`（页面）、`components/`（可复用 `Setting*Widget` 卡片组件）、`thread/`（QThread 任务线程）、`common/`（config、`signal_bus.py` 全局事件总线）、`task_factory.py`（任务组装）。
- **任务模型**：`core/entities.py` 定义 `TranscribeTask / SubtitleTask / SynthesisTask / TranscriptAndSubtitleTask / FullProcessTask` 等任务实体与对应 `*Config`；`ui/task_factory.py` 的 `TaskFactory` 是**纯静态工厂**，只负责「按当前 UI 配置（`ui.common.config.cfg`）组装任务对象」，不含队列与调度；调度由 `ui/thread/*` 与各 view 承担。
- **线程模型**：`ui/thread/` 按职责拆 `subtitle_thread / subtitle_pipeline_thread / transcript_thread / video_synthesis_thread / batch_process_thread / modelscope_download_thread / version_checker_thread / video_download_thread` 等；全流程由 `SubtitlePipelineThread` 顺序驱动子线程并做**阶段进度映射**（转录 0~40%、字幕优化 40~60%、合成 70~100%），任一阶段 `has_error` 即中断后续。**取消是 `stop()` 里 `terminate()` 强杀线程（无协作式取消、无暂停标志）**——这一点正是其 2.0 PR 要改成协作式取消的原因，可视为反面教材。
- **数据流**（官方 `docs/dev/architecture.md`）：`视频/音频 → ASR → ASRData → 分割 → 优化 → 翻译 → 字幕文件 → 视频合成`。
- **打包**：PyInstaller（`VideoCaptioner.spec`）+ `scripts/build_desktop.py`：`git describe` 取版本 → 生成 `_version.py` → `static-ffmpeg` 下载本平台 ffmpeg/ffprobe → PyInstaller → `verify_bundle()` 校验必需资源 → zip。macOS 另出 `.app`。
- **质量工具**：pytest（markers：integration / slow / llm / translator）、ruff（line-length 100，select E,F,I,W）、pyright（basic）。

## 完整工作流（导入视频 → 导出成品）

| 步 | 环节 | 关键机制 |
|---|---|---|
| 0 | 启动 | `videocaptioner` 无参数直接开 GUI；配置优先级 **命令行 > 环境变量 > config.toml > 默认值** |
| 1 | 导入 | 本地文件 / 拖入 / 内置 yt-dlp 下载（可导入浏览器 Cookie） |
| 2 | 抽音 | ffmpeg 转为 flac / m4a / mp3 / wav（`SUPPORTED_SOUND_FORMAT`；whisper-cpp 强制 WAV） |
| 3 | 识别 | 选引擎 → 用 **文件 CRC32** 作缓存键查 diskcache（2 天过期），命中即复用；长音频走 `chunked_asr` 分块并行 → `chunk_merger` 按 10s 重叠 + 滑动窗口对齐合并 → `ASRData`。词级时间戳由 `split_to_word_segments()` 按 `CHARS_PER_PHONEME=4` **按时长比例估算**（非强制对齐），`WORD_LEVEL_THRESHOLD=0.8` 判定是否词级 |
| 4 | 断句 | 先按 `SEGMENT_WORD_THRESHOLD=500` 词分块（在 ±30 词窗口找**最大时间间隔**处切），`ThreadPoolExecutor` 并发；每块**先走 LLM 语义断句**（`<br>` 分隔，相似度 <0.96 判失败并回灌重试，`MAX_STEPS=2`），**异常自动回退规则断句**（500ms 时间间隔分组 → 常见词前后切分 → 递归二分）；CJK 上限 25 字 / 英文 18 字 |
| 5 | 优化 | LLM 分批校正（`MAX_STEPS=3`、`temperature=0.2`、`json_repair` 解析）；**校验「返回键集合与输入完全一致」+ 逐条相似度阈值（≤10 词 0.3，否则 0.7）**，失败把错误回灌重试，最后用 `SubtitleAligner.align_texts()` 行级对齐修复；提示词从 `core/prompts/optimize/*.md` 加载 |
| 6 | 翻译 | 工厂创建 translator（各引擎默认批大小：Google 5 / Bing 10 / DeepLX 5），多线程并发，上下文感知 + 反思优化 |
| 7 | 校对 | 行表逐句核对编辑（仅原文/译文可改），点击某行跳播该句；快捷键 Space / ←→ ±3s / Ctrl+M 合并 / Delete 删除 / Ctrl+T 重译；样式实时预览 |
| 8 | 合成 / 导出 | 软字幕封装 或 硬字幕烧录（ASS / PIL 圆角）→ ffmpeg 输出成品；也可只导出字幕文件；`dub` 子命令另出配音音轨 + `*.dubbing.json` |
| 9 | 落盘 | 约定命名 `【原始字幕】…srt` / `【样式字幕】…ass` / `【卡卡】<stem>.mp4` / `<stem>_final`；PR #1150 后改为 per-run 任务目录 + 统一 `{stem}.{tag}.{ext}`（tag = 语言码 / optimized / subtitled / dubbed），中间产物成功后清理 |

## 安装与分发

- **pip（当前主推）**：`pip install videocaptioner`，同时提供 `videocaptioner`（CLI）与 `videocaptioner-gui`（GUI）两个入口点；PyPI 发布走 OIDC 可信发布（`publish-pypi.yml`，tag 触发）。
- **Windows**：早期（v1.0~v1.3）Release 直接给 `VideoCaptioner-Setup-win64-v1.3.3.exe` 安装包；v1.4 起改为 GitHub Actions 产出的 PyInstaller zip 桌面包。
- **macOS**：脚本安装 `curl -fsSL .../scripts/run.sh | bash`；CI 出 macOS Intel 的 zip 与 `.app.zip`（bundle id `com.weifeng.videocaptioner`）。
- **ffmpeg**：独立 release `ffmpeg-bin`（5 个平台 zip）；桌面打包时由 `static-ffmpeg` 自动下载并内置（essentials 约 96 MB）。
- **Docker**：实验性（streamlit，8501 端口），见 getting-started，README 未突出。
- **平台要求**：Win10/11、macOS 10.15+、Ubuntu 20.04+/Debian 11+/Fedora 35+；Python 3.10+；内存 4 GB（本地 Whisper 建议 8 GB+）。
- **无 Web 版**：官网只是文档 + 下载入口。

## UI/UX 亮点与文档体系

**UI/UX 亮点**

- 首页即向导：任务创建 / 转录 / 字幕 / 视频合成 四个子界面串成「选文件 → 选引擎 → 跑 → 校对 → 导出」，符合用户心智。
- PyQt-Fluent-Widgets 统一观感，设置项全部抽象为 `Setting*Widget` 卡片组件（十几个），配置界面一致性高。
- **全局事件总线 `signalBus`**：广播字幕排布/优化/翻译/目标语言/转录模型/软字幕/画质/渲染模式变化，以及视频控制（play / pause / stop / source_changed / `segment_play(ms,ms)`），页面与播放器解耦。
- **校对界面**：字幕以 `QAbstractTableModel` 行表呈现（起止时间 + 原文 + 译文四列，仅原文/译文可编辑），点击某行即跳播该句（句末留 −50ms 余量）；快捷键 Space 播放/暂停、←/→ ±3s、Ctrl+M 合并选中行、Delete 删除、Ctrl+T 重译，右键 `RoundMenu` 复用同三项；处理中禁用这些操作防并发改数据。**注意：它没有内嵌时间轴、也没有波形**，播放靠独立的 VLC 播放器组件——这正是 Voice2Word「多轨时间轴 + 波形 + ±0.1s 微调」的差异化优势所在。
- **样式预览链路**：左侧参数面板（固定 350px 滚动区）+ 右侧预览卡片；改参数即写入配置并另起 `AssPreviewThread` / `RoundedBgPreviewThread` 生成预览图（**以 720P 为基准渲染**，再按控件宽高 ×0.98 缩放），用 `blockSignals` / disconnect 防信号回环，`_loading_style` 标志抑制加载期回调；样式 JSON 按 `mode` 字段过滤（`ass-*` / `rounded-*`）。
- **独立的「LLM 请求日志」窗口**（`llm_logs_interface`）+ 日志窗口，把黑盒 API 调用可见化，用户能自己定位「翻译失败/费用异常」。
- 批量处理界面、设置界面分组（LLM / ASR / 翻译 / 配音 / 通用）。
- **免费开箱默认值**：必剪识别 + 必应/谷歌翻译，零 API Key 即可跑通；只有 LLM 才需要 Key，且支持任意 OpenAI 兼容服务商（官方中转站、SiliconCloud、DeepSeek）。
- 文档里给 **ASR 选型建议**：中文 FasterWhisper Medium+、英文 Small、其他语言 Large-v2。

**文档体系**

- README 中英双语，含功能特性、演示截图、安装、工作流、FAQ、**Claude Code Skill 章节**、Star History、捐赠。
- VitePress + Vue3 + TS 文档站，部署 GitHub Pages（https://weifeng2333.github.io/VideoCaptioner/），中英双语（`docs/` 与 `docs/en/`，英文多为占位）。
- 目录结构：`guide/`（getting-started、quick-example、configuration、llm-config、cookies-config、workflow、faq、desktop-release）、`config/`（asr、llm、translator、cookies）、`dev/`（architecture、api、contributing、view-structure、asr-chunk-merger、asr-chunked-usage、translate-module、siliconflow-gemini-api-research）、`cli.md`。
- `docs/public` 存放大量界面截图（main.png、preview1/2.png、setting.png、style.png、subtitle.png、bath.png、whisper.png、test_ted1~3.png 等），README 直接引用。
- SEO 全套：title / description / keywords / OG / Twitter Card / JSON-LD / sitemap / robots.txt / canonical。
- Wiki 仅 4 页（Home、大模型配置说明、视频配置字幕实践测试、Cookie 配置说明），内容基本被 VitePress 站取代。
- **社区入口**：QQ 反馈群（issue #30 置顶）、微信 bkfeng46、GitHub Discussions（Announcement / General / Idea / Show and tell）、Issues、捐赠二维码。
- 官网 videocaptioner.cn 卖点文案：「4 分钟处理 14 分钟视频 · 成本不足 ¥0.01 · 支持 99 种语言」「95%+ 识别准确度」「99 种语言识别 / 37 种语言翻译」。

## 维护与社区

| 指标 | 数值 |
|---|---|
| Star / Fork / Watch | 16212 / 1408 / 59 |
| Issue（累计 / 已关 / 开放） | 1014 / 833 / 210 |
| PR（累计 / 开放） | 109 / 29 |
| 贡献者 | 13（含匿名）；主作者 WEIFENG2333 355 commits，其余多为 1~4 commits，匿名提交 87 / 74 |
| 建仓时间 | 2024-10-31 |
| 最近提交 | 2026-05-24 `feat: make Edge TTS the default dubbing provider` |
| 近 52 周提交 | 82 次 |

- **更新节奏**：2024-11~2025-03 高频发 Windows 安装包（v1.0.0 → v1.3.3）；2025 年中起转向 PyPI / CLI / 打包重构；2026-03~05 一轮（v1.4.1、v1.4.2）；**2026-06 之后基本停更**。
- **发布节奏**：`v1.4.2`（2026-05-24，whl + tar.gz）、`ffmpeg-bin`（2026-06-17，5 平台 zip）、`v1.4.1`（2026-03-28）、`v1.3.3`（2025-03-02 exe）…… 早期以 exe 为主，后期以 PyPI + PyInstaller zip 为主。
- **重要在途 PR #1150「VideoCaptioner 2.0」**（2026-06-12 建，仍 open）：workbench UI、配音、统一命名、统一下载引擎下沉 core、统一 `WorkerThread` 基类 + 协作式取消、AppConfig dataclass 共享配置、per-run 任务目录 + 内容寻址 TTS 缓存。
- **CI/CD**：`ci.yml`（ruff + pyright + pytest + build）、`publish-pypi.yml`、`build-desktop.yml`（matrix Windows x64 + macOS Intel，含 smoke test 与 tag 上传 Release）、`deploy-docs.yml`、`claude-code-review.yml`（Claude 自动 PR review）。
- **Issue/PR 模板**：中英双语 bug 模板要求附 `AppData/logs/app.log`，另有 feature request / question 模板；labels 含 bug / enhancement / documentation / good first issue / help wanted。
- **风险观察**：单人主导 + 13 贡献者 → bus factor 低；停更期与 2.0 PR 长期 open 并存；GPL-3.0 与官网「MIT」口径不一致。

## 对 Voice2Word 的可借鉴点

> 现状依据：Voice2Word v0.1.7（Rust + GPUI，MIT，全本地离线优先）。痛点集中在：无引擎抽象、无数据目录规范、缺断点续传与低置信质检闭环、无审计/崩溃上报、无签名与自动更新、文档漂移、巨文件（editor.rs 308KB / state.rs 193KB）。

### 【高】1. 字幕样式预设外置为 JSON + 双渲染模式 + 可复用算法

- **对应现状**：Voice2Word 已有「4 种字幕样式 + 实时预览」，但样式定义与渲染逻辑耦合在 Rust 代码里，用户无法导出/分享/新增样式。
- **借鉴做法**：`core/subtitle/styles.py` 用 dataclass（`RoundedBgStyle`：font_name / font_size / bg_color `#191919C8` 带 alpha / text_color / corner_radius / padding_h / padding_v / margin_bottom / line_spacing / letter_spacing / layout）描述样式，预设以 JSON 存 `resource/subtitle_style/`，schema 形如 `{"name","description","mode":"ass"|"rounded", ...}`；渲染分 ASS（ffmpeg + libass，可 CUDA）与 PIL 圆角背景两条路；预览走 `render_ass_preview()` / `render_preview()`，**以 720P 为基准**再缩放。
- **两个可直接复用的算法**：① **ASS↔PIL 字号换算** `get_ass_to_pil_ratio()` ≈ 1.448（ASS 用 usWinAscent+usWinDescent，PIL 用 unitsPerEm，ASS 74px ≈ PIL 51px）；② **平衡换行算法**（先算最小行数，target=total/num_lines，达 90% 目标且下一字符超 110% 则换行，平衡度从 50% 提升到 96%）。另注意 ASS 需用 `\q2` 禁用 libass 自动换行。
- **落地建议**：把 Voice2Word 现有 4 种样式抽成 `styles/*.json` 预设 + 一个 `StylePreset` 结构，渲染层读取预设而非硬编码；导出的 ASS 与界面预览共用同一套换行/换算函数，避免「预览与成品不一致」。

### 【高】2. 引擎抽象与能力声明（模板方法 + 缓存键）

- **对应现状**：Voice2Word `pipeline.rs` 中 `is_sensevoice` / `can_stream` / `sv_parallel` 三分支样板重复，新增引擎要改多处 match。
- **借鉴做法**：`core/asr/base.py` 的 `BaseASR` 用模板方法 `run(callback)` → 查缓存 → 调子类 `_run()` → `_make_segments()`；子类只需实现 `_run` 与覆写 `_get_key()`（默认 **文件 CRC32**，可把参数拼进键）。能力用类属性声明：`use_cache`、`need_word_time_stamp`、`SUPPORTED_SOUND_FORMAT`。进度回调统一为 `callback(progress:int, message:str)`。引擎侧还内建**公益服务限流**（`RATE_LIMIT_MAX_CALLS=100` / `MAX_DURATION=360*60s` / `TIME_WINDOW=12h`）。
- **落地建议**：给 Voice2Word 定义 `trait AsrEngine { fn key(..)->String; fn word_timestamps(&self)->bool; fn run(..); }`，把 `is_sensevoice` 之类的分支收敛为 trait 方法；缓存键用「文件哈希 + 引擎 + 参数」，顺带修正它「内容指纹缓存」目前只按内容取键的问题。

### 【高】3. 长音频分块合并算法（Voice2Word 目前只分不并）

- **对应现状**：Voice2Word 已做「分块并行」，但缺块间合并与断点续传；长音频块边界处容易出现重复句或丢句。
- **借鉴做法**：`core/asr/chunk_merger.py` 的 `ChunkMerger(min_match_count=2, fuzzy_threshold=0.7)`（思路源自 Groq API Cookbook）：相邻块保留 **10s 重叠**；`_find_best_alignment` 用滑动窗口找最佳对齐，score = matches/i 并加 epsilon 偏好长匹配；有词级时间戳时做精确文本匹配，句子级时用 difflib 相似度 > 0.7 模糊匹配；`_infer_chunk_offsets` 自动推断时间偏移；全部无匹配时按时间边界切分；重叠区在**中点**切分。
- **落地建议**：Voice2Word 的 whisper.cpp 分块输出可直接套用这套合并器（Rust 侧用 `strsim` 或自写 difflib 等价物），并在合并处落盘检查点，顺势补上「断点续传」。

### 【高】4. 用户数据与程序目录分离（三模式路径探测）

- **对应现状**：Voice2Word 配置散落在 `config.toml` + `config.local.toml` + 环境变量，无 `%APPDATA%` 规范、数据目录不可配置、模型约 5 GB 不随包分发。
- **借鉴做法**（`config.py` + PR #1019）：用 `sys.frozen` / 项目根是否有 `resource/` / pip 安装三种模式探测路径；frozen 与 pip 都用 `platformdirs.user_data_path(APP_NAME)`（Win `%LOCALAPPDATA%/VideoCaptioner/`，mac `~/Library/Application Support/VideoCaptioner/`）；程序目录只放只读资源与 `work-dir/`；用户数据目录含 settings.json / logs / cache / models / bin / 样式 / 字体；首次运行 `_copy_missing_tree` 把内置样式与字体复制到用户目录；PATH 注入顺序 `用户下载的二进制 > 内置二进制`；升级 = 只替换程序目录。
- **落地建议**：Voice2Word 增加 `data_dir()`（默认 `%APPDATA%/Voice2Word`，可用 `VOICE2WORD_DATA_DIR` 覆盖），把 models / logs / history.db / cache 全部迁入；模型管理页显示「实际路径 + 占用」，并预留「导出/导入数据目录」用于备份恢复。

### 【高】5. LLM 输出校验与自愈闭环（补上「低置信质检」空白）

- **对应现状**：Voice2Word 的 Qwen 润色/翻译没有输出校验，模型漏句、改错句或返回非 JSON 时会静默污染字幕，且没有低置信标记与人工复核入口。
- **借鉴做法**：`core/optimize/optimize.py` 的 `SubtitleOptimizer`：`MAX_STEPS=3`、`temperature=0.2`，用 `json_repair.loads()` 容错解析；`_validate_optimization_result()` 要求**返回键集合与输入完全一致**，再逐条用 `difflib.SequenceMatcher` 比相似度（≤10 词阈值 0.3，否则 0.7）；校验失败把 assistant 原文 + `"Validation failed: ..."` 作为新 user 消息**回灌重试**；最后 `_repair_subtitle()` 用 `SubtitleAligner.align_texts()` 做行级对齐修复，`_create_segments()` 重建段落并保留原 start/end。断句侧同理：`split_by_llm` 用相似度 <0.96 判失败并重试。
- **落地建议**：Voice2Word 在润色/翻译后加一道 `validate(原句, 新句)`：序号必须一一对应、相似度低于阈值即标为「低置信」并回退原文，在校对台用颜色高亮待人工确认；重试 1~2 次仍失败则整批回退。这与它「低置信质检闭环」的空白正好对应。

### 【中】6. 提示词文件化（Prompt 即资源）

- **对应现状**：Voice2Word 的 Qwen 润色 / 翻译提示词硬编码在 Rust 里，改一句话要重新编译。
- **借鉴做法**：`core/prompts/__init__.py` 把所有提示词以 **Markdown 文件**存于 `core/prompts/<stage>/*.md`（analysis / optimize / split / translate）；`get_prompt("split/semantic", max_word_count_cjk=18)` 用 `string.Template.safe_substitute` 做变量替换；`functools.lru_cache` 缓存文件读取；提供 `list_prompts()` / `reload_cache()` 支持开发期热重载。
- **落地建议**：把 Voice2Word 的润色/翻译/断句提示词抽到 `prompts/*.md`（随包分发，用户可覆盖），加一个「高级：自定义提示词」入口，配合术语表形成完整可调链路。

### 【中】7. LLM 请求日志（JSONL + 轮转 + 任务上下文）

- **对应现状**：Voice2Word 无审计日志、无崩溃上报，在线翻译失败时用户与开发者都拿不到证据。
- **借鉴做法**：`core/llm/request_logger.py` 用 httpx `event_hooks`（request/response）捕获 `/chat/completions`，先暂存 pending 再合并写 **JSONL**（`LOG_PATH/llm_requests.jsonl`），**10 MB 自动轮转**到 `.jsonl.old`；每条含 task_id / file_name / stage / url / status / duration_ms / request / response；`core/llm/context.py` 提供 `get_task_context()` 注入任务上下文（线程里 `set_task_context` → `update_stage` → `clear_task_context`）；GUI 有独立查看窗口。
- **落地建议**：Voice2Word 的在线翻译调用（ureq）加一层「请求记录器」，写 `logs/llm_requests.jsonl` 并复用其轮转策略；界面加「诊断 → 请求日志」页，为后续崩溃上报打底。

### 【中】8. 断句降级链与切分参数（LLM 优先、规则兜底）

- **对应现状**：Voice2Word 有标点恢复但无「LLM 语义断句」，长句与无标点段落只能靠固定规则。
- **借鉴做法**：`core/split/split.py` 先按 `SEGMENT_WORD_THRESHOLD=500` 词分块（`word_count // 500` 向上取整），在 ±`SPLIT_SEARCH_RANGE=30` 词窗口内找**最大时间间隔**处切块，再用 `ThreadPoolExecutor` 并发；`_process_single_segment()` **先走 LLM 断句，异常自动回退规则断句**：① 按 500ms 时间间隔分组（大间隔 = 超近 5 个间隔均值 ×3 且组内 >5 词）；② 常见词切分——`and/or/but/和/及/与/但/而/或` 等**词前**切（需已达上限 ×0.6），`. , ! ? 。，！？/的/了/着/过` 等**词后**切（需已达上限 ×0.4）；③ 超长段递归二分。LLM 结果再用滑动窗口 + `difflib`（阈值 0.5）回贴原时间戳。
- **落地建议**：Voice2Word 在标点恢复之后加一层「语义断句」开关（LLM 可用本地 Qwen，失败自动降级到现有规则），并把「最大行字数」按 CJK / 英文分设（参考其 25/18 与函数默认 18/12）。

### 【中】9. CLI 设计与诊断命令（doctor）

- **对应现状**：Voice2Word 只有 GUI，无自动化/脚本入口；CI 与问题排查都靠手动。
- **借鉴做法**：`cli/main.py` 用 argparse 子命令划分（gui / transcribe / subtitle / synthesize / dub / process / download / style / config / doctor），参数用分组 helper（`_add_llm_options` / `_add_output_options` / `_add_style_options` / `_add_common_options`）复用；**高级参数用 `help=argparse.SUPPRESS` 隐藏**（如 `--target-language` 与 `--to` 并存、`--fw-vad-threshold` 调优项）；`transcribe` 会做输入校验（文件不存在 → 3，参数非法 → 2）与输出路径解析（目录 → `{stem}.{fmt}`、缺后缀自动补），whisper-api 时把 key/base 注入 `OPENAI_API_KEY` / `OPENAI_BASE_URL` 环境变量；`doctor` 用 `Check(name,status,message,fix)` dataclass 逐项体检（Python 版本区间、ffmpeg/ffprobe、yt-dlp 是否超 90 天、配置完整性、可选 `--check-api`），支持 `--json`，打印 `OK/WARN/ERROR` + `fix:` 建议命令。
- **退出码体系**（`cli/exit_codes.py`）：0 SUCCESS / 1 GENERAL_ERROR / 2 USAGE_ERROR / 3 FILE_NOT_FOUND / 4 DEPENDENCY_MISSING / 5 RUNTIME_ERROR。
- **落地建议**：Voice2Word 加 `voice2word-cli`（`transcribe / export / doctor / models`），`doctor` 复用现有「硬件监控 + 检查更新」逻辑，输出 JSON 便于用户贴到 issue；退出码体系直接照搬。

### 【中】10. 打包脚本的自校验与产物命名

- **对应现状**：Voice2Word 用 Inno Setup + `build_release.ps1`，但无「必需资源校验」与 smoke test，也无代码签名。
- **借鉴做法**：`scripts/build_desktop.py`：`git describe` 定版本 → 生成 `_version.py` → 自动下载 ffmpeg/ffprobe → PyInstaller → **`verify_bundle()` 校验必需资源**（logo/字体/样式预设/ffmpeg/ffprobe，缺一即失败）→ 产出 `VideoCaptioner-{version}-{platform}.zip`（平台 tag 形如 `windows-x64`）；CI 在打包后跑 **smoke test**，tag 时自动上传 Release。
- **落地建议**：给 `build_release.ps1` 加 `Assert-RequiredFiles`（whisper-cli、模型清单、样式资源、VC 运行库）与「安装包静默安装后启动一次」的冒烟步骤；产物名固定为 `Voice2Word-{version}-windows-x64-setup.exe`，便于后续「检查更新」稳定匹配。

### 【中】11. 统一产物命名 + per-run 任务目录 + 内容寻址缓存

- **对应现状**：Voice2Word 中间产物与导出命名不统一，历史库靠内容指纹缓存但无「任务级」隔离。
- **借鉴做法**（PR #1150）：输出命名统一为 `{stem}.{tag}.{ext}`（tag = 语言码 / optimized / subtitled / dubbed），定义在 `core/application/output_paths.py` 单点；中间产物放 per-run 任务目录、成功后清理；TTS 片段做**内容寻址缓存**（相同文本 + 音色直接复用）。现状代码里的命名前缀（`【原始字幕】` / `【样式字幕】` / `【卡卡】` / `_final`）正是它要收敛掉的历史包袱，可作反例参考。
- **落地建议**：Voice2Word 引入 `output_paths.rs` 统一命名函数，导出 SRT/ASS/译文/双语时按 tag 生成，避免用户在 `output/` 里看到一堆 `xxx_final_v2.srt`。

### 【中】12. 校对台最小交互集与快捷键对齐

- **对应现状**：Voice2Word 校对台功能远超对方（多轨时间轴 + 波形 + ±0.1s 微调 + Ctrl+H 批量替换 + 撤销重做），但「点击字幕即跳播该句」的交互与快捷键习惯可以对齐。
- **借鉴做法**：VideoCaptioner 的校对界面只有「行表 + 点击跳句 + 快捷键」：Space 播放/暂停、←/→ ±3s、Ctrl+M 合并选中行、Delete 删除、Ctrl+T 重译，右键菜单复用同三项；跳播时把句末 −50ms 避免切到下一句；`_is_processing()` 期间禁用所有改数据操作。
- **落地建议**：① 把「点击行 → 跳播该句（末端留 50ms 余量）」做成 Voice2Word 校对台默认行为；② 统一 Space / ←→ / Ctrl+M / Delete 快捷键；③ 所有会改字幕的批量操作在流水线运行中置灰。这三条都是低成本、直接提升手感的小改动。

### 【低】13. Claude Code Skill（AI 助手直接调用 CLI）

- **借鉴做法**：仓库内置 `skills/SKILL.md`（frontmatter 声明 `name` / `description` / `allowed-tools: Bash(videocaptioner *, ffprobe *, ffmpeg -ss *)`），含 10 个使用场景 + 命令参考表，安装到 `~/.claude/skills/videocaptioner/`；README 有专门章节介绍。
- **落地建议**：Voice2Word 一旦有了 CLI，就可以照此提供一个 Skill，让 AI 助手直接驱动转写/导出，是低成本的生态扩展点。

### 【低】14. 国际化与社区/文档运营

- **借鉴做法**：Qt 侧用 `resource/translations` 做界面多语言；README 中英双语；issue 模板中英双语并强制附日志；VitePress 文档站 + SEO 全套 + 大量界面截图；Discussions 分四类 + QQ 群置顶 + 微信/捐赠；「用数据说明默认档位合理性」的选型表。
- **落地建议**：Voice2Word 目前文案硬编码中文、`docs/开发文档.md` 仍停留在 PySide6 旧实现（文档漂移）。可先做两件事：① 把用户可见文案抽到一个字符串表（为 i18n 留位）；② 用 VitePress 或纯 Markdown 站重建文档，把 `docs/` 下已有的基准/优化指南作为「性能」章节，并补上界面截图。这与「无 i18n / 无 a11y」的短板直接对应，优先级低但收益长期。