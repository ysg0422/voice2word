# 同类开源项目调研（三）：其他字幕 / 本地化工具

> 调研日期：2026-10-12。数据来自 GitHub API（stars / license / 语言为抓取时的实际值）。
> 本文件只覆盖「其它同类项目」，与 01 / 02 两篇不重叠。

## YouDub WebUI（liuzhao1225/YouDub-webui · 5643★ · Python · Apache-2.0）

- **一句话定位**：把 YouTube / Bilibili / 本地视频端到端转成目标语言版本（硬字幕、配音、或两者兼备）的本地化流水线。
- **核心功能**：yt-dlp 下载 → Demucs 分离人声与背景音 → Whisper 词级时间戳识别 → OpenAI 兼容 API 并发翻译 → VoxCPM2 配音合成 → FFmpeg 压制输出；三种输出内容（硬字幕保留原音 / 无字幕配音 / 两者）；可上传已翻译 `.srt` 跳过识别与翻译。
- **技术栈 / 分发**：Next.js + shadcn/ui + Tailwind 前端，FastAPI + SQLite + 进程内后台 worker 后端，yt-dlp / Demucs / openai-whisper / VoxCPM2 / FFmpeg；仅源码部署（Python 3.12 + Node 20 + CUDA），无安装包。
- **UI/UX 与工程亮点**：任务按阶段（stage）建模，每个阶段有状态、耗时、日志与错误；失败任务可从失败阶段续跑并复用已成功阶段的缓存产物；Demucs 分轨用「pending 文件 → 原子替换 final」的发布协议避免半成品；翻译默认 50 并发；本地优先（SQLite / Cookie / 中间产物全在本机），后端强制 Argon2id 登录 + HttpOnly 会话 Cookie + 每会话 CSRF token。
- **对 Voice2Word 的可借鉴点**：
  1. 阶段化任务模型 + 断点续跑 + 原子产物发布（pending→final）：Voice2Word 的批量队列可升级为「每阶段产物落盘 + 失败阶段单独重试」，避免崩溃后整条流水线重跑。
  2. 「一次转写、多产物分支」的流水线组织方式，与 Voice2Word 的导出内容三态（仅原文 / 仅译文 / 双语）同构，可参考其把分支放在末端的做法，保持前端与导出共用一份时间轴。
  3. 人声分离 + 配音是 Voice2Word 的空白区；即便暂不做 TTS，Demucs 分轨也可用于「背景音乐干扰下的识别增强」。

## Video Subtitle Extractor（YaoFANGUK/video-subtitle-extractor · 9582★ · Python · Apache-2.0）

- **一句话定位**：把视频中烧录的硬字幕 OCR 提取成 srt 的本地深度学习工具，无需任何在线 OCR API。
- **核心功能**：关键帧抽取 → 字幕区域检测 → 文本识别 → 非字幕区域过滤（水印 / 台标）→ 重复行去除 → 生成 srt / txt；支持 87 种语言、批量提取；`backend/configs/typoMap.json` 可做错字替换与指定文本删除。
- **技术栈 / 分发**：PaddlePaddle + PaddleOCR，字幕检测引擎用 VideoSubFinder，支持 CUDA / CPU / DirectML / ONNX 四种后端；PyQt 图形界面（`gui.py` + `ui/`）与 CLI（`backend/main.py`）；Release 提供解压即用压缩包。
- **UI/UX 与工程亮点**：用「快速 / 自动 / 精准」三档模式把「速度 vs 召回」的选择交给用户，并在文档里列出每档的模型尺寸与检测引擎；对「路径含中文或空格会出错」这类环境坑有显式提示。
- **对 Voice2Word 的可借鉴点**：
  1. 「快速 / 自动 / 精准」三档预设 + 每档标注模型体积与代价，可直接搬到 Voice2Word 的 Whisper 档位选择器上，从「列模型」升级为「选档位」。
  2. 用户可编辑的 typoMap（替换表 / 删除表）与 Voice2Word 的术语表互补：可增设「清洗替换表」处理同音错字与广告词。
  3. 硬字幕 OCR 提取是 Voice2Word 的空白入口，可作为「导入已有字幕」之外的另一条输入路径。

## AutoSubs（tmoroney/auto-subs · 4345★ · TypeScript · MIT）

- **一句话定位**：端上字幕生成器，可独立使用，也能直连 DaVinci Resolve、Premiere Pro 与 After Effects。
- **核心功能**：语音转带时间戳字幕、说话人标注、1000+ 语言转写与翻译、可选词级时间戳（MMS 强制对齐）；多族 ASR 模型（Whisper GGML、Moonshine、Parakeet、SenseVoice、Canary、Cohere、GigaAM、Omni-ASR）+ Silero VAD + 说话人分离；提供 CLI。
- **技术栈 / 分发**：Tauri（Rust `src-tauri` + React / Vite / TypeScript），Rust 侧用 whisper-rs / transcribe-rs / pyannote-rs；分发 Windows `.exe`、macOS `.pkg`（含 Homebrew cask）、Linux `.deb` / `.rpm`；另有 Resolve 的 Lua server + Fusion 宏与 Adobe 的 CEP 扩展。
- **UI/UX 与工程亮点**：以「插件式桥接」进剪辑软件而非只导出文件（Resolve 走 Workspace→Scripts 脚本 + Fusion 宏，Adobe 走 CEP）；模型管理页统一下载与切换，每个模型标注体积、内存占用与准确度星级；可选权重单独声明许可（MMS 为 CC BY-NC 4.0）。
- **对 Voice2Word 的可借鉴点**：
  1. 模型选择器按「体积 / 内存 / 语言 / 准确度星级」四列呈现，正对应 Voice2Word「性能设置 → Whisper 档位」的信息密度升级。
  2. 与 Voice2Word 现有的 FCPXML / Premiere XML 导出互补：可考虑 Resolve 脚本或宏式直连，把「导出后手动导入」变成「一键回传时间线」。
  3. 「可选模型单独标注许可、非商用权重单独下载」的做法，可作为 Voice2Word 引入第三方模型时的合规模板。

## Caption Trans（cddqssc/Caption-Trans · 858★ · Dart · MIT）

- **一句话定位**：面向日语优化的「本地转录 + LLM 翻译」一站式桌面工具。
- **核心功能**：本地转录（WhisperX 侧车）→ 调用 Gemini / OpenAI / DeepSeek 等 OpenAI 兼容 API 翻译字幕；macOS（Apple 芯片）与 Windows（NVIDIA）自动 GPU 加速；支持 macOS arm64 与 Windows x64。
- **技术栈 / 分发**：Flutter 桌面应用（flutter_bloc 状态管理 + ffmpeg_kit 系列做音频处理），本地推理用打包的 `whisperx_worker.py` 侧车进程（`runtime_manifest.json` 描述运行时）；Release 提供 macOS dmg 与 Windows zip，仓库体积仅约 3 MB。
- **UI/UX 与工程亮点**：用「Flutter 界面 + Python 侧车」绕开 Dart 生态缺少 ASR 绑定的问题，架构简单、打包体积小；内置更新服务（`update_service.dart`）；README 明确推荐 gemini flash lite 作为高性价比翻译模型，并给出 macOS 未签名应用的打开指引。
- **对 Voice2Word 的可借鉴点**：
  1. 「UI 壳 + 独立推理侧车」的分层与 `runtime_manifest.json` 式清单：Voice2Word 已是 Rust 主进程 + whisper 子进程，可把模型与运行时版本、路径、校验集中声明，直接服务于模型管理与诊断报告。
  2. 面向单一语向（日→中）专项优化提示词与预处理的思路，可对应 Voice2Word 的「目标语言分档 + 术语表」做成语向模板。
  3. 发布资产命名规范（`caption_trans-macos-arm64.dmg` / `caption_trans-windows-x64.zip`）值得借鉴，便于「检查更新」按钮稳定匹配平台产物。

## Whisper-WebUI（jhj0517/Whisper-WebUI · 2895★ · Python · Apache-2.0）

- **一句话定位**：基于 Gradio 的 Whisper 网页界面，定位「好用的字幕生成器」。
- **核心功能**：可选 openai/whisper、faster-whisper（默认）、insanely-fast-whisper 三种实现；来源支持文件 / YouTube / 麦克风；输出 SRT、WebVTT、txt；Whisper 端到端语音转英文、字幕文本翻译（NLLB 模型或 DeepL API）；Silero VAD 预处理、UVR 分离 BGM、pyannote 说话人分离；另提供 REST API。
- **技术栈 / 分发**：Python + Gradio + PyTorch；Docker / docker-compose（镜像约 7 GB）、Pinokio 一键安装、`install.bat/sh` 脚本本地运行；模型目录支持直接填 HuggingFace repo id 自动下载。
- **UI/UX 与工程亮点**：README 用表格对比 openai/whisper 与 faster-whisper 的耗时与显存（4m30s / 11325MB vs 54s / 4755MB），把「为什么默认 faster-whisper」讲清楚；`configs/translation.yaml` 集中管理界面多语言翻译，方便社区贡献。
- **对 Voice2Word 的可借鉴点**：
  1. 「用数据说明默认档位合理性」的对比表，适合放进 Voice2Word 的档位说明与界面提示（项目已有 Whisper 基准文档，可复用成 UI 文案）。
  2. 模型下拉框支持填 HuggingFace repo id 自动下载，可扩展 Voice2Word 的自定义 `.gguf` 输入（目前支持本地路径，可加 HF / ModelScope 标识与一键拉取）。
  3. Whisper 原生的「语音直接转英文」能力，可作为 Voice2Word 翻译环节之外的一条补充路径。

## VideoSubtitleGenerator（buxuku/VideoSubtitleGenerator · 877★ · JavaScript · MIT）

- **一句话定位**：批量为本地视频生成字幕并翻译成其它语言的命令行工具，跨 Windows / macOS。
- **核心功能**：ffmpeg 抽音频 → whisper.cpp 生成原语言 srt → 调翻译服务生成目标语言 srt；批量处理整个目录、可只生成不翻译；支持火山 / 百度 / deeplx / ollama 四种翻译；字幕文件名与内容模板均可配置（仅译文 / 原文+译文 / 译文+原文）。
- **技术栈 / 分发**：Node.js（`index.js` + `service/` + `translate.js`），内置 whisper.cpp 二进制（`whisper-bin-x64/`）与 fluent-ffmpeg，无需另装 ffmpeg；npm / yarn 安装后 `npm start` 运行；作者另做了带界面的 SmartSub。
- **UI/UX 与工程亮点**：以「目录即队列」为交互范式——原/译字幕与视频同目录同名，播放器可任意挂载；配置集中在 `config.js` 且逐项带注释；模板字符串支持 `${fileName}` / `${sourceLanguage}` / `${targetLanguage}` 变量。
- **对 Voice2Word 的可借鉴点**：
  1. 「字幕与视频同目录同名」的落盘即挂载约定，可作为 Voice2Word 批量模式的默认输出策略（目前需导出到自选文件夹）。
  2. 字幕内容模板 + 文件名模板的变量体系，与 Voice2Word 的 `{name}` / `{ext}` / `{date}` 模板同源，可补充「原文与译文的上下顺序」以及语言代码变量。
  3. 目录级批处理 + 「只生成不翻译」的分阶段开关，可映射为 Voice2Word 队列中的「跳过翻译」快捷选项。

## 共同模式与差异化机会

- **输入侧都在「去平台化」**：下载器（yt-dlp）、目录扫描、剪辑软件桥接，本质是把「取音视频」这一步做到零手工。Voice2Word 已有拖拽文件夹与导入字幕，可补齐 URL 入口与剪辑软件直连。
- **引擎抽象层是共识**：几乎所有项目都支持多 ASR / 多翻译后端切换（VSE 四后端、Whisper-WebUI 三实现、AutoSubs 八族模型、VSG 四翻译服务）。Voice2Word 的 SenseVoice / Whisper 多档 + 本地 / 在线翻译已对齐，差距主要在「模型元数据与选择体验」。
- **可观测性是标配**：阶段状态 + 日志 + 失败续跑（YouDub 最完整）。Voice2Word 的队列持久化已有基础，可升级为阶段级状态机。
- **本地优先是公共叙事**：无云、无订阅、数据不出本机，与 Voice2Word「完全本地运行」定位一致，属于同一阵营而非竞争点。
- **差异化机会**：(a) 硬字幕 OCR 提取（VSE 独占，Voice2Word 空白）；(b) 配音 / TTS 与人声分离（YouDub 独占，Voice2Word 空白）；(c) 剪辑软件直连（AutoSubs 独占，Voice2Word 目前只有文件导出）；(d) 桌面原生校对台——Voice2Word 的多轨时间轴、字幕统计、术语表与质检复核表是这批项目里最完整的，值得放大为主卖点。
- **取舍提示**：VSE / VSG 走「配置即产品」的极简路线，AutoSubs 走「深度集成但维护成本高」的路线（Lua 宏 + CEP 双插件）。Voice2Word 作为 Rust 原生桌面应用居于两者之间，宜优先做「一键档位 + 落盘即用」的低摩擦体验。