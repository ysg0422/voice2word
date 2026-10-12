//! 音频波形峰值提取 (F-014)
//!
//! 把整轨音频解码成单声道 PCM，按等宽时间桶取「绝对值峰值」，得到一条与
//! 时间轴严格线性对应的包络曲线。渲染时把这条曲线栅格化成一张 BGRA 位图，
//! 交给 GPUI 当纹理上传——比每帧创建上千个 div 便宜得多。
//!
//! 数据只保留「峰值」而非原始采样：1 小时片 8kHz 单声道是 57MB 原始 PCM，
//! 而 2000 桶的包络只有 8KB，足够支撑时间轴宽度的绘制精度。
//!
//! 解码走流式路径（[`FFmpegEngine::decode_pcm_mono_chunked`]）：边读边折进
//! [`PeakAccumulator`]，累加器恒定只占几百 KB。此前必须先攒出一份与素材等长的
//! PCM（1 小时 57MB、4 小时 230MB）才能算出那 8KB 包络，长素材既慢又容易出现
//! 大块瞬时分配。

use anyhow::Result;
use std::path::Path;

use crate::engines::FFmpegEngine;

/// 波形解码采样率。8kHz 足以表达包络与音高，数据量只有 16kHz 的一半。
pub const WAVEFORM_SAMPLE_RATE: u32 = 8_000;

/// 峰值桶数量上限。时间轴可视宽度一般不超过 2000px，再密也画不出更多信息。
pub const MAX_WAVEFORM_BUCKETS: usize = 2_000;

/// 峰值累加器的基准槽长（毫秒）。
///
/// 流式提取在读完之前不知道总长，桶边界只能事后算，所以累加器先按固定槽长记录
/// 槽内峰值，收尾再等宽归并成桶。槽越细，桶边界处的误差越小：2ms 在 8kHz 下是
/// 16 个样本，比显示端「200 根柱」的时间分辨率还细两个数量级。
const ACC_SLOT_MS: u64 = 2;

/// 累加器槽数上限，涨到它的两倍就两两归并、槽长翻倍。
///
/// 取桶上限的 32 倍，意味着每根显示柱背后至少还有 32 个槽参与取最大值；代价是
/// 恒定 256KB（u32 × 槽数），换来「提取波形的内存与素材时长彻底解耦」：
/// 1 小时素材的槽会自动长到 64ms，占用仍然只有 200KB 出头。
const MAX_ACC_SLOTS: usize = MAX_WAVEFORM_BUCKETS * 32;

/// 一段音频的波形包络
#[derive(Debug, Clone)]
pub struct WaveformData {
    /// 等宽时间桶的归一化峰值 (0.0 ~ 1.0)，下标越大时间越靠后
    pub peaks: Vec<f32>,
    /// 音频总时长 (秒)
    pub duration: f64,
    /// 提取峰值所用的媒体路径，用于判断缓存是否还对应当前工程
    pub source: std::path::PathBuf,
}

impl WaveformData {
    /// 按时间区间取峰值切片。返回的下标范围用于把整条包络裁剪到可视窗口，
    /// 时间轴放大后仍然只画看得见的那一段。
    pub fn bucket_range(&self, start_sec: f64, end_sec: f64) -> std::ops::Range<usize> {
        if self.peaks.is_empty() || self.duration <= 0.0 {
            return 0..0;
        }
        let to_idx = |t: f64| {
            ((t.max(0.0) / self.duration) * self.peaks.len() as f64)
                .floor()
                .clamp(0.0, self.peaks.len() as f64) as usize
        };
        let lo = to_idx(start_sec);
        let hi = to_idx(end_sec).max(lo);
        lo..hi.min(self.peaks.len())
    }

    /// 指定时间处的峰值（用于播放游标处的电平读数）
    pub fn peak_at(&self, time_sec: f64) -> f32 {
        let r = self.bucket_range(time_sec, time_sec + 0.001);
        self.peaks.get(r.start).copied().unwrap_or(0.0)
    }
}

/// 从媒体文件提取波形包络。
///
/// `buckets` 会被收敛到 `1..=MAX_WAVEFORM_BUCKETS`：极短视频至少给 1 个桶，
/// 超长视频也不让内存与栅格化成本随长度线性膨胀。
///
/// 全程流式：只解一遍音频，边解边折成槽峰值，因此内存与素材时长无关。没有音轨、
/// 音轨损坏、FFmpeg 卡死超时都会以 `Err` 上报（波形轨降级为「没有波形」即可，
/// 不牵动字幕主流程）。
pub fn extract(ffmpeg: &FFmpegEngine, media: &Path, buckets: usize) -> Result<WaveformData> {
    let buckets = buckets.clamp(1, MAX_WAVEFORM_BUCKETS);
    let mut acc = PeakAccumulator::new(WAVEFORM_SAMPLE_RATE);
    // 每解出 60 秒音频打一条 debug 进度：提取看起来「没动静」时，
    // 日志里能区分是「还在解」还是「第一秒就卡住了」。
    let step = 60 * WAVEFORM_SAMPLE_RATE as u64;
    let mut next_log = step;
    ffmpeg.decode_pcm_mono_chunked(media, WAVEFORM_SAMPLE_RATE, None, &mut |chunk| {
        acc.push(chunk);
        if acc.total >= next_log {
            next_log += step;
            tracing::debug!(
                decoded_sec = acc.total / WAVEFORM_SAMPLE_RATE as u64,
                "波形包络解码中"
            );
        }
        Ok(())
    })?;
    let data = acc.finish(media, WAVEFORM_SAMPLE_RATE, buckets);
    tracing::debug!(
        bars = data.peaks.len(),
        duration_sec = data.duration,
        "波形包络提取完成"
    );
    Ok(data)
}

/// 由已解码的 PCM 构造包络（与 [`extract`] 分离，便于单测直接喂数据）。
pub fn build(media: &Path, samples: &[i16], sample_rate: u32, buckets: usize) -> WaveformData {
    let buckets = buckets.max(1);
    let duration = if sample_rate == 0 {
        0.0
    } else {
        samples.len() as f64 / sample_rate as f64
    };
    if samples.is_empty() {
        return WaveformData {
            peaks: Vec::new(),
            duration,
            source: media.to_path_buf(),
        };
    }

    // 桶宽向上取整：宁可让最后一个桶短一点，也不要出现「空桶」导致曲线尾部塌成 0
    let per_bucket = samples.len().div_ceil(buckets);
    let mut peaks = Vec::with_capacity(buckets);
    for chunk in samples.chunks(per_bucket) {
        let peak = chunk
            .iter()
            .map(|s| (*s as i32).unsigned_abs())
            .max()
            .unwrap_or(0) as f32
            / i16::MAX as f32;
        peaks.push(peak.clamp(0.0, 1.0));
    }

    WaveformData {
        peaks,
        duration,
        source: media.to_path_buf(),
    }
}

/// 取一段样本的绝对值峰值（原始整数刻度，未归一化）。
///
/// 与 [`build`] 内部用的是同一个表达式：两条路径必须逐位一致，否则「同一素材走
/// 流式与走整轨得到不同波形」就是很难查的脏 bug（`streaming_matches_build_*`
/// 系列单测盯着这件事）。
fn peak_scale(samples: &[i16]) -> u32 {
    samples
        .iter()
        .map(|s| (*s as i32).unsigned_abs())
        .max()
        .unwrap_or(0)
}

/// 流式峰值累加器：把任意长的 PCM 折成定长槽峰值，内存不随素材时长增长。
///
/// 槽长只会翻倍（两两归并），因此任意时刻已封口的槽都严格覆盖样本前缀
/// `[0, slots.len() * slot_samples)`，收尾时按「样本下标 → 槽下标」换算取桶峰值，
/// 与整轨实现的 `chunks(per_bucket)` 在桶边界与槽对齐时逐位等价。
struct PeakAccumulator {
    /// 当前槽长（样本数），归并一次翻一倍
    slot_samples: usize,
    /// 已封口槽的绝对值峰值
    slots: Vec<u32>,
    /// 还没凑满一个槽的尾部样本
    tail: Vec<i16>,
    /// 已吃进的样本总数
    total: u64,
}

impl PeakAccumulator {
    fn new(sample_rate: u32) -> Self {
        // 采样率异常（0）也不能除零：退化成「一个样本一槽」，只是更吃内存
        let base = (sample_rate as u64 * ACC_SLOT_MS / 1000).max(1) as usize;
        Self {
            slot_samples: base,
            slots: Vec::new(),
            tail: Vec::with_capacity(base),
            total: 0,
        }
    }

    /// 吃进一批样本。这个方法会被回调调用几千次，开销必须只与「这一批」有关。
    fn push(&mut self, chunk: &[i16]) {
        self.total += chunk.len() as u64;
        self.tail.extend_from_slice(chunk);
        let slot = self.slot_samples;
        let full = self.tail.len() / slot;
        for i in 0..full {
            let peak = peak_scale(&self.tail[i * slot..(i + 1) * slot]);
            self.slots.push(peak);
            // 涨到两倍上限就归并：此刻槽数必然是偶数，归并后槽的栅格始终是
            // 「基准槽长的 2 的幂倍」，收尾算桶边界不会碰上半个槽。
            if self.slots.len() == 2 * MAX_ACC_SLOTS {
                self.coalesce();
            }
        }
        // 只留不足一槽的尾巴；容量也稳定在这个量级，不会随素材变长
        self.tail.drain(..full * slot);
    }

    /// 两两归并：新槽 = 相邻两旧槽取最大值，槽长翻倍。
    fn coalesce(&mut self) {
        // 归并只会在槽数恰好翻倍到 2 * MAX_ACC_SLOTS 时触发（偶数），
        // as_chunks 的尾块恒为空，这里显式忽略以保持无 panic 路径。
        let merged: Vec<u32> = self
            .slots
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| pair[0].max(pair[1]))
            .collect();
        self.slots = merged;
        self.slot_samples *= 2;
    }

    /// 收尾：按时间等宽归并成至多 `buckets` 个桶。
    fn finish(self, media: &Path, sample_rate: u32, buckets: usize) -> WaveformData {
        let buckets = buckets.max(1);
        let total = self.total as usize;
        let duration = if sample_rate == 0 {
            0.0
        } else {
            total as f64 / sample_rate as f64
        };
        if total == 0 {
            return WaveformData {
                peaks: Vec::new(),
                duration,
                source: media.to_path_buf(),
            };
        }

        // 桶宽与整轨实现同式：向上取整，宁可最后一个桶短一点，也不要空桶让曲线塌成 0
        let per_bucket = total.div_ceil(buckets);
        let bucket_count = total.div_ceil(per_bucket);
        let slot = self.slot_samples;
        let closed = self.slots.len() * slot; // 已封口槽覆盖了多少样本
        let mut peaks = Vec::with_capacity(bucket_count);
        for i in 0..bucket_count {
            let start = i * per_bucket;
            let end = (start + per_bucket).min(total);
            let mut peak = 0u32;
            // 桶边界落在槽中间时会把该槽整个算进来（最多偏高一个槽）。槽长只有素材
            // 的十万分之一量级，比显示端一根柱还细得多，肉眼看不出来。
            let lo = (start / slot).min(self.slots.len());
            let hi = end.div_ceil(slot).min(self.slots.len());
            if lo < hi {
                peak = self.slots[lo..hi].iter().copied().max().unwrap_or(0);
            }
            // 最后一个不满槽的样本还没进 slots，单独扫一遍，否则结尾会塌成 0
            if end > closed {
                let from = start.max(closed) - closed;
                let to = (end - closed).min(self.tail.len());
                peak = peak.max(peak_scale(&self.tail[from..to]));
            }
            peaks.push((peak as f32 / i16::MAX as f32).clamp(0.0, 1.0));
        }

        WaveformData {
            peaks,
            duration,
            source: media.to_path_buf(),
        }
    }
}

/// 把包络栅格化成 GPUI 可直接上传的 BGRA 位图。
///
/// 每一列对应一个时间桶，柱体以垂直中线为基准上下对称生长；
/// 返回 `(宽, 高, BGRA 字节)`，宽 = 桶数、高 = `height`。
/// 之所以自己拼 BGRA 而不是 RGBA：GPUI 的 `RenderImage` 底层纹理格式是
/// `Bgra8Unorm`，颜色通道顺序写反会得到红蓝互换的波形。
pub fn render_bars_bgra(
    peaks: &[f32],
    height: u32,
    color: (u8, u8, u8),
    alpha: u8,
) -> (u32, u32, Vec<u8>) {
    let height = height.max(2);
    let width = peaks.len().max(1) as u32;
    let (r, g, b) = color;
    let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
    let center = height as f32 / 2.0;

    for (x, peak) in peaks.iter().enumerate() {
        // 留 1px 呼吸空间，避免满量程时柱体顶到边缘看起来像被裁掉
        let half = ((*peak).clamp(0.0, 1.0) * (center - 1.0)).max(0.5);
        let top = (center - half).floor().max(0.0) as u32;
        let bottom = (center + half).ceil().min(height as f32) as u32;
        for y in top..bottom {
            let idx = ((y as usize) * (width as usize) + x) * 4;
            // 越靠近中线越亮，形成「实心柱 + 高光芯」的观感
            let dist = (y as f32 - center).abs() / center.max(1.0);
            let a = (alpha as f32 * (1.0 - dist * 0.45)).clamp(0.0, 255.0) as u8;
            buf[idx] = b;
            buf[idx + 1] = g;
            buf[idx + 2] = r;
            buf[idx + 3] = a;
        }
    }
    (width, height, buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn pcm(len: usize, amplitude: i16) -> Vec<i16> {
        (0..len)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    #[test]
    fn peaks_are_normalized_and_cover_full_duration() {
        let samples = pcm(8_000 * 4, i16::MAX); // 4 秒满量程
        let data = build(&PathBuf::from("a.wav"), &samples, 8_000, 8);
        assert_eq!(data.peaks.len(), 8);
        assert!((data.duration - 4.0).abs() < 1e-9);
        assert!(
            data.peaks.iter().all(|p| (*p - 1.0).abs() < 1e-6),
            "{:?}",
            data.peaks
        );
    }

    #[test]
    fn quiet_and_loud_halves_produce_different_peaks() {
        let mut samples = pcm(4_000, 3_000); // 前 0.5 秒较响
        samples.extend(pcm(4_000, 100)); // 后 0.5 秒很轻
        let data = build(&PathBuf::from("a.wav"), &samples, 8_000, 2);
        assert!(data.peaks[0] > data.peaks[1] * 10.0, "{:?}", data.peaks);
    }

    #[test]
    fn bucket_range_tracks_time() {
        let samples = pcm(8_000 * 10, i16::MAX);
        let data = build(&PathBuf::from("a.wav"), &samples, 8_000, 100);
        let mid = data.bucket_range(5.0, 5.1);
        assert_eq!(mid.start, 50);
        assert!(mid.end >= 51 && mid.end <= 52, "{mid:?}");
        assert_eq!(data.bucket_range(-3.0, 0.5).start, 0);
    }

    #[test]
    fn zero_buckets_do_not_panic_and_yield_one_column() {
        // 线上调用方会传常量桶数，但 `build` 是公开 API：buckets=0 过去会走进
        // `div_ceil(0)` 直接 panic。现在收敛为 1 个桶。
        let samples = pcm(800, i16::MAX);
        let data = build(&PathBuf::from("a.wav"), &samples, 8_000, 0);
        assert_eq!(data.peaks.len(), 1);
        assert!((data.peaks[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn empty_pcm_does_not_panic() {
        let data = build(&PathBuf::from("a.wav"), &[], 8_000, 16);
        assert!(data.peaks.is_empty());
        assert_eq!(data.duration, 0.0);
        let (w, h, bytes) = render_bars_bgra(&data.peaks, 8, (16, 185, 129), 200);
        assert_eq!((w, h), (1, 8));
        assert_eq!(bytes.len(), 8 * 4);
    }

    #[test]
    fn bgra_channel_order_is_blue_green_red_alpha() {
        let peaks = [1.0f32];
        let (w, h, bytes) = render_bars_bgra(&peaks, 4, (0x10, 0x20, 0x30), 255);
        assert_eq!((w, h), (1, 4));
        // 取最靠近中线的一行校验通道顺序
        let idx = 2 * 4;
        assert_eq!(&bytes[idx..idx + 4], &[0x30, 0x20, 0x10, 255]);
    }
    /// 段内幅度按段号伪随机、段中部带一个尖峰：每段的峰值互不相同，
    /// 桶边界一旦错位、或某个槽被漏掉，下面的逐位对比就会立刻炸。
    fn stepped(len: usize, step: usize) -> Vec<i16> {
        (0..len)
            .map(|i| {
                let seg = i / step;
                let base = 1_000 + (seg.wrapping_mul(2_654_435_761) % 20_000) as i32;
                if i % step == step / 2 {
                    (base * 3 / 2).min(32_000) as i16
                } else {
                    base as i16
                }
            })
            .collect()
    }

    /// 按给定分片模式把样本喂进流式累加器（生产环境 FFmpeg 管道回来的批次大小不可控）。
    fn feed_streaming(sample_rate: u32, samples: &[i16], pattern: &[usize]) -> PeakAccumulator {
        let mut acc = PeakAccumulator::new(sample_rate);
        let mut offset = 0;
        let mut i = 0;
        while offset < samples.len() {
            let step = pattern[i % pattern.len()].max(1);
            let end = (offset + step).min(samples.len());
            acc.push(&samples[offset..end]);
            offset = end;
            i += 1;
        }
        acc
    }

    #[test]
    fn streaming_matches_build_when_bucket_edges_align_with_slots() {
        // 4 秒、8 桶 → 每桶 4000 样本，正好是基准槽长（16 样本）的整数倍
        let samples = stepped(8_000 * 4, 4_000);
        let expected = build(&PathBuf::from("a.wav"), &samples, 8_000, 8);
        let streamed = feed_streaming(8_000, &samples, &[37, 1, 999, 4_096, 15, 16_385, 2]).finish(
            &PathBuf::from("a.wav"),
            8_000,
            8,
        );
        assert_eq!(streamed.peaks, expected.peaks);
        assert_eq!(streamed.duration, expected.duration);
    }

    #[test]
    fn streaming_peaks_ignore_chunk_boundaries() {
        let samples = stepped(8_000 * 4, 4_000);
        let one = feed_streaming(8_000, &samples, &[1]).finish(&PathBuf::from("a.wav"), 8_000, 16);
        let many = feed_streaming(8_000, &samples, &[3_999, 4_096, 7]).finish(
            &PathBuf::from("a.wav"),
            8_000,
            16,
        );
        assert_eq!(one.peaks, many.peaks);
        assert_eq!(
            one.peaks,
            build(&PathBuf::from("a.wav"), &samples, 8_000, 16).peaks
        );
    }

    #[test]
    fn streaming_coalesces_slots_on_long_audio_without_losing_peaks() {
        // 2,048,000 样本 = 基准槽 16 样本 × 128_000 槽：恰好触发一次归并
        let samples = stepped(2_048_000, 1_600);
        let mut acc = PeakAccumulator::new(8_000);
        for chunk in samples.chunks(100_000) {
            acc.push(chunk);
        }
        assert_eq!(acc.slot_samples, 32, "长素材应当把槽长翻倍");
        assert_eq!(acc.slots.len(), MAX_ACC_SLOTS);
        assert!(
            acc.slots.len() <= 2 * MAX_ACC_SLOTS,
            "槽数必须被上限压住，内存不能随素材时长膨胀"
        );

        // 每桶 1600 样本 = 50 个归并后的槽，边界对齐 → 与整轨实现逐位一致
        let expected = build(&PathBuf::from("a.wav"), &samples, 8_000, 1_280);
        let streamed = acc.finish(&PathBuf::from("a.wav"), 8_000, 1_280);
        assert_eq!(streamed.peaks, expected.peaks);
        assert_eq!(streamed.duration, expected.duration);
    }

    #[test]
    fn streaming_keeps_tail_until_last_slot_and_matches_build() {
        // 5 个样本凑不满一个 16 样本的槽：必须留在 tail 里，且最后一个桶要看得见它
        let samples = [0i16, 0, 0, 0, i16::MIN];
        let mut acc = PeakAccumulator::new(8_000);
        acc.push(&samples);
        assert!(acc.slots.is_empty(), "不满一槽不该封口");
        assert_eq!(acc.tail.len(), 5);
        assert_eq!(acc.total, 5);

        let streamed = acc.finish(&PathBuf::from("a.wav"), 8_000, 16);
        let expected = build(&PathBuf::from("a.wav"), &samples, 8_000, 16);
        assert_eq!(streamed.peaks, expected.peaks);
        assert_eq!(streamed.peaks.last().copied(), Some(1.0));
        assert_eq!(streamed.duration, expected.duration);
    }

    #[test]
    fn streaming_handles_zero_sample_rate_and_empty_input() {
        let data = [1i16, 2, 3];
        let mut acc = PeakAccumulator::new(0); // 采样率 0 不能除零
        acc.push(&data);
        let streamed = acc.finish(&PathBuf::from("a.wav"), 0, 4);
        assert_eq!(
            streamed.peaks,
            build(&PathBuf::from("a.wav"), &data, 0, 4).peaks
        );
        assert_eq!(streamed.duration, 0.0);

        let empty = PeakAccumulator::new(8_000).finish(&PathBuf::from("a.wav"), 8_000, 16);
        assert!(empty.peaks.is_empty());
        assert_eq!(empty.duration, 0.0);
    }
}
