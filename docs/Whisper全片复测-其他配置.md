# Whisper 全片复测：其他配置（`-ac` / 预处理器 / SenseVoice）

本文件是 `docs/Whisper人工标准字幕对比.md` 的配套产物：把**其他优化工作线**拉到与
「全片 CER 四档复测」完全相同的口径上，使数字可与那四档直接比较。

复测日期：2026-09-30（承接 2026-09-29 夜间被中断的任务，见
`docs/夜间任务交接-2026-09-29.md` 第 3.1 节）。

---

## 0. 口径、素材与复用说明

**评测口径**（与 `docs/Whisper人工标准字幕对比.md` 的「全片复测」和
`examples/eval_whisper_parallel.rs` 逐字相同）：

- 黄金字幕：`testVideo/03.1.3概率不等式_success.srt`（1155 条，覆盖全片）。
- 评估区间：**0 – 1942 秒**（全片；影片时长 1942.40 s）。
- 归一化：繁转简 → 只保留字母数字 → 小写。
- 指标：字符级编辑距离 / 参考字幕归一化字数。**参考字幕归一化 = 9786 字**。
- 计算工具：`examples/eval_whisper_parallel.rs`（`-- <json目录> 0 0 1942`）与
  `examples/eval_gold.rs`（`-- <字幕.srt>`）。两者是同一套归一化与编辑距离代码的两份
  拷贝；本次已交叉校验：`eval_gold` 读 `%TEMP%\v2w_full\gpu_nocarry.srt` 得
  `16.89% (1653 / 9786 字)`，与「全片复测」四档表里的 GPU 生产参数逐格相等，故两工具
  在本文件范围内可互换。

**素材**：全片 16 kHz 单声道 WAV `%TEMP%\v2w_full\clip_0_1942.wav`（62,144,120 字节），
直接复用，本次**未重新转码**。

**哪些复用、哪些重跑**：

| 项 | 状态 | 输出目录 |
| --- | --- | --- |
| `-ac` 四档（GPU） | **今天重跑**。昨晚只跑到 `ac1500` 一半就被杀，无 SRT/JSON 产出，无可复用结果 | `%TEMP%\v2w_ac_full_fa\` |
| `-ac 1500` CPU 对照 | 今天新跑（见 §1.5） | `%TEMP%\v2w_ac_full_ng\` |
| 预处理器三档 | **今天重跑**。昨晚零产出（卡在 release 编译，见 §2.1） | `%TEMP%\v2w_prep_full\` |
| SenseVoice 全片转写 | **复用昨晚产物**：那轮已完整跑完（`finished` 事件、519 句、`elapsed_sec` 502.566 s），本次未重跑推理 | `%TEMP%\v2w_sv_full\` |
| SenseVoice 的 CER | 今天新算 | 同上 |

**未动**：`src/**`、`docs/Whisper人工标准字幕对比.md`、任何更早的 `%TEMP%\v2w_*` 目录；
未执行 `cargo clean`。

## 0.1 本次最大的环境干扰：机器 CPU 今天比昨晚慢约 35%

这条先写在前面，因为它是后面所有 CPU 墙钟数字的前提。§1.5 用**与历史完全同参数、
同素材的 CPU 对照**证明：同一份 `-t 12 -p 1 -ng` 全片任务，今天墙钟 268.75 s，昨晚
记录 197.40 s（**+36%**），而两者的编码次数（59）、解码 token（10841）、句数（1212）、
编辑距离（1629）、**乃至输出 SRT 的 SHA256 都完全相同**——即内容逐字节一致，纯粹是
机器吞吐差异。复测期间机器上确有其他工具在跑（`Antigravity`、`Cursor`、
`MSPCManagerService`、`LeASPacWorker`、`cursor-byok-desktop` 等进程；这些进程的
**累计** CPU 时间合计数千秒——累计值不等于复测期间的实际占用，此处只作为「机器上
确有其他工作线」的旁证，不作为定量依据）。

因此：**本文件所有 CPU 侧墙钟与历史数字不可直接比**；同一轮内各档之间的墙钟可比
（噪声约 ±5%，见 §2.3）。GPU（Vulkan）侧受影响小得多——今天的 `-ac 1500` GPU 墙钟
80.48 s 反而比昨晚的 90.74 s 快 11%。

---

## 1. `-ac`（audio context，编码器窗口）全片四档

### 1.1 复测配置

脚本：`scripts/bench_whisper_audio_ctx_full.ps1`（本次首次实际使用；此前的 3.1 回报中
它「刚进入 ac1500 档」就被停掉了）。除 `-ac` 外全部钉死在当前生产参数（已去掉
`--carry-initial-prompt`）：

```
-m models\whisper\ggml-small-q5_0.bin  -f %TEMP%\v2w_full\clip_0_1942.wav
-l zh -t 12 -p 1  --vad -vm ggml-silero-v6.2.0.bin -vt 0.50 -vsd 250
-bo 1 -bs 1 -mc 32 -sns -nf  --prompt 以下是普通话录音。
-fa            (Vulkan GPU)
```

`-ac` 与编码窗口的关系：窗口长度 = `-ac` × 20 ms，默认 `-ac 1500` → 30 s。

调用方式：**进程内 `& script.ps1`，未使用 `-File`**（`-File` 会把 `[int[]]` 拆成位置
参数，把 `StartSec`/`DurationSec` 冲成 1000/750 —— 昨晚踩过这个坑，本次按要求避开）。

### 1.2 实测结果（全片 0–1942 s）

墙钟与 whisper 内部分项（`%TEMP%\v2w_ac_full_fa\ac_full_results.json` 与各 `.err`）：

| 档位 | 编码窗口 | 墙钟 | whisper total | 编码 | 解码 | 编码次数 | 解码 token | 句数 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `-ac 1500`（默认） | 30 s | 80.48 s | 79.98 s | 17.81 s | 51.96 s | 59 | 10967 | 1257 |
| `-ac 1000` | 20 s | 60.12 s | 59.83 s | 16.53 s | 33.68 s | 89 | 7460 | 1096 |
| `-ac 750` | 15 s | 81.35 s | 81.08 s | 11.02 s | 57.64 s | 94 | 13056 | 311 |
| `-ac 512` | 10.24 s | 61.98 s | 61.71 s | 5.86 s | 45.54 s | 75 | 10349 | 521 |

CER（`eval_whisper_parallel -- <json> 0 0 1942`；参考 9786 字）：

| 档位 | 句数 | 假设归一化字数 | 编辑距离 | CER | 相对 `-ac 1500` 速度 |
| --- | ---: | ---: | ---: | ---: | ---: |
| `-ac 1500`（默认） | 1257 | 10139 | 1653 | **16.89%** | ×1.00 |
| `-ac 1000` | 1096 | 5800 | 7007 | **71.60%** | ×1.34 |
| `-ac 750` | 311 | 12905 | 8987 | **91.84%** | ×0.99 |
| `-ac 512` | 521 | 10330 | 8613 | **88.01%** | ×1.30 |

两条口径说明：

- **`-ac 1500` 是「只改 `-ac`」的恒等对照**，且可严格复现既有基准：`ac1500.json` 与
  `%TEMP%\v2w_full\gpu_nocarry.json` 的 SHA256 相同（245,204 B），`ac1500.srt` 与
  `gpu_nocarry.srt` 的 SHA256 相同（78,803 B），编辑距离 1653、CER 16.89%、句数 1257
  逐格等于「全片复测」四档表的 GPU 生产参数。说明去掉 `--carry-initial-prompt` 后的
  生产参数在 `-ac` 默认值下可严格复现既有基准。
- `-ac 750` / `-ac 512` 的 whisper JSON 含**非法 UTF-8 字节**（分别有 4 处、2 处无法
  解码的位置），
  `eval_whisper_parallel` 直接读会报错。已用 `errors='replace'` 解出并重新落盘到
  `%TEMP%\v2w_ac_full_fa\json_sanitized\` 后再算 CER。替换字符（U+FFFD）不是字母数字，
  会被归一化丢弃，对 CER 无实际影响。**这是本次唯一的中间处理，不是原始文件。**

### 1.3 与历史窗口/裁片数字对照

历史数字出自 `docs/Whisper人工标准字幕对比.md` 第 2 节 CPU 路径表（步骤 10 / 11）：
样本是 **05:00–15:00 的 600 s 裁片**，评估区间 **305–895 s**（归一化参考 3043 字），
CPU `-ng -t 16 -p 1`，参数链**含** `--carry-initial-prompt`。

| 档位 | 历史（600 s 裁片，305–895 s） | 本次（全片 0–1942 s） | 差异 |
| --- | ---: | ---: | --- |
| `-ac 1500`（默认） | 未单列（即该表基线本身，CER 12.98%） | 16.89% | 口径不同，见下 |
| `-ac 1000` | 无历史记录 | 71.60% | — |
| `-ac 750` | 55.42 s · CER **83.80%** | 81.35 s · CER **91.84%** | **+8.0 pp** |
| `-ac 512` | 38.00 s · CER **86.30%** | 61.98 s · CER **88.01%** | **+1.7 pp** |

三点对照结论：

1. **方向完全一致，量级更差。** 裁片上 `-ac` 把 CER 打到 83.80% / 86.30%，全片上打到
   91.84% / 88.01%。「`-ac` 必须否决」这个结论在全片口径下**更强**，不是更弱。
2. **速度优势在全片上基本消失。** 裁片上 `-ac 750` 相对同链基线（62.91–77.60 s）快
   1.14–1.40 倍、`-ac 512` 快 1.66–2.04 倍；全片上以 `-ac 1500`（80.48 s）为基准，
   `-ac 750` 是 **×0.99（反而更慢）**、`-ac 512` 只有 **×1.30**。机制在数据里直接可见：
   编码确实变便宜（`ac1500` 17.81 s / 59 次 → `ac750` 11.02 s / 94 次 → `ac512`
   5.86 s / 75 次），但**解码反而变贵**（51.96 s → 57.64 s / 45.54 s，解码 token
   10967 → 13056 / 10349）。窗口变短后输出字数也失控（`ac750` 假设归一化 12905 字，
   比参考 9786 字还多 32%，是重复/幻觉），多出来的解码量吃掉了编码的节省。
3. **差异归因（谨慎）**：两处口径不同——历史是 600 s 裁片 + 305–895 s 评估窗口 + CPU，
   本次是 0–1942 s 全片 + Vulkan GPU。按「全片复测」的分桶结论，305–895 s 正好落在全片
   最干净的 400–800 s 段（四档在此 10.7–12.5%，全片 16.3–17.3%），所以裁片数字系统性
   偏乐观；但 `-ac` 的 CER 由**内容丢失量**主导，而丢失量取决于该处语音密度与 VAD 切段
   方式，**不能用「样本代表性」一条解释完**。本次没有做分桶复测，故不把 ±pp 精确归因，
   只能说：全片比裁片高 1.7–8.0 pp，方向不变。

### 1.4 结论

- `-ac 1500`（默认值）**不要动**：它是当前生产参数的一部分，且可严格复现既有基准。
- 任何小于 1500 的 `-ac` 取值在全片口径下**都是否决项**：`-ac 1000` 已丢到 71.60%，
  `-ac 750` / `-ac 512` 是 91.84% / 88.01%。
- 「`-ac` 既省不了多少时间、又毁掉内容」在全片上**更成立**：`-ac 750` 全片墙钟甚至比
  默认更慢。

### 1.5 CPU 对照（用来给 §0.1 定性）

用同一脚本、同一素材、同一参数链，只把后端换成 CPU（`-ng`），跑 `-ac 1500` 一档：

| 项 | 本次（今天） | 历史 `cpu_nocarry_mc32`（昨晚） | 差异 |
| --- | ---: | ---: | --- |
| 墙钟 | 268.75 s | 197.40 s | **+36%** |
| whisper total | 268.44 s | 197.15 s | +36% |
| 编码 | 113.24 s / 59 次 | 91.22 s / 59 次 | +24% |
| 解码 | 137.28 s / 10841 token | 91.17 s / 10841 token | +51% |
| 句数 | 1212 | 1212 | 0 |
| 编辑距离 / CER | 1629 / 16.65% | 1629 / 16.65% | 逐格相同 |
| 输出 SRT SHA256 | `3F6FF56B…E88D6E` | `3F6FF56B…E88D6E` | **逐字节相同** |

**编码次数、解码 token 数、句数、编辑距离、输出字节全部相同**，只有耗时不同，
所以这不是配置差异，是本机 CPU 吞吐/负载差异。这条同样是后面 §2 里 CPU 墙钟的解释。

---

## 2. 预处理器三档（默认 / `--no-compact` / `--no-prep`）

### 2.1 复测配置

脚本/入口：`examples/bench_preprocess.rs`（把 `src/core/pipeline.rs` 的 Whisper 路径搬到
无 UI 入口，复用生产代码的同一套滤镜链、同一套 `CompactionPlan`、同一个 `WhisperEngine`）。
命令：

```
bench_preprocess <mp4> 0 1942 <out.srt> [--no-compact | --no-prep]
```

三档含义（源码：`src/engines/audio_prep.rs`）：

- **默认**：语音增强（`highpass=70:poles=2` + `afftdn=nr=12:nf=-30:tn=1:gs=8` +
  `dynaudnorm=f=250:g=15:p=0.90:m=4.0` + soxr 重采样）+ 停顿压实
  （`plan_compaction`）。
- **`--no-compact`**：只做语音增强，不计算压实计划。
- **`--no-prep`**：不做任何预处理，直接把裁片喂给 Whisper。

`WhisperEngine` 参数：`threads=12, processors=1, use_gpu=false（CPU）,
no_fallback=true, max_context=32, yield_to_desktop=true`（即生产链路的 CPU 档）。

**为什么昨晚这条线零产出、今天为什么能跑**：昨晚它停在 `cargo run --release`，日志
三档全是同一个错误——

```
error: failed to run custom build command for `gpui v0.2.2`
thread 'main' panicked at gpui-0.2.2/build.rs:356:9: Failed to find fxc.exe
```

本机没有任何 `fxc.exe`：`C:\Program Files (x86)\Windows Kits\10` 整个目录不存在，
`C:\Program Files` 与 `C:\Program Files (x86)` 两棵树递归搜索 `fxc.exe` 的结果都是 0 个。
所以 **release 档永远编不出 `voice2word` 这个 lib**，而 `bench_preprocess` 依赖该 lib。今天的绕法：**改用 debug 档**——
debug 目标目录里 gpui 早已成功编译并缓存（`libgpui-d1dc01….rlib` 在
`D:\cargo-target\voice2word\debug\deps\`），cargo 不会重跑它的 build script，
`cargo build --offline --example bench_preprocess` 12.4 s 完成。

代价与影响：harness 自身的 Rust 代码以 `opt-level=1` 运行（见 `Cargo.toml` 的
`[profile.dev]`）。**ASR 本体是外部 `whisper-cli.exe`，不受 debug/release 影响**，
所以 `[转写]` 数字可用；但 `[预处理]` 阶段（PCM 解码、`plan_compaction`）可能被 debug
拖慢，见 §2.4 第 3 条。

构建产物目录是 `D:\cargo-target\voice2word`（由 `.cargo/config.toml` 的
`build.target-dir` 指定），不是仓库内的 `target/`。**未执行 `cargo clean`。**

### 2.2 实测结果（全片 0–1942 s）

| 档位 | 实际生效的预处理 | 裁片 | 预处理 | 转写 | 墙钟合计(示例自报) | 外层秒表 | 句数 | 字符 | 编辑距离 | CER |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **默认** | 仅增强（**压实被否，实际未压缩**） | 1.46 s | 4.12 s | 258.39 s | 263.97 s | 264.41 s | 1187 | 10188 | 1587 | **16.22%** |
| **`--no-compact`** | 仅增强 | 1.29 s | 3.84 s | 270.84 s | 275.97 s | 276.07 s | 1187 | 10188 | 1587 | **16.22%** |
| **`--no-prep`** | 无 | 1.47 s | 0.00 s | 263.51 s | 264.98 s | 265.09 s | 1211 | 10176 | 1633 | **16.69%** |

CER 用 `eval_gold -- <srt>` 计算（全片，参考 9786 字）。

### 2.3 三个关键发现

**① 「默认」档根本没走到压实路径，所以三档实际只有两个不同配置。**

默认档日志：

```
[预处理] 4.12s | 增强+压实 1942.0s -> 1942.0s (切除 0%, 压实=false)
```

`use_plan = !p.is_identity() && p.is_worthwhile(0.10)` 判为 false，于是直接用原始
（已增强的）PCM，不写临时 WAV、不做时间轴映射。**结果：默认档与 `--no-compact` 档的
输出 SRT 逐字节相同**——两者 SHA256 都是 `C6C4FB77…1498F`（1187 句 / 1587 编辑 /
16.22%）。

需要说清的一点：日志里的「切除 0%」是 `use_plan=false` 时直接拿原 PCM 造成的**显示
结果**，**不代表真实的静音占比**——`plan_compaction` 算出的实际保留比例没有被打印。
能确定的只有：**保留率 ≥ 90%**（没通过 10% 的判分点），即全片上可切除的静音本来就极少。

**② 语音增强确实有效，且几乎不花时间。**

增强档 `16.22%`（1587 处编辑）对无预处理档 `16.69%`（1633 处编辑）：**−0.47 pp**，
少 46 处编辑，句数 1187 对 1211。墙钟上：263.97 s 对 264.98 s，**净 −1.0 s**——
预处理阶段多花 4.12 s，但 ASR 阶段反而快 5.1 s（增强后的音频 VAD 切段略有不同）。
这与文档既有结论「增强并入音频通道、实测 0.0 s 额外开销」一致。

**③ 本机本轮 CPU 全片转写的重复运行噪声约 ±5%。**

默认档与 `--no-compact` 档的输入音频与输出 SRT 逐字节相同，`[转写]` 却差 12.45 s
（258.39 对 270.84，+4.8%）。所以同一轮内跨档比较墙钟时，小于约 5% 的差异不构成结论。
（这条也说明：不能拿 §2.2 里默认档 263.97 s 去和历史 197.40 s 比——那 34% 的差是机器
吞吐，见 §0.1 与 §1.5。）

### 2.4 与历史窗口/裁片数字对照

历史数字来自 `docs/Whisper人工标准字幕对比.md` 第 32–41 行，样本是**同一个 600 s 裁片**
（05:00–15:00），CPU，参数链含 `--carry-initial-prompt`；该文档明确记录
`examples/bench_preprocess.rs` **当时未接入使用**，所以**没有历史 CER 可对照**，
只有墙面耗时与其内部分项。

| 维度 | 历史（600 s 裁片） | 本次（全片 0–1942 s） | 对照 |
| --- | --- | --- | --- |
| 停顿压实 | 600 s → **538.8 s**（压掉约 10%），ASR 从 70.17 s 涨到 **112.26 s**（+60%）；编码窗口 / 解码 token 都没减少（18 / 3242 → 18 / 3218） | 全片上**连 10% 判分点都没过，压实根本未触发**（`压实=false`） | **强化**「维持关闭」的既有结论：全片比裁片更没必要开压实 |
| 语音增强 | 只说「并入 FFmpeg 通道，未单独测开销」 | −0.47 pp，墙钟净 −1.0 s | 首次给出全片口径的增强增益数字 |
| `--no-prep` vs CPU 生产参数 | `cpu_nocarry_mc32`：197.40 s / 1212 句 / 1629 编辑 / 16.65% | 264.98 s / 1211 句 / 1633 编辑 / 16.69% | **内容几乎重合**（差 1 句、4 处编辑），**墙钟 +34% 纯属机器吞吐差异**（§1.5 已用同参数对照证明） |

三条补充说明：

1. **压实那条的「+60% ASR 耗时」在本次没有复现，因为压实压根没跑。** 这不矛盾：历史
   裁片上压实能把 600 s 压到 538.8 s（过 10% 判分点），所以走了那条更慢的
   「写临时 WAV + 放弃纯内存推流」通道；全片上静音本来就只有不到 10%，判分点直接拦下。
   换言之，`preprocess_compact` 的默认关闭（`config.toml: preprocess_compact = false`）
   与代码里的 10% 判分点，在全片这个真实样本上是**冗余的双保险**。
2. **`--no-prep` 与历史 CPU 生产参数不是逐字节相同，但差异极小**：1211 对 1212 句、
   1633 对 1629 编辑。本次 ASR 音频由示例自己用 `FFmpegEngine::extract_audio_window`
   重新抽取（1.47 s，**无任何滤镜**，`-ar 16000 -ac 1 -pcm_s16le`），与
   `clip_0_1942.wav` 应是同一 PCM；剩下的 1 句 / 4 处差异**未做归因**（可能来自
   `--prompt`、`-mc` 等参数与历史组合的细微不同，或前后端版本）。
3. **debug 档对 `[预处理]` 的影响未被隔离。** 那 3.84–4.12 s 是 debug（opt-level 1）
   下的数字，release 下应更小。由于 release 档在本机编不出来（§2.1），本次无法给出
   release 对照。这一项只影响预处理阶段的小数字，不影响 CER 与 ASR 墙钟。

### 2.5 结论

- **全片口径下 `preprocess_compact` 是彻底的空操作**：连 10% 的判分点都过不了。
  维持默认关闭是对的，而且关闭的收益比文档原结论更强（原结论是「开了更慢」，
  现在是「开了也不会生效」）。
- **语音增强建议保留**：全片 −0.47 pp，不花净时间。这是本次给「增强」这条线
  补上的第一个全片口径数字。
- 「三档」实际上只测出**两个**不同配置；如果以后要真正评估压实，需要把
  `is_worthwhile(0.10)` 判分点调低（或直接旁路）才能让全片跑到压实路径。

---

## 3. SenseVoice 全片

### 3.1 复测配置与复用情况

- 转换器：`scripts/sensevoice_json_to_srt.py`；包装脚本：
  `scripts/bench_sensevoice_full.ps1`（**含中文注释，在 GBK 控制台下解析报错，
  本次未使用它**，而是直接做 JSON 形状转换，见下）。
- 输入：`%TEMP%\v2w_full\clip_0_1942.wav`（与 Whisper 各档同一份字节）。
- **复用昨晚产物**：`%TEMP%\v2w_sv_full\sensevoice_full.json`。

复核结论：该 JSON **完整可用**。顶层为 `event: "finished"`、`count: 519`、
`elapsed_sec: 502.566`，含全部 519 条 `segments`；首条起于 2.662 s、末条止于
1941.984 s，覆盖全片；`json.loads` 在严格 UTF-8 下解析成功。昨晚 3.1 回报的
「JSON 尚未成型」与事实不符——`%TEMP%\v2w_sv_full\sensevoice_full.log` 的**末行就是
`finished` 事件**，说明那轮 runner 是**自行跑完**的，不是被杀在半途。

因此本次**没有重跑那 502 秒推理**。只做格式转换：把 runner JSON 映射成
`eval_whisper_parallel` 认识的 Whisper JSON 形状
（`transcription[].offsets.{from,to}` 毫秒 + `text`），落在
`%TEMP%\v2w_sv_full\json_full\sensevoice_full.json`；同时用现有脚本产出
`%TEMP%\v2w_sv_full\sensevoice_full.srt`。CER 用
`eval_whisper_parallel -- %TEMP%\v2w_sv_full\json_full 0 0 1942` 计算——
与四档、`-ac`、预处理器同一条代码路径。

**未验证项**：该 JSON 是昨晚**直接调用 python runner**产生的，命令行只记录到
`--output %TEMP%\v2w_sv_full\sensevoice_full.json`，`--threads` / `--language` /
`--total-duration` 的取值**无法从产物或日志中恢复**。runner 的 `--threads` 默认值是
**4**，而 `bench_sensevoice_full.ps1` 的意图是 **16**。这**只影响墙钟的可比性，
不影响 CER**（本节 CER 与线程数无关）。要拿到严格同口径的墙钟，需要一次线程数已知的重跑。

### 3.2 实测结果（全片 0–1942 s）

| 项 | 数值 |
| --- | ---: |
| 句数 | 519 |
| 假设归一化字数 | 10186 |
| 参考归一化字数 | 9786 |
| 编辑距离 | 1945 |
| **全片 CER** | **19.88%** |
| 墙钟（runner 自报 `elapsed_sec`，含模型/VAD 初始化） | 502.57 s |
| 覆盖区间 | 2.662 s – 1941.984 s（全片） |

### 3.3 与历史数字对照

**SenseVoice 此前没有任何 CER 记录**——`docs/Whisper人工标准字幕对比.md` 明确写着
`tests/bench_sensevoice_parallel.rs` 未复测 CER（第 152、205 行），全仓库也搜不到
SenseVoice 的 CER 数字。所以本行**没有可对照的历史窗口/裁片 CER**，是一次全新测量。

唯一同口径可比的是「全片复测」的四档 Whisper：

| 配置 | 句数 | 编辑距离 | 全片 CER |
| --- | ---: | ---: | ---: |
| Whisper Small-Q5 CPU 出厂默认 | 1346 | 1594 | 16.29% |
| Whisper Small-Q5 CPU 当前生产参数 | 1212 | 1629 | 16.65% |
| Whisper Small-Q5 GPU 当前生产参数 | 1257 | 1653 | 16.89% |
| Whisper Small-Q5 GPU 出厂默认 | 1336 | 1693 | 17.30% |
| **SenseVoice-Small（本节）** | **519** | **1945** | **19.88%** |

即：在完全相同的全片口径与黄金字幕下，SenseVoice 的 CER 比四档 Whisper
**高 2.58–3.59 个百分点**（相对基准抬高约 15–22%）。句数只有 Whisper 的约 4 成
（519 对 1212–1346），切句粒度明显更粗。

速度侧只能给量级参考：Whisper Small-Q5 的 CPU 当前生产参数全片 197.40 s（**注意这是
昨晚机器条件下的数字，今天的 CPU 环境慢约 35%，见 §0.1**），SenseVoice 这一轮自报
502.57 s。两者线程数还未必相同，**这个倍数不作结论**。

### 3.4 结论

- SenseVoice 是**目前唯一在全片口径下 CER 明显落后于 Whisper Small-Q5 的引擎**
  （19.88% 对 16.29–17.30%），且切句粒度更粗。作为「Whisper 的质量替代品」证据不足。
- 它的价值只可能落在速度/资源侧，但本节没拿到可信的墙钟对照，需要一次线程数已知的
  重跑才能判断。
- 「SenseVoice 全片 CER 从未被测过」这个空白现已补上。

---

## 4. 未验证与不确定项（诚实清单）

1. **CPU 侧墙钟与历史数字不可比。** 今天 CPU 全片吞吐比昨晚慢约 36%
   （§1.5 用同参数、同内容、输出逐字节相同的对照证明这是机器差异）。
   本文件所有 CPU 墙钟只在同一轮内可比；本轮内的噪声地板约 ±5%（§2.3 ③）。
2. **`-ac` 的 pp 级差异没有做归因实验。** §1.3 只说全片比裁片差 1.7–8.0 pp，
   没有分桶复测，因此无法把这点差异分解成「样本代表性」与「切段方式」两部分。
3. **`-ac 750` / `-ac 512` 的 CER 经过一次字节修复**（非法 UTF-8 → U+FFFD → 归一化时
   丢弃，4 / 2 个字符）。影响应可忽略，但严格说不是原始文件的直接读数。
4. **`-ac 1000` 在仓库里找不到任何历史对照**（裁片只跑过 750 / 512）。
5. **预处理器「默认」档的真实压实保留率未知。** 只确定没通过 10% 判分点、
   保留率 ≥ 90%；`plan_compaction` 算出的具体比例没有被打印。
6. **预处理器的 `[预处理]` 阶段是 debug 档数字**（opt-level 1），release 档在本机编不出来
   （`gpui` 需要 `fxc.exe`，本机没有）。这不影响 CER 与 ASR 墙钟，但会让那 4 秒级数字偏大。
7. **`--no-prep` 与历史 `cpu_nocarry_mc32` 差 1 句 / 4 处编辑，未归因。**
8. **SenseVoice 的墙钟线程数未知**（runner 默认 4，脚本意图 16），
   因此 502.57 s 与他项不可严格比。
9. **SenseVoice 的 `elapsed_sec` 含模型与 VAD 初始化**，不是纯推理时间。

## 5. 复现命令

```powershell
# 1) -ac 四档全片（GPU/Vulkan；必须进程内调用，不要用 -File 传数组）
& .\scripts\bench_whisper_audio_ctx_full.ps1
# 1b) CPU 对照（单档）
& .\scripts\bench_whisper_audio_ctx_full.ps1 -Backend ng -AudioCtx @(1500)
# CER
cargo run --offline --example eval_whisper_parallel -- "$env:TEMP\v2w_ac_full_fa\json" 0 0 1942

# 2) 预处理器三档（debug 档可跑；release 档需要 fxc.exe，本机没有）
cargo build --offline --example bench_preprocess
$m=(Get-ChildItem .\testVideo\03.1.3*.mp4).FullName
& D:\cargo-target\voice2word\debug\examples\bench_preprocess.exe $m 0 1942 $env:TEMP\v2w_prep_full\prep_default_0_1942.srt
& D:\cargo-target\voice2word\debug\examples\bench_preprocess.exe $m 0 1942 $env:TEMP\v2w_prep_full\prep_no_compact_0_1942.srt --no-compact
& D:\cargo-target\voice2word\debug\examples\bench_preprocess.exe $m 0 1942 $env:TEMP\v2w_prep_full\prep_no_prep_0_1942.srt --no-prep
# CER
cargo run --offline --example eval_gold -- $env:TEMP\v2w_prep_full\prep_default_0_1942.srt

# 3) SenseVoice：复用 %TEMP%\v2w_sv_full\sensevoice_full.json，转成评测输入后算 CER
#    （转换是把 runner JSON 的 segments 映射成 transcription[].offsets + text）
cargo run --offline --example eval_whisper_parallel -- "$env:TEMP\v2w_sv_full\json_full" 0 0 1942
```
