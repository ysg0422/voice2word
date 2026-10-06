# Voice2Word — 音视频智能字幕生成与校对工具

Voice2Word 是一款**完全本地运行**的音视频字幕工作台。音视频不出本机，识别、标点、
翻译、校对、导出全部离线完成（在线翻译 API 为可选功能）。

Rust + [GPUI](https://github.com/zed-industries/zed) 实现，Windows 桌面原生应用。

---

## ✨ 功能一览

### 语音识别（多引擎可选）

| 引擎 | 模型 | 特点 |
| --- | --- | --- |
| **SenseVoice** | `model.int8.onnx` | 阿里达摩院非自回归模型，单次出字，速度最快（5~8×） |
| **Whisper Base** | `ggml-base.bin` | 39M 参数，轻量 |
| **Whisper Small** | `ggml-small-q5_0.bin` | 244M 参数，默认档位，速度与精度均衡 |
| **Whisper Turbo Q5** | `ggml-large-v3-turbo-q5_0.bin` | 破带宽量化，提速 25%~30% |
| **Whisper Turbo Q8** | `ggml-large-v3-turbo-q8_0.bin` | 旗舰精度，抗口音吞音 |

- **静音加速**：内置 Silero VAD 自动跳过空白段，长视频提速显著。
- **音频预处理**：可选降噪 / 响度归一化 / 高通滤波 / 静音段压缩，弱语音场景下提升识别率。
- **长音频分块**：超长音视频自动切块并行转写，避免单次推理内存爆掉。
- **说话人分离**：可选，最多 4 位说话人（基于音频聚类）。

### 标点与润色

- **CT-Transformer 标点恢复**（`model.int8.onnx`）：毫秒级补全标点，出厂默认。
- **Qwen 大模型深度润色**：可切换，纠正同音错别字与口语碎词，较慢。

### 多语言字幕翻译

- **本地 Qwen 离线翻译**：免费、无需联网、无需密钥。
- **在线 API 翻译**：兼容 OpenAI / DeepSeek 等格式，需在「性能设置」中填写 API Key
  （留空时回退读取环境变量 `VOICE2WORD_API_KEY`）。
- 目标语种：简体中文 / 繁体中文 / English / 日本語 / 한국어 / Русский / Français / Deutsch。
- **手工订正译文**：对照表里点某句的译文格（或点「改译文」）即可修改机翻；
  **术语表**可固定人名 / 缩写 / 专有名词的译法，保证全篇一致（「性能设置 → 术语表」）。

### 剪辑校对工作台

- **音画同步监视器**：点时间轴刻度或字幕块，实时预览对应视频帧与字幕叠层。
- **多轨时间轴**：字幕轨 + 音频波形轨，共用一条时间刻度，方便对着语音峰值卡点。
- **字幕精修**：直接改错字、一键插入标点、起止时间 ±0.1s / ±0.5s 微调、长句拆分、短句合并、片段删除。
- **字幕样式**：4 种视觉预设（白字黑影 / 黄字黑边 / 半透明黑框 / 电影沉浸），
  可调字号、字间距、行间距、单行字数、底边距，并带实时预览条。
- **虚拟化清单**：千行级字幕表仅渲染可视行，滚动不掉帧；支持原文 / 译文关键字搜索。

### 导出

一键导出为以下格式（均可直接导入对应剪辑软件）：

| 格式 | 说明 |
| --- | --- |
| **剪映草稿** | 一键直出到本机草稿库，或导出到自选文件夹 |
| **SRT** / **ASS** | 标准字幕 / 特效字幕（ASS 会写入预设的描边、阴影、底框参数） |
| **FCPXML** | 达芬奇 / Final Cut Pro |
| **Premiere XML** | Adobe Premiere |
| **TXT** / **VTT** | 纯文本 / 网页字幕 |

导出前可在底部导出栏选择**导出内容**：`仅原文` / `仅译文` / `双语对照`。该选项对字幕文件与
剪映 / FCPXML / Premiere 工程文件统一生效（工程文件里一条字幕=一行文字，双语会压成
「原文  译文」单行）；视频监视器与 FFplay 弹窗预览也会同步按该模式显示，做到所见即所得。

### 其他

- **历史视频库**：内置 SQLite 记录处理过的任务，随时恢复二次编辑。
- **硬件监控**：CPU / 内存占用实时看板，支持 GPU 解码与推理卸载开关。
- **深色 / 浅色双主题**。

---

## 📖 使用教程

1. **选择音视频** — 左侧「智能转写」页载入文件（支持 mp4 / mkv / mov / avi / flv / webm /
   mp3 / wav / flac / m4a）。
2. **一键生成** — 选好模型档位与语言，点「开始智能处理」。界面实时流式展示识别出的文字。
3. **校对精修** — 转写完成自动进入「剪辑校对」工作台：
   - 点底部时间轴的字幕块或刻度，上方监视器同步显示画面；
   - 右侧面板在「字幕样式」与「字幕翻译」两个视图间切换：
     - **字幕样式**：调字体观感、微调当前句的时间 / 拆分 / 合并 / 删除；
     - **字幕翻译**：选引擎与目标语言，一键翻译。
4. **导出** — 右下角选择导出格式，点「导出」。

---

## 🛠️ 部署与运行

### 1. 环境准备

- **操作系统**：Windows 10 / 11 (x64)
- **Rust**：[1.80+](https://www.rust-lang.org/tools/install)（edition 2021）
- **C/C++ 构建工具**：Visual Studio，勾选「使用 C++ 的桌面开发」（`rusqlite` 需要）

### 2. 克隆仓库

```bash
git clone https://github.com/ysg0422/voice2word.git
cd voice2word
```

### 3. 准备模型与组件

模型与可执行组件体积很大（合计约 5 GB），**不进版本库**。

> **推荐：直接在界面里一键下载。** 打开「性能设置 → 模型与组件」，
> 点“一键补齐缺失的 N 个组件”或单个组件的下载按钮即可。
> 模型走 `hf-mirror.com`（HuggingFace 国内镜像）、可执行组件走免梯子的
> GitHub 代理，**均无需梯子**；llama.cpp 与 whisper.cpp 打包在 zip 里，
> 下载后会自动解压到 `tools/`。下面的目录结构只是供你手动安排时参考。

目录名必须一致，文件名可在 `config.toml` 里改。

```
tools/
  ffmpeg.exe                                  # FFmpeg 主程序
  llama-completion.exe                        # llama.cpp 命令行程序（LLM 润色 / 翻译用）
  llama-server.exe                            # 同上（常驻服务，热加载更快，与上者同目录）
  whisper-vulkan/
    whisper-1.8.4-windows-x64/
      whisper-cli.exe                         # whisper.cpp 识别程序（Vulkan 加速版）
  punc_runner.py                              # 标点引擎 Python 胶水（仓库自带）
  sensevoice_runner.py                        # SenseVoice Python 胶水（仓库自带）

models/
  whisper/
    ggml-silero-v6.2.0.bin                    # Silero VAD（静音检测）
    ggml-small-q5_0.bin                       # 默认档位模型（按需换成其它档位）
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
- **whisper.cpp**：已内置一键下载（官方 CPU 构建，自动解压）；若想用 GPU 加速，
  可自行编译带 Vulkan 后端的 `whisper-cli.exe` 并在 `config.toml` 里指向它。
- **llama.cpp**：已内置一键下载（「性能设置 → 模型与组件」）；手动准备时下载预编译包，取 `llama-completion.exe` 与 `llama-server.exe`（需与各 DLL 同目录）。
- **模型**：Whisper 系列从 [HuggingFace ggerganov/whisper.cpp](https://huggingface.co/ggerganov/whisper.cpp) 取；
  Qwen 从 [Qwen2.5 GGUF](https://huggingface.co/Qwen) 取；SenseVoice / CT-Punc 的 ONNX 模型可从
  ModelScope 获取（`tools/download_sensevoice.py` 提供了 SenseVoice 的下载脚本）。

> 只用某一种引擎时，其余模型可以不放——对应功能在界面上不可用，但程序能正常启动。

### 4. 配置路径

打开根目录的 `config.toml`，把路径改成本地实际位置（**相对路径以项目根为基准，也可写绝对路径**）：

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
output_format     = "srt"          # srt / ass
enable_polish     = true           # 是否开启标点与润色
enable_vad        = true           # 是否开启静音加速
whisper_threads   = 8              # Whisper CPU 线程数
whisper_processors = 2             # 并行处理器数
llm_threads       = 8
llm_ctx           = 4096
parallel_workers  = 0              # 长音频分块并行数，0 = 自动

[translate]
mode       = "offline_qwen"        # offline_qwen / online_api
api_base   = "https://api.deepseek.com/v1"
api_key    = ""                    # 留空则读环境变量 VOICE2WORD_API_KEY
api_model  = "deepseek-chat"
```

### 5. 编译与启动

```powershell
cargo run                 # 开发模式运行
cargo build --release     # 发布版构建
```

发布产物位于 `target/release/voice2word.exe`。**双击运行时，请确保工作目录是项目根目录**
（程序靠当前目录下的 `models/` 定位项目根），或直接用 `cargo run` 启动。

### 6. 运行测试

```powershell
cargo test --lib          # 单元测试（不依赖模型文件）
```

`tests/` 下的集成测试需要 `testVideo/` 与 `models/` 就位，未准备素材时会自动跳过。

---

## 📁 目录结构

```
src/
  app/        应用状态机、任务调度
  core/       性能监控、指标、主管道
  engines/    FFmpeg / Whisper / SenseVoice / 标点 / LLM / 翻译 / 波形 / 说话人分离 / 视频解码
  storage/    SQLite 历史库
  subtitle/   字幕模型与导出器（SRT / ASS / FCPXML / Premiere / 剪映 / VTT）
  ui/         设计系统（theme / primitives）+ 各工作台视图
  utils/      配置、日志、帧缓存、临时文件守卫
examples/     基准与评测工具（cargo run --example <name>）
scripts/      算法 A/B、CER 评测、模型量化等一次性脚本
docs/         优化记录与设计规范
```

---

## 📄 许可证

本项目基于 MIT License 协议发布。
