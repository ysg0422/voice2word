# Voice2Word — 音视频智能字幕生成与校对工具

Voice2Word 是一款本地运行的音视频字幕自动生成与精修桌面应用。无需联网上传音视频，100% 本地完成语音识别、标点润色错别字纠错、多轨时间轴画面对齐以及字幕导出。

---

## 🌟 主要功能

- **本地一键转写**：支持 MP4、MKV、MOV、FLV、MP3、WAV 等主流音视频格式，自动提取音频并转写为带时间戳的字幕。
- **静音加速检测**：内置语音活动检测（VAD），自动跳过空白静音段，长视频转写速度大幅提升。
- **智能标点与错别字纠错**：转写完成后自动补全标点符号，纠正常见同音字与语音识别错别字。
- **可视化时间轴校对工作台**：
  - **音画对齐预览**：拖动或点击时间轴刻度，实时预览对应视频画面与字幕叠层。
  - **字幕快捷精修**：直接在界面上修改错字，支持常用标点一键插入。
  - **毫秒级微调与编辑**：支持字幕起止时间微调（±0.1s / ±0.5s）、长句拆分、短句合并与片段删除。
- **多格式导出**：一键重新生成并导出为标准 **SRT**、**ASS** 或 **TXT** 字幕文件。
- **历史记录保存**：内置本地数据库，处理过的任务自动保存，随时点击恢复并二次编辑。

---

## 📖 使用教程

1. **选择音视频**：打开软件后，在左侧点击「选择音视频文件」载入目标文件。
2. **一键生成字幕**：确认语言和导出格式，点击「开始智能处理」。界面会实时展示识别进度与转写出来的文字。
3. **校对与精修**：
   - 转写完成后，应用会自动进入「剪辑校对工作台」。
   - 点击底部时间轴上的任意字幕色块或时间刻度，上方监视器会同步展示当前画面的视频帧。
   - 在右侧「字幕属性」面板中直接修改错别字、微调时间或拆分/合并句子。
4. **导出成品**：在右上角点击「重新导出 SRT / ASS」，即可保存最终字幕。

---

## 🛠️ 个人部署与运行指南

### 1. 环境准备

- **操作系统**：Windows 10 / 11 (x64)
- **Rust 环境**：安装 [Rust 1.80+](https://www.rust-lang.org/tools/install)
- **C/C++ 编译环境**：Visual Studio（勾选“使用 C++ 的桌面开发”）

### 2. 克隆仓库

```bash
git clone https://github.com/ysg0422/voice2word.git
cd voice2word
```

### 3. 准备模型与组件

为保证离线高效运行，本应用调用以下本地组件与模型文件：

1. **FFmpeg**：
   - 下载 Windows 版 FFmpeg，解压得到 `ffmpeg.exe`。
2. **语音识别组件**：
   - 准备 `whisper-cli.exe` 识别程序。
   - 下载语音识别模型（如 `ggml-base.bin`）以及静音检测模型（如 `ggml-silero-v6.2.0.bin`）放入 `models/whisper/` 目录。
3. **大语言模型（可选，用于纠错）**：
   - 准备 `llama-completion.exe` 推理程序。
   - 下载语言模型（如 `qwen2.5-0.5b-instruct-q4_k_m.gguf`）放入 `models/llm/` 目录。

### 4. 配置路径

打开根目录下的 `config.toml`，将工具路径和模型路径配置为您本地的实际存放路径：

```toml
[paths]
ffmpeg = "tools/ffmpeg.exe"
whisper_cli = "tools/whisper-cli.exe"
whisper_model = "models/whisper/ggml-base.bin"
vad_model = "models/whisper/ggml-silero-v6.2.0.bin"
llama_cli = "tools/llama-completion.exe"
llm_model = "models/llm/qwen2.5-0.5b-instruct-q4_k_m.gguf"

[pipeline]
language = "zh"           # 默认识别语言 (zh / en / auto)
output_format = "srt"     # 默认导出格式 (srt / ass)
enable_polish = true      # 是否开启智能润色与纠错
enable_vad = true         # 是否开启静音加速检测
whisper_threads = 8       # Whisper CPU 线程数
llm_threads = 8           # 大模型 CPU 线程数
llm_ctx = 4096            # 上下文大小
```

### 5. 编译与启动

在项目根目录下执行以下命令即可启动桌面客户端：

```powershell
cargo run
```

如需编译为发布版独立可执行文件：

```powershell
cargo build --release
```
编译成功后，产物位于 `target/release/voice2word.exe`，直接双击运行即可。

---

## 📄 许可证

本项目基于 [MIT License](LICENSE) 协议发布。
