//! 轻量说话人分离 (F-015)
//!
//! 不引入额外的声纹模型，只用音频本身的低阶声学特征做聚类：
//! - **基频 (pitch)**：自相关法估出的浊音基频，是区分说话人最有效的单维特征；
//! - **能量 (RMS)**：区分「谁在说」与「谁离麦远」，也用来识别纯静音段；
//! - **过零率 (ZCR)**：清辅音/摩擦音比例，反映发声方式差异；
//! - **谱质心代理**：一阶差分的平均绝对值 / 平均绝对值，反映高频占比。
//!
//! 每段字幕只均匀采样固定帧数（`FRAMES_PER_SEGMENT`），因此总计算量与
//! 字幕条数成正比而非与音频时长成正比——1 小时片和 10 分钟片在这里的成本
//! 是同一个量级。
//!
//! 聚类用确定性初始化的 k-means（按基频分位数布点 + 固定迭代上限），
//! 同一份输入永远得到同一份结果，便于用户复现与对比。

use crate::subtitle::Segment;

/// 说话人数量上限（UI 滑条量程与这里保持一致）
pub const MAX_SPEAKERS: u32 = 4;
/// 每段字幕参与统计的采样帧数
const FRAMES_PER_SEGMENT: usize = 12;
/// 分析帧长（样本数，8kHz 下 50ms）
const FRAME_LEN: usize = 400;
/// 帧间跳距（样本数，8kHz 下 25ms）
const FRAME_HOP: usize = 200;
/// 自相关搜索的最小/最大延迟：8kHz 下 20~160 对应 50Hz~400Hz，覆盖人声基频范围
const MIN_LAG: usize = 20;
const MAX_LAG: usize = 160;
/// 自相关归一化峰值低于该值视为清音（无稳定基频）
const VOICED_THRESHOLD: f32 = 0.30;
/// 能量低于全体中位数的该比例时视为静音段，不参与聚类也不打标签
const SILENCE_RATIO: f32 = 0.15;
/// 浊音帧占比低于该值时视为无有效人声
const MIN_VOICED_RATIO: f32 = 0.25;
/// k-means 迭代上限
const KMEANS_ITERS: usize = 40;

/// 单帧声学特征
#[derive(Debug, Clone, Copy, Default)]
struct FrameFeatures {
    rms: f32,
    zcr: f32,
    /// 基频（Hz），清音帧为 0
    pitch: f32,
    /// 谱质心代理（0~1，越大高频越多）
    centroid: f32,
    /// 该帧是否为浊音
    voiced: bool,
}

/// 单段字幕聚合后的特征向量（已按维度加权，见 [`FEATURE_WEIGHTS`]）
type FeatureVec = [f32; 4];

/// 特征向量各维的下标。集中定义是为了避免「注释说按基频、代码却按下标 0
/// （能量）」这类静默错位——曾经因此让聚类退化成单一说话人。
const RMS_DIM: usize = 0;
const ZCR_DIM: usize = 1;
const PITCH_DIM: usize = 2;
const CENTROID_DIM: usize = 3;

/// 各维特征的权重。基频最能区分说话人，因此权重显著高于其余维度；
/// 若与能量/过零率同权，音量差异会盖过音色差异，把同一个人拆成两簇。
/// 顺序必须与特征向量的维度顺序一致：`[能量, 过零率, 基频, 谱质心]`。
const FEATURE_WEIGHTS: [f32; 4] = [1.0, 0.8, 3.0, 0.6];

/// 对一批字幕做说话人分离。
///
/// 输入 `samples` 为单声道 s16le PCM，`sample_rate` 与解码时一致。
/// 返回与 `segments` 等长的标签：`Some(0)` 表示第 1 个说话人，`None` 表示
/// 该段没有有效人声（纯静音或过短），不参与聚类也不显示标签。
pub fn detect_speakers(
    samples: &[i16],
    sample_rate: u32,
    segments: &[Segment],
    speakers: u32,
) -> Vec<Option<u32>> {
    let k = speakers.clamp(2, MAX_SPEAKERS) as usize;
    if segments.is_empty() || samples.is_empty() || sample_rate == 0 {
        return vec![None; segments.len()];
    }

    // 过滤掉越界时间戳（例如上游用 `-ss` 输入侧 seek 后仍按全片时间轴回填）：
    // 不滤的话，落在末尾之后的段会被 `min(samples.len())` 压成一个极短片段，
    // 甚至多段塌到同一处，白白喂给聚类一堆伪特征。越界段一律不给说话人标签，
    // 与「纯静音段」的处理一致。
    let total_sec = samples.len() as f64 / sample_rate as f64;
    let stats: Vec<Option<SegmentStats>> = segments
        .iter()
        .map(|seg| {
            if seg.end <= 0.0 || seg.start >= total_sec {
                return None;
            }
            segment_stats(samples, sample_rate, seg)
        })
        .collect();

    // 静音门限取全体有效段的 RMS 中位数，自动适配素材整体音量，
    // 避免固定阈值在轻录素材上把所有段都判成静音。
    let mut rms_sorted: Vec<f32> = stats
        .iter()
        .flatten()
        .map(|s| s.rms)
        .filter(|v| *v > 0.0)
        .collect();
    if rms_sorted.is_empty() {
        return vec![None; segments.len()];
    }
    rms_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_rms = rms_sorted[rms_sorted.len() / 2];
    let gate = median_rms * SILENCE_RATIO;

    // 只有「能量够 + 有稳定基频」的段才参与聚类：纯噪声/音乐段能量可能很高，
    // 但没有说话人特征，硬拉进聚类会凭空造出一个「说话人」。
    let candidates: Vec<usize> = (0..segments.len())
        .filter(|&i| {
            stats[i]
                .as_ref()
                .map(|s| s.rms > gate && s.features.is_some())
                .unwrap_or(false)
        })
        .collect();
    if candidates.len() < k {
        // 有效段太少（例如只有一两句话），聚类没有统计意义，全部留空
        return vec![None; segments.len()];
    }

    let vectors: Vec<FeatureVec> = candidates
        .iter()
        .map(|&i| stats[i].as_ref().and_then(|s| s.features).unwrap())
        .collect();
    let normalized = zscore_normalize(&vectors);
    let assignments = kmeans(&normalized, k);

    // 按「首次出现顺序」重排说话人编号：视频里先开口的人恒为「说话人 1」，
    // 而不是取决于聚类中心的任意编号。
    let mut order: Vec<u32> = Vec::new();
    let mut labels = vec![None; segments.len()];
    for (slot, &seg_idx) in candidates.iter().enumerate() {
        let cluster = assignments[slot];
        let label = match order.iter().position(|c| *c == cluster) {
            Some(pos) => pos as u32,
            None => {
                order.push(cluster);
                (order.len() - 1) as u32
            }
        };
        labels[seg_idx] = Some(label);
    }
    labels
}

/// 单段字幕的统计结果：能量始终有值（供静音门限使用），
/// 声学特征仅在浊音占比达标时才有值（否则该段无说话人特征可言）。
#[derive(Debug, Clone, Copy)]
struct SegmentStats {
    rms: f32,
    features: Option<FeatureVec>,
}

/// 提取单段字幕的聚合特征：在段内均匀采样若干帧后取均值。
///
/// 均匀采样而不是逐帧扫描，是为了让成本与「字幕条数」成正比；
/// 同时要求浊音帧占比达标，否则该段没有稳定的说话人特征可言。
fn segment_stats(samples: &[i16], sample_rate: u32, seg: &Segment) -> Option<SegmentStats> {
    let seg_start = (seg.start.max(0.0) * sample_rate as f64) as usize;
    let seg_end = (seg.end.max(0.0) * sample_rate as f64) as usize;
    let seg_len = seg_end.saturating_sub(seg_start);
    if seg_len < FRAME_LEN / 2 {
        return None;
    }

    // 段内可用帧起点：末帧起点要保证完整一帧仍在段内
    let last_start = seg_start + seg_len.saturating_sub(FRAME_LEN);
    let span = last_start.saturating_sub(seg_start);
    let take = FRAMES_PER_SEGMENT.min(seg_len / FRAME_HOP).max(1);

    let mut sum = [0.0f32; 4];
    let mut pitch_sum = 0.0f32;
    let mut voiced = 0usize;
    let mut counted = 0usize;
    for i in 0..take {
        // 在段内均匀铺开采样点，首尾各留半帧，避免切到相邻段的音频
        let offset = if take == 1 {
            span / 2
        } else {
            span * i / (take - 1)
        };
        let frame_start = (seg_start + offset).min(last_start);
        let frame_end = (frame_start + FRAME_LEN).min(samples.len());
        if frame_end <= frame_start + 8 {
            continue;
        }
        let f = frame_features(&samples[frame_start..frame_end], sample_rate);
        sum[0] += f.rms;
        sum[1] += f.zcr;
        sum[3] += f.centroid;
        if f.voiced {
            // 清音帧基频为 0，直接平均会被大量 0 拉低，因此只在浊音帧上累计
            pitch_sum += f.pitch;
            voiced += 1;
        }
        counted += 1;
    }
    if counted == 0 {
        return None;
    }

    let n = counted as f32;
    let rms = sum[0] / n;
    let features = if voiced as f32 / n >= MIN_VOICED_RATIO {
        let mut v = [0.0f32; 4];
        v[RMS_DIM] = rms;
        v[ZCR_DIM] = sum[1] / n;
        v[PITCH_DIM] = pitch_sum / voiced as f32;
        v[CENTROID_DIM] = sum[3] / n;
        Some(v)
    } else {
        None
    };
    Some(SegmentStats { rms, features })
}

/// 单帧声学特征：能量 / 过零率 / 自相关基频 / 谱质心代理
fn frame_features(frame: &[i16], sample_rate: u32) -> FrameFeatures {
    if frame.is_empty() {
        return FrameFeatures::default();
    }
    let n = frame.len();
    let mean = frame.iter().map(|s| *s as f32).sum::<f32>() / n as f32;

    let mut energy = 0.0f64;
    let mut crossings = 0usize;
    let mut diff_abs = 0.0f64;
    let mut abs_sum = 0.0f64;
    let mut prev = frame[0] as f32 - mean;
    for (i, &s) in frame.iter().enumerate() {
        let x = s as f32 - mean;
        energy += (x as f64) * (x as f64);
        if i > 0 {
            if (prev < 0.0) != (x < 0.0) {
                crossings += 1;
            }
            diff_abs += (x - prev).abs() as f64;
            abs_sum += x.abs() as f64;
        }
        prev = x;
    }
    let rms = (energy / n as f64).sqrt() as f32 / i16::MAX as f32;
    let zcr = crossings as f32 / n as f32;
    let centroid = if abs_sum > 1e-6 {
        (diff_abs / abs_sum).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };

    // 自相关估基频：归一化到帧能量，避免把「响」误判成「浊」
    let r0: f64 = energy;
    let mut best_lag = 0usize;
    let mut best_val = 0.0f64;
    if r0 > 1e-6 {
        let max_lag = MAX_LAG.min(n.saturating_sub(1));
        for lag in MIN_LAG..=max_lag {
            let mut acc = 0.0f64;
            for i in 0..(n - lag) {
                acc += (frame[i] as f32 - mean) as f64 * (frame[i + lag] as f32 - mean) as f64;
            }
            if acc > best_val {
                best_val = acc;
                best_lag = lag;
            }
        }
    }
    let norm = if r0 > 1e-6 {
        (best_val / r0) as f32
    } else {
        0.0
    };
    let voiced = norm >= VOICED_THRESHOLD && best_lag > 0;
    let pitch = if voiced {
        sample_rate as f32 / best_lag as f32
    } else {
        0.0
    };

    FrameFeatures {
        rms,
        zcr,
        pitch,
        centroid,
        voiced,
    }
}

/// 按维度做 z-score 标准化并施加权重，让不同量纲的特征可以公平参与距离计算
fn zscore_normalize(vectors: &[FeatureVec]) -> Vec<FeatureVec> {
    let n = vectors.len() as f32;
    let mut mean = [0.0f32; 4];
    for v in vectors {
        for d in 0..4 {
            mean[d] += v[d];
        }
    }
    for m in mean.iter_mut() {
        *m /= n;
    }
    let mut std = [0.0f32; 4];
    for v in vectors {
        for d in 0..4 {
            std[d] += (v[d] - mean[d]).powi(2);
        }
    }
    for s in std.iter_mut() {
        *s = (*s / n).sqrt();
    }

    vectors
        .iter()
        .map(|v| {
            let mut out = [0.0f32; 4];
            for d in 0..4 {
                // 常量维度（std≈0）直接归零：它不携带任何区分信息，
                // 若不处理会因除零产生 NaN 而毁掉整轮聚类
                out[d] = if std[d] > 1e-6 {
                    (v[d] - mean[d]) / std[d] * FEATURE_WEIGHTS[d]
                } else {
                    0.0
                };
            }
            out
        })
        .collect()
}

/// 确定性 k-means：初始质心按基频维的分位数布置，迭代到收敛或触顶。
///
/// 返回每个输入点的簇号。之所以不用随机初始化：同一份素材每次点「说话人分离」
/// 应该得到同样的结果，否则用户会以为程序在乱猜。
fn kmeans(vectors: &[FeatureVec], k: usize) -> Vec<u32> {
    // 空输入直接返回：下面的质心布点会访问 `order[slot]`，而空输入的
    // `vectors.len() - 1` 会下溢成 usize::MAX，越界 panic。当前调用方
    // （detect_speakers）已保证非空，这里把该前置条件钉成函数自身的不变量。
    if vectors.is_empty() {
        return Vec::new();
    }
    let k = k.min(vectors.len()).max(1);
    let mut order: Vec<usize> = (0..vectors.len()).collect();
    // 必须按基频维排序。若按能量维排序，遇到整段音量一致的素材（很常见）
    // 该维会被 z-score 归零，排序退化成原序，两个种子可能落到音色相同的点上。
    order.sort_by(|&a, &b| {
        vectors[a][PITCH_DIM]
            .partial_cmp(&vectors[b][PITCH_DIM])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut centroids: Vec<FeatureVec> = Vec::with_capacity(k);
    for i in 0..k {
        let mut slot = ((i as f32 + 0.5) / k as f32 * vectors.len() as f32) as usize;
        slot = slot.min(vectors.len() - 1);
        // 分位数落点可能与已选质心特征完全相同（例如多段音色一致）。
        // 质心重合会让所有点都被吸进第 1 簇，因此顺延到一个特征不同的点。
        let mut guard = 0;
        while guard < vectors.len() && centroids.iter().any(|c| *c == vectors[order[slot]]) {
            slot = (slot + 1) % vectors.len();
            guard += 1;
        }
        centroids.push(vectors[order[slot]]);
    }

    let mut assignments = vec![0u32; vectors.len()];
    for _ in 0..KMEANS_ITERS {
        let mut changed = false;
        for (i, v) in vectors.iter().enumerate() {
            let mut best = 0usize;
            let mut best_dist = f32::MAX;
            for (c, centroid) in centroids.iter().enumerate() {
                let dist = sq_dist(v, centroid);
                if dist < best_dist {
                    best_dist = dist;
                    best = c;
                }
            }
            if assignments[i] != best as u32 {
                assignments[i] = best as u32;
                changed = true;
            }
        }
        if !changed {
            break;
        }

        let mut sums = vec![[0.0f32; 4]; k];
        let mut counts = vec![0usize; k];
        for (i, v) in vectors.iter().enumerate() {
            let c = assignments[i] as usize;
            counts[c] += 1;
            for d in 0..4 {
                sums[c][d] += v[d];
            }
        }
        for c in 0..k {
            if counts[c] > 0 {
                for d in 0..4 {
                    centroids[c][d] = sums[c][d] / counts[c] as f32;
                }
            }
        }
    }
    assignments
}

fn sq_dist(a: &FeatureVec, b: &FeatureVec) -> f32 {
    (0..4).map(|d| (a[d] - b[d]).powi(2)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成一段「带基频的正弦 + 指定音量」的伪语音
    fn tone(len: usize, freq: f32, sample_rate: u32, amp: i16) -> Vec<i16> {
        (0..len)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (amp as f32 * (2.0 * std::f32::consts::PI * freq * t).sin()) as i16
            })
            .collect()
    }

    fn seg(i: usize, start: f64, end: f64) -> Segment {
        Segment::new(i, start, end, format!("第 {i} 句"))
    }

    #[test]
    fn silent_track_yields_no_speaker_labels() {
        let samples = vec![0i16; 8_000 * 4];
        let segs = vec![seg(1, 0.0, 1.0), seg(2, 1.0, 2.0)];
        let labels = detect_speakers(&samples, 8_000, &segs, 2);
        assert_eq!(labels, vec![None, None]);
    }

    #[test]
    fn two_alternating_pitches_are_split_into_two_speakers() {
        // 8 秒素材，每 1 秒一句，低音(120Hz)与高音(260Hz)交替
        let sample_rate = 8_000u32;
        let mut samples = Vec::new();
        for i in 0..8 {
            let freq = if i % 2 == 0 { 120.0 } else { 260.0 };
            samples.extend(tone(sample_rate as usize, freq, sample_rate, 12_000));
        }
        let segs: Vec<Segment> = (0..8)
            .map(|i| seg(i + 1, i as f64, i as f64 + 1.0))
            .collect();

        let labels = detect_speakers(&samples, sample_rate, &segs, 2);
        let first = labels[0].expect("第一句应有说话人标签");
        assert!(labels.iter().all(|l| l.is_some()), "{labels:?}");
        for (i, l) in labels.iter().enumerate() {
            assert_eq!(
                *l,
                Some(if i % 2 == 0 { first } else { 1 - first }),
                "交替音高应被判为两个不同说话人: {labels:?}"
            );
        }
    }

    #[test]
    fn speaker_numbering_follows_first_appearance() {
        let sample_rate = 8_000u32;
        // 第一句高音（后出现的簇），第二句低音
        let mut samples = Vec::new();
        samples.extend(tone(sample_rate as usize, 260.0, sample_rate, 12_000));
        samples.extend(tone(sample_rate as usize, 120.0, sample_rate, 12_000));
        samples.extend(tone(sample_rate as usize, 260.0, sample_rate, 12_000));
        let segs = vec![seg(1, 0.0, 1.0), seg(2, 1.0, 2.0), seg(3, 2.0, 3.0)];
        let labels = detect_speakers(&samples, sample_rate, &segs, 2);
        assert_eq!(labels[0], Some(0), "先开口的人必须是说话人 1: {labels:?}");
        assert_eq!(labels[1], Some(1));
        assert_eq!(labels[2], Some(0));
    }

    #[test]
    fn single_segment_cannot_be_clustered() {
        let sample_rate = 8_000u32;
        let samples = tone(sample_rate as usize, 180.0, sample_rate, 10_000);
        let segs = vec![seg(1, 0.0, 1.0)];
        let labels = detect_speakers(&samples, sample_rate, &segs, 2);
        assert_eq!(labels, vec![None]);
    }

    #[test]
    fn out_of_range_segments_get_no_label_but_keep_alignment() {
        let sample_rate = 8_000u32;
        // 4 秒音频，前 4 句落在范围内、第 5 句整个落在末尾之后
        let mut samples = Vec::new();
        for i in 0..4 {
            let freq = if i % 2 == 0 { 120.0 } else { 260.0 };
            samples.extend(tone(sample_rate as usize, freq, sample_rate, 12_000));
        }
        let mut segs: Vec<Segment> = (0..4)
            .map(|i| seg(i + 1, i as f64, i as f64 + 1.0))
            .collect();
        segs.push(seg(5, 90.0, 91.0)); // 越界段

        let labels = detect_speakers(&samples, sample_rate, &segs, 2);
        assert_eq!(labels.len(), segs.len(), "标签长度必须与 segments 对齐");
        assert_eq!(labels[4], None, "越界段不应被强行打标签");
        assert!(labels[..4].iter().all(|l| l.is_some()), "{labels:?}");
    }

    #[test]
    fn zscore_handles_constant_dimension_without_nan() {
        let vectors = vec![[1.0, 5.0, 0.0, 0.0], [2.0, 5.0, 0.0, 0.0]];
        let out = zscore_normalize(&vectors);
        assert!(out.iter().flatten().all(|v| v.is_finite()), "{out:?}");
        assert_eq!(out[0][1], 0.0);
    }

    #[test]
    fn kmeans_handles_degenerate_inputs() {
        // 空输入过去会在 `vectors.len() - 1` 处下溢 panic
        assert!(kmeans(&[], 2).is_empty());
        // 单点：簇数被压到 1，且不能有下标越界
        let one = vec![[0.0f32, 0.0, 1.0, 0.0]];
        assert_eq!(kmeans(&one, 4), vec![0]);
        // 特征完全相同的多点：质心顺延逻辑不能死循环
        let same = vec![[1.0f32, 0.0, 2.0, 0.0]; 5];
        let labels = kmeans(&same, 3);
        assert_eq!(labels.len(), 5);
        assert!(labels.iter().all(|c| *c < 3));
    }
}
