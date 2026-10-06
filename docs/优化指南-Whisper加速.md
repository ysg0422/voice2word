# Whisper CPU 加速配置指南

当前项目的实际配置（`config.toml` + `src/main.rs:58`）：模型 `ggml-small-q5_0.bin`、`whisper_threads = 12`、`[gpu] whisper_offload = true`，因此默认链路**走 Vulkan GPU 推理**，并叠加 `gpu_limit_percent = 60` 的占空比限速；只有关掉 `whisper_offload` 或检测不到可用显卡时才会退回 `-ng` 纯 CPU。Small-Q5 仍是 Whisper Small，只是将权重做 Q5_0 量化。

下面的 GPU/Vulkan 段落是历史记录。此处曾写作「当前默认链路不启用 GPU」，与 `config.toml` 的 `whisper_offload = true`、`src/main.rs` 的判定以及运行日志（`Whisper 使用 GPU 推理`）矛盾，已更正为「默认启用 GPU」。CPU 参数仍以 `config.toml` 为准（关掉 offload 时走这条路）。

10 分钟（600 秒）标准片段、1.00x 速率的实测：GPU 路径 28.13 秒、CER 12.03%，纯 CPU（`-ng`）65.67 秒、CER 11.76%（均为 `ggml-small-q5_0`、`-t 12`、当前生产参数）。历史记录的「Small-Q5 CPU 16 线程约 57.65 秒、CER 12.88%」用的是 16 线程，与当前 `whisper_threads = 12` 不同，仅供参照。此处旧版还写过「未量化 Small 34.46 秒」，该数字比量化后的 Q5（57.65 秒）还快、且未能复现，与同一份文档的 CER 表自相矛盾，已废弃（详见 `docs/Whisper人工标准字幕对比.md` 第 5 节）。长音频会按现有切块逻辑并行处理，10 分钟两块并行约 52.5 秒。Q4_0 约 46.33 秒但 CER 16.10%，不设默认。

生成量化模型：

```powershell
.\scripts\quantize_whisper_small.ps1 -Type q5_0
```

如果只保留原始模型，把 `whisper_model` 指回 `models/whisper/ggml-small.bin`，并把 `whisper_threads` 按机器调整。`-bo 1 -bs 1 -nf -mc 32 -vsd 250` 已是当前测试过的速度参数组合；继续提高 VAD 阈值或缩短静音间隔会增加切段重算，收益不稳定。

## 🎯 目标
将 Whisper 语音识别速度从 **1x 实时** 提升至 **10-50x 实时**

## 方法 1：Vulkan GPU 加速（已部署 whisper-vulkan）

你的配置已经指向 `whisper-vulkan`，只需确认启用 GPU：

### 1. 验证 GPU 可用性
```bash
# 检查 Vulkan 是否可用
vulkaninfo | grep "deviceName"

# 或直接运行 whisper-cli 查看是否自动检测到 GPU
tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe --help
```

### 2. 修改 `src/engines/whisper.rs` 启用 GPU
在 `transcribe` 方法的 Command 构建部分添加 GPU 参数：

```rust
// 在第 136 行 .arg(processors.to_string()) 之后添加：
.arg("--gpu-device")
.arg("0")  // 使用第一个 GPU
.arg("--compute-type")
.arg("auto")  // 自动选择最佳精度
```

### 3. 性能对比
- **CPU (8线程)**: 1 小时视频需要 30-60 分钟
- **GPU (Vulkan)**: 1 小时视频仅需 3-10 分钟

---

## 方法 2：使用更快的 Whisper 模型

当前使用 `ggml-base.bin` (74MB)，可以根据需求调整：

| 模型 | 大小 | 速度 | 准确度 | 适用场景 |
|------|------|------|--------|----------|
| tiny | 75MB | ⚡⚡⚡⚡⚡ | ⭐⭐⭐ | 快速预览、草稿 |
| base | 142MB | ⚡⚡⚡⚡ | ⭐⭐⭐⭐ | **推荐日常使用** |
| small | 466MB | ⚡⚡⚡ | ⭐⭐⭐⭐⭐ | 高质量字幕 |
| medium | 1.5GB | ⚡⚡ | ⭐⭐⭐⭐⭐ | 专业场景 |

**建议配置**：
- 快速预览：`ggml-tiny.bin`
- 正式字幕：`ggml-base.bin`（当前已使用）

---

## 方法 3：优化 VAD 参数（已启用）

你已经启用了 Silero VAD，可以调整阈值加速：

在 `src/engines/whisper.rs:127` 修改：
```rust
.arg("-vt")
.arg("0.60")  // 从 0.50 提高到 0.60，更激进地跳过静音
```

---

## 方法 4：whisper.cpp 编译优化（推荐，已落地为脚本）

**直接用项目自带脚本一键编译并部署**（MSVC + Vulkan + Ninja）：

```bat
scripts\build_whisper_msvc_vulkan.bat
```

脚本会自动定位 Visual Studio、检查 Vulkan SDK、下载 whisper.cpp 源码、
编译 `whisper-cli`，并把 exe + 全部 DLL（含 `ggml-vulkan.dll`）复制到
`tools/whisper-vulkan/whisper-1.8.4-windows-x64/`——即 `config.toml` 默认指向的位置，
无需再改配置。

> 注意：CMake 开关是 **`-DGGML_VULKAN=ON`**（不是 `-DWHISPER_VULKAN=ON`，
> 后者是旧版写法、当前版本无效）。前端 `whisper.cpp` 仓库也已从 `ggerganov` 更名到
> `ggml-org`。

**为什么必须用 MSVC 而不是 MinGW**：MinGW 产物依赖 `libgcc_s_seh-1.dll` /
`libstdc++-6.dll` / `libgomp-1.dll`，用户机器缺一个就 `0xC0000139`
（STATUS_ENTRYPOINT_NOT_FOUND）秒退、stderr 全空，极难排查；MSVC 产物只依赖
系统自带的 VC++ 运行库（`VCRUNTIME140` / `MSVCP140` / `VCOMP140`），开箱即用。

**实测收益（本机 AMD 核显、105s 切片、`ggml-small-q5_0`）**：

| 后端 | 耗时 | 相对 CPU |
| --- | --- | --- |
| 官方 CPU 包 | 13.21 s | 1.00× |
| MSVC + Vulkan (-fa) | **7.33 s** | **1.80×** |

两者转写结果逐段一致（均为 52 段）。详见 `docs/Whisper后端基准-MSVC-Vulkan.md`。

若想手动编译（等价于脚本内部步骤）：

```bat
call "D:\...\VC\Auxiliary\Build\vcvars64.bat"
cmake -S whisper.cpp -B whisper.cpp/build -G Ninja ^
  -DCMAKE_BUILD_TYPE=Release -DGGML_VULKAN=ON -DGGML_OPENMP=ON ^
  -DWHISPER_BUILD_EXAMPLES=ON -DWHISPER_BUILD_TESTS=OFF -DWHISPER_BUILD_SERVER=OFF
cmake --build whisper.cpp/build --config Release --target whisper-cli -j
```
产物在 `whisper.cpp/build/bin/`，把该目录下 exe 与 DLL 一起拷到部署目录。
