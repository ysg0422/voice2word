# Whisper GPU 加速配置指南

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

## 方法 4：whisper.cpp 编译优化

如果你有 C++ 编译环境，可以自己编译 whisper.cpp 启用所有优化：

```bash
# 克隆 whisper.cpp
git clone https://github.com/ggerganov/whisper.cpp.git
cd whisper.cpp

# 启用 Vulkan + 全优化编译
cmake -B build -DWHISPER_VULKAN=ON -DCMAKE_BUILD_TYPE=Release
cmake --build build --config Release

# 编译产物在 build/bin/Release/whisper-cli.exe
```

然后修改 `config.toml`:
```toml
whisper_cli = "tools/whisper-custom/whisper-cli.exe"
```
