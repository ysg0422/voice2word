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

档位在「性能设置 → Whisper 模型档位」下拉选择：每一档列出参数量、体积与**是否已下载**，
未下载的就在同一行点「下载」，已下载的可「删除」释放磁盘（有二次确认）。

- **静音加速**：内置 Silero VAD 自动跳过空白段，长视频提速显著。
- **音频预处理**：可选降噪 / 响度归一化 / 高通滤波 / 停顿压实 / 输入音频加速，
  弱语音与远场录音场景下提升识别率（「性能设置」里可调，默认收起）。
- **长音频分块**：超长音视频自动切块并行转写，避免单次推理内存爆掉。
- **说话人分离**：可选，最多 4 位说话人（基于音频聚类）。

### 标点与润色

- **CT-Transformer 标点恢复**（`model.int8.onnx`）：毫秒级补全标点，出厂默认。
- **Qwen 大模型深度润色**：可切换，纠正同音错别字与口语碎词，较慢。

### 多语言字幕翻译

- **本地 Qwen 离线翻译**：免费、无需联网、无需密钥。也可以指向**你自己的 `.gguf`**
  （「性能设置 → 字幕翻译 → 本地模型」粘贴或浏览选择，路径旁即时校验，可一键恢复默认）。
- **在线 API 翻译**：兼容 OpenAI / DeepSeek 等格式，需在「性能设置」中填写 API Key
  （留空时回退读取环境变量 `VOICE2WORD_API_KEY`）。
- 目标语种：简体中文 / 繁体中文 / English / 日本語 / 한국어 / Русский / Français / Deutsch。
- **手工订正译文**：对照表里点某句的译文格（或点「改译文」）即可修改机翻；
  **术语表**可固定人名 / 缩写 / 专有名词的译法，保证全篇一致（「性能设置 → 术语表」）。

### 剪辑校对工作台

- **音画同步监视器**：点时间轴刻度或字幕块，实时预览对应视频帧与字幕叠层。
- **多轨时间轴**：字幕轨 + 音频波形轨，共用一条时间刻度，方便对着语音峰值卡点。
- **字幕精修**：直接改错字、一键插入标点、起止时间 ±0.1s / ±0.5s 微调、长句拆分、短句合并、片段删除。
- **批量查找替换**（Ctrl+H）：可选区分大小写、可一并替换译文，替换前自动记撤销点。
- **字幕统计**：句数 / 总字数 / 有语音时长占比 / 平均与最快语速（CPS）/ 过长句数；
  语速上限按字符集分档（中日韩约 9 字/秒、拉丁约 17 字/秒），超标项可点击跳到那一句。
- **整轨时间轴调整**：一键平移 / 缩放 / 按目标时长铺满，先预览后应用。
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
| **JSON** | 结构化字幕（无损字段：原文/译文/置信度/说话人/语言，供程序化消费与质检） |
| **EBU-TT-D** / **Netflix TTAL** | TTML 广播与流媒体交付格式（UTF-8，中文原样保留） |

导出前可在底部导出栏选择**导出内容**：`仅原文` / `仅译文` / `双语对照`。该选项对字幕文件与
剪映 / FCPXML / Premiere 工程文件统一生效（工程文件里一条字幕=一行文字，双语会压成
「原文  译文」单行）；视频监视器与 FFplay 弹窗预览也会同步按该模式显示，做到所见即所得。

导出**格式**与**内容模式**都是长期偏好，会随 `config.toml` 记住，重启后仍是上次的选择。

导出**文件名**支持模板：在剪辑台导出栏填 `{name}` / `{ext}` / `{date}`，例如
`{name}.{date}.{ext}` 得到 `课程01.20261009.srt`。留空或不填占位符都有安全兜底
（回落默认名、自动补扩展名）。

### 其他

- **历史视频库**：内置 SQLite 记录处理过的任务，随时恢复二次编辑；一键**查重**
  （按内容指纹，改名 / 复制 / 重下都认得出），避免重复转写。
- **质检复核表**：把低置信 / 术语违规 / 未翻译 / 碎片句导出成 CSV 或 Markdown，交给别人复核。
- **编辑日志**：本次会话改过哪些句、改前改后是什么，可导出留档。
- **诊断报告**：一键导出「实际解析到的路径 + 各组件状态 + 运行库能否解析」，排查环境问题。
- **模型管理**：全部组件的体积合计、逐项「定位 / 删除」、「打开目录」、一键备份 / 恢复数据。
- **硬件监控**：CPU / 内存占用实时看板，支持 GPU 解码与推理卸载开关。
- **检查更新**：只在点击时查一次 GitHub Releases，给出下载入口（不会自动替换运行中的程序）。
- **深色 / 浅色双主题**。

---

## 📖 使用教程

1. **选择音视频** — 左侧「智能转写」页载入文件（支持 mp4 / mkv / mov / avi / flv / webm /
   mp3 / wav / flac / m4a）。支持**拖拽文件夹**（含子目录，自动递归收集媒体文件）或点
   「导入整个文件夹」批量入队；扫描结果会说明收了多少个、跳过了什么（层级过深 / 不可读 /
   快捷方式）。
   也可以点「导入已有字幕」直接载入 SRT / VTT / TXT（跳过转写），接着编辑或导出——
   UTF-8 解不开的会自动按 GBK 再试一次。
2. **一键生成** — 选好模型档位与语言，点「开始智能处理」。界面实时流式展示识别出的文字。
3. **校对精修** — 转写完成自动进入「剪辑校对」工作台：
   - 点底部时间轴的字幕块或刻度，上方监视器同步显示画面；
   - 右侧三张面板互斥切换：**字幕样式**（观感 + 单句微调 + 整轨时间轴）、
     **字幕翻译**（选引擎与目标语言）、**字幕统计**（字数 / 语速 / 过长句）。
4. **导出** — 右下角选择导出格式与文件名模板，点「导出」。

### 快捷键

`Ctrl+Enter` 开始转写 · `Ctrl+Space` 播放/暂停 · `Alt+↑↓` 上下句 · `Alt+←→` ±1 秒 ·
`Ctrl+F` 搜索 · `Ctrl+H` 查找替换 · `Ctrl+E` 导出 · `Ctrl+Z` / `Ctrl+Y` 撤销重做 ·
`Ctrl+K` 命令面板 · `Ctrl+T` 主题 · `Esc` 关闭/终止

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
      whisper-cli.exe                         # whisper.cpp 识别程序（官方 CPU 包；GPU 见 build_whisper_msvc_vulkan.bat）
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
- **whisper.cpp**：已内置一键下载（官方 CPU 构建，自动解压，免梯子）。
  **想用 GPU 加速**：跑 `scripts/build_whisper_msvc_vulkan.bat`（MSVC + Vulkan 一键编译并
  自动部署到 `tools/whisper-vulkan/`）。实测 AMD 核显上比 CPU 快约 1.8×；
  产物自带 `ggml-vulkan.dll`，程序识别为「自编译构建」后不会再被 CPU 包覆盖。
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
output_format     = "srt"          # srt / vtt / ass / txt / json / ttml / ttal
enable_polish     = true           # 是否开启标点与润色
polish_mode       = "punc"         # punc（CT-Punc 极速）/ qwen（大模型润色）
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

[ui]
theme             = "dark"         # dark / light
export_mode       = "bilingual"    # raw / translation / bilingual
export_format     = "jianying"     # 剪辑台导出格式
export_name_template = "{name}.{ext}"   # 支持 {name} / {ext} / {date}
```

> 机器专属的绝对路径（如 ffmpeg 装在别处）不需要写进 `config.toml`——
> 程序会把它们自动收进 `config.local.toml`，该文件不进版本库。

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
  subtitle/   字幕模型 + 读写与工具（导出器 / 导入 / 查找替换 / 统计 / 时间轴 / 质检表 / 编辑日志）
  ui/         设计系统（theme / primitives）+ 各工作台视图
  utils/      配置、日志、模型下载与管理、媒体扫描、数据备份、诊断、查重、更新检查
examples/     基准与评测工具（cargo run --example <name>）
scripts/      算法 A/B、CER 评测、模型量化等一次性脚本
docs/         优化记录与设计规范
```

---

## 📄 许可证

本项目基于 MIT License 协议发布。
