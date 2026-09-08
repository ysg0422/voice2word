# Voice2Word — 音视频智能字幕生成器 (Rust + GPUI)

<div align="center">

**基于 Rust 与 GPUI 打造的现代、轻量、高颜值的音视频本地智能字幕生成工具**

[![Language](https://img.shields.io/badge/Language-Rust_1.80+-orange.svg)](https://www.rust-lang.org/)
[![UI Framework](https://img.shields.io/badge/UI-GPUI_(Zed)-6366f1.svg)](https://github.com/zed-industries/zed)
[![Audio](https://img.shields.io/badge/Audio-FFmpeg_6+-007800.svg)](https://ffmpeg.org/)
[![ASR](https://img.shields.io/badge/ASR-Whisper.cpp-black.svg)](https://github.com/ggerganov/whisper.cpp)
[![LLM](https://img.shields.io/badge/LLM-Qwen2.5_(llama.cpp)-blue.svg)](https://github.com/ggerganov/llama.cpp)

</div>

---

## 📖 项目简介

**Voice2Word** 是一款面向视频创作者、会议记录者与字幕编辑者的本地桌面应用。项目使用 **Rust** 进行系统级状态编排与数据管线调度，界面采用来自 Zed 编辑器的 GPU 加速图形库 **GPUI**，视觉风格深度参考 **Codex / Zed** 的极简暗黑美学。

外部计算引擎秉承“各司其职、成熟为先”原则，无缝集成工业级 **FFmpeg**、**whisper.cpp** 和 **llama.cpp**，全程离线运行，保护隐私，无须消耗任何付费 API。

---

## ✨ 核心特性

- 🎨 **Zed 现代极简桌面美学**：彻底告别传统“表单软件”与 Qt 风格，拥有高质感的深灰背景、极细分界、大留白排版与平滑流畅的 GPU 加速渲染体验。
- ⚡ **毫秒级全链路自动化**：
  $$\text{音视频输入} \xrightarrow{\text{FFmpeg}} \text{16kHz WAV} \xrightarrow{\text{Whisper}} \text{带时间戳片段} \xrightarrow{\text{Qwen}} \text{标点/错字润色} \xrightarrow{\text{Writer}} \text{SRT / ASS / TXT}$$
- 🧵 **绝对丝滑响应**：基于 **Tokio** 异步执行后端，主界面采用帧驱动事件轮询，高负载模型推理全程**零卡顿**。
- 🧠 **大模型智能标点恢复**：针对 Whisper 输出的无标点文本，通过内置的 **Qwen2.5 Few-Shot** 引擎精准恢复标点符号（逗号、句号、感叹号、问号）并修正同音近音错别字。
- 📦 **主流音视频格式支持**：兼容 MP4、MKV、MOV、AVI、FLV、WebM、MP3、WAV、FLAC、M4A 等。
- 💾 **本地任务持久化**：内置 SQLite 数据库，自动沉淀处理历史记录与字幕草稿，支持一键载入与回放。
- 📤 **多字幕规范导出**：一键导出为 **SRT**（标准外挂字幕）、**ASS**（高级特效样式字幕）以及 **TXT**（纯文本记录）。

---

## 🖥 界面概览

```text
┌───────────────────────────────────────────────────────────────┐
│ Voice2Word                                                    │
├───────────────────┬───────────────────────────────────────────┤
│                   │                                           │
│  [INPUT MEDIA]    │          字 幕 工 作 区                    │
│  📁 sample.mp4     │                                           │
│                   │  #001  [00:00:00,000 ➔ 00:00:04,960] ✦已润色│
│  [CONFIG]         │  你好，欢迎使用音视频智能字幕生成器，      │
│  语言: 中文 / 英文 │  这是一个测试。                           │
│  格式: SRT / ASS  │                                           │
│  Qwen 润色: 已启用 │                                           │
│                   │                                           │
│  [RECENT TASKS]   │                                           │
│  • 会议录音.mp4    │                                           │
│  • 课程讲座.mp4    │                                           │
├───────────────────┴───────────────────────────────────────────┤
│ 状态: 完成 100%  [████████████████████]   [💾 导出] [▶ 开始处理] │
└───────────────────────────────────────────────────────────────┘
```

---

## 🛠 代码架构

```text
Voice2Word/
├── Cargo.toml               # Rust 依赖声明与优化配置
├── config.toml              # 路径与超参数配置 (TOML)
├── voice2word.db            # SQLite 本地任务数据库
├── models/                  # 本地 GGUF / GGML 模型目录
│   ├── whisper/             # ggml-large-v3-turbo-q8_0.bin
│   └── llm/                 # Qwen2.5-3B-Instruct-Q4_K_M.gguf
│
├── tests/
│   └── test_pipeline.rs     # 端到端全链路自动化集成测试
│
└── src/
    ├── main.rs              # 入口：配置加载、数据库初始化、GPUI 窗口创建
    ├── lib.rs               # 模块库定义
    ├── app/
    │   └── state.rs         # 全局应用状态 AppState (选定文件/处理状态/字幕列表)
    ├── core/
    │   └── pipeline.rs      # Tokio 异步任务管线 (Channel 驱动，阶段流转)
    ├── engines/
    │   ├── ffmpeg.rs        # FFmpeg 音频提取与时长检测
    │   ├── whisper.rs       # whisper.cpp 命令行适配与时间戳解析
    │   └── llm.rs           # llama.cpp 标点恢复引擎 (UTF-8 传参 + Few-Shot)
    ├── subtitle/
    │   ├── segment.rs       # Segment 数据模型 (秒作为标准时间单位)
    │   └── writer.rs        # SRT / ASS / TXT 规范导出器
    ├── storage/
    │   └── db.rs            # SQLite 任务历史与片段持久化
    ├── ui/
    │   ├── mod.rs           # GPUI 主视图布局与帧驱动事件消费
    │   └── theme.rs         # 配色规范 (Slate 深灰体系、细分界、Mint 强调色)
    └── utils/
        ├── config.rs        # TOML 配置文件读写与路径自动解析
        └── time.rs          # 秒数 ↔ SRT/ASS 标准时间格式相互转换
```

---

## 🚀 快速开始

### 1. 环境准备
- **Rust 工具链**：Rust 1.80+ (推荐使用 `rustup` 安装)
- **C/C++ 构建工具**：Windows 平台需要 Visual Studio C++ 工具链（MSVC）
- **外部原生依赖**：
  - [FFmpeg](https://ffmpeg.org/download.html)
  - [whisper.cpp](https://github.com/ggerganov/whisper.cpp)
  - [llama.cpp](https://github.com/ggerganov/llama.cpp)

### 2. 检查配置
打开项目根目录下的 `config.toml`，确认外部组件和模型文件路径正确：

```toml
[paths]
ffmpeg = "A:\\cppsoft\\ffmpeg-6.9\\bin\\ffmpeg.exe"
whisper_cli = "A:\\cppsoft\\whisper\\Release\\whisper-cli.exe"
whisper_model = "models/whisper/ggml-large-v3-turbo-q8_0.bin"
llama_cli = "A:\\cppsoft\\llama.cpp\\build\\bin\\Release\\llama-completion.exe"
llm_model = "models/llm/Qwen2.5-3B-Instruct-Q4_K_M.gguf"

[pipeline]
language = "zh"           # 默认识别语言 (zh / en / ja / auto)
output_format = "srt"     # 默认导出格式 (srt / ass / txt)
enable_polish = true      # 默认开启大模型润色
whisper_threads = 4       # Whisper 计算线程
llm_threads = 4           # LLM 计算线程
llm_ctx = 4096            # 上下文大小
```

### 3. 运行桌面客户端
```powershell
cargo run
```
也可以直接运行已编译生成的独立二进制程序：
```powershell
target/debug/voice2word.exe
```

---

## 🧪 自动化测试

项目内置完整的端到端集成测试，可自动运行 `FFmpeg -> Whisper -> Qwen -> SRT` 整个流水线并校验生成结果：

```powershell
cargo test --test test_pipeline -- --nocapture
```

测试输出示例：
```text
running 1 test
[1/4] FFmpeg 提取音频...
      音频提取完成: sample_voice2word.wav (16000Hz, mono, s16le)
[2/4] Whisper 识别语音...
      转写完成，片段数: 1
      #1: [0.00 -> 4.96] 你好欢迎使用音视频智能字幕生成器这是一个测试
[3/4] Qwen LLM 润色...
      #1: 原文='你好欢迎使用音视频智能字幕生成器这是一个测试'
          润色='你好，欢迎使用音视频智能字幕生成器，这是一个测试。'
[4/4] 导出标准 SRT 文件...
      字幕成功生成，时间戳格式 00:00:00,000 --> 00:00:04,960。

test test_full_pipeline_run ... ok
```

---

## 📄 开源许可证

本项目基于 [MIT License](LICENSE) 开源发布。
