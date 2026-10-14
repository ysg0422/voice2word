//! 音频前端预处理：语音增强滤镜链 + 停顿压实 + 时间轴回映射。
//!
//! # 为什么做前端预处理，而不是继续调线程
//!
//! 实测（30s 样片 / small-q5 / 16 线程 / CPU）：
//!
//! | 阶段   | 耗时    | 占比  |
//! |--------|---------|-------|
//! | mel    | 0.03 s  | 0.0%  |
//! | encode | 2.8 s   | 3.1%  |
//! | decode | 87.5 s  | 96.9% |
//!
//! 同一份日志里 `prompt time` 显示「≥16 token 的批量前向」是 19 ms/token，
//! 而 `decode time` 是 148 次「单 token 前向」、每次 591~636 ms——**单 token
//! 解码的每次调用有约 0.5 秒的固定开销，与 token 内容无关，只与调用次数成正比**。
//! 编码器批量前向在这台机器上能跑到 95 GFLOPS，说明硬件没问题；瓶颈是
//! batch=1 解码那条「12 层 × 十几个算子」的串行延迟链，加线程只会增加同步开销。
//!
//! 因此有效的方向只有两个：**减少喂给模型的音频量**、**减少解码调用次数**。
//! 本模块负责前者，手段是：
//!
//! 1. **语音增强滤镜链**：高通去低频轰鸣 → FFT 谱减降噪 → 语音电平归一 →
//!    soxr 高质量重采样。噪声与过低电平是 Whisper 产生幻觉 token、以及
//!    误判 `no_speech` 从而切出大量碎段的主因，而每个碎段都要重付一轮
//!    SOT/时间戳解码。
//! 2. **停顿压实**：把句间长静音整段切除，模型只解码真正的语音。
//! 3. **时间轴回映射**：切除后时间轴不再连续，必须按映射表把片段起止还原
//!    回原视频时间轴，否则字幕会整体前移。

use anyhow::{Context, Result};
use std::path::Path;

/// 全链路统一的音频采样率（Whisper 要求 16 kHz 单声道）。
pub const SAMPLE_RATE: u32 = 16_000;

/// 高通截止频率上限（Hz）。
///
/// 用户可以在 config 里手写任意 f64（JSON 里的 `1e400` 会解析成 `inf`）。实测
/// ffmpeg 对 `highpass=f` 的可接受范围是 `[0, 999999]`：`inf` / `1e300` 会被判
/// "out of range"，让整条滤镜图直接失败（`-af` 报错 → 预处理整体失败）；而
/// 落在 `(源 Nyquist, 999999]` 的值不报错、却把语音整段削成静音。
/// 取 2000 Hz：低于任何常见语音采样率（≥8 kHz）的 Nyquist，高通永远不可能把
/// 整段语音削没，同时离默认值 70 Hz / UI 上限 200 Hz 还有十倍余量。
const MAX_HIGHPASS_HZ: f64 = 2_000.0;

/// `aresample` 允许的目标采样率区间（Hz）。0 表示不做重采样。
const MIN_TARGET_RATE: u32 = 8_000;
const MAX_TARGET_RATE: u32 = 192_000;

/// `atempo` 的速率**下界**：1.0，而不是 ffmpeg 允许的 0.5。
///
/// 本工程只把变速当作「加速以减少送进模型的音频量」的手段：`core/pipeline.rs` 已经把
/// `audio_speed` 夹在 `[1.0, 1.5]`，变慢只会把音频拉长、让 batch=1 的解码调用次数变多，
/// 正是本模块要消掉的那部分开销；停顿压实的时间轴回映射也是按 `× speed`（speed ≥ 1）
/// 还原原始时间轴的。所以这里对外承诺的区间是 `[1.0, 100.0]`，不再对外声称 `[0.5, 100]`。
const MIN_SPEED: f64 = 1.0;

/// `atempo` 的速率上限（实测 ffmpeg 对 `tempo` 的合法区间是 `[0.5, 100]`；
/// 本模块只使用其中的 `[1.0, 100]`，下界理由见 [`MIN_SPEED`]）。
const MAX_SPEED: f64 = 100.0;

// ───────────────────────── 语音增强滤镜链 ─────────────────────────

/// 语音增强开关集合。默认值按「保守但有效」选取：
/// 只做有明确物理依据的处理，不引入会改变音色的强处理。
#[derive(Debug, Clone, Copy)]
pub struct SpeechFilterOptions {
    /// 高通截止频率（Hz）。0 = 关闭。
    /// Whisper 的 mel 从 80 Hz 起，低于此的频率只贡献能量不贡献信息；
    /// 去直流与低频轰鸣还能避免 VAD 被低频噪声触发。
    /// 拼串时会净化：`NaN` / `inf` / 负值按关闭处理，上限见 [`MAX_HIGHPASS_HZ`]。
    pub highpass_hz: f64,
    /// FFT 谱减降噪（afftdn）。稳态底噪（空调、风扇、电流声）会让
    /// Whisper 在静音处「听出」词语。
    pub denoise: bool,
    /// 语音动态电平归一（dynaudnorm）。远场/手机录音电平常低到
    /// 让 `no_speech_thold` 误判，归一后 VAD 阈值也才可靠。
    pub normalize: bool,
    /// 输出采样率。0 = 不重采样；非 0 时限制在 `[MIN_TARGET_RATE, MAX_TARGET_RATE]`。
    pub target_rate: u32,
}

impl Default for SpeechFilterOptions {
    fn default() -> Self {
        Self {
            highpass_hz: 70.0,
            denoise: true,
            normalize: true,
            target_rate: SAMPLE_RATE,
        }
    }
}

/// 净化之后**实际会生效**的语音增强级。
///
/// 存在的唯一理由：让 `is_noop()` 与 `chain()` 同源。两处各写一遍判断条件，同一份配置
/// 就会出现「`is_noop()` 说没什么可做、`chain()` 却拼出了 `aresample`」这种自相矛盾的
/// 组合（只改 `target_rate` 时必然发生）；`NaN` 更糟——`NaN <= 0.0` 是 `false`，于是
/// `chain()` 那边按「关闭」处理、`is_noop()` 这边却认为「高通开着」，结论正好相反。
/// 所以「哪些级会生效」只判断一次，两边都只读这个结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveStages {
    /// 取整并限幅后的高通截止频率（Hz）。0 = 关闭。
    highpass_hz: u32,
    denoise: bool,
    normalize: bool,
    /// 限幅后的目标采样率（Hz）。0 = 不重采样。
    target_rate: u32,
}

impl ActiveStages {
    /// 一个级都不生效。
    fn is_empty(&self) -> bool {
        self.highpass_hz == 0 && !self.denoise && !self.normalize && self.target_rate == 0
    }
}

impl SpeechFilterOptions {
    /// 把用户可手写的裸值净化成「实际会生效的级」。
    ///
    /// config 里这两项都是裸数值（JSON 的 `1e400` 会解析成 `inf`）：`NaN` / `inf` /
    /// 负值 / 取整后不足 1 Hz 的高通一律按**关闭该级**处理——`inf` 拼进 `-af` 会被
    /// ffmpeg 判 out of range 让整条链失败，落在 `(Nyquist, 999999]` 的值不报错、
    /// 却把语音静默削没（见 [`MAX_HIGHPASS_HZ`]）；非法采样率（1 Hz / `u32::MAX`）ffmpeg
    /// 同样不报错、只把音频重采样成静音，所以也要限幅。取整成整数 Hz 再拼串是因为
    /// `{:.0}` 的银行家舍入会把 (0, 0.5) 里的值格式化成 `highpass=f=0`（实测 ffmpeg 把
    /// f=0 当无滤波），配置上却显示「高通已开」。
    ///
    /// `is_noop()` 与 [`Self::chain_with_speed`] 都只依据这个结果，不再各自比较原始字段。
    fn active_stages(&self) -> ActiveStages {
        ActiveStages {
            highpass_hz: if self.highpass_hz.is_finite() && self.highpass_hz > 0.0 {
                self.highpass_hz.min(MAX_HIGHPASS_HZ).round() as u32
            } else {
                0
            },
            denoise: self.denoise,
            normalize: self.normalize,
            target_rate: if self.target_rate == 0 {
                0
            } else {
                self.target_rate.clamp(MIN_TARGET_RATE, MAX_TARGET_RATE)
            },
        }
    }

    /// 是否完全不做任何处理。
    ///
    /// 恒等式：`is_noop() == chain().is_none()`——两者都由 [`Self::active_stages`] 决定，
    /// 调用方据此判断「要不要走预处理通道」时不会再和实际拼出来的滤镜链打架。
    pub fn is_noop(&self) -> bool {
        self.active_stages().is_empty()
    }

    /// 构建 FFmpeg `-af` 滤镜链（不含变速）。返回 `None` 表示无需处理。
    ///
    /// 顺序有讲究：**先降噪再归一**。反过来的话，归一器会把底噪一起抬起来，
    /// 降噪器面对的噪声底已经被人为放大，谱减量估不准，反而更容易产生
    /// 音乐化伪影（musical noise），而伪影比原噪声更伤 ASR。
    pub fn chain(&self) -> Option<String> {
        self.chain_with_speed(1.0)
    }

    /// 在增强链之后追加 `atempo` 变速，最后统一重采样到目标采样率。
    ///
    /// 变速放在降噪/归一之后：降噪器与归一器都基于「自然语速下的语音统计量」
    /// 估计参数，先变速会让它们看到非自然的包络。
    pub fn chain_with_speed(&self, speed: f64) -> Option<String> {
        // 「哪些级会生效」（含非有限值 / 非法采样率的净化与限幅）只算一次，且与
        // `is_noop()` 共用，见 [`Self::active_stages`]。
        let active = self.active_stages();
        let mut stages: Vec<String> = Vec::new();

        if active.highpass_hz > 0 {
            // poles=2 是 12 dB/oct，足够削掉直流与轰鸣又不动 80 Hz 以上的语音基频
            stages.push(format!("highpass=f={}:poles=2", active.highpass_hz));
        }
        if active.denoise {
            // nr=12：只衰减 12 dB，宁可少降也不引入伪影
            // nf=-30：初始噪声底 -30 dBFS（典型室内底噪量级）
            // tn=1：自适应跟踪噪声底，避免把持续的低电平语音当成噪声削掉
            // gs=8：增益平滑，抑制音乐化伪影
            stages.push("afftdn=nr=12:nf=-30:tn=1:gs=8".to_string());
        }
        if active.normalize {
            // f=250：250 ms 分析窗，匹配语音音节尺度
            // p=0.90：目标峰值，留 1 dB 余量避免削顶
            // m=4.0：最大放大 4 倍（12 dB）。这个上限很关键——不设限的话
            //        归一器会把句间底噪放大到「像语音」，反而制造幻觉
            stages.push("dynaudnorm=f=250:g=15:p=0.90:m=4.0".to_string());
        }
        // ffmpeg 对 atempo 的合法区间是 [0.5, 100]：`inf` / `NaN` 会被判 out of range
        // 让整条滤镜图失败（实测 exit≠0）。上游虽然已经 limit 过一次，这里作为公开 API
        // 仍然自己兜底：非有限值 = 不变速，其余限幅到本模块的契约区间
        // [`MIN_SPEED`, `MAX_SPEED`] = [1.0, 100.0]（下界不是 0.5，理由见 MIN_SPEED）。
        let speed = if speed.is_finite() {
            speed.clamp(MIN_SPEED, MAX_SPEED)
        } else {
            MIN_SPEED
        };
        if speed > MIN_SPEED {
            // 1.0~1.5 区间内 atempo 不需要级联（>2.0 才需要）
            stages.push(format!("atempo={speed:.3}"));
        }
        if active.target_rate != 0 {
            // 0 = 不重采样（保留源采样率）；非 0 已经在 `active_stages()` 里限幅到
            // `[MIN_TARGET_RATE, MAX_TARGET_RATE]`。
            // soxr 在 16 kHz 目标下的阻带抑制明显优于默认 swr，
            // mel 谱更干净；precision=28 是 32 位浮点下的实用上限
            stages.push(format!(
                "aresample={}:resampler=soxr:precision=28:cutoff=0.97",
                active.target_rate
            ));
        }

        if stages.is_empty() {
            None
        } else {
            Some(stages.join(","))
        }
    }
}

// ───────────────────────── 停顿压实 ─────────────────────────

/// 停顿压实参数。默认值面向「课堂/讲座录音」：句间停顿通常 0.5~3 s，
/// 而语流内部的换气停顿 < 0.3 s，两者用 300 ms 的合并阈值可以干净分开。
#[derive(Debug, Clone, Copy)]
pub struct CompactionConfig {
    /// 能量分析帧长（ms）
    pub frame_ms: u32,
    /// 噪声底噪估计所用的分位数（0.20 = 取第 20 百分位）
    pub noise_percentile: f64,
    /// 语音判定门限 = 噪声底 + 该余量（dB）
    pub speech_margin_db: f64,
    /// 绝对静音门限（dBFS）。噪声底估计偏低时的兜底。
    pub abs_floor_db: f64,
    /// 最短语音段（ms），更短的判为瞬态噪声丢弃
    pub min_speech_ms: u32,
    /// 段间间隔小于该值则合并（ms），避免把词间换气切成碎段
    pub min_gap_ms: u32,
    /// 语音段前后额外保留（ms），防止削掉起音与尾音
    pub pad_ms: u32,
    /// 静音切除后每处保留的过渡长度（ms），并入下一段语音的前沿。
    /// 全切干净会让相邻两句之间没有任何边界线索，模型容易把两句粘成一句。
    pub keep_gap_ms: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            frame_ms: 20,
            noise_percentile: 0.20,
            speech_margin_db: 9.0,
            abs_floor_db: -48.0,
            min_speech_ms: 200,
            min_gap_ms: 300,
            pad_ms: 180,
            keep_gap_ms: 120,
        }
    }
}

/// 压实后时间轴上的一个片段（秒）。`compact_start` 是压实后音频里的起点，
/// `orig_start` 是它在原始音频里的起点。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimePiece {
    pub compact_start: f64,
    pub orig_start: f64,
    pub duration: f64,
}

/// 压实结果：保留的样本区间 + 时间轴映射表。
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// 需要保留的原始样本区间（左闭右开），按时间升序且互不相接
    pub spans: Vec<(usize, usize)>,
    pub map: Vec<TimePiece>,
    pub total_samples: usize,
    pub kept_samples: usize,
    pub sample_rate: u32,
}

impl CompactionPlan {
    /// 恒等计划：不做任何切除，映射表把时间原样返回。
    /// 关闭压实时直接用它，让下游的回映射逻辑只有一条代码路径。
    pub fn identity(total_samples: usize, rate: u32) -> Self {
        Self {
            spans: if total_samples == 0 {
                Vec::new()
            } else {
                vec![(0, total_samples)]
            },
            map: if total_samples == 0 {
                Vec::new()
            } else {
                vec![TimePiece {
                    compact_start: 0.0,
                    orig_start: 0.0,
                    duration: total_samples as f64 / rate.max(1) as f64,
                }]
            },
            total_samples,
            kept_samples: total_samples,
            sample_rate: rate,
        }
    }

    /// 是否真的切掉了内容（恒等计划为 false）
    pub fn is_identity(&self) -> bool {
        self.kept_samples >= self.total_samples
    }

    /// 保留比例（1.0 = 没有可切除的静音）
    pub fn ratio(&self) -> f64 {
        if self.total_samples == 0 {
            1.0
        } else {
            self.kept_samples as f64 / self.total_samples as f64
        }
    }

    /// 是否值得走压实路径：静音占比太低时，压实带来的收益抵不过
    /// 「多一次完整解码 + 多写一个临时 WAV」的开销。
    pub fn is_worthwhile(&self, min_saving: f64) -> bool {
        1.0 - self.ratio() >= min_saving
    }

    /// 把压实时间轴上的时刻还原到原始时间轴。
    pub fn to_original(&self, t: f64) -> f64 {
        if self.map.is_empty() {
            return t;
        }
        let idx = match self.map.binary_search_by(|p| {
            p.compact_start
                .partial_cmp(&t)
                .unwrap_or(std::cmp::Ordering::Equal)
        }) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        };
        let piece = &self.map[idx];
        // 落在片段尾之后（浮点误差）时按线性外推，保证映射单调
        (piece.orig_start + (t - piece.compact_start)).max(0.0)
    }

    /// 还原一个时间区间，并保证 end >= start。
    pub fn to_original_span(&self, start: f64, end: f64) -> (f64, f64) {
        let s = self.to_original(start);
        let e = self.to_original(end).max(s);
        (s, e)
    }

    /// 按映射表把压实音频还原成连续音频样本。
    pub fn materialize(&self, pcm: &[i16]) -> Vec<i16> {
        let mut out = Vec::with_capacity(self.kept_samples);
        for &(s, e) in &self.spans {
            let s = s.min(pcm.len());
            let e = e.min(pcm.len());
            if e > s {
                out.extend_from_slice(&pcm[s..e]);
            }
        }
        out
    }
}

/// 逐帧 RMS（dBFS），用于能量 VAD。
fn frame_db(pcm: &[i16], frame: usize) -> Vec<f64> {
    // 帧长按 1 兜底：`pcm.chunks(0)` 会直接 panic（首帧都进不去）。
    let frame = frame.max(1);
    let mut out = Vec::with_capacity(pcm.len() / frame + 1);
    for chunk in pcm.chunks(frame) {
        if chunk.is_empty() {
            continue;
        }
        let sum: f64 = chunk
            .iter()
            .map(|&s| {
                let v = s as f64;
                v * v
            })
            .sum();
        let rms = (sum / chunk.len() as f64).sqrt();
        let db = if rms <= 1.0 {
            -100.0
        } else {
            20.0 * (rms / 32768.0).log10()
        };
        out.push(db.max(-100.0));
    }
    out
}

/// 用分位数估计噪声底。比「最小帧」稳健：最小帧常被偶发的一个纯静音块拉低。
fn estimate_noise_floor(dbs: &[f64], percentile: f64) -> f64 {
    if dbs.is_empty() {
        return -100.0;
    }
    let mut sorted = dbs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p = percentile.clamp(0.0, 1.0);
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// 毫秒 → 帧数，向上取整。
///
/// 必须向上取整：按截断算，150 ms 退出保持 / 20 ms 帧长会退化成 7 帧（140 ms），
/// 静音尾巴少 10 ms，尾音更容易被削；帧长 16 ms 时只剩 9 帧（144 ms）。
fn ms_to_frames(ms: u32, frame_ms: u32) -> usize {
    ms.div_ceil(frame_ms.max(1)) as usize
}

/// 能量 VAD + 停顿压实规划。
///
/// 算法：
/// 1. 20 ms 逐帧 RMS → dBFS；
/// 2. 取第 20 百分位作噪声底，门限 = max(噪声底 + 9 dB, -48 dBFS)；
/// 3. 双门限滞回（进入门限、退出门限低 3 dB，退出需连续 150 ms）切出语音游程；
/// 4. 丢弃 < 200 ms 的游程（瞬态噪声），合并间隔 < 300 ms 的游程（词间换气）；
/// 5. 前后各留 180 ms，防止削掉起音与尾音；
/// 6. 静音区每处只保留 120 ms 作过渡，其余整段切除；
/// 7. 按保留区间生成时间轴映射表。
pub fn plan_compaction(pcm: &[i16], rate: u32, cfg: &CompactionConfig) -> CompactionPlan {
    let total = pcm.len();
    let identity = |total: usize, rate: u32| CompactionPlan::identity(total, rate);

    // 采样率 0 无法解释时间轴：帧长、时段与映射表都要按 rate 换算，放过去会在
    // 映射表里算出 `inf` 时长（`(e - s) as f64 / 0.0`），回映射时把整条字幕时间轴
    // 打飞。0 采样率下不做任何判断，直接退回恒等计划。
    if rate == 0 {
        return identity(total, rate);
    }

    let frame = ((rate as u64 * cfg.frame_ms.max(1) as u64) / 1000).max(1) as usize;
    if total < frame * 4 {
        return identity(total, rate);
    }

    let dbs = frame_db(pcm, frame);
    let noise_floor = estimate_noise_floor(&dbs, cfg.noise_percentile);
    let enter_db = (noise_floor + cfg.speech_margin_db).max(cfg.abs_floor_db);
    let exit_db = enter_db - 3.0;

    let exit_hold = ms_to_frames(150, cfg.frame_ms).max(1);
    let min_speech = ms_to_frames(cfg.min_speech_ms, cfg.frame_ms).max(1);
    let min_gap = ms_to_frames(cfg.min_gap_ms, cfg.frame_ms);
    let pad = ((cfg.pad_ms as u64 * rate as u64) / 1000) as usize;
    let keep_gap = ((cfg.keep_gap_ms as u64 * rate as u64) / 1000) as usize;

    // ── 双门限滞回切游程 ──
    let mut runs: Vec<(usize, usize)> = Vec::new(); // 帧下标，左闭右开
    let mut in_speech = false;
    let mut run_start = 0usize;
    let mut below = 0usize;
    for (i, &db) in dbs.iter().enumerate() {
        if !in_speech {
            if db >= enter_db {
                in_speech = true;
                run_start = i;
                below = 0;
            }
        } else if db < exit_db {
            below += 1;
            if below >= exit_hold {
                runs.push((run_start, i + 1 - below));
                in_speech = false;
            }
        } else {
            below = 0;
        }
    }
    if in_speech {
        runs.push((run_start, dbs.len()));
    }

    // ── 丢短游程 + 合并近邻 ──
    runs.retain(|&(s, e)| e - s >= min_speech);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for run in runs {
        match merged.last_mut() {
            Some(last) if run.0.saturating_sub(last.1) < min_gap => last.1 = run.1,
            _ => merged.push(run),
        }
    }

    // ── 转样本区间、加前后留白、再次合并 ──
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for &(s, e) in &merged {
        let s = (s * frame).saturating_sub(pad);
        let e = ((e * frame) + pad).min(total);
        match spans.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => spans.push((s, e)),
        }
    }
    if spans.is_empty() {
        // 全程被判定为非语音（纯静音/纯噪声）：不压实，交给 VAD 处理
        return identity(total, rate);
    }

    // ── 静音区只留过渡 ──
    //
    // 过渡必须**紧贴下一段语音的前沿**，而不是跟在上一段语音的尾巴后面：
    // 若写成 `(prev_end, prev_end + keep)`，中间剩下的整段静音仍被切掉，
    // 保留下来的 120 ms 就成了一段孤立的静音块（既不在语音里，也不是边界线索），
    // 还会多产出一个时间轴片段。这里改成把过渡并入下一段的起点。
    let mut kept: Vec<(usize, usize)> = Vec::new();
    for (i, &(s, e)) in spans.iter().enumerate() {
        let s = if i > 0 {
            let prev_end = spans[i - 1].1;
            let keep = s.saturating_sub(prev_end).min(keep_gap);
            s - keep
        } else {
            s
        };
        kept.push((s, e));
    }
    kept.retain(|&(s, e)| e > s);

    let kept_samples: usize = kept.iter().map(|&(s, e)| e - s).sum();
    // 收益太小就退回原样：多一次完整解码不划算
    if kept_samples as f64 >= total as f64 * 0.95 {
        return identity(total, rate);
    }

    // ── 生成映射表 ──
    let mut map = Vec::with_capacity(kept.len());
    let mut cursor = 0usize;
    for &(s, e) in &kept {
        let duration = (e - s) as f64 / rate as f64;
        map.push(TimePiece {
            compact_start: cursor as f64 / rate as f64,
            orig_start: s as f64 / rate as f64,
            duration,
        });
        cursor += e - s;
    }

    CompactionPlan {
        spans: kept,
        map,
        total_samples: total,
        kept_samples,
        sample_rate: rate,
    }
}

// ───────────────────────── WAV 输出 ─────────────────────────

/// 组装 16 位单声道 PCM 的 44 字节标准 WAV 头。
///
/// 单独成函数是为了让「样本数超 4 GiB」能在不开 4 GiB 缓冲的前提下被测到：
/// 原实现直接 `(samples.len() * 2) as u32`，超 4 GiB 会静默回绕，写出的头长与
/// 真实数据不符，播放器按头截断，尾部样本被整段丢弃。
fn wav_header_mono16(sample_count: usize, rate: u32) -> Result<Vec<u8>> {
    // 采样率 0 写出的是非法 WAV：头里 byte_rate / 采样率都是 0，解码端要么直接
    // 拒绝、要么按 0 做除法。这里的调用方都传真实采样率，所以直接报错而不是静默
    // 落一个坏文件。
    if rate == 0 {
        anyhow::bail!("WAV 采样率必须大于 0（收到 0），否则写出的是无法解析的非法 WAV");
    }
    let data_len = u32::try_from(sample_count.checked_mul(2).context("WAV 数据长度溢出")?)
        .context("WAV 数据超过 4 GiB，超出标准 WAV 头可表示范围")?;
    let riff_len = 36u32
        .checked_add(data_len)
        .context("WAV RIFF 块大小溢出（36 + data 长度超出 u32）")?;
    let byte_rate = rate
        .checked_mul(2)
        .context("WAV 字节率溢出（rate * 2 超出 u32）")?;

    let mut buf = Vec::with_capacity(44);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&riff_len.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    buf.extend_from_slice(&rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes()); // 字节率
    buf.extend_from_slice(&2u16.to_le_bytes()); // 块对齐
    buf.extend_from_slice(&16u16.to_le_bytes()); // 位深
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_len.to_le_bytes());
    Ok(buf)
}

/// 写出 16 位单声道 PCM WAV（44 字节标准头）。
pub fn write_wav_mono16<P: AsRef<Path>>(path: P, samples: &[i16], rate: u32) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 WAV 输出目录失败: {parent:?}"))?;
    }
    let header = wav_header_mono16(samples.len(), rate)?;
    let mut buf = Vec::with_capacity(header.len() + samples.len() * 2);
    buf.extend_from_slice(&header);
    for s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, &buf).with_context(|| format!("写出压实 WAV 失败: {path:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ms_to_frames, plan_compaction, wav_header_mono16, write_wav_mono16, CompactionConfig,
        CompactionPlan, SpeechFilterOptions,
    };

    /// 生成 rate 采样率、指定秒数的语音样信号（正弦，幅度 amplitude）
    fn tone(seconds: f64, rate: u32, amplitude: i16) -> Vec<i16> {
        let n = (seconds * rate as f64) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / rate as f64;
                (amplitude as f64 * (2.0 * std::f64::consts::PI * 220.0 * t).sin()) as i16
            })
            .collect()
    }

    fn silence(seconds: f64, rate: u32) -> Vec<i16> {
        vec![0i16; (seconds * rate as f64) as usize]
    }

    #[test]
    fn filter_chain_orders_denoise_before_normalize_and_resample_last() {
        let opts = SpeechFilterOptions::default();
        let chain = opts.chain().unwrap();
        let pos = |needle: &str| {
            chain
                .find(needle)
                .unwrap_or_else(|| panic!("缺少 {needle}: {chain}"))
        };
        assert!(pos("highpass") < pos("afftdn"), "高通应在降噪前: {chain}");
        assert!(
            pos("afftdn") < pos("dynaudnorm"),
            "降噪必须在归一之前: {chain}"
        );
        assert!(
            pos("dynaudnorm") < pos("aresample"),
            "重采样必须是最后一级: {chain}"
        );
        assert!(chain.contains("resampler=soxr"), "应使用 soxr: {chain}");
        assert!(
            !chain.contains("atempo"),
            "1.0 倍速不应插入 atempo: {chain}"
        );
    }

    #[test]
    fn filter_chain_inserts_atempo_before_resample() {
        let chain = SpeechFilterOptions::default()
            .chain_with_speed(1.35)
            .unwrap();
        let pos = |needle: &str| {
            chain
                .find(needle)
                .unwrap_or_else(|| panic!("缺少 {needle}: {chain}"))
        };
        assert!(
            pos("dynaudnorm") < pos("atempo"),
            "变速应在归一之后: {chain}"
        );
        assert!(
            pos("atempo") < pos("aresample"),
            "变速应在重采样之前: {chain}"
        );
        assert!(chain.contains("atempo=1.350"));
    }

    #[test]
    fn filter_chain_can_be_fully_disabled() {
        let opts = SpeechFilterOptions {
            highpass_hz: 0.0,
            denoise: false,
            normalize: false,
            target_rate: 0,
        };
        assert!(opts.is_noop());
        assert!(opts.chain().is_none());
    }

    #[test]
    fn compaction_removes_long_pauses_and_keeps_speech() {
        let rate = 16_000u32;
        // 语音 2s + 静音 5s + 语音 2s：应只保留约 4s + 过渡
        let mut pcm = tone(2.0, rate, 8000);
        pcm.extend(silence(5.0, rate));
        pcm.extend(tone(2.0, rate, 8000));
        let total = pcm.len();

        let plan = plan_compaction(&pcm, rate, &CompactionConfig::default());
        assert_eq!(plan.total_samples, total);
        assert!(
            plan.is_worthwhile(0.20),
            "5s 静音应被判定为值得压实: ratio={}",
            plan.ratio()
        );
        assert!(plan.ratio() < 0.55, "保留比例应显著下降: {}", plan.ratio());
        assert_eq!(
            plan.spans.len(),
            2,
            "两段语音各留一个区间: {:?}",
            plan.spans
        );
        // 第一段保留区间的起点不应早于 0，末尾不应吞掉第二段
        assert_eq!(plan.spans[0].0, 0);
        assert!(plan.spans[1].1 == total);
    }

    #[test]
    fn compaction_is_identity_on_dense_speech() {
        let rate = 16_000u32;
        let pcm = tone(6.0, rate, 8000); // 全程连续语音，无停顿
        let plan = plan_compaction(&pcm, rate, &CompactionConfig::default());
        assert_eq!(plan.spans, vec![(0, pcm.len())]);
        assert!((plan.ratio() - 1.0).abs() < 1e-9);
        assert!(!plan.is_worthwhile(0.05));
    }

    #[test]
    fn compaction_ignores_pure_silence() {
        let rate = 16_000u32;
        let pcm = silence(4.0, rate);
        let plan = plan_compaction(&pcm, rate, &CompactionConfig::default());
        // 纯静音没有可保留的语音，退回原样交给 VAD，不做压实
        assert_eq!(plan.spans, vec![(0, pcm.len())]);
    }

    #[test]
    fn remap_restores_original_timeline() {
        let rate = 16_000u32;
        let mut pcm = tone(2.0, rate, 8000);
        pcm.extend(silence(5.0, rate));
        pcm.extend(tone(2.0, rate, 8000));
        let plan = plan_compaction(&pcm, rate, &CompactionConfig::default());

        // 压实音频里的 0s 对应原音频 0s
        assert!((plan.to_original(0.0) - 0.0).abs() < 1e-6);
        // 压实音频里的 1.5s 仍在第一段语音内 → 原时间轴同样约 1.5s
        assert!(
            (plan.to_original(1.5) - 1.5).abs() < 1e-6,
            "{}",
            plan.to_original(1.5)
        );
        // 第二段语音的起点在压实时间轴上约 2.1s（2s 语音 + 120ms 过渡），
        // 但映射回原时间轴必须是 7s 左右
        let second = plan.map.last().unwrap();
        assert!(
            (second.orig_start - 7.0).abs() < 0.3,
            "第二段应映射回 7s 附近: {}",
            second.orig_start
        );
        let remapped = plan.to_original(second.compact_start);
        assert!(
            (remapped - 7.0).abs() < 0.3,
            "压实 2.1s → 原始 7s，实得 {remapped}"
        );

        // 映射必须单调不减
        let mut prev = -1.0;
        let mut t = 0.0;
        while t < plan.kept_samples as f64 / rate as f64 {
            let o = plan.to_original(t);
            assert!(o >= prev, "映射必须单调: t={t} o={o} prev={prev}");
            prev = o;
            t += 0.05;
        }
    }

    #[test]
    fn materialize_concatenates_kept_spans() {
        let rate = 16_000u32;
        let mut pcm = tone(1.0, rate, 8000);
        pcm.extend(silence(3.0, rate));
        pcm.extend(tone(1.0, rate, 8000));
        let plan = plan_compaction(&pcm, rate, &CompactionConfig::default());
        let out = plan.materialize(&pcm);
        assert_eq!(out.len(), plan.kept_samples);
        assert!(out.len() < pcm.len());
        // 压实后音频总时长应与映射表末尾对齐
        let last = plan.map.last().unwrap();
        let expected = (last.compact_start + last.duration) * rate as f64;
        assert!((out.len() as f64 - expected).abs() <= 1.0);
    }

    #[test]
    fn identity_plan_remaps_without_touching_timeline() {
        let rate = 16_000u32;
        let plan = CompactionPlan::identity(4 * rate as usize, rate);
        assert!(plan.is_identity());
        // 恒等映射：任意时刻原样返回，含右边界与边界外
        for t in [0.0, 1.5, 4.0, 9.0] {
            assert!((plan.to_original(t) - t).abs() < 1e-9, "t={t}");
        }
        // 空映射（total_samples == 0）也必须恒等，而不是回落到 0
        let empty = CompactionPlan::identity(0, rate);
        assert!(empty.map.is_empty());
        assert!(empty.spans.is_empty());
        assert_eq!(empty.to_original(3.0), 3.0);
        assert_eq!(empty.to_original(-1.0), -1.0);
        assert_eq!(empty.ratio(), 1.0);
    }

    #[test]
    fn to_original_span_never_inverts_and_is_monotonic_at_boundaries() {
        let rate = 16_000u32;
        let mut pcm = tone(2.0, rate, 8000);
        pcm.extend(silence(5.0, rate));
        pcm.extend(tone(2.0, rate, 8000));
        let plan = plan_compaction(&pcm, rate, &CompactionConfig::default());

        // 末尾与越过末尾都必须单调外推，且区间不反转
        let kept_sec = plan.kept_samples as f64 / rate as f64;
        let (s, e) = plan.to_original_span(kept_sec, kept_sec + 1.0);
        assert!(e >= s, "区间不可反转: {s}..{e}");
        // 起点在前、终点在后时同样不反转
        let (s2, e2) = plan.to_original_span(1.0, 0.0);
        assert!(e2 >= s2);
        // 映射结果永不早于 0
        assert!(plan.to_original(-5.0) >= 0.0);
    }

    #[test]
    fn wav_header_fields_match_pcm16_mono() {
        let dir = std::env::temp_dir().join("v2w_audio_prep_hdr_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hdr.wav");
        // 奇数个样本：data 段长度为偶数（2 字节/样本），且头里不能出现补齐字节
        let samples = [7i16, -7, 1234];
        write_wav_mono16(&path, &samples, 16_000).unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(raw.len(), 44 + samples.len() * 2, "16 位单声道不应有补齐");
        assert_eq!(&raw[0..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
            36 + (samples.len() * 2) as u32,
            "RIFF 块大小应为 36 + data 长度"
        );
        assert_eq!(&raw[8..12], b"WAVE");
        assert_eq!(&raw[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes([raw[16], raw[17], raw[18], raw[19]]), 16);
        assert_eq!(u16::from_le_bytes([raw[20], raw[21]]), 1, "格式应为 PCM");
        assert_eq!(u16::from_le_bytes([raw[22], raw[23]]), 1, "通道数应为 1");
        assert_eq!(
            u32::from_le_bytes([raw[24], raw[25], raw[26], raw[27]]),
            16_000
        );
        assert_eq!(
            u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]),
            16_000 * 2,
            "字节率应为 rate * 块对齐"
        );
        assert_eq!(u16::from_le_bytes([raw[32], raw[33]]), 2, "块对齐应为 2");
        assert_eq!(u16::from_le_bytes([raw[34], raw[35]]), 16, "位深应为 16");
        assert_eq!(&raw[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes([raw[40], raw[41], raw[42], raw[43]]),
            (samples.len() * 2) as u32
        );
        // 小端样本逐个对齐
        for (i, s) in samples.iter().enumerate() {
            let off = 44 + i * 2;
            assert_eq!(
                i16::from_le_bytes([raw[off], raw[off + 1]]),
                *s,
                "第 {i} 个样本"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wav_round_trip_header() {
        let dir = std::env::temp_dir().join("v2w_audio_prep_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe.wav");
        let samples = tone(0.25, 16_000, 4000);
        write_wav_mono16(&path, &samples, 16_000).unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[0..4], b"RIFF");
        assert_eq!(&raw[8..12], b"WAVE");
        assert_eq!(raw.len(), 44 + samples.len() * 2);
        assert_eq!(
            u32::from_le_bytes([raw[24], raw[25], raw[26], raw[27]]),
            16_000
        );
        assert_eq!(u16::from_le_bytes([raw[22], raw[23]]), 1);
        let _ = std::fs::remove_file(&path);
    }

    /// 150 ms 退出保持按帧长向上取整：20 ms 帧长必须得 8 帧（160 ms 覆盖），
    /// 而不是截断得到的 7 帧（140 ms，少 10 ms 静音尾巴，尾音更容易被削）。
    #[test]
    fn exit_hold_rounds_up_to_full_150ms() {
        assert_eq!(ms_to_frames(150, 20), 8, "150/20 必须向上取整到 8 帧");
        assert_eq!(
            ms_to_frames(150, 16),
            10,
            "150/16=9.375 必须向上取整到 10 帧"
        );
        assert_eq!(ms_to_frames(200, 20), 10, "整除时不得多留一帧");
        assert_eq!(ms_to_frames(300, 20), 15, "整除时不得多留一帧");
        assert_eq!(ms_to_frames(30, 0), 30, "帧长 0 按 1 ms 兜底，避免除零");
    }

    /// 超 4 GiB 的样本数必须报错而不是把长度静默回绕；输出目录建不出来时必须带
    /// 上下文冒泡，而不是 `.ok()` 吞掉后在别处报一个看不懂的写盘错误。
    #[test]
    fn write_wav_rejects_oversize_and_reports_dir_creation_failure() {
        let oversize = (u32::MAX as usize / 2) + 1;
        let err = wav_header_mono16(oversize, 16_000).expect_err("超 4 GiB 必须报错");
        let msg = format!("{err:#}");
        assert!(msg.contains("4 GiB"), "错误必须点明 4 GiB 上限: {msg}");

        let dir = std::env::temp_dir().join("v2w_audio_prep_wav_dir");
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "占位").unwrap();
        let bad_path = blocker.join("sub").join("out.wav");
        let err =
            write_wav_mono16(&bad_path, &[0i16; 4], 16_000).expect_err("父路径被文件占住必须报错");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("创建 WAV 输出目录失败"),
            "错误必须点明目录创建失败: {msg}"
        );
        let _ = std::fs::remove_file(&blocker);
    }
}

/// 参数净化与退化输入的下限测试：全部是纯函数，不碰子进程、不联网。
#[cfg(test)]
mod sanitize_tests {
    use super::{
        frame_db, plan_compaction, wav_header_mono16, CompactionConfig, SpeechFilterOptions,
        MAX_HIGHPASS_HZ, MAX_SPEED, MAX_TARGET_RATE, MIN_SPEED, MIN_TARGET_RATE,
    };

    /// 一个除了被测字段外全部关闭的选项集，避免其它级干扰断言。
    fn bare() -> SpeechFilterOptions {
        SpeechFilterOptions {
            highpass_hz: 0.0,
            denoise: false,
            normalize: false,
            target_rate: 0,
        }
    }

    fn tone(seconds: f64, rate: u32, amplitude: i16) -> Vec<i16> {
        let n = (seconds * rate as f64) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / rate as f64;
                (amplitude as f64 * (2.0 * std::f64::consts::PI * 220.0 * t).sin()) as i16
            })
            .collect()
    }

    fn silence(seconds: f64, rate: u32) -> Vec<i16> {
        vec![0i16; (seconds * rate as f64) as usize]
    }

    /// config 里的 highpass 是裸 f64：`inf` / `NaN` / 负值必须按「关闭」处理，
    /// 极大值必须限幅。`inf` 拼进 `-af` 会被 ffmpeg 判 out of range（实测整条链
    /// 失败），而 (Nyquist, 999999] 的极大值不报错、只把语音静默削没。
    #[test]
    fn filter_chain_rejects_nonfinite_highpass_and_clamps_extremes() {
        assert!(bare().chain().is_none());

        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -12.0, -0.0, 0.0] {
            let opts = SpeechFilterOptions {
                highpass_hz: bad,
                ..bare()
            };
            assert!(opts.chain().is_none(), "highpass={bad} 不得产生任何滤镜串");
        }

        for huge in [1e300, 999_999.0, MAX_HIGHPASS_HZ + 1.0] {
            let chain = SpeechFilterOptions {
                highpass_hz: huge,
                ..bare()
            }
            .chain()
            .expect("限幅后必须仍有高通级");
            assert_eq!(
                chain,
                format!("highpass=f={MAX_HIGHPASS_HZ:.0}:poles=2"),
                "highpass={huge} 必须限幅到 {MAX_HIGHPASS_HZ}"
            );
        }

        // 上限以内的合法值原样保留（用户可感知的档位不得被改）
        let kept = SpeechFilterOptions {
            highpass_hz: 120.0,
            ..bare()
        }
        .chain()
        .unwrap();
        assert_eq!(kept, "highpass=f=120:poles=2");

        // 取整：不足 1 Hz 的档位丢弃，刚好 0.5 Hz 取整成 1 Hz 而不是 `f=0`
        assert!(
            SpeechFilterOptions {
                highpass_hz: 0.4,
                ..bare()
            }
            .chain()
            .is_none(),
            "round 到 0 Hz 的高通必须整级丢弃"
        );
        assert_eq!(
            SpeechFilterOptions {
                highpass_hz: 0.5,
                ..bare()
            }
            .chain()
            .unwrap(),
            "highpass=f=1:poles=2"
        );
        assert_eq!(
            SpeechFilterOptions {
                highpass_hz: 119.6,
                ..bare()
            }
            .chain()
            .unwrap(),
            "highpass=f=120:poles=2"
        );
    }

    /// 非法采样率不能让 ffmpeg 静默把音频重采样成静音；0 仍然是「不重采样」。
    #[test]
    fn filter_chain_clamps_invalid_target_rate() {
        assert!(
            SpeechFilterOptions {
                target_rate: 0,
                ..bare()
            }
            .chain()
            .is_none(),
            "target_rate=0 表示不重采样"
        );

        for (given, want) in [
            (16_000u32, 16_000u32),
            (1, MIN_TARGET_RATE),
            (4_000, MIN_TARGET_RATE),
            (8_000, MIN_TARGET_RATE),
            (48_000, 48_000),
            (MAX_TARGET_RATE, MAX_TARGET_RATE),
            (800_000, MAX_TARGET_RATE),
            (u32::MAX, MAX_TARGET_RATE),
        ] {
            let chain = SpeechFilterOptions {
                target_rate: given,
                ..bare()
            }
            .chain()
            .expect("非 0 采样率必须产出 aresample 级");
            assert_eq!(
                chain,
                format!("aresample={want}:resampler=soxr:precision=28:cutoff=0.97"),
                "target_rate={given} 应限幅为 {want}"
            );
        }
    }

    /// `speed` 非有限值不得变成 `atempo=inf`，超大值必须限幅到 ffmpeg 的合法区间。
    #[test]
    fn filter_chain_sanitizes_speed() {
        let opts = SpeechFilterOptions::default();

        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let chain = opts.chain_with_speed(bad).expect("增强级仍在，链不为空");
            assert!(
                !chain.contains("atempo"),
                "speed={bad} 不得产生 atempo: {chain}"
            );
        }

        for slow in [0.25, 0.999, 1.0] {
            let chain = opts.chain_with_speed(slow).unwrap();
            assert!(!chain.contains("atempo"), "speed={slow} 不应变速: {chain}");
        }

        assert!(
            opts.chain_with_speed(1.35)
                .unwrap()
                .contains("atempo=1.350"),
            "区间内的合法变速必须保留"
        );
        assert!(
            opts.chain_with_speed(1e9)
                .unwrap()
                .contains(&format!("atempo={MAX_SPEED:.3}")),
            "超大 speed 必须限幅到 {MAX_SPEED}"
        );
    }

    /// 滤镜串唯一的安全前提：所有用户可控量都是数值，拼出来只能由数字和滤镜语法
    /// 字符组成，不可能出现能拆开 `-af` 的引号/分号/反斜杠/方括号，也不可能出现
    /// `inf` / `NaN` 这类会被参数校验拒掉的记号。
    #[test]
    fn filter_chain_contains_only_filter_graph_syntax() {
        const KNOWN: [&str; 5] = ["highpass", "afftdn", "dynaudnorm", "atempo", "aresample"];

        for hp in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -3.0,
            0.5,
            70.0,
            1e30,
            MAX_HIGHPASS_HZ,
        ] {
            let opts = SpeechFilterOptions {
                highpass_hz: hp,
                denoise: true,
                normalize: true,
                target_rate: 16_000,
            };
            let chain = opts.chain_with_speed(1e30).expect("默认开关下链必然存在");

            for ch in chain.chars() {
                assert!(
                    ch.is_ascii_alphanumeric() || matches!(ch, '=' | ':' | ',' | '.' | '_' | '-'),
                    "滤镜串出现可疑字符 {ch:?}: {chain}"
                );
            }
            assert!(!chain.to_ascii_lowercase().contains("inf"), "{chain}");
            assert!(!chain.contains("NaN"), "{chain}");

            for stage in chain.split(',') {
                let (name, params) = stage
                    .split_once('=')
                    .unwrap_or_else(|| panic!("每级滤镜必须是 name=params: {chain}"));
                assert!(
                    KNOWN.contains(&name),
                    "出现未知滤镜名（可能是注入）: {name} in {chain}"
                );
                assert!(!params.is_empty(), "滤镜参数不得为空: {chain}");
            }

            if let Some(stage) = chain.split(',').find(|s| s.starts_with("highpass=")) {
                let f: u32 = stage
                    .trim_start_matches("highpass=f=")
                    .split(':')
                    .next()
                    .unwrap()
                    .parse()
                    .expect("高通截止必须是整数 Hz");
                assert!(
                    f >= 1 && f <= MAX_HIGHPASS_HZ as u32,
                    "highpass f={f} 越界: {chain}"
                );
            }
        }
    }

    /// 0 采样率下不得算出 `inf` 时长：直接退回恒等计划，不做任何压实判断。
    #[test]
    fn plan_compaction_zero_rate_returns_identity() {
        // 1200 个固定电平样本接 1200 个 0：若不拦 0 采样率，这段会被判成
        //「有语音 + 有静音」，然后在映射表里做 `(e - s) as f64 / 0.0`，得到 inf。
        let mut pcm = vec![3_000i16; 1_200];
        pcm.extend(vec![0i16; 1_200]);
        let plan = plan_compaction(&pcm, 0, &CompactionConfig::default());

        assert!(plan.is_identity(), "0 采样率必须恒等: {:?}", plan.spans);
        assert_eq!(plan.spans, vec![(0, pcm.len())]);
        assert_eq!(plan.kept_samples, pcm.len());
        assert_eq!(plan.ratio(), 1.0);
        for piece in &plan.map {
            assert!(
                piece.duration.is_finite() && piece.compact_start.is_finite(),
                "0 采样率不得产出非有限时长: {piece:?}"
            );
        }
    }

    /// `chunks(0)` 会 panic；帧长 0 必须按 1 样本兜底。
    #[test]
    fn frame_db_zero_frame_len_does_not_panic() {
        let dbs = frame_db(&[0i16, 3_000, -3_000, 8_000], 0);
        assert_eq!(dbs.len(), 4, "帧长 0 按 1 样本兜底");
        assert_eq!(dbs[0], -100.0, "全 0 帧按 -100 dBFS");
        assert!(dbs.iter().all(|v| v.is_finite() && *v >= -100.0));
    }

    /// 采样率 0 写出的是非法 WAV：必须报错，而不是静默落一个 byte_rate=0 的文件。
    #[test]
    fn wav_header_rejects_zero_sample_rate() {
        let err = wav_header_mono16(8, 0).expect_err("0 采样率必须报错");
        let msg = format!("{err:#}");
        assert!(msg.contains("采样率"), "错误必须点明采样率: {msg}");

        // 非 0 采样率不受影响，空音频也要能写出合法头
        assert!(wav_header_mono16(0, 16_000).is_ok());
        assert!(wav_header_mono16(1, 8_000).is_ok());
    }

    /// 空音频 / 极短音频必须恒等，且 materialize 与 to_original 都不出格。
    #[test]
    fn plan_compaction_handles_empty_and_tiny_audio() {
        let cfg = CompactionConfig::default();

        let empty = plan_compaction(&[], 16_000, &cfg);
        assert!(empty.spans.is_empty() && empty.map.is_empty());
        assert_eq!(empty.kept_samples, 0);
        assert!(empty.is_identity());
        assert_eq!(empty.to_original(3.0), 3.0, "空映射必须恒等透传");
        assert_eq!(empty.to_original(-1.0), -1.0);
        assert!(empty.materialize(&[]).is_empty());

        let tiny = plan_compaction(&[0i16; 3], 16_000, &cfg);
        assert_eq!(tiny.spans, vec![(0, 3)], "极短音频不得被压实");
        assert_eq!(tiny.materialize(&[0i16; 3]).len(), 3);
    }

    /// 退化配置（frame_ms=0 / pad 极大 / min_* 为 0 / 分位数越界或 NaN）下，
    /// 计划本身必须自洽：区间有序不重叠不越界、kept_samples 与区间总长一致、
    /// 映射表与区间一一对应、时间全部有限且单调、materialize 长度对得上。
    #[test]
    fn plan_compaction_degenerate_configs_stay_consistent() {
        let rate = 16_000u32;
        let mut pcm = tone(1.0, rate, 8_000);
        pcm.extend(silence(3.0, rate));
        pcm.extend(tone(1.0, rate, 8_000));

        let mut cfgs: Vec<CompactionConfig> = Vec::new();
        for frame_ms in [0u32, 1, 20, u32::MAX] {
            for noise_percentile in [f64::NAN, -1.0, 0.0, 0.20, 1.0, 2.0] {
                for pad_ms in [0u32, 180, u32::MAX] {
                    cfgs.push(CompactionConfig {
                        frame_ms,
                        noise_percentile,
                        pad_ms,
                        min_speech_ms: 0,
                        min_gap_ms: 0,
                        keep_gap_ms: u32::MAX,
                        ..CompactionConfig::default()
                    });
                }
            }
        }

        for cfg in cfgs {
            let plan = plan_compaction(&pcm, rate, &cfg);
            assert_eq!(plan.total_samples, pcm.len());

            let mut prev_end = 0usize;
            let mut sum = 0usize;
            for &(s, e) in &plan.spans {
                assert!(s < e, "空/倒挂区间: {s}..{e} ({cfg:?})");
                assert!(s >= prev_end, "区间重叠或乱序: {:?} ({cfg:?})", plan.spans);
                assert!(e <= plan.total_samples, "区间越界: {:?}", plan.spans);
                prev_end = e;
                sum += e - s;
            }
            assert_eq!(sum, plan.kept_samples, "kept_samples 必须等于区间总长");
            assert_eq!(plan.map.len(), plan.spans.len(), "映射表必须与区间一一对应");

            for piece in &plan.map {
                assert!(
                    piece.duration.is_finite()
                        && piece.compact_start.is_finite()
                        && piece.orig_start.is_finite(),
                    "映射表出现非有限值: {piece:?} ({cfg:?})"
                );
                assert!(piece.duration > 0.0);
                assert!(piece.orig_start >= 0.0);
            }
            if let Some(first) = plan.map.first() {
                assert_eq!(first.compact_start, 0.0);
                let last = plan.map.last().unwrap();
                let end = (last.compact_start + last.duration) * rate as f64;
                assert!(
                    (end - plan.kept_samples as f64).abs() < 1e-6,
                    "映射表末尾必须等于 kept_samples: {end} vs {}",
                    plan.kept_samples
                );
            }

            let kept_sec = plan.kept_samples as f64 / rate as f64;
            let mut prev = -1.0f64;
            let mut t = 0.0f64;
            while t <= kept_sec {
                let o = plan.to_original(t);
                assert!(o.is_finite(), "t={t} 映射出 {o}");
                assert!(o >= prev - 1e-9, "映射不单调: t={t} o={o} prev={prev}");
                prev = o;
                t += 0.05;
            }
            assert!(plan.to_original(f64::NAN).is_finite());
            assert!(plan.to_original(-1.0) >= 0.0);

            assert_eq!(
                plan.materialize(&pcm).len(),
                plan.kept_samples,
                "materialize 长度必须等于 kept_samples"
            );
        }
    }

    /// kept_samples 与实际音频对不上时，materialize 只能截断，不能 panic 或越界。
    #[test]
    fn materialize_tolerates_short_input() {
        let rate = 16_000u32;
        let mut pcm = tone(1.0, rate, 8_000);
        pcm.extend(silence(3.0, rate));
        pcm.extend(tone(1.0, rate, 8_000));

        let plan = plan_compaction(&pcm, rate, &CompactionConfig::default());
        assert!(plan.kept_samples < pcm.len(), "该素材应被压实");

        let short = &pcm[..pcm.len() / 4];
        let out = plan.materialize(short);
        assert!(out.len() <= short.len());
        assert!(out.len() <= plan.kept_samples);
        assert!(plan.materialize(&[]).is_empty());
    }

    /// ② `is_noop()` 与 `chain()` 必须同源：同一份配置两者只能给出同一个结论。
    /// 覆盖默认配置、只改 `target_rate`、只改 `highpass`（含 `NaN` / 负值 / 取整到 0）。
    #[test]
    fn is_noop_agrees_with_chain_on_every_stage_combination() {
        for highpass_hz in [0.0, -1.0, f64::NAN, f64::INFINITY, 0.4, 70.0] {
            for denoise in [false, true] {
                for normalize in [false, true] {
                    for target_rate in [0u32, 16_000, u32::MAX] {
                        let opts = SpeechFilterOptions {
                            highpass_hz,
                            denoise,
                            normalize,
                            target_rate,
                        };
                        assert_eq!(
                            opts.is_noop(),
                            opts.chain().is_none(),
                            "is_noop 与 chain 结论必须一致: {opts:?}"
                        );
                    }
                }
            }
        }
    }

    /// ②④ 语义细化：重采样本身就是一级真实处理（只改 `target_rate` 不算 noop）；
    /// `NaN` / `inf` / 负值 / 取整到 0 的 highpass 一律视为「关闭该级」。
    #[test]
    fn is_noop_treats_target_rate_as_stage_and_bad_highpass_as_off() {
        // 只改 target_rate：真会产出 aresample，所以不能算 noop（旧实现漏了这件事）
        let resample_only = SpeechFilterOptions {
            target_rate: 16_000,
            ..bare()
        };
        assert!(!resample_only.is_noop());
        assert!(resample_only
            .chain()
            .expect("重采样级必然存在")
            .contains("aresample=16000"));

        // 只改 highpass 且该值是「关闭」语义：两个方法都必须说没有活干
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 0.4] {
            let hp_only = SpeechFilterOptions {
                highpass_hz: bad,
                ..bare()
            };
            assert!(hp_only.is_noop(), "highpass={bad} 应视为关闭: {hp_only:?}");
            assert!(hp_only.chain().is_none(), "highpass={bad} 不得产出滤镜串");
        }

        // 默认配置：两个方法都必须说「有活干」
        let defaults = SpeechFilterOptions::default();
        assert!(!defaults.is_noop());
        assert!(defaults.chain().is_some());
    }

    /// ③ 速度契约：注释里的区间必须与 clamp 一致——下界是 `MIN_SPEED = 1.0`（不允许变慢）。
    /// 上游 `core/pipeline.rs` 已把 `audio_speed` 夹到 `[1.0, 1.5]`，变慢只会把音频拉长、
    /// 让单 token 解码的调用次数变多，与「减少喂给模型的音频量」相悖。
    #[test]
    fn filter_chain_speed_floor_is_one_and_matches_docs() {
        assert_eq!(MIN_SPEED, 1.0, "对外契约下界");
        let opts = SpeechFilterOptions::default();

        for slow in [0.0, 0.5, 0.999, 1.0, -3.0] {
            let chain = opts.chain_with_speed(slow).expect("增强级仍在，链不为空");
            assert!(
                !chain.contains("atempo"),
                "speed={slow} 不得插入 atempo: {chain}"
            );
        }
        assert!(
            opts.chain_with_speed(MIN_SPEED + 0.001)
                .unwrap()
                .contains("atempo=1.001"),
            "刚过下界就必须变速"
        );
        assert!(
            opts.chain_with_speed(1e9)
                .unwrap()
                .contains(&format!("atempo={MAX_SPEED:.3}")),
            "上限仍是 MAX_SPEED"
        );
    }
}
