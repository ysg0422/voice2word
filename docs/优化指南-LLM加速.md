# LLM 大模型润色速度优化

## 🎯 目标
将 Qwen 润色时间从 **几分钟** 降低到 **几秒**

## 当前状态分析

你的配置：
- 模型：`qwen2.5-0.5b-instruct-q4_k_m.gguf` (300MB，已经是最小量化版本)
- 线程：8 核心
- 上下文：4096 tokens
- 批处理：每批 8 条字幕

**已经做得很好的地方**：
✅ 使用了常驻服务模式（复用已加载模型）
✅ 批量处理字幕（8 条/批）
✅ 使用了 4-bit 量化模型

---

## 优化方案

### 1️⃣ **提高批处理大小**（推荐，立即见效）

修改 `src/engines/llm.rs` 第 15-16 行：

```rust
// 原配置（保守）
const MAX_SEGMENTS_PER_BATCH: usize = 8;
const MAX_BATCH_CHARS: usize = 1_200;

// 优化配置（激进，速度提升 2-3 倍）
const MAX_SEGMENTS_PER_BATCH: usize = 24;  // 8 → 24
const MAX_BATCH_CHARS: usize = 3_600;      // 1200 → 3600
```

**原理**：
- Qwen 0.5B 的 4096 上下文可以容纳更多字幕
- 减少模型调用次数 = 减少推理开销
- 例如 240 条字幕：从 30 批 → 10 批

**权衡**：
- ✅ 速度提升 2-3 倍
- ⚠️ 单批失败影响范围更大（可接受）

---

### 2️⃣ **降低温度参数，加速生成**

修改 `src/engines/llm.rs` 第 67 行：

```rust
// 原配置
"temperature": 0.05,

// 优化配置（确定性更强，速度更快）
"temperature": 0.01,
```

再修改第 177 行：

```rust
.arg("--temp")
.arg("0.01")  // 从 0.05 降低到 0.01
```

---

### 3️⃣ **启用 GPU 加速（效果最佳）**

如果你有 NVIDIA GPU，重新编译 llama.cpp 启用 CUDA/Vulkan：

```bash
# 方案 A：CUDA 加速（NVIDIA 独显）
cd llama.cpp
cmake -B build -DLLAMA_CUDA=ON -DCMAKE_BUILD_TYPE=Release
cmake --build build --config Release

# 方案 B：Vulkan 加速（通用 GPU）
cmake -B build -DLLAMA_VULKAN=ON -DCMAKE_BUILD_TYPE=Release
cmake --build build --config Release
```

然后在 `src/engines/llm.rs` 第 166 行添加 GPU 参数：

```rust
let output = Command::new(&self.cli_path)
    .arg("-m")
    .arg(&self.model_path)
    .arg("-c")
    .arg(self.ctx_size.to_string())
    .arg("-t")
    .arg(self.threads.to_string())
    .arg("-ngl")      // 新增：GPU 层数
    .arg("99")        // 新增：全部层使用 GPU
    .arg("-f")
    // ... 其他参数
```

**性能对比**：
- CPU (8核)：240 条字幕约 2-3 分钟
- GPU (CUDA)：240 条字幕约 10-20 秒（提升 10x）

---

### 4️⃣ **优化提示词长度**

修改 `src/engines/llm.rs` 第 279-282 行：

```rust
// 原提示词（较长）
let prompt = format!(
    "<|im_start|>system\n你是严格的字幕润色工具。为每条语音识别字幕添加标点并修正明显错字，保持原意。必须逐行输出，格式为 [序号] 润色文本；不解释，不合并，不遗漏。<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n",
    source
);

// 优化提示词（精简 30%）
let prompt = format!(
    "<|im_start|>system\n字幕润色工具：添加标点、修正错字。格式：[序号] 文本<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n",
    source
);
```

---

### 5️⃣ **可选：完全禁用润色**

如果你对 Whisper 原始识别结果满意，可以直接关闭润色：

在 UI 中点击 **"Qwen LLM 润色"** 开关，或在 `config.toml` 中：

```toml
enable_polish = false  # 关闭润色
```

**效果**：
- 总处理时间减少 30-50%
- 适合快速预览或短视频

---

## 综合优化建议

### 配置 1：极速模式（牺牲少量质量）
```rust
MAX_SEGMENTS_PER_BATCH: usize = 32;
MAX_BATCH_CHARS: usize = 4_800;
temperature: 0.01
enable_polish = false  // 直接关闭
```

### 配置 2：平衡模式（推荐）
```rust
MAX_SEGMENTS_PER_BATCH: usize = 24;
MAX_BATCH_CHARS: usize = 3_600;
temperature: 0.01
enable_polish = true
+ GPU 加速
```

### 配置 3：质量优先
```rust
// 保持当前配置
enable_polish = true
+ GPU 加速
```

---

## 实际效果预估

以 1 小时视频（约 240 条字幕）为例：

| 配置 | 润色时间 | 提升 |
|------|---------|------|
| 当前 (CPU, 8条/批) | ~3 分钟 | 基准 |
| 优化批处理 (24条/批) | ~1 分钟 | 3x |
| + GPU 加速 | ~10 秒 | 18x |
| 关闭润色 | 0 秒 | ∞ |
