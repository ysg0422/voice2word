# Voice2Word — 音视频智能字幕工作台

![license](https://img.shields.io/badge/license-MIT-blue.svg)
![platform](https://img.shields.io/badge/platform-Windows%2010%20%2F%2011%20x64-0078D6)
![rust](https://img.shields.io/badge/rust-1.80%2B-orange)

Voice2Word 是一款**完全本地运行**的音视频字幕工作台，覆盖「转写 → 标点 → 翻译 → 校对 → 导出」全流程。音视频与识别结果全程留在本机，只有在选用在线翻译 API 时才需要联网。

界面使用 Rust + [GPUI](https://github.com/zed-industries/zed) 开发，为 Windows 桌面原生应用。

## 目录

- [功能特性](#功能特性)
- [安装](#安装)
- [快速上手](#快速上手)
- [从源码构建](#从源码构建)
- [配置说明](#配置说明)
- [目录结构](#目录结构)
- [命令行与集成](#命令行与集成)
- [许可证](#许可证)

## 功能特性

### 语音识别

多引擎可选，档位可在界面内切换：

| 引擎 | 模型 | 说明 |
| --- | --- | --- |
| SenseVoice | `model.int8.onnx` | 非自回归模型，单次输出，速度最快（约 5~8×） |
| Whisper Base | `ggml-base.bin` | 39M 参数，轻量 |
| Whisper Small | `ggml-small-q5_0.bin` | 244M 参数，默认档位，速度与精度均衡 |
| Whisper Turbo Q5 | `ggml-large-v3-turbo-q5_0.bin` | 量化版本，相比未量化提速约 25%~30% |
| Whisper Turbo Q8 | `ggml-large-v3-turbo-q8_0.bin` | 高精度，抗口音、吞音 |

在「性能设置 → Whisper 模型档位」下拉中选择：每一项都标出参数量、体积与下载状态，未下载可当场下载，已下载可删除以释放磁盘（带二次确认）。

- **静音加速**：内置 Silero VAD 自动跳过空白片段，长视频提速明显。
- **音频预处理**：降噪、响度归一化、高通滤波、停顿压实、输入音频加速，可提升弱语音与远场录音的识别率（「性能设置」中可调）。
- **长音频分块**：超长音视频自动切块并行转写，避免单次推理内存溢出。
- **说话人分离**：可选，最多支持 4 位说话人（基于音频聚类）。

### 标点与润色

- **CT-Transformer 标点恢复**（`model.int8.onnx`）：毫秒级补全标点，默认启用。
- **Qwen 大模型润色**：可切换，纠正同音错别字与口语碎词，速度较慢。

### 字幕翻译

- **本地离线翻译**：基于 Qwen，免费、无需联网与密钥；也可指向自定义的 `.gguf` 模型（「性能设置 → 字幕翻译 → 本地模型」，支持粘贴或浏览选择，路径即时校验，可一键恢复默认）。
- **在线 API 翻译**：兼容 OpenAI / DeepSeek 等接口格式，在「性能设置」中填写 API Key 即可（留空时读取环境变量 `VOICE2WORD_API_KEY`）。
- 支持的目标语言：简体中文、繁体中文、English、日本語、한국어、Русский、Français、Deutsch。
- **手工订正译文**：在对照表中直接修改某句译文；**术语表**可固定人名、缩写、专有名词的译法，保证全篇一致。

### 校对工作台

- **音画同步监视器**：点击时间轴刻度或字幕块，实时预览对应视频帧与字幕叠加效果。
- **多轨时间轴**：字幕轨与音频波形轨共用一条时间刻度，便于对着语音峰值卡点。
- **字幕精修**：修改错字、一键插入标点、起止时间 ±0.1s / ±0.5s 微调、长句拆分、短句合并、片段删除。
- **批量查找替换**（Ctrl+H）：可区分大小写、可一并替换译文，替换前自动记录撤销点。
- **字幕统计**：句数、总字数、有语音时长占比、平均与最快语速（CPS）、过长句数；语速上限按字符集分档（中日韩约 9 字/秒，拉丁约 17 字/秒），超标项可点击跳转。
- **整轨时间轴调整**：平移、缩放、按目标时长铺满，先预览后应用。
- **字幕样式**：6 种视觉预设（白字黑影、黄字黑边、半透明黑框、电影沉浸、霓虹极光、活力暖橙），可调字号、字间距、行间距、单行字数与底边距，并带实时预览。
- **虚拟化清单**：千行级字幕表仅渲染可视行，滚动流畅；支持原文 / 译文关键字搜索。

### 导出

一键导出为以下格式，均可直接导入对应剪辑软件：

| 格式 | 说明 |
| --- | --- |
| 剪映草稿 | 直出到本机草稿库，或导出到自选文件夹 |
| SRT / ASS | 标准字幕 / 特效字幕（ASS 写入预设的描边、阴影、底框参数） |
| FCPXML | DaVinci Resolve / Final Cut Pro |
| Premiere XML | Adobe Premiere |
| TXT / VTT | 纯文本 / 网页字幕 |
| JSON | 结构化字幕，无损保留原文、译文、置信度、说话人、语言等字段 |
| EBU-TT-D / Netflix TTAL | TTML 广播与流媒体交付格式（UTF-8，中文原样保留） |

导出前可在底部导出栏选择**导出内容**：`仅原文` / `仅译文` / `双语对照`。该选项对字幕文件与剪映 / FCPXML / Premiere 工程文件统一生效；监视器与 FFplay 预览也会同步按该模式显示。

导出**格式**与**内容模式**会随 `config.toml` 记住，重启后保持上次选择。导出**文件名**支持模板：`{name}` / `{ext}` / `{date}`，例如 `{name}.{date}.{ext}` 得到 `课程01.20261009.srt`；留空或未填占位符时有安全兜底。

### 其他能力

- **历史视频库**：内置 SQLite 记录处理过的任务，可随时恢复二次编辑；支持按文件名 / 路径搜索、按未翻译 / 已翻译筛选与多种排序；一键查重（按内容指纹，改名、复制、重新下载都能识别）。
- **队列持久化**：排好的批量队列与每条进度自动落盘，关窗或崩溃后重启仍在。
- **崩溃提示**：上次异常退出会在启动时提示（可关闭），并可直接导出诊断报告。
- **质检复核表**：将低置信、术语违规、未翻译、碎片句导出为 CSV 或 Markdown。
- **编辑日志**：记录本次会话修改过的句子及修改前后内容，可导出留档。
- **诊断报告**：一键导出实际解析到的路径、各组件状态与运行库解析结果，便于排查环境问题。
- **模型管理**：查看全部组件体积合计、逐项定位 / 删除、打开目录、一键备份与恢复数据。
- **硬件监控**：实时查看 CPU / 内存占用，支持 GPU 解码与推理卸载开关。
- **检查更新**：点击时查询一次 GitHub Releases，给出下载入口（不会自动替换运行中的程序）。
- **深色 / 浅色双主题**。

## 安装

### 方式一：下载安装包（推荐）

前往 [Releases](https://github.com/ysg0422/voice2word/releases/latest) 下载最新的 `Voice2Word-vX.Y.Z-Setup.exe` 并运行。安装向导支持**自定义安装位置**，可选择任意磁盘与目录；同时会创建桌面快捷方式、开始菜单项与卸载入口。

### 方式二：从源码构建

参见 [从源码构建](#从源码构建)。

## 快速上手

1. **选择音视频**：在左侧「智能转写」页载入文件（支持 mp4 / mkv / mov / avi / flv / webm / mp3 / wav / flac / m4a）。支持拖拽文件夹（含子目录，自动递归收集）或「导入整个文件夹」批量入队；扫描结束会说明收录与跳过的文件。也可点「导入已有字幕」直接载入 SRT / WebVTT / LRC / ASS(SSA) / TTML / JSON / TXT 跳过转写，UTF-8 解码失败时会自动按 GBK 重试。
2. **一键生成**：选好模型档位与语言，点「开始智能处理」，界面实时流式展示识别文字。
3. **校对精修**：转写完成后自动进入「剪辑校对」工作台。点击底部时间轴的字幕块或刻度，上方监视器同步显示画面；右侧面板可在**字幕样式**、**字幕翻译**、**字幕统计**之间切换。
4. **导出**：在右下角选择导出格式与文件名模板，点击「导出」。

### 快捷键

| 快捷键 | 功能 |
| --- | --- |
| `Ctrl+Enter` | 开始转写 |
| `Ctrl+Space` | 播放 / 暂停 |
| `Alt+↑` / `Alt+↓` | 上一句 / 下一句 |
| `Alt+←` / `Alt+→` | 后退 / 前进 1 秒 |
| `Ctrl+F` | 搜索 |
| `Ctrl+H` | 查找替换 |
| `Ctrl+E` | 导出 |
| `Ctrl+Z` / `Ctrl+Y` | 撤销 / 重做 |
| `Ctrl+K` | 命令面板 |
| `Ctrl+T` | 切换主题 |
| `Esc` | 关闭 / 终止 |

## 从源码构建

### 环境要求

- 操作系统：Windows 10 / 11 (x64)
- [Rust](https://www.rust-lang.org/tools/install) 1.80 及以上（edition 2021）
- C / C++ 构建工具：Visual Studio，安装时勾选「使用 C++ 的桌面开发」（`rusqlite` 需要）

### 克隆与构建

```bash
git clone https://github.com/ysg0422/voice2word.git
cd voice2word
cargo run                 # 开发模式运行
cargo build --release     # 发布版构建
```

发布产物位于 `target/release/voice2word.exe`。直接双击运行时，请确保工作目录为项目根目录（程序依赖当前目录下的 `models/` 定位项目根），或直接使用 `cargo run` 启动。

### 模型与组件

模型与可执行组件体积较大（合计约 5 GB），不随版本库分发。

推荐做法：打开「性能设置 → 模型与组件」，点「一键补齐缺失的组件」或单项下载。模型经由 `hf-mirror.com`（HuggingFace 国内镜像）下载，可执行组件经由 GitHub 加速代理下载，均可直连；llama.cpp 与 whisper.cpp 以 zip 打包，下载后自动解压到 `tools/`。

手动准备时，目录名需保持一致，文件名可在 `config.toml` 中修改：

```
tools/
  ffmpeg.exe                                  # FFmpeg 主程序
  llama-completion.exe                        # llama.cpp 命令行（LLM 润色 / 翻译）
  llama-server.exe                            # llama.cpp 常驻服务（与上者同目录）
  whisper-vulkan/
    whisper-1.8.4-windows-x64/
      whisper-cli.exe                         # whisper.cpp 识别程序（官方 CPU 包）
  punc_runner.py                              # 标点引擎 Python 胶水（仓库自带）
  sensevoice_runner.py                        # SenseVoice Python 胶水（仓库自带）

models/
  whisper/
    ggml-silero-v6.2.0.bin                    # Silero VAD（静音检测）
    ggml-small-q5_0.bin                       # 默认档位模型
  punc/
    model.int8.onnx                           # CT-Transformer 标点模型
  sensevoice/
    model.int8.onnx                           # SenseVoice 识别模型
    tokens.txt
    silero_vad.onnx
  llm/
    qwen2.5-0.5b-instruct-q4_k_m.gguf         # 本地润色 / 翻译用的 Qwen 模型
```

- **FFmpeg**：从 [ffmpeg.org](https://ffmpeg.org/download.html) 下载 Windows 版，取 `ffmpeg.exe`。
- **whisper.cpp**：内置一键下载（官方 CPU 构建，自动解压）。需要 GPU 加速时，运行 `scripts/build_whisper_msvc_vulkan.bat`（MSVC + Vulkan 一键编译并部署到 `tools/whisper-vulkan/`），产物自带 `ggml-vulkan.dll`，程序识别为自编译构建后不会再被 CPU 包覆盖。
- **llama.cpp**：内置一键下载；手动准备时下载预编译包，取 `llama-completion.exe` 与 `llama-server.exe`（需与各 DLL 同目录）。
- **模型**：Whisper 系列取自 [ggerganov/whisper.cpp](https://huggingface.co/ggerganov/whisper.cpp)，Qwen 取自 [Qwen2.5 GGUF](https://huggingface.co/Qwen)，SenseVoice / CT-Punc 的 ONNX 模型可从 ModelScope 获取（`tools/download_sensevoice.py` 提供 SenseVoice 下载脚本）。

> 只使用某一种引擎时，其余模型可以不放：对应功能在界面上不可用，但程序可正常启动。

### 运行测试

```bash
cargo test --lib          # 单元测试（不依赖模型文件）
```

`tests/` 下的集成测试需要 `testVideo/` 与 `models/` 就位，未准备素材时会自动跳过。

## 配置说明

编辑根目录的 `config.toml`，将路径改为本地实际位置（相对路径以项目根为基准，也可使用绝对路径）：

```toml
[paths]
ffmpeg         = "tools/ffmpeg.exe"
whisper_cli    = "tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe"
whisper_model  = "models/whisper/ggml-small-q5_0.bin"
vad_model      = "models/whisper/ggml-silero-v6.2.0.bin"
punc_model     = "models/punc/model.int8.onnx"
sensevoice_model = "models/sensevoice/model.int8.onnx"
llama_cli      = "tools/llama-completion.exe"
llm_model      = "models/llm/qwen2.5-0.5b-instruct-q4_k_m.gguf"
python         = "python"          # 标点 / SenseVoice 胶水脚本的解释器

[pipeline]
language          = "zh"           # 识别语言：zh / en / auto
output_format     = "srt"          # srt / vtt / ass / txt / json / ttml / ttal
enable_polish     = true           # 是否开启标点与润色
polish_mode       = "punc"         # punc（CT-Punc 极速）/ qwen（大模型润色）
enable_vad        = true           # 是否开启静音加速
whisper_threads   = 8              # Whisper CPU 线程数
whisper_processors = 2             # 并行处理器数
llm_threads       = 8
llm_ctx           = 4096
parallel_workers  = 0              # 长音频分块并行数，0 表示自动

[translate]
mode       = "offline_qwen"        # offline_qwen / online_api
api_base   = "https://api.deepseek.com/v1"
api_key    = ""                    # 留空则读取环境变量 VOICE2WORD_API_KEY
api_model  = "deepseek-chat"

[ui]
theme             = "dark"         # dark / light
export_mode       = "bilingual"    # raw / translation / bilingual
export_format     = "jianying"     # 剪辑台导出格式
export_name_template = "{name}.{ext}"   # 支持 {name} / {ext} / {date}
```

> 机器专属的绝对路径（例如 ffmpeg 安装在别处）无需写入 `config.toml`：程序会自动收集到 `config.local.toml`，该文件不纳入版本库。

## 目录结构

```
src/
  app/        应用状态机、任务调度
  core/       性能监控、指标、主管道
  engines/    FFmpeg / Whisper / SenseVoice / 标点 / LLM / 翻译 / 波形 / 说话人分离 / 视频解码
  storage/    SQLite 历史库
  subtitle/   字幕模型与读写工具（导出器 / 导入 / 查找替换 / 统计 / 时间轴 / 质检表 / 编辑日志）
  ui/         设计系统（theme / primitives）与各工作台视图
  utils/      配置、日志、模型下载与管理、媒体扫描、数据备份、诊断、查重、更新检查
examples/     基准与评测工具（cargo run --example <name>）
scripts/      算法 A/B、CER 评测、模型量化等脚本
docs/         优化记录与设计规范
```

## 命令行与集成

除图形界面外，Voice2Word 提供无头命令行（CLI），并附带 Claude Code / Codex Skill 与 MCP 服务器，便于脚本化与 AI 工具调用：

- 完整说明见 [集成说明：CLI 与 MCP](docs/集成说明-CLI与MCP.md)。
- 集成资产位于 [integrations/](integrations/)：
  - [integrations/voice2word-skill/](integrations/voice2word-skill/)：Skill（`SKILL.md` + `scripts/transcribe.ps1`）。
  - [integrations/mcp-server/](integrations/mcp-server/)：MCP stdio 服务器（`voice2word_mcp.py` + 协议测试）。

快速开始：

```powershell
voice2word transcribe talk.mp4 -o out\talk.srt --format srt --lang zh
```

进度与诊断写入 stderr，最终结果路径写入 stdout，便于重定向只取结果。

## 许可证

本项目基于 MIT License 发布。
