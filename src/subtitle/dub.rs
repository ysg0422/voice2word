//! 配音时间轴规划：把「逐句 TTS 自然时长」摆到字幕时间轴上。
//!
//! 纯函数模块：不碰网络、不碰子进程、不碰文件系统，便于单测。真正的合成在
//! [`crate::engines::tts`]，音轨替换在 [`crate::engines::ffmpeg`]。
//!
//! # 为什么需要「速率自适应」而不是音频拉伸
//!
//! 字幕时间窗是硬约束。一句 1.8s 的字幕配上 3.2s 的配音，必须压缩。两条路：
//! ① 音频时间拉伸（atempo/WSOLA）保音高但实现复杂、过度拉伸有金属音；② 让 TTS
//! 自己说快一点（Edge-TTS `--rate=+N%`）——音高音色自然，代价是多一次合成。
//! 这里选②，见 [`required_rate_percent`]。
//!
//! # 放置规则（为什么允许溢出、什么时候截断）
//!
//! 语音的句间停顿本来就该连读；只要不压到**下一句的起点**，让这一句稍微拖长一点
//! 比硬切掉字尾自然得多。真压到了，就在下一句起点前 30ms 淡出截断——两个人同时
//! 说话会听不清主次。

/// 一句配音的落位结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DubPlacement {
    /// 对应 `Segment::index`。
    pub seg_index: usize,
    /// 起始样本（= round(start * sample_rate)）。
    pub start_sample: usize,
    /// 实际写入的样本数（可能因截断小于自然时长）。
    pub len_samples: usize,
    /// 自然时长超出时间窗（说明速率补偿后仍偏长）。
    pub overflow: bool,
    /// 因压到下一句起点而被截断。
    pub trimmed: bool,
}

/// 截断时给下一句留的静音余量（秒）：30ms 足以避免爆音与叠字。
pub const TRIM_GUARD_SECS: f64 = 0.03;

/// 判定是否需要提高语速，返回需要的 `--rate` 百分数。
///
/// - 自然时长落在窗内（含 2% 容差）→ `None`，不必重合成；
/// - 超出 → `Some(ceil((natural/window - 1) * 100) 封顶到 max_percent)`；
/// - 参数非法（窗宽 ≤ 0、时长非有限）→ `None`（调用方按「放不下」处理）。
///
/// 容差 2%：TTS 时长本身有几个百分点的抖动，为 2% 的重合成纯属浪费一次网络请求。
pub fn required_rate_percent(natural_secs: f64, window_secs: f64, max_percent: u32) -> Option<u32> {
    if !natural_secs.is_finite() || !window_secs.is_finite() || window_secs <= 0.0 {
        return None;
    }
    if natural_secs <= window_secs * 1.02 {
        return None;
    }
    let ratio = natural_secs / window_secs - 1.0;
    // 减 1e-6 再 ceil：`1.03/1.0` 在 f64 里是 3.0000000000000027%，直接 ceil 会
    // 取到 4%——多要 1% 不致命，但会让「刚好 3%」的用例与预期对不上。
    let percent = ((ratio * 100.0) - 1e-6).ceil();
    let percent = if percent.is_finite() && percent > 0.0 {
        percent as u32
    } else {
        1
    };
    Some(percent.min(max_percent.max(1)))
}

/// 把「每句自然时长」摆到时间轴上。
///
/// `entries` 是 `(seg_index, start, end)`；`durations` 与 `entries` **一一对应**
/// （同序）。`total_secs` 是目标音轨总长（通常是媒体时长；0 表示用字幕终点兜底）。
///
/// 输入允许乱序：内部按 start 排序后再处理，输出按 start 升序。
pub fn plan_placements(
    entries: &[(usize, f64, f64)],
    durations: &[f64],
    sample_rate: u32,
    total_secs: f64,
) -> Vec<DubPlacement> {
    if entries.is_empty() || sample_rate == 0 {
        return Vec::new();
    }
    let rate = sample_rate as f64;
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&a, &b| {
        entries[a]
            .1
            .partial_cmp(&entries[b].1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| entries[a].0.cmp(&entries[b].0))
    });

    // 目标总长：给了正数就用媒体时长，否则取字幕终点兜底。
    //
    // **不能**取 `max(媒体时长, 字幕终点)`：字幕时间戳超出媒体时长是坏数据
    // （手动编辑/导入外部字幕都可能有），拿它撑大音轨会让成品多出一段静音。
    let subtitle_end = entries
        .iter()
        .map(|(_, _, end)| *end)
        .filter(|e| e.is_finite())
        .fold(0.0_f64, f64::max);
    let total = if total_secs.is_finite() && total_secs > 0.0 {
        total_secs
    } else {
        subtitle_end.max(1.0)
    };
    let total_samples = (total * rate).round().max(1.0) as usize;

    let mut placements: Vec<DubPlacement> = Vec::with_capacity(order.len());
    for (pos, &i) in order.iter().enumerate() {
        let (seg_index, start, end) = entries[i];
        let duration = durations.get(i).copied().unwrap_or(0.0);
        if !start.is_finite() || !duration.is_finite() || duration <= 0.0 {
            continue;
        }
        let start_sample = (start.max(0.0) * rate).round() as usize;
        let len = (duration * rate).round().max(1.0) as usize;
        let end_sample = start_sample.saturating_add(len);

        // 溢出判定必须用**未截断**的自然终点：截断只是「不让它压到下一句」，
        // 不代表它没超窗（否则提速统计会把所有被截断的句子漏掉）。
        let window_end = ((end.max(start)) * rate).round() as usize;
        let overflow = end_sample > window_end;

        // 只对「真的有下一句」的情况做截断。末句对着音轨总长截断毫无意义——
        // 那一段本来就会被下面的总长裁剪掉，还白白丢 30ms 尾巴。
        let mut trimmed = false;
        let mut final_end = end_sample;
        if let Some(&j) = order.get(pos + 1) {
            let next_start_sample = (entries[j].1.max(0.0) * rate).round() as usize;
            let guard = (TRIM_GUARD_SECS * rate).round() as usize;
            if next_start_sample > start_sample && final_end > next_start_sample {
                let target = next_start_sample.saturating_sub(guard);
                if target > start_sample {
                    final_end = target;
                    trimmed = true;
                }
            }
        }

        placements.push(DubPlacement {
            seg_index,
            start_sample,
            len_samples: final_end.saturating_sub(start_sample),
            overflow,
            trimmed,
        });
    }

    // 越过目标总长的落位截掉（正常不会发生，除非字幕时间戳超出媒体时长）。
    for p in placements.iter_mut() {
        if p.start_sample >= total_samples {
            p.len_samples = 0;
        } else if p.start_sample + p.len_samples > total_samples {
            p.len_samples = total_samples - p.start_sample;
        }
    }
    placements.retain(|p| p.len_samples > 0);
    placements
}

/// 把每句 PCM 按落位混进整轨缓冲（重叠处相加，`i16` 饱和）。
///
/// `pcms` 与 `placements` 同序（`pcms[i]` 对应 `placements[i]`）。写入长度取
/// `min(pcm.len(), placement.len_samples)`：截断后的尾巴不写，避免和下一句叠字。
pub fn mix_track(placements: &[DubPlacement], pcms: &[Vec<i16>], total_samples: usize) -> Vec<i16> {
    let mut track = vec![0i16; total_samples];
    for (p, pcm) in placements.iter().zip(pcms.iter()) {
        let write_len = pcm.len().min(p.len_samples);
        for (k, &sample) in pcm.iter().take(write_len).enumerate() {
            let at = p.start_sample + k;
            if at >= track.len() {
                break;
            }
            let mixed = track[at] as i32 + sample as i32;
            track[at] = mixed.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
    }
    track
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_none_when_fits_within_tolerance() {
        // 1.0s 音频放进 1.0s 窗、放进 1.01s 窗（2% 容差内）都不必重合成。
        assert_eq!(required_rate_percent(1.0, 1.0, 35), None);
        assert_eq!(required_rate_percent(1.0, 1.01, 35), None);
        assert_eq!(required_rate_percent(1.03, 1.0, 35), Some(3));
    }

    #[test]
    fn rate_computes_percent_and_caps() {
        assert_eq!(
            required_rate_percent(2.0, 1.0, 35),
            Some(35),
            "翻倍需求应封顶"
        );
        assert_eq!(required_rate_percent(1.4, 1.0, 35), Some(35));
        assert_eq!(required_rate_percent(1.2, 1.0, 35), Some(20));
        assert_eq!(required_rate_percent(1.0, 0.0, 35), None, "窗宽非法不判");
        assert_eq!(required_rate_percent(f64::NAN, 1.0, 35), None);
    }

    #[test]
    fn placements_are_ordered_and_trim_against_next_line() {
        // 第 2 句（0..2s）的自然配音 3.5s，会压到下一句 2.0s 起点 → 截断。
        let entries = vec![(1, 0.0, 2.0), (2, 2.0, 3.0)];
        let durations = vec![3.5, 0.8];
        let out = plan_placements(&entries, &durations, 1000, 5.0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].start_sample, 0);
        assert!(out[0].trimmed, "超窗且压到下一句，应截断");
        assert_eq!(
            out[0].start_sample + out[0].len_samples,
            1970,
            "留 30ms 余量"
        );
        assert!(out[0].overflow);
        assert!(!out[1].trimmed);
        assert_eq!(out[1].len_samples, 800);
    }

    #[test]
    fn placements_allow_overflow_into_gap() {
        // 第 1 句自然时长 1.5s，窗只有 1s，但下一句 4s 才开始 → 允许溢出、不截断。
        let entries = vec![(1, 0.0, 1.0), (2, 4.0, 5.0)];
        let durations = vec![1.5, 0.5];
        let out = plan_placements(&entries, &durations, 1000, 6.0);
        assert!(!out[0].trimmed, "有空隙时应允许自然溢出");
        assert_eq!(out[0].len_samples, 1500);
        assert!(out[0].overflow);
    }

    #[test]
    fn placements_handle_unordered_input_and_skip_invalid() {
        let entries = vec![(9, 4.0, 5.0), (1, 0.0, 1.0), (5, f64::NAN, 2.0)];
        let durations = vec![0.5, 0.5, 0.5];
        let out = plan_placements(&entries, &durations, 1000, 6.0);
        assert_eq!(out.len(), 2, "非法起点应跳过");
        assert_eq!(out[0].seg_index, 1);
        assert_eq!(out[1].seg_index, 9);
        assert!(out[0].start_sample < out[1].start_sample);
    }

    #[test]
    fn placements_clip_to_total_length() {
        let entries = vec![(1, 0.0, 1.0), (2, 9.5, 10.5)];
        let durations = vec![0.5, 2.0];
        let out = plan_placements(&entries, &durations, 1000, 10.0);
        let last = out.last().unwrap();
        assert_eq!(last.start_sample + last.len_samples, 10_000, "不得越过总长");
    }

    #[test]
    fn mix_track_adds_overlap_and_saturates() {
        let placements = vec![
            DubPlacement {
                seg_index: 1,
                start_sample: 0,
                len_samples: 3,
                overflow: false,
                trimmed: false,
            },
            DubPlacement {
                seg_index: 2,
                start_sample: 2,
                len_samples: 3,
                overflow: false,
                trimmed: false,
            },
        ];
        let pcms = vec![
            vec![10_000i16, 10_000, 10_000],
            vec![30_000i16, 30_000, 30_000],
        ];
        let track = mix_track(&placements, &pcms, 5);
        assert_eq!(track[0], 10_000);
        assert_eq!(track[2], i16::MAX, "重叠相加应饱和而不是回绕");
        assert_eq!(track[4], 30_000);
    }

    #[test]
    fn mix_track_respects_trimmed_length() {
        let placements = vec![DubPlacement {
            seg_index: 1,
            start_sample: 0,
            len_samples: 2,
            overflow: true,
            trimmed: true,
        }];
        let pcms = vec![vec![1i16, 2, 3, 4]];
        let track = mix_track(&placements, &pcms, 10);
        assert_eq!(&track[..4], &[1, 2, 0, 0], "截断后的尾巴不得写入");
    }

    #[test]
    fn empty_inputs_yield_empty_plan() {
        assert!(plan_placements(&[], &[], 1000, 5.0).is_empty());
        assert!(plan_placements(&[(1, 0.0, 1.0)], &[0.5], 0, 1.0).is_empty());
    }
}
