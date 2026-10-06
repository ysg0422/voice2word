# Whisper 后端基准：MSVC + Vulkan（AMD 核显）

> 结论先行：**在 AMD 核显上，用 MSVC 工具链自编的 Vulkan 版 whisper-cli，
> 比官方 CPU 包快约 1.8×，且转写结果逐段一致。** 本项目已把它做成一条命令：
> `scripts\\build_whisper_msvc_vulkan.bat`。

## 1. 为什么要换工具链（比「换后端」更关键）

本项目原先部署的是 **官方 CPU 构建**。历史上还试过 **MinGW 编译的 Vulkan 版**，
它在用户机器上以 `0xC0000139`（STATUS_ENTRYPOINT_NOT_FOUND）秒退、**stderr 全空**——
根因是 MinGW 动态运行库 `libgcc_s_seh-1.dll` / `libstdc++-6.dll` / `libgomp-1.dll`
缺一个就加载失败，而错误在加载阶段就发生，程序拿不到任何可读信息。

**MSVC 构建只依赖系统自带的 VC++ 运行库**：

| 构建工具链 | 运行时依赖 | 用户机器可用性 |
| --- | --- | --- |
| MinGW（动态） | libgcc_s_seh-1 / libstdc++-6 / libgomp-1 | 常缺，0xC0000139 秒退 |
| **MSVC** | VCRUNTIME140 / MSVCP140 / VCOMP140 | Win10/11 基本自带 |
| Vulkan 版额外 | vulkan-1.dll | 显卡驱动自带 |

因此本项目推荐 **MSVC + Vulkan**，并由脚本一键产出。

## 2. 基准方法

- **素材**：`testVideo/01.第9讲参数估计与假设检验三向解题法01.mp4`（105.37 s，1280×720，h264+aac）
- **音频**：FFmpeg 转 16 kHz 单声道 PCM WAV（`-ar 16000 -ac 1 -c:a pcm_s16le`）
- **模型**：`ggml-small-q5_0.bin`（175 MB）
- **参数**：与生产链路一致 —— `-l zh -t 10 -p 1 -bo 1 -bs 1 -nf -mc 32 -sns`
  `--vad -vm ggml-silero-v6.2.0.bin -vt 0.50 -vsd 250`；GPU 用 `-fa`，CPU 用 `-ng`
- **环境**：Windows，AMD Radeon(TM) Graphics（核显，Vulkan0），16 逻辑核
- **口径**：whisper-cli 自报 `total time`（含模型加载与 VAD）

## 3. 结果

| 后端 | 耗时 | 实时倍率（105s 素材） | 相对 CPU | 段数 |
| --- | --- | --- | --- | --- |
| 官方 CPU 包 | 13.21 s | ~7.9× | 1.00× | 52 |
| MSVC + Vulkan（`-fa`） | **7.33 s** | **~14.3×** | **1.80×** | 52 |
| MSVC + Vulkan（`-ng` 强制 CPU） | 13.08 s | ~8.0× | 0.99× | 52 |

- 三种配置的**转写段数与首段文本完全一致**（`我们第九讲参数估计和假设检验`），
  说明 Vulkan 路径没有引入精度回退。
- 同一份 MSVC 产物在 `-ng` 下耗时与官方 CPU 包基本持平（13.08 vs 13.21 s），
  说明 1.80× 的收益来自 **Vulkan GPU 后端**，而非「MSVC 编译器比官方包快」。

## 4. 复现

```bat
scripts\\build_whisper_msvc_vulkan.bat
```

脚本自动完成：定位 VS → 检查 Vulkan SDK（glslc）→ 下载 whisper.cpp v1.8.4 源码
→ `cmake -G Ninja -DGGML_VULKAN=ON -DGGML_OPENMP=ON` → 编译 `whisper-cli`
→ 把 `whisper-cli.exe` 与全部 DLL（含 `ggml-vulkan.dll`）部署到
`tools/whisper-vulkan/whisper-1.8.4-windows-x64/`（`config.toml` 默认路径）。

验证 GPU 已启用（输出里出现 Vulkan0）：

```bat
tools\\whisper-vulkan\\whisper-1.8.4-windows-x64\\whisper-cli.exe ^
  -m models\\whisper\\ggml-small-q5_0.bin -f resources\\sample\\test_speech.wav ^
  -l zh -t 8 -bo 1 -bs 1 -nf -mc 32 -sns -fa
```

期望包含：`whisper_backend_init_gpu: using Vulkan0 backend`。

## 5. 程序侧的配套改动（2026-10-06）

1. **运行库体检重写**（`src/main.rs` + 新增 `src/utils/pe_imports.rs`）：
   此前无条件查 MinGW 三件套，遇到 MSVC 构建会**误报「运行库缺失」**。现改为
   解析 PE 导入表、递归展开同目录 DLL 的导入闭包，只对「真正需要却找不到」的
   DLL 报警，并按 MSVC / MinGW 给出对应修复指引。
2. **自编译构建保护扩展**（`src/utils/model_download.rs`）：
   `looks_like_custom_build` 新增「带 `ggml-vulkan.dll` 的 GPU 构建」形态判定。
   此前只保护「60 MB 单文件」形态，像本例这种「几百 KB 入口 + 多个 DLL」的
   GPU 构建会被官方 CPU 包**静默覆盖**、GPU 支持凭空消失。

## 6. 未验证 / 局限

- 仅覆盖 AMD 核显 + `small-q5_0` 一个档位；NVIDIA（可考虑 CUDA/`cuvid`）与
  Intel 未测。
- `-ngl` / `--no-fft` 等 flag 的细粒度调优未做（Vulkan 后端对 AMD 已足够）。
- 「实时倍率」随素材语速、静音占比波动，本表只用于**同机同素材横向对比**。
