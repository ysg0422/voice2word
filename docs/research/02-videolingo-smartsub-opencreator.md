# 同类开源项目调研笔记（二）：VideoLingo / SmartSub / OpenCreator

> 调研对象：`Huanshere/VideoLingo`、`buxuku/SmartSub`、`krillinai/OpenCreator`（原 KrillinAI）
> 调研目的：为 **Voice2Word**（Windows 本地音视频字幕工具，Rust + GPUI 桌面 GUI）寻找可借鉴的产品设计、架构与工程实践
> 数据来源：GitHub REST API 实测 + 仓库原始文件内容（`config.yaml` / `config-example.toml` / `requirements.txt` / `package.json` / `go.mod` / `contents` / `releases` / `contributors` / `languages` 等）；
> 　　　　　star、fork、更新时间均为 **2026-10-12（Asia/Shanghai）** 的快照，会随时间漂移。
> 每节结构：一句话定位 → 核心功能 → 技术栈 / 架构 → 工作流 → 安装与分发 → UI/UX 与文档 → 维护与社区 → **对 Voice2Word 的可借鉴点**（标优先级）。文末附三者横向对比、共同趋势与合并优先级清单。

---

## 目录

- [VideoLingo（Huanshere/VideoLingo）](#videolingo)
- [SmartSub（buxuku/SmartSub）](#smartsub)
- [OpenCreator（krillinai/OpenCreator）](#opencreator)
- [三者横向对比 + 共同趋势](#三者横向对比--共同趋势)

---

## VideoLingo（Huanshere/VideoLingo · 18754 stars · Python · Apache-2.0）

- **一句话定位**：一个"Netflix 级"的 AI 视频字幕组——把语音识别、字幕切分、翻译、对齐、TTS 配音串成一条流水线，在 Streamlit Web UI 里一键跑完，输出双语字幕文件、硬字幕视频或配音视频（README 原话：Netflix-level subtitle cutting, translation, alignment, and even dubbing）。

### 核心功能
- **视频导入/下载**：支持本地上传（mp4/mov/avi/mkv/flv/wmv/webm 等，见 config.yaml `allowed_video_formats`），以及用 yt-dlp 从 YouTube 下载（`ytb_resolution` 默认 1080）。
- **词级语音识别与对齐**：默认本地 ASR 为 Qwen3-ASR + Qwen3-ForcedAligner（`whisper.qwen_model` 可选 1.7b / 0.6b），也可切换 ElevenLabs、MAI-Transcribe-2；WhisperX 需按文档手动安装；可选 Demucs 人声分离（`demucs: false`）。
- **字幕切分**：NLP + AI 双重切分（先句法/标点，再 LLM 按语义合并），`subtitle.max_split_length` 默认 20 词，可配置长度上限。
- **术语库**：内置 `custom_terms.xlsx` + AI 生成专业术语，落盘为 `output\log\terminology.json`，可人工调整（`pause_before_translate` 暂停点）。
- **翻译**：OpenAI 兼容 Chat Completions，支持 `reflect_translate`（反思回译）与 `translation_style` 风格改写；`pause_after_translate` 可在生成字幕前暂停、人工编辑 `output\log\translation_results.xlsx` 的 Translation 列。
- **字幕输出**：生成字幕文件（`_6_gen_sub.py`）、烧录进视频（`_7_sub_into_vid.py`，`burn_subtitles: true`）、支持双语字幕。
- **配音（TTS）**：`tts_method` 可选 edge_tts / openai_tts / fish_audio / f5tts / sf_fish_tts / sf_cosyvoice2 / gpt_sovits / custom_tts；含音色克隆参考音频（`_9_refer_audio.py`）与音频合并/变速（`speed_factor`、`min_subtitle_duration`、`min_trim_duration`、`tolerance`）。
- **一键启动 + 任务控制**：Streamlit 里一键处理，支持 pause / resume / stop，带详细日志与进度恢复。
- **HTTP API（替代旧的 Excel 批量模式）**：`uv run start.py --api`，端口 8000，交互式文档 `/docs`，串行批处理。
- **模型搜索框**：从服务商全量模型列表里搜索/过滤（streamlit-searchbox）。

### 技术栈 / 架构
- **主语言**：Python（GitHub languages：Python 707900、Jupyter Notebook 93828、Batchfile 3786、Dockerfile 1150、PowerShell 487）。
- **关键依赖（requirements.txt 实测）**：streamlit / streamlit-searchbox（UI）、fastapi / uvicorn（HTTP API）、yt-dlp（下载）、librosa / pydub / static-ffmpeg（音频、自动带 ffmpeg+ffprobe）、spacy（NLP 切分）、openai（LLM/TTS 客户端）、openpyxl / pandas（xlsx 术语与翻译表）、json-repair（容错解析 LLM 输出）、edge-tts、silero-vad（VAD）、qwen-asr==0.0.6、transformers、huggingface-hub、mlx-audio（Apple Silicon）、torch / torchaudio / torchvision（按平台分 wheel）、syllables / pypinyin / g2p-en（切分辅助）。
- **目录结构（顶层，实测 contents API）**：`core/`、`docs/`、`scripts/`、`tests/`、`translations/`、`.streamlit/`；入口文件 `start.py`、`launch.py`、`st.py`（Streamlit 主程序）、`api.py`（HTTP API）、`install.py` / `installer.py` / `setup.py` / `setup_env.py` / `runtime_libraries.py`（安装器）、`config.yaml`、`requirements.txt`、`custom_terms.xlsx`、`OneKeyStart.bat`、`Dockerfile`、`VideoLingo_colab.ipynb`、`.cursorrules`。
- **模块划分（core/ 实测，编号即流水线阶段）**：`_1_ytdlp.py`（下载）→ `_2_asr.py` / `_2_import_subtitles.py`（识别/导入字幕）→ `_3_1_split_nlp.py` / `_3_2_split_meaning.py`（切分）→ `_4_1_summarize.py` / `_4_2_translate.py`（摘要/翻译）→ `_5_split_sub.py` → `_6_gen_sub.py`（生成字幕）→ `_7_sub_into_vid.py`（烧录）→ `_8_1_audio_task.py` / `_8_2_dub_chunks.py` → `_9_refer_audio.py`（参考音频）→ `_10_gen_audio.py` → `_11_merge_audio.py` → `_12_dub_to_vid.py`（配音合成）；另有 `pipeline.py`、`task_runner.py`、`prompts.py`、`translate_lines.py`，及子目录 `asr_backend/`、`tts_backend/`（含 `custom_tts.py`）、`spacy_utils/`、`st_utils/`、`utils/`。
- **配置文件 config.yaml（实测）**：分"基础设置 / 高级设置 / 配音设置 / 附加设置"四大区；基础项有 `display_language`、`api.model`、`max_workers`、`target_language`、`demucs`、`whisper.qwen_model`、`burn_subtitles`；高级项（前缀 *，注释说明"不在 Streamlit 页出现，只能改 config"）有 `ffmpeg_gpu`（h264_nvenc 硬件加速）、`subtitle.summary_length`、`subtitle.max_split_length`、`reflect_translate`、`translation_style`、暂停点等；配音区有 `tts_method` 及各家 TTS 参数；附加区有 `model_dir: './_model_cache'`、允许的格式白名单、`spacy_model_map`（en/ru/fr/ja/es/de/it/zh 各对应 spacy 模型）、`language_split_with_space` / `language_split_without_space`（中/日等不使用空格分词）。

### 工作流（导入→导出）
1. 导入：上传本地视频/音频，或填 YouTube 链接用 yt-dlp 下载（`_1_ytdlp.py`）。
2. 可选 Demucs 人声分离 → ASR 识别并产出词级时间戳（`_2_asr.py`，Qwen3-ASR + ForcedAligner）。
3. NLP 粗切 + LLM 按语义合并成句（`_3_1` / `_3_2`，受 `max_split_length` 约束）。
4. 摘要上下文（`_4_1_summarize.py`）+ 生成/整理术语表 → 翻译（`_4_2_translate.py`，含反思与风格改写）。
5. 按长度限制切分字幕（`_5_split_sub.py`）→ 生成字幕文件（`_6_gen_sub.py`）→ 可选烧录进视频（`_7_sub_into_vid.py`）。
6. 配音支线：按句生成音频（`_8`~`_10`）、取参考音频做音色克隆（`_9_refer_audio.py`）、合并音频（`_11_merge_audio.py`）、配音合成回视频（`_12_dub_to_vid.py`）。
7. 导出：字幕文件（含双语）、硬字幕视频、配音视频；中间产物在 `output/`（含 `output/log`、`output/audio/raw.mp3`、`output/gpt_log`）。

### 安装与分发
- **Windows 一键**：下载最新 Release 的 Source code (zip) → 双击 `OneKeyStart.bat`，首次运行自动装 uv、Python 3.12、依赖与 FFmpeg（需联网），装完自动打开浏览器。
- **源码安装（Win/macOS/Linux）**：`git clone ... && uv run start.py`；之后 `uv run start.py` 再次启动。Apple Silicon 走 MLX，Intel Mac 走 CPU 识别。
- **Docker**：`docker build -t videolingo .` / `docker run -d -p 8501:8501 --gpus all videolingo`，面向 Linux NVIDIA，默认 CUDA 12.8.1/cu128。
- **HTTP API**：`uv run start.py --api`（8000 端口，`/docs`）。
- **Colab**：仓库内 `VideoLingo_colab.ipynb`。
- **是否 Windows exe**：无独立 exe，通过 `OneKeyStart.bat` 一键脚本安装。
- **支持平台**：Windows、macOS（Apple Silicon / Intel）、Linux。
- **GPU 要求**：非强制；本地 ASR 可用 CPU/MLX/CUDA，`ffmpeg_gpu` 开启需 GPU 支持 h264_nvenc，Docker 版建议 NVIDIA GPU。

### UI/UX 与文档亮点
- **Streamlit Web UI**（默认 8501），一键处理、多语言界面（`display_language`）、模型搜索框、pause/resume/stop 任务控制、详细日志与断点恢复。
- **官网/文档**：homepage `https://docs.videolingo.io`；仓库内 `docs/pages/docs/*`（如 start.zh-CN.md、docker.en-US.md、whisperx-manual.en-US.md、api.md）。
- **多语言 README**：`translations/` 下有 zh / zh-TW / ja / es / ru / fr 等多语种版本。
- **双语字幕**、**音色克隆**（GPT-SoVITS / CosyVoice2 / F5-TTS）为演示亮点。
- **联系方式**：GitHub Issues/PR、Twitter @Huanshere、邮箱 team@videolingo.io（README 未提 Discord/QQ 群，此处标注"未获取"）。

### 维护与社区
- **Star 数**：18754；**Fork**：2070；**Watcher(subscribers)**：107。
- **最近更新时间**：`pushed_at` 2026-09-30；`updated_at` 2026-10-12；仓库创建于 2024-08-09。
- **Issue/PR**：`open_issues_count` = 19（GitHub 该字段含 PR，未细分，标注"未获取"具体拆分）。
- **贡献者数量**：API 首页（per_page=100）返回 24 位，首位 Huanshere（906 commits）。
- **发版频率**：releases 首页 10 个版本，最新 v3.1.1（2026-09-27），此前 v3.1.0（2026-09-27）、v3.0.4（2026-09-15）、v3.0.1（2026-02-28）、v3.0.0（2025-05-05）、v2.2.1（2025-02-16）等，更新活跃。
- **License**：Apache-2.0。
- **Topics**：ai-translation、dubbing、localization、video-translation、voice-cloning。
- **默认分支**：main；仓库体积约 16284 KB。

### 对 Voice2Word 的可借鉴点
1. **【高】显式编号的阶段流水线 + 断点续跑**：借鉴 `core/_1`~`_12` 的"一文件一阶段"和 `pipeline.py` / `task_runner.py` 的编排。落到 Voice2Word：把 ASR→切分→翻译→对齐→导出定义成显式 `Stage` 枚举，每阶段产物落盘（如 `output/<job>/01_asr.json`、`02_split.json`…），失败只重跑单步，缓存可复用（对应其 `output/gpt_log` 复用思路）。
2. **【高】词级时间戳作为一等数据结构**：借鉴 Qwen3-ForcedAligner / WhisperX 的词级对齐 + silero-vad 去非语音（防止片头/噪声干扰对齐）。落到 Voice2Word：Rust 侧统一保存 word-level timestamps，字幕切分、翻译回填、时间轴调整都基于它，避免"先切后对"的漂移。
3. **【高】术语库与人工干预点**：借鉴 `custom_terms.xlsx` + AI 生成术语 + `terminology.json` 可编辑 + `pause_before_translate` 暂停校对。落到 Voice2Word：GPUI 里做可导入（xlsx/csv）的术语表编辑器，翻译前可选暂停让人工校对术语，保证人名/专有名词一致。
4. **【高】NLP + LLM 两级字幕切分**：借鉴 `_3_1_split_nlp.py` / `_3_2_split_meaning.py` 与 `spacy_model_map`、`language_split_with_space/without_space`（中/日不按空格切）。落到 Voice2Word：本地先按语言分派标点/空格规则做粗切，再可选调 LLM 按语义合并到 `max_split_length` 上限，兼顾准确与可控。
5. **【中】TTS 适配器层**：借鉴 `core/tts_backend/` 的多后端 + `custom_tts.py` 自定义适配 + `config.tts_method` 切换。落到 Voice2Word：定义 `trait TtsBackend`，内置 Edge/系统 TTS，其余以插件方式注册，配音后端与主流程解耦。
6. **【中】单文件配置 + 基础/高级分区**：借鉴 `config.yaml` 的四大分区、`*` 标记"仅 config 可改"、注释写明单位与取值。落到 Voice2Word：用单一 TOML/YAML 配置 + schema 校验，GUI 只暴露基础项，高级项（GPU 加速、切分阈值等）留在配置文件。
7. **【中】任务控制与进度恢复**：借鉴 Streamlit 的 pause/resume/stop + 详细日志 + 进度恢复。落到 Voice2Word：GPUI 侧实现可暂停的 stage 队列，每步落盘缓存，界面展示阶段进度与结构化日志，长任务可中断续跑。
8. **【低】多平台分发与 CLI/API 并存**：借鉴 `OneKeyStart.bat` 自动装 uv/Python/FFmpeg、`VideoLingo_colab.ipynb`、Docker，以及 `api.py` + `start.py --api` 的 HTTP 接口。落到 Voice2Word：Windows 出便携一键包（内嵌 ffmpeg），同时保留 CLI 与本地 HTTP API 供脚本/批处理调用。

> 数据来源：全部来自本机对 GitHub REST API 的实际请求（repos / readme / contents / releases / contributors / languages），取数时间 2026-10-12（Asia/Shanghai）。未获取到的字段一律标注"未获取"。

---

## SmartSub（buxuku/SmartSub · 5648 stars · TypeScript · MIT）

- **一句话定位**：开源跨平台桌面「一站式」音视频字幕工具——把「在线视频下载 → 语音转文字 → 字幕翻译 → 校对润色 → TTS 配音/声音克隆 → 字幕烧录合成」整条流水线装进一个 Electron 桌面应用，本地模型可完全离线跑通，并额外提供 111 个 MCP 工具与 `smartsub` CLI 供外部 AI Agent 与脚本调度。

### 核心功能
1. **在线视频下载**：yt-dlp（YouTube 等 1800+ 站点）+ lux（B 站/抖音/小红书等国内平台）双引擎按平台自动匹配，应用内一键安装/更新；可同时抓取平台官方字幕（含自动生成字幕）并在任务向导中自动配对；支持导入站点 Cookie（浏览器提取 / cookies.txt / 手动粘贴），Cookie 仅存本地。
2. **字幕生成（转写）**：8 类引擎逐任务切换——内置 whisper.cpp（默认，ggml 量化）、faster-whisper（CTranslate2，模型按需从 HuggingFace 下载）、FunASR（SenseVoice / Paraformer-zh）、Qwen3-ASR（0.6b/1.7b）、FireRedASR（AED large）、NVIDIA Parakeet（英/多语/日）、本地 Whisper CLI、云端听写（9 家服务商）；批量并发数可调；可选 AI 字幕精修（语义断句 + 批量校正同音字/语气词/标点，失败自动回退规则断句）；简繁转换、自定义字幕文件名、中文字幕去标点。
3. **字幕翻译**：20 个翻译服务——内置免费翻译（必应/谷歌免费接口，自动回退与限速）、百度、阿里云、腾讯、讯飞、火山引擎、豆包、小牛、DeepLX、Azure、Google、Ollama（本地模型）、DeepSeek、Gemini、通义千问、SiliconFlow、Azure OpenAI、DeerAPI 等；兼容任意 OpenAI 风格 API；输出纯译文或「原文+译文」双语；每个服务可在界面配置自定义请求参数并导出导入。
4. **字幕校对**：内置校对台逐句对照视频检查修改，撤销/重做、单条删除可恢复、AI 一键润色，并可随时呼出右侧 AI 助手对话式修改。
5. **TTS 配音与声音克隆**：独立配音工作台（一份字幕 + 可选视频，逐条合成并自动对齐时间轴）；本地免费引擎 Kokoro（多语 103 音色）、VITS（中文 174 音色）；声音克隆本地 ZipVoice 零样本（一段参考音频即建即用），云端支持火山引擎声音复刻 2.0、ElevenLabs 即时克隆；云端服务另有 Edge TTS 免费档、OpenAI 兼容端点、Azure Speech、豆包、Xiaomi MiMo；时间轴对齐含语速预控制、实测复核、静音间隙借用，超限行列入人工处理清单；逐行试听/换音色/重新合成，背景音可静音或 ducking；输出 wav/mp3、替换音轨、混音视频或 MKV 双音轨。
6. **视频合成（字幕烧录）**：硬字幕（永久烧进画面）与软字幕（流复制无损封装可切换字幕轨）双模式；字体/字号/颜色/描边/阴影/九宫格位置 + 多种预设样式，所见即所得实时预览。
7. **AI 创作助手（智能副驾）**：Ctrl/⌘+J 随时呼出，具备工作台上下文感知、多模态截图排障、文件与媒体拖拽对话（音视频/字幕/文稿/图片）。
8. **MCP 与 CLI 自动化**：111 个标准 MCP 工具与对应 CLI 命令，复用应用自带 Electron/Node 运行时（外部客户端**无需**另装 Node.js），一键导入 Cursor / 复制 Codex TOML 配置 / 注册 Claude Code；首次调用自动拉起无窗口 Headless 守护进程，与桌面端共享任务状态与模型资源。

### 技术栈 / 架构
- **桌面框架**：Electron 30 + Nextron 9（Next.js 14 + Electron），**不是 Tauri**。主进程代码在 `main/`，渲染层在 `renderer/`；根目录 `nextron.config.js`、`electron-builder.yml`、`dev-app-update.yml` 为构建/自动更新配置。
- **语言构成**（`/languages`）：TypeScript 7,623,440 字节（绝对主力）、JavaScript 1,454,018、Python 116,330、Shell 33,548、CSS 6,290。
- **UI 层**：React 18 + Tailwind CSS 3 + Radix UI（约 20 个 `@radix-ui/*` 包）+ lucide-react + sonner + vaul + cmdk + `@tanstack/react-virtual`；表单用 react-hook-form + zod；主题用 next-themes。
- **国际化**：next-i18next + react-i18next，`next-i18next.config.js`，README 有中文/英文/日文三版（`README.md` / `README_EN.md` / `README_JA.md`），`package.json` 里有 `check:i18n` 校验脚本。
- **媒体处理**：ffmpeg-static + `@ffmpeg-installer/ffmpeg` + fluent-ffmpeg；jassub（libass WASM 字幕渲染）、srt-webvtt、wavesurfer.js（波形）、opencc-js（简繁）、fontkit（字体嵌入）。
- **语音运行时**：`extraResources/` 内含 `addons/`（whisper 原生插件）、`sherpa/`（sherpa-onnx 原生库，承载 FunASR/Qwen3-ASR/FireRedASR/Parakeet 与本地 TTS）、`python-review/`（faster-whisper 自包含运行时），以及 VAD 模型 `ggml-silero-v6.2.0.bin`；构建钩子 `yarn native:fetch` / `sherpa:fetch` / `addon:fetch` 自动拉取。
- **云端 SDK**：`openai`、`@volcengine/openapi`、`@alicloud/alimt20181012` + `@alicloud/credentials`、`msedge-tts`、`@modelcontextprotocol/sdk`、isomorphic-git、electron-store、electron-updater、systeminformation（硬件探测）、zod。
- **仓库形态**：**单体单包仓库，不是 monorepo**——根目录下**没有** `apps/` 或 `packages/` 目录。顶层目录为 `.cursor/`、`.github/`、`.husky/`、`Changelog/`、`Design/`、`assets/`、`automation/`、`docs/`、`extraResources/`、`main/`、`openspec/`、`renderer/`、`resources/`、`scripts/`、`types/`；顶层文件含 `package.json`、`components.json`、`jest.config.cjs`、`CLAUDE.md`、`vercel.json`、`LICENSE`、`README.md`。其中 `automation/` 承载 MCP/CLI 与无窗口守护，`docs/` 是 Docusaurus 文档站点。

### 工作流（导入→导出）
1. **入口**：启动台（Launchpad）选择任务类型；拖入本地音视频/字幕文件，或粘贴 B 站/YouTube 链接在应用内直接下载（可顺带配对官方字幕）。
2. **参数**：设置原语言、目标语言等；选择转写引擎与模型（首次需在「引擎与模型」页下载语音模型，或改用云端听写免模型）。
3. **转写**：批量生成字幕（SRT/VTT 等），可选开启 AI 语义断句 + 批量校正。
4. **翻译**：选择翻译服务，输出纯译文或双语字幕。
5. **校对**：校对台逐句对照视频核对修改（撤销/重做、AI 润色、助手对话式改），确认后写盘。
6. **配音（可选）**：配音工作台逐条合成 → 时间轴对齐 → 试听/换音色 → 处理超限行 → 输出音频/替换音轨/混音/双音轨 MKV。
7. **合成导出**：视频合成台做硬字幕烧录或软字幕封装，导出成品视频/音频/字幕。

### 安装与分发
- **三平台安装包**（electron-builder 产物 + electron-updater 自动更新），每个 release 固定 10 个资产：`SmartSub_Windows_x64.exe`、`SmartSub_Mac_arm64.dmg` / `.zip`、`SmartSub_Mac_x64.dmg` / `.zip`、`SmartSub_Linux_amd64.deb`、`SmartSub_Linux_x86_64.AppImage`，外加 `latest.yml` / `latest-mac.yml` / `latest-linux.yml` 更新元数据。
- **版本与下载量**（API 实测）：最新 **v3.9.0（2026-09-24）**，`SmartSub_Windows_3.9.0_x64.exe` 下载 6184 次、`SmartSub_Mac_3.9.0_arm64.dmg` 1042 次、`SmartSub_Linux_3.9.0_amd64.deb` 135 次；历史峰值如 v3.7.0 的 Windows exe 达 11146 次。`package.json` 版本号 3.9.0。
- **其他分发渠道**：Homebrew cask（`brew tap buxuku/tap` + `brew install --cask smartsub`）；国内提供夸克网盘入口；官网 https://smartsub.linxiaodong.com/ 。
- **是否需要自备 API Key**：**不需要**。本地链路（whisper.cpp / sherpa-onnx 转写、内置免费翻译、本地 Kokoro/VITS/ZipVoice 配音、内置 ffmpeg 烧录）零成本、无用量限制；20 个翻译服务 / 9 家云端听写 / 6 类云端配音全部是可选增强。
- **GPU 要求**：支持 NVIDIA CUDA、AMD/Intel Vulkan、Apple Core ML / Metal；加速包在应用内按需下载（不必手动装 CUDA Toolkit），加载失败自动回退 CPU；macOS Intel 版仅 CPU。低配设备建议 tiny/base 模型。
- **是否 Docker**：README 与仓库顶层均**未提供** Docker 分发（未获取到 docker 相关文件）。仓库体积约 143 MB（含资源）。

### UI/UX 与文档亮点
- **桌面 GUI 布局**：启动台（任务入口）+ 校对台 + 配音工作台 + 视频合成台 + 「引擎与模型」统一管理页 + 设置页；README 内嵌界面截图 `resources/preview/home.png`、`merge.png`、`profread.png`。
- **全局 AI 助手面板**：Ctrl/⌘+J 呼出，支持上下文引用（当前播放进度/选中字幕行/错误日志）、相机截图多模态排障、文件与媒体拖拽对话。
- **多语言**：README 中/英/日三版；应用内 i18n 走 next-i18next，并有 `check:i18n` 校验。
- **文档**：`docs/` 是 Docusaurus 站点（`docusaurus.config.ts`、`sidebars.ts`、`src/`、`static/`、`docs/docs/`），正文引用真实文档路径 `docs/docs/features/ai-assistant.md`、`docs/docs/guides/automation.md`、`docs/docs/guides/automation-reference.md`；仓库内另有 `CLAUDE.md`、`openspec/`、`Changelog/`、`Design/`。
- **社区**：QQ 交流群 655348339；README 含微信赞赏码/支付宝收款码与 DigitalOcean 赞助标识。
- **质量工程**：`package.json` 内登记了极大量测试脚本（Playwright e2e + Jest 单测），覆盖校对、配音、合成、翻译、GPU 枚举/探测、任务持久化、存储路径、字幕样式、下载流水线等，另有 `size:check` 体积检查与 `smoke:*` 冒烟脚本。

### 维护与社区
- **stars 5648 / forks 444 / watchers(subscribers) 28**；创建于 **2024-05-21**，最近推送 **2026-10-12T01:38:28Z**（pushed_at），updated_at 2026-10-12T01:38:33Z —— 维护极其活跃，近乎日更。
- **open_issues_count = 53**（GitHub 该字段含 PR，未单独获取 issue/PR 拆分）。
- **贡献者**：`contributors?per_page=100&anon=true` 返回 **16 条**——buxuku(310)、匿名条目(105)、nightt5879(53)、web-flow(19)、MaurUppi(19)、magicdawn(10)、SNamiris(4)、LauraGPT(3)、eltociear(2)，其余 6 个账号各 1 次。核心作者占比极高。
- **发版频率**：单次请求返回 **20 个 release**（≥20）；v3.0.0（2026-06-23）到 v3.9.0（2026-09-24）约 3 个月内发了 10 个版本，基本周更。
- **License：MIT**（`LICENSE`，spdx_id 实测为 MIT）。topics：deepseek、faster-whisper、fireredasr、funasr、ollama、openai、qwen3-asr、sherpa-onnx、subtitle、translate、whisper、whisper-cpp。

### 对 Voice2Word 的可借鉴点
1. **【高】把「引擎与模型」做成独立管理页**：SmartSub 将引擎切换、模型下载（多源）、手动导入、GPU 加速包、加速模式（仅 CPU 回退）集中在一页。借鉴 → Voice2Word 用 GPUI 建一个 `Engines & Models` 视图，列出各 ASR 后端与模型路径/体积/显存占用，支持「导入本地模型」与「强制仅 CPU」开关，下载失败时给镜像源（如 hf-mirror）备选，避免用户被单一模型源卡死。
2. **【高】默认本地零成本全流程，云端只做可选增强**：SmartSub 明确承诺本地转写+免费翻译+本地 TTS+内置 ffmpeg 零 API Key 跑通，云端全部折叠为可选。借鉴 → Voice2Word 首次启动就走本地路径，把「云端 API」放进设置页的可选分区，并在翻译层提供「本地 → 免费源 → 用户 Key」的 provider 链与失败自动回退，UI 显示当前生效源。
3. **【高】校对台「逐句对照 + 可撤销 + 实时预览」三件套**：SmartSub 校对台逐句对照视频、撤销/重做、单条删除可恢复、AI 一键润色。借鉴 → Voice2Word 用 GPUI 做「左列表 + 右播放器」双栏，选中行与播放进度双向联动，所有编辑进撤销栈（Ctrl+Z/Ctrl+Y），编辑即刻反映到预览渲染，且区分「草稿态」与「已保存到文件」状态避免误写盘。
4. **【中】硬字幕烧录与软字幕封装做成同一套样式源**：SmartSub 提供硬烧与流复制软封装两种导出，样式（字体/描边/九宫格/预设）所见即所得。借鉴 → Voice2Word 统一用一份 ASS 生成逻辑同时喂给预览渲染器与 ffmpeg `subtitles` 滤镜，软字幕用 `-c copy` 封装，保证「预览 = 导出」，并内置若干样式预设。
5. **【中】先做 CLI，再挂 MCP 的自动化分层**：SmartSub 用 `smartsub` CLI + 111 个 MCP 工具 + Headless 守护，且复用自带运行时免装 Node.js。借鉴 → Voice2Word 先实现薄 CLI（`voice2word transcribe|translate|burn`）+ JSON stdin/stdout 与任务状态查询，作为 Rust 内核的稳定接口；随后再包一层 MCP server。好处是自动化能力与 GUI 共用同一内核，且不引入额外运行时依赖。
6. **【中】任务队列与并发控制显式化**：SmartSub 的批量转写/下载都有并发数可调、任务状态可轮询、Headless 与桌面端共享任务状态。借鉴 → Voice2Word 建立持久化任务表（落盘，可重启恢复）+ 可配置并发上限 + 每任务日志面板，把「任务失败原因」直接呈现给用户而不是只弹一个错误。
7. **【低】GPU 能力探测 + 自动降级 + 可读诊断**：SmartSub 应用内下载加速包、失败自动回退 CPU，并在诊断面板给出原因。借鉴 → Voice2Word 启动时探测 CUDA/Vulkan 可用性，加速初始化失败时自动降级到 CPU 并把原因写进日志面板；同时把「设备/后端/模型」三元组显示在状态栏，减少「为什么这么慢」的支持成本。
8. **【低】文档站点与 i18n 工程化、测试脚本成体系**：SmartSub 用 Docusaurus 建站、README 三语、`check:i18n` 校验、大量 e2e/单测脚本。借鉴 → Voice2Word 建 `docs/` 站点，把 CLI 与关键流程写成可执行示例；加一个 i18n 资源键一致性校验脚本（防止漏译），并为「转写/翻译/烧录」三条主链路补端到端测试，长期能显著降低回归成本。

> 数据来源：GitHub REST API 实测（repos / readme / contents / releases / contributors / languages / package.json）。未获取到的字段已标注「未获取」。

---

## OpenCreator（krillinai/OpenCreator · 12703 stars · TypeScript · Apache-2.0）

- **一句话定位**：原 KrillinAI，现已改名为 OpenCreator——不再是单纯的"视频翻译/配音工具"，而是**以 Codex CLI 为 Agent 引擎的本地 AI 创作工作台**：Web 与桌面共用同一套 React 前端，加一个本地 Fastify Runtime 与 Electron 宿主，把视频翻译、配音、下载、图像/视频生成、文案写作、火柴人动画等创作工具与 Agent 对话统一在同一套工作流状态机上（Apache-2.0，主语言 TypeScript，另含 1 MB 级 Go 运行时）。

### 核心功能
- **视频翻译 Video Translation（✅ 可用）**：导入本地或公开视频；云端或本地 Whisper 服务转录；用 LLM 做上下文断句、对齐、术语、翻译；可配双语字幕、配音或自定义音色样本、字幕样式、横屏/竖屏构图；导出 SRT / 音频 / 视频。README 明确写"支持 14 种源语言、101 种目标语言"，源语言为简中、英、西、法、意、巴葡、印尼、泰、日、韩、德、土、俄、马来。
- **视频下载 Video Downloader（✅）**：解析 YouTube、Bilibili、X、TikTok、Instagram、抖音、Facebook、小红书、Pinterest 的公开视频，比较格式后下载视频/音频；部分站点需要平台 cookie（README 说明 OpenCreator 不会自动导入浏览器 cookie，小红书需完整 `explore/<24位hex>` 链接）。
- **缩略图生成 Thumbnail Generator（✅）**、**图像生成 Image Generation（✅，GPT Image）**、**文章写作 Article Writer（✅，可导出 Markdown/HTML/PDF）**、**小红书文案 Xiaohongshu Posts（✅）**、**短视频脚本 Short Video Script（✅）**。
- **火柴人动画 Stick Figure Animation（✅）**：把文本或 YouTube 内容变成旁白、配音、一致角色分镜、字幕与可下载动画（内置角色素材在 `resources/stickman/`，渲染组件是 `packages/stickman-remotion`）。
- **智能配音 Smart Dubbing（✅）**：脚本→配音，可选音色、语速、情绪。
- **视频生成 Video Generation（✅）**：Seedance 按提示词/参考图生成，可预览、重生成、下载每个版本。
- **开发中**：Auto Clips（长视频高光切片）、Digital Avatar（数字人口播）。
- **Agent 能力**：对话式任务、Runs 时间线、审批、附件、Skills、MCP、定时任务（每个计划独占一条常驻会话）、通知、记忆（全局/项目/线程）、诊断。

### 技术栈 / 架构
- **语言构成（/languages）**：TypeScript 10,824,873 B、Go 1,022,601 B、CSS 633,314 B、JavaScript 392,385 B、HTML 92,125 B、Shell 1,036 B。全树 2836 个条目；文件数 .ts 946 / .tsx 203 / .go 172 / .json 208。
- **pnpm monorepo**：根 `package.json`（name `opencreator-agent`，`packageManager: pnpm@9.15.0`），要求 Node.js 22+。
- **`apps/`**：`web`（唯一前端实现，React 18 + Vite 6 + TypeScript，react-router-dom 6、codemirror、lucide-react、fuse.js）、`daemon`（本地 Fastify Runtime + Codex 适配器，Runtime API + SSE，只监听 127.0.0.1，除健康检查外都要 Bearer token）、`desktop`（Electron 43.7.7 + electron-builder 26 + electron-updater，主进程/预加载桥、托盘、原生通知、原生目录选择、自动更新）、`harness`（Runtime 命令行验证工具）。
- **`packages/`**：`protocol`（Web/Daemon/Desktop 共享运行时契约）、`skill-market`（技能市场模型）、`config`、`stickman-remotion`（Remotion 渲染）、`writing-templates`（写作模板 + schema + scripts + templates）。
- **`runtime/krillinai`**：Go 1.22 编写的 KrillinAI CLI/Server（`go.mod` module 名 `krillin-ai`；依赖 gin、BurntSushi/toml、aliyun SDK、go-ego/gse 等）。`internal/` 含 api、cli、deps、dto、handler、pipeline、router、server、service、storage、subtitle_style、ttsprovider、types、voices；`pkg/` 含 openai、aliyun、volcengine、minimax、funasr、fasterwhisper、whisper、whispercpp、whisperkit、whisperx、localtts、image、util。
- **配置文件**：`runtime/krillinai/config/config-example.toml` → 复制为 `config/config.toml`（CLI 以进程工作目录为基准加载）。段落与真实字段：`[app]`（segment_duration、transcribe_parallel_num、translate_parallel_num、transcribe_max_attempts、translate_max_attempts、max_sentence_length、short_subtitle_max_chars、target_language_first、proxy）、`[server]`（127.0.0.1:8888）、`[llm]`（OpenAI 兼容 base_url/api_key/model/json）、`[transcribe]`（provider 可选 openai / fasterwhisper / whisperkit / whisper.cpp / aliyun / volcengine；注释写明 fasterwhisper 不支持 macOS、whisperkit 只支持 M 芯片、fasterwhisper 可开 GPU 加速且 50 系显卡需开启）、`[tts]`（provider 可选 openai / aliyun / edge-tts / minimax / volcengine）。
- **媒体链**：FFmpeg/ffprobe、yt-dlp（内置版本 + 7 天检查更新、只在用户显式操作时安装、失败保留可用旧版本）、Whisper 家族（openai whisper / whisper.cpp / faster-whisper / WhisperKit，另有 `pkg/funasr` 与 `docs/funasr-transcription.md`）、LLM 翻译（GPT/DeepSeek/Qwen/Kimi/GLM/Grok/豆包/文心/混元/MiniMax）、TTS（OpenAI TTS、MiniMax、Edge TTS、阿里云、火山引擎）。
- **语音克隆**：`runtime/krillinai/pkg/aliyun/voice_clone.go`（阿里云声音复刻），对应 README 视频翻译里的"dubbing or a custom voice sample"。
- **本地数据**：SQLite `.runtime/app.sqlite` + `.runtime/runs`（脱敏日志/诊断）+ `.runtime/attachments` + `.runtime/workspaces`；凭据走系统凭据存储；Codex 会话仍在 `$CODEX_HOME`。
- **内置 Codex 运行时**：`resources/codex-runtime/{win32-x64,darwin-arm64,darwin-x64,linux-x64}/manifest.json`——桌面端自带 Codex CLI，用户无需装 Node/pnpm。
- **桌面安全加固**：ASAR 完整性、cookie 加密、关闭 RunAsNode / NODE_OPTIONS / Node CLI Inspector。

### 工作流（导入→导出）
1. **首次启动**：桌面端启动本地 Runtime、自动准备默认项目、扫描本机 Codex 配置——可一键"复用本机 ChatGPT 登录或 API key"，或用向导选 provider 填 model/endpoint/API key。
2. **导入**：本地文件或公开链接（yt-dlp 解析/下载，必要时填平台 cookie）。
3. **转录**：按 `[transcribe]` 选云端或本地 Whisper 引擎。
4. **LLM 处理**：断句、时间轴对齐、术语表、翻译（`[llm]`）。
5. **配音**：按 `[tts]` 生成目标语言音轨，或使用自定义音色样本。
6. **合成**：字幕样式 + 横屏/竖屏（`render-horizontal` / `render-vertical`）。
7. **导出**：SRT / 音频 / 视频；每步产出记录在 `krillinai_manifest.json`（默认键如 `origin_video` → `<workdir>/origin_video.mp4`、`origin_audio` → `<workdir>/origin_audio.mp3`），CLI 用 JSON Lines 输出进度、有退出码与错误约定、支持 `--dry-run`（`pipeline` 命令仅支持 dry-run 校验计划）。
8. **GUI 侧编排**：每个创作流程被建模为状态机（source input → configuration → generation → review → revision → export），工作区操作与对话指令进同一个状态机，进度/版本/结果双向投影；**每次修改生成新版本而不覆盖旧结果**。

### 安装与分发
- **桌面安装包（Releases）**：`OpenCreator-3.2.6-win-x64.exe`、`OpenCreator-3.2.6-mac-arm64.dmg`、`OpenCreator-3.2.6-mac-x64.dmg`（另有对应 `.zip` + `.zip.blockmap` 供自动更新），并附 `SHA256SUMS.txt`、`latest.yml` / `latest-mac.yml` / `latest-x64-mac.yml`（electron-updater 通道）。v3.2.6（2026-10-11）共 24 个资产。**桌面端只有 macOS（arm64/x64）与 Windows x64；Linux 桌面安装包未获取**。
- **CLI / Server 归档**：`KrillinAI-CLI-3.2.6-{linux-arm64,linux-x64,mac-arm64,mac-x64}.tar.gz`、`KrillinAI-CLI-3.2.6-win-x64.zip`，以及同平台的 `KrillinAI-Server-*`；另有 `Remotion-3.2.6-{darwin-arm64,darwin-x64,win32-x64}.tar.gz`（火柴人渲染组件单独发包）。
- **源码运行**：Node.js 22+、pnpm 9.15.0（`corepack enable`）、本机可用 codex CLI；`pnpm install && pnpm web:dev` → 打开 `http://127.0.0.1:19861/`（开发服务器按需拉起 daemon 并注入临时 Runtime token）；只跑后端用 `pnpm daemon:dev`。
- **打包命令**：`pnpm desktop:package`（目录）/ `desktop:dist`（安装包）/ `desktop:release`（正式发布入口）；`pnpm krillinai:package` 出独立 Server/CLI 归档。打包会重建 Web 并比对 `apps/web/dist` 与应用内嵌资源哈希，不一致即失败。
- **GPU 要求**：未获取统一的 GPU 门槛；仅 config.toml 提到 fasterwhisper 的 `enable_gpu_acceleration`（50 系显卡必须开启）。走云端 ASR/TTS 路线则不需要本地 GPU。
- **Docker**：未获取——当前仓库全树（2836 条目）中未发现 Dockerfile / docker-compose，旧 KrillinAI 时代的 Docker 部署方式在本仓库已不存在。
- **账户与订阅**：个人中心显示账号、已确认订阅与积分；登录跳 `https://www.open-creator.ai/device` 走设备码，本地 Daemon 自建会话；订阅走 Stripe Checkout / Billing Portal。

### UI/UX 与文档亮点
- **官网**：`https://www.open-creator.ai/en/`（来自 API 元数据 homepage 字段；页面内容未抓取）。
- **桌面客户端首启配置向导**：复用本机 Codex 登录或改用其它 provider，README 配了产品截图（`docs/images/product/opencreator-codex-setup.png` 等）。
- **界面语言**：Web/桌面支持简体中文、English、Svenska（README 原文），可自动识别系统语言或手动选择。
- **README 多语言**：英文为规范版（根 `README.md`），另有 `docs/{zh,ja,ko,id,es,fr,de,pt,ru,ar,th}/README.md` 共 11 个翻译版；CONTRIBUTING 有 10 种语言版本；仓库约定"全文翻译完成才加进语言切换器"。
- **社区入口**：Discord（discord.gg/3GwBGsjs8）、QQ 群 754069680、B 站 space.bilibili.com/242124650、AtomGit 镜像、Trendshift 徽章。
- **文档体系（真实文件）**：`docs/development/`（codex-image-generation、video-translation-components、runtime-connection-recovery、creator-error-diagnostics、remotion-components）、`docs/operations/`（opencreator-desktop-release-runbook.md、opencreator-desktop-windows-release.md）、`docs/plans/`、`docs/specs/`、`docs/performance/`、`docs/test-reports/`，以及 `docs/runtime-api-for-ui-v1.md`、`docs/visual-component-guidelines.md`、`docs/opencreator-user-guide-and-troubleshooting.md`、`docs/2026-07-03-codex-native-agent-runtime-design.md`。
- **技能化工作流**：`skills/` 下 7 个真实技能目录——krillinai-cli（含 references/cli-contract.md）、krillinai-subtitle、krillinai-tts、krillinai-render-horizontal、krillinai-render-vertical、krillinai-cover、krillinai-pipeline，每个一份 `SKILL.md`，并配 `docs/contributing/skills-contributing.md`。
- **模板库**：`template/image-generation/` 约 100 个模板目录 + `template/video-generation/` 约 30 个（真实目录名如 `y2k-streetwear-mobile-landing-page`、`vertical-crane-reveal`、`cinematic-shaolin-action`），另有 `template/cover-generator/`，配 `docs/contributing/templates-contributing.md`。
- **工程化亮点**：`.github/workflows/{ci,desktop-release,star-coverage}.yml`；`apps/desktop/e2e` 12 个文件（含 packaged-app E2E、fake-codex/fake-yt-dlp fixture、real-codex smoke，默认关闭，需 `OPENCREATOR_RUN_REAL_CODEX_SMOKE=1`）；Playwright + vitest + `pnpm perf:check` 性能基线；`star-coverage` 工作流生成星标国家/地区覆盖地图（README 称"至少 100 个国家和地区"的用户 star）。

### 维护与社区
- **指标（API 实测）**：stars 12703、forks 1312、watchers(subscribers) 59、open issues+PR 30（GitHub 该字段含 PR）、license Apache-2.0。
- **时间**：created_at 2024-12-17；pushed_at 2026-10-11T09:06:39Z；updated_at 2026-10-12T01:24:57Z；仓库体积 385478 KB（约 376 MB）；默认分支 `master`。
- **贡献者**：`contributors?per_page=100&anon=true` 返回 32 条（含匿名条目与重复登录名，去重后约 30 位）；README 另设维护者 Crew 名单：wulien（Code & Bug Fixes）、DLe-kb（Creation templates）、xiaheyuan（Design & assets）、krillinai（Skills & Docs）、hbxugang（Enterprise deployment）。
- **发版节奏**：v3.2.1(2026-09-20) → v3.2.2(09-21) → v3.2.3(09-24) → v3.2.4(10-02) → v3.2.5(10-04) → v3.2.6(10-11)，约每周一个小版本，全部非 prerelease，每版 21–24 个资产；仓库内还留有 `apps/desktop/release-notes/v3.2.1.md`～`v3.2.6.md`。
- **topics**：agent-skills、codex-cli、deskop-app、dubbing、image-generation、localization、skills、tts、video-editing、video-generation、video-transcription、video-translation。
- **改名说明**：`krillinai/KrillinAI` 与 `krillinai/OpenCreator` 两个路径都可用，API 返回同一个 id 904490600（旧名 301 到新名）。

### 对 Voice2Word 的可借鉴点
- **【高】把流水线做成独立 CLI，GUI 只做编排**：OpenCreator 的创作能力全在 `runtime/krillinai`（Go CLI/Server），Electron 只负责调起、读进度、渲染可视化，产出统一写进 `krillinai_manifest.json`。借鉴：在 Rust workspace 里加一个 `voice2word-cli` bin（与 GPUI 共用同一套 core crate），GPUI 只调用它并解析 manifest/JSON Lines——批处理、脚本化、CI 回归都不用穿过 GUI。落地：定义 `manifest.json`（输入路径、音频/字幕/成片输出、模型版本、耗时）作为唯一交付契约。
- **【高】阶段子命令 + `--dry-run` 计划校验**：CLI 拆成 subtitle / tts / speech / render-horizontal / render-vertical / cover / voices / pipeline，且 `pipeline` 只做 dry-run 校验计划、真正执行走各阶段命令。借鉴：Voice2Word 拆出 `extract-audio`、`vad-split`、`asr`、`segment`、`translate`、`tts`、`burn-subtitle`、`render` 子命令，加 `--dry-run` 输出计划与预估耗时/磁盘占用，再提供 `all` 串联；GPUI 的进度条直接映射阶段事件。
- **【高】单一状态机 + 产物版本化（不覆盖历史）**：README 的 Core principles 明确"工作区与对话是同一状态机的两个投影""每次修订生成新版本而非替换结果"。借鉴：Voice2Word 每个任务保存 revisions（字幕 JSON、样式、音轨、成片路径、参数快照），GUI 给版本列表 + 对比 + 回滚，解决"重跑一次就把上次结果冲掉"的痛点。
- **【中】把并发/重试/断句阈值全暴露到 config.toml**：`config-example.toml` 暴露了 segment_duration、transcribe_parallel_num、translate_parallel_num、transcribe/translate_max_attempts、max_sentence_length、short_subtitle_max_chars、target_language_first（双语字幕目标语言是否在上）、proxy。借鉴：Voice2Word 用 TOML（Rust 生态成熟）做同名配置，GUI 提供"高级"面板改这些值并做范围校验（如建议值区间），比硬编码更抗不同机器/不同素材。
- **【中】多 provider 抽象 + 无 key 本地回退**：ASR 抽象 openai/fasterwhisper/whisperkit/whisper.cpp/aliyun/volcengine，TTS 抽象 openai/aliyun/edge-tts/minimax/volcengine，且 Edge TTS 这类系统级 provider 不需要 API Key。借鉴：Voice2Word 用 Rust trait（AsrProvider/TtsProvider/TranslateProvider）+ 注册表，默认走本地（whisper.cpp / Windows 内置语音 / 免 key 通道），云端作可选增强；凭据写 Windows 凭据管理器（对应其"系统凭据存储"），配置文件只留 provider 名与非敏感字段。
- **【中】分发形态与"第三方组件"更新策略**：桌面安装包只出 macOS + Windows x64（electron-updater 的 latest.yml + blockmap + SHA256SUMS.txt），Linux 只发 CLI/Server 归档，渲染组件单独发包；yt-dlp 走"内置版本 vs 当前版本 vs 最新版本 + 7 天检查、绝不自动安装、失败回滚旧版本"。借鉴：Voice2Word 发布物拆成"GUI 安装包（MSI/portable exe + 更新清单 + SHA256SUMS）+ 独立 CLI zip"，把 ffmpeg、whisper 模型、yt-dlp 当作可独立更新的"第三方组件"页（显示三个版本号、手动更新、失败保留旧版），避免整包重发几百 MB。
- **【中】技能/模板化预设 + 多语言文档站**：7 个 `SKILL.md` 把"字幕/配音/横竖屏/封面/流水线"写成人和 Agent 共读的操作手册，`template/` 下有上百个可直接套用的模板目录，`docs/` 分 development/operations/plans/test-reports 且有 12 种语言 README。借鉴：Voice2Word 内置若干 preset（长视频课程字幕、短视频竖屏双语、访谈双人字幕……用 TOML/JSON 描述语言对、断句策略、样式、导出格式），并在仓库维护 `docs/zh/` + `docs/en/`、发布手册（对应其 desktop release runbook）与 CHANGELOG，降低上手与排障成本。
- **【低】本地优先 + 可导出的脱敏诊断包**：daemon 只监听 127.0.0.1、除健康检查外都要 Bearer token、Run 日志与诊断先脱敏再返回、敏感记忆需二次确认。借鉴：Voice2Word 的本地接口（HTTP/命名管道）沿用"仅回环 + token"，并加"导出诊断包"按钮（版本、GPU/驱动、模型路径、最近 N 次任务日志、脱敏后的 config），显著降低 Windows 环境类问题的沟通成本。

> 数据来源：GitHub REST API 实测（repo 元数据、readme、contents、git/trees?recursive=1、releases、contributors、languages）+ 仓库原始文件内容（config-example.toml、go.mod、根与 apps 的 package.json、skills/krillinai-cli/references/cli-contract.md）。凡 API 未返回或仓库中不存在的字段，已标注"未获取"。

---

## 三者横向对比 + 共同趋势

### 一、总览对比表

| 维度 | VideoLingo | SmartSub | OpenCreator |
| --- | --- | --- | --- |
| 仓库 | Huanshere/VideoLingo | buxuku/SmartSub | krillinai/OpenCreator（原 KrillinAI，同一 repo id，旧名 301） |
| Star（2026-10-12 快照） | 18754 | 5648 | 12703 |
| 主语言 / 许可 | Python / Apache-2.0 | TypeScript / MIT | TypeScript（另含 Go 运行时）/ Apache-2.0 |
| 产品形态 | Streamlit 本地 Web 服务（浏览器操作） | Electron 桌面应用 | Electron 桌面 + Web + CLI/Server 三形态 |
| 支持平台 | Windows / macOS / Linux + Docker + Colab | Windows / macOS / Linux（三平台安装包） | 桌面仅 Windows x64 + macOS；Linux 只发 CLI/Server |
| 本地优先程度 | 中：ASR 可本地，翻译/配音偏 API | 高：转写+翻译+TTS+烧录可全离线、零 API Key | 中：默认云端 provider，内置 Codex 运行时 |
| 语音识别 | Qwen3-ASR + ForcedAligner、WhisperX、ElevenLabs | 8 类引擎（whisper.cpp / faster-whisper / sherpa-onnx 系 / Qwen3-ASR / FireRedASR / Parakeet / 本地 CLI / 云端 9 家） | 6 provider（openai / fasterwhisper / whisperkit / whisper.cpp / aliyun / volcengine） |
| 字幕切分 | NLP 粗切 + LLM 语义合并 | 规则断句 + AI 语义断句（失败回退规则） | LLM 上下文断句 |
| 翻译 | LLM + 术语库 + 反思回译 | 20 个服务 + 内置免费源自动回退 | LLM + 术语表 + 双语 |
| 配音 / 声音克隆 | 多 TTS 后端（8 种）+ 音色克隆 | 本地 Kokoro/VITS/ZipVoice 零样本 + 云端克隆 | 5 provider（含 edge-tts 免 Key） |
| 字幕烧录 | 硬字幕 | 硬字幕 + 软字幕流复制双模式 | 横屏 / 竖屏渲染 |
| 自动化接口 | HTTP API（FastAPI，/docs） | CLI + 111 个 MCP 工具 + Headless 守护 | CLI/Server + MCP + Skills |
| 配置方式 | 单文件 config.yaml（基础 / 高级分区） | 应用内设置 + 独立「引擎与模型」页 | 单文件 config.toml（字段全暴露） |
| 文档 / 社区 | 文档站 + 6 语言 README + Twitter | Docusaurus 文档站 + 3 语言 README + QQ 群 | 文档站 + 12 语言 README + Discord / QQ |
| 维护热度 | pushed 2026-09-30，贡献者 24，最新 v3.1.1 | pushed 2026-10-12（近乎日更），贡献者 16，最新 v3.9.0 | pushed 2026-10-11，贡献者 32，最新 v3.2.6（周更） |

### 二、共同趋势

1. **本地优先、零成本可跑通，已是开源字幕工具的入场券。** SmartSub 明确承诺本地转写 + 免费翻译 + 本地 TTS + 内置 ffmpeg 全链路离线零 API Key；VideoLingo 把 ASR/TTS 本地化、云端仅作可选项；OpenCreator 也在 config 里把 fasterwhisper / whisper.cpp 与 edge-tts 这类免 Key 通道与云端并列。三者都把「云端」降级为可选增强，而非默认路径。
2. **多引擎 provider 抽象 + 失败自动回退已成标配。** 三家都定义了统一的识别 / 翻译 / 配音后端接口（SmartSub 8+20+6、OpenCreator 6+5、VideoLingo 多 ASR + 8 TTS），并普遍带「本地 → 免费源 → 用户 Key」的降级链与「加速不可用自动回退 CPU」。
3. **流水线被显式阶段化，产物落盘、可断点续跑。** VideoLingo 用 core/_1~_12「一文件一阶段」+ pipeline.py 编排；OpenCreator 拆成 subtitle / tts / render-horizontal / render-vertical / pipeline 子命令并统一写 manifest；SmartSub 把转写 → 翻译 → 校对 → 配音 → 合成做成互相独立、可单独重跑的工作台。
4. **词级时间戳 / 强制对齐是质量控制的地基。** VideoLingo 用 Qwen3-ForcedAligner / WhisperX 产出词级时间戳再做切分与配音对齐；SmartSub 的配音台有语速预控制、实测复核与静音间隙借用。对齐精度决定字幕卡点与配音自然度。
5. **术语库 + 人工干预暂停点，是「专业感」的来源。** VideoLingo 的 custom_terms.xlsx / terminology.json + pause_before_translate；OpenCreator 的术语表 + 人工审校环节；都把「专有名词全篇一致」当作翻译质量的核心卖点。
6. **校对台三件套（逐句对照 + 撤销/重做 + 实时预览）成为标配。** SmartSub 校对台逐句对照视频、单条删除可恢复、AI 一键润色；三者都强调「所见即所得」——预览样式与最终导出走同一套渲染逻辑。
7. **GUI 与 CLI/API 共用同一内核，并向 MCP / Agent 延伸。** SmartSub 的 CLI 与 111 个 MCP 工具复用应用自带运行时、免装 Node.js；OpenCreator 的 Electron 只做编排、全部能力在 Go CLI/Server 里；VideoLingo 也提供 FastAPI HTTP 接口。自动化能力不再与 GUI 割裂。
8. **分发工程化：三平台安装包 + 自动更新 + 第三方组件单独更新。** 三家都用 electron-updater / 一键脚本 + latest.yml + SHA256SUMS 分发；OpenCreator 把 yt-dlp、渲染组件当作可独立更新的「第三方组件」（内置版本 vs 当前版本 vs 最新版本 + 7 天检查、绝不静默安装、失败保留旧版），避免整包重发。
9. **配置外置 + 基础/高级分区，文档站 + 多语言 + 社区群。** 单文件 config（yaml/toml）承载并发数、重试次数、断句阈值等高级项；界面只暴露基础项。三家都有官网/文档站、多语言 README（3~12 种）与 Discord/QQ 群。
10. **产物版本化，不覆盖历史结果。** OpenCreator 明确「每次修订生成新版本而非替换结果」，工作区与对话是同一状态机的两个投影；这是长任务创作工具避免「重跑一次冲掉上次成果」的关键设计。

### 三、给 Voice2Word 的合并优先级清单

| 优先级 | 借鉴点 | 主要来源 |
| --- | --- | --- |
| 高 | 显式 Stage 枚举 + 每阶段产物落盘 + 断点续跑 | VideoLingo core/_1~_12、OpenCreator CLI 阶段子命令 |
| 高 | 词级时间戳作为一等数据结构，切分/回填/时间轴都基于它 | VideoLingo ForcedAligner / WhisperX |
| 高 | 独立「引擎与模型」管理页（切换 / 下载 / 导入 / 删除 / 显存 / 强制仅 CPU / 镜像源） | SmartSub |
| 高 | 校对台三件套：逐句对照 + 撤销/重做 + 预览与导出同源 | SmartSub、VideoLingo |
| 高 | 翻译 provider 链 + 免费源自动回退，UI 显示当前生效源 | SmartSub、OpenCreator |
| 高 | 术语库可编辑（导入 csv/xlsx）+ 翻译前人工暂停点 | VideoLingo |
| 高 | 流水线抽成独立 CLI，GUI 只做编排（共用 core crate + manifest/JSON Lines） | OpenCreator、SmartSub |
| 中 | 产物版本化与回滚（revisions + 版本对比） | OpenCreator |
| 中 | 第三方组件（ffmpeg / 模型 / yt-dlp）独立更新，失败保留旧版 | OpenCreator、SmartSub |
| 中 | 配置外置 + 高级项（并发 / 重试 / 断句阈值）暴露并做范围校验 | 三者共性 |
| 低 | CLI 之上再挂 MCP server，供外部 Agent 复用同一内核 | SmartSub |
| 低 | 文档站 + i18n 资源键校验脚本 + 主链路端到端测试 | SmartSub、OpenCreator |

---

> 说明：本文所有 star / fork / 更新时间 / 目录与文件路径均来自实际 API 请求与仓库原始文件，未获取到的字段在各节内已标注「未获取」。star 等指标为 2026-10-12 快照，阅读时请以 GitHub 实时数据为准。
