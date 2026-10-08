//! 音频波形峰值提取 (F-014)
//!
//! 把整轨音频解码成单声道 PCM，按等宽时间桶取「绝对值峰值」，得到一条与
//! 时间轴严格线性对应的包络曲线。渲染时把这条曲线栅格化成一张 BGRA 位图，
//! 交给 GPUI 当纹理上传——比每帧创建上千个 div 便宜得多。
//!
//! 数据只保留「峰值」而非原始采样：1 小时片 8kHz 单声道是 57MB 原始 PCM，
//! 而 2000 桶的包络只有 8KB，足够支撑时间轴宽度的绘制精度。

use anyhow::Result;
use std::path::Path;

use crate::engines::FFmpegEngine;

/// 波形解码采样率。8kHz 足以表达包络与音高，数据量只有 16kHz 的一半。
pub const WAVEFORM_SAMPLE_RATE: u32 = 8_000;

/// 峰值桶数量上限。时间轴可视宽度一般不超过 2000px，再密也画不出更多信息。
pub const MAX_WAVEFORM_BUCKETS: usize = 2_000;

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
pub fn extract(ffmpeg: &FFmpegEngine, media: &Path, buckets: usize) -> Result<WaveformData> {
    let buckets = buckets.clamp(1, MAX_WAVEFORM_BUCKETS);
    let samples = ffmpeg.decode_pcm_mono(media, WAVEFORM_SAMPLE_RATE)?;
    Ok(build(media, &samples, WAVEFORM_SAMPLE_RATE, buckets))
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
}
