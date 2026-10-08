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

// ───────────────────────── 语音增强滤镜链 ─────────────────────────

/// 语音增强开关集合。默认值按「保守但有效」选取：
/// 只做有明确物理依据的处理，不引入会改变音色的强处理。
#[derive(Debug, Clone, Copy)]
pub struct SpeechFilterOptions {
    /// 高通截止频率（Hz）。0 = 关闭。
    /// Whisper 的 mel 从 80 Hz 起，低于此的频率只贡献能量不贡献信息；
    /// 去直流与低频轰鸣还能避免 VAD 被低频噪声触发。
    pub highpass_hz: f64,
    /// FFT 谱减降噪（afftdn）。稳态底噪（空调、风扇、电流声）会让
    /// Whisper 在静音处「听出」词语。
    pub denoise: bool,
    /// 语音动态电平归一（dynaudnorm）。远场/手机录音电平常低到
    /// 让 `no_speech_thold` 误判，归一后 VAD 阈值也才可靠。
    pub normalize: bool,
    /// 输出采样率
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

impl SpeechFilterOptions {
    pub fn is_noop(&self) -> bool {
        self.highpass_hz <= 0.0 && !self.denoise && !self.normalize
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
        let mut stages: Vec<String> = Vec::new();

        if self.highpass_hz > 0.0 {
            // poles=2 是 12 dB/oct，足够削掉直流与轰鸣又不动 80 Hz 以上的语音基频
            stages.push(format!("highpass=f={:.0}:poles=2", self.highpass_hz));
        }
        if self.denoise {
            // nr=12：只衰减 12 dB，宁可少降也不引入伪影
            // nf=-30：初始噪声底 -30 dBFS（典型室内底噪量级）
            // tn=1：自适应跟踪噪声底，避免把持续的低电平语音当成噪声削掉
            // gs=8：增益平滑，抑制音乐化伪影
            stages.push("afftdn=nr=12:nf=-30:tn=1:gs=8".to_string());
        }
        if self.normalize {
            // f=250：250 ms 分析窗，匹配语音音节尺度
            // p=0.90：目标峰值，留 1 dB 余量避免削顶
            // m=4.0：最大放大 4 倍（12 dB）。这个上限很关键——不设限的话
            //        归一器会把句间底噪放大到「像语音」，反而制造幻觉
            stages.push("dynaudnorm=f=250:g=15:p=0.90:m=4.0".to_string());
        }
        if speed > 1.0 {
            // 1.0~1.5 区间内 atempo 不需要级联（>2.0 才需要）
            stages.push(format!("atempo={speed:.3}"));
        }
        if self.target_rate != 0 {
            // soxr 在 16 kHz 目标下的阻带抑制明显优于默认 swr，
            // mel 谱更干净；precision=28 是 32 位浮点下的实用上限
            stages.push(format!(
                "aresample={}:resampler=soxr:precision=28:cutoff=0.97",
                self.target_rate
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

    let frame = ((rate as u64 * cfg.frame_ms.max(1) as u64) / 1000).max(1) as usize;
    if total < frame * 4 {
        return identity(total, rate);
    }

    let dbs = frame_db(pcm, frame);
    let noise_floor = estimate_noise_floor(&dbs, cfg.noise_percentile);
    let enter_db = (noise_floor + cfg.speech_margin_db).max(cfg.abs_floor_db);
    let exit_db = enter_db - 3.0;

    let exit_hold = ((150 / cfg.frame_ms.max(1)) as usize).max(1);
    let min_speech = ((cfg.min_speech_ms / cfg.frame_ms.max(1)) as usize).max(1);
    let min_gap = (cfg.min_gap_ms / cfg.frame_ms.max(1)) as usize;
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

/// 写出 16 位单声道 PCM WAV（44 字节标准头）。
pub fn write_wav_mono16<P: AsRef<Path>>(path: P, samples: &[i16], rate: u32) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let data_len = (samples.len() * 2) as u32;
    let mut buf = Vec::with_capacity(44 + data_len as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_len).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    buf.extend_from_slice(&rate.to_le_bytes());
    buf.extend_from_slice(&(rate * 2).to_le_bytes()); // 字节率
    buf.extend_from_slice(&2u16.to_le_bytes()); // 块对齐
    buf.extend_from_slice(&16u16.to_le_bytes()); // 位深
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, &buf).with_context(|| format!("写出压实 WAV 失败: {path:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
