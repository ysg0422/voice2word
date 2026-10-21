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

    /// 把可视窗口内的峰值**聚合**成固定数量的柱子，供时间轴按视口宽度绘制。
    ///
    /// # 为什么要「聚合」而不是「取子集」
    ///
    /// 时间轴过去把柱数写死在 200，缩放只能靠取 `peaks` 的一段切片：切出来多少桶就画多少柱，
    /// 于是放大后柱子数骤减、疏密跳变，缩小时又挤成一团。这里改成**按窗口重采样**：
    /// 无论窗口多宽多窄，都固定给出 `bars` 根柱子，每根取该段内所有桶的**最大值**。
    ///
    /// 取最大值而不是平均值：峰值的两级累积（槽峰值 → 桶峰值）用的就是 max，max 满足结合律，
    /// 所以「先聚合再取最大」与「直接对原始样本取最大」等价；换成平均值会让放大后的波形
    /// 整体矮一截（与实际响度不符）。
    ///
    /// `bars` 大于窗口内的桶数时会出现同一桶被多根柱子采样（放大到比数据还细）：刻意如此——
    /// 缩放不该凭空造出细节，重复采样表现为「平顶」，比插值造假更诚实。
    ///
    /// 边界：`bars == 0`、没有峰值、时长为 0、`start/end` 非有限、窗口反向或完全落在
    /// 音频之外，都返回空表（调用方据此不画波形，而不是画一根假的）。
    pub fn peaks_in_window(&self, start_sec: f64, end_sec: f64, bars: usize) -> Vec<f32> {
        if bars == 0 || self.peaks.is_empty() || self.duration <= 0.0 {
            return Vec::new();
        }
        if !start_sec.is_finite() || !end_sec.is_finite() {
            return Vec::new();
        }
        // 窗口先夹到音频范围内：视口可以被拖到音频之外（前后留白），那不是错误。
        let start = start_sec.clamp(0.0, self.duration);
        let end = end_sec.clamp(0.0, self.duration);
        if end <= start {
            return Vec::new();
        }
        let span = self.bucket_range(start, end);
        if span.is_empty() {
            return Vec::new();
        }
        let total = span.len();
        (0..bars)
            .map(|bar| {
                // 每根柱子覆盖的桶区间：用整数乘除分配，保证不重不漏、最后一根一定收到末尾。
                let lo = span.start + total * bar / bars;
                let hi = (span.start + total * (bar + 1) / bars)
                    .max(lo + 1)
                    .min(span.end);
                self.peaks[lo..hi].iter().copied().fold(0.0f32, f32::max)
            })
            .collect()
    }
    /// 指定时间处的峰值（用于播放游标处的电平读数）
    pub fn peak_at(&self, time_sec: f64) -> f32 {
        let r = self.bucket_range(time_sec, time_sec + 0.001);
        self.peaks.get(r.start).copied().unwrap_or(0.0)
    }

    /// 获取全部波形采样列 (时间戳与归一化峰值)
    pub fn columns(&self) -> Vec<WaveformColumn> {
        if self.peaks.is_empty() || self.duration <= 0.0 {
            return Vec::new();
        }
        let total = self.peaks.len();
        let bucket_dur = self.duration / total as f64;
        self.peaks
            .iter()
            .enumerate()
            .map(|(i, &peak)| {
                let time = (i as f64 + 0.5) * bucket_dur;
                WaveformColumn { time, peak }
            })
            .collect()
    }

    /// 在当前波形数据与边缘端点上执行高精度磁性吸附计算。
    ///
    /// 内部使用等宽时间桶的 O(1) 索引映射定位搜索区间，避免遍历整个音频包络，
    /// 无堆内存分配，支持在 60fps/120fps 拖拽事件循环中高频调用。
    pub fn snap_time(
        &self,
        target_t: f64,
        search_radius: f64,
        edges: &[f64],
        silence_threshold: Option<f32>,
    ) -> Option<f64> {
        if !target_t.is_finite() || !search_radius.is_finite() || search_radius <= 0.0 {
            return None;
        }

        // (a) 优先检查相邻字幕边缘端点（防重叠与无缝拼合）
        let mut closest_edge = None;
        let mut min_edge_dist = f64::INFINITY;
        for &edge in edges {
            if !edge.is_finite() || edge < 0.0 {
                continue;
            }
            let dist = (edge - target_t).abs();
            if dist <= search_radius && dist < min_edge_dist {
                min_edge_dist = dist;
                closest_edge = Some(edge);
            }
        }

        if let Some(edge) = closest_edge {
            return Some(edge);
        }

        // (b) 其次在波形采样点中检测局部极小值 / 静音谷底
        if self.peaks.is_empty() || self.duration <= 0.0 {
            return None;
        }

        let total = self.peaks.len();
        let bucket_dur = self.duration / total as f64;
        let threshold = silence_threshold.unwrap_or(DEFAULT_SILENCE_THRESHOLD);

        let t_min = (target_t - search_radius).max(0.0);
        let t_max = (target_t + search_radius).min(self.duration);
        if t_min > t_max {
            return None;
        }

        let min_idx = ((t_min / bucket_dur).floor() as usize).min(total.saturating_sub(1));
        let max_idx = ((t_max / bucket_dur).ceil() as usize).min(total.saturating_sub(1));
        if min_idx > max_idx {
            return None;
        }

        let mut best_valley: Option<(f64, f32, f64)> = None;

        for i in min_idx..=max_idx {
            let peak = self.peaks[i];
            if peak > threshold {
                continue;
            }

            let is_valley = match total {
                1 => true,
                _ if i == 0 => peak <= self.peaks[1],
                _ if i == total - 1 => peak <= self.peaks[total - 2],
                _ => peak <= self.peaks[i - 1] && peak <= self.peaks[i + 1],
            };

            if !is_valley {
                continue;
            }

            let col_time = (i as f64 + 0.5) * bucket_dur;
            let dist = (col_time - target_t).abs();
            if dist > search_radius {
                continue;
            }

            match best_valley {
                None => {
                    best_valley = Some((col_time, peak, dist));
                }
                Some((_, b_peak, b_dist)) => {
                    let peak_diff = peak - b_peak;
                    let is_better = if peak_diff.abs() <= 0.005 {
                        dist < b_dist || (dist == b_dist && peak < b_peak)
                    } else {
                        peak < b_peak
                    };
                    if is_better {
                        best_valley = Some((col_time, peak, dist));
                    }
                }
            }
        }

        best_valley.map(|(t, _, _)| t)
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
/// 为指定媒体文件和桶数构建波形磁盘缓存路径
fn waveform_cache_path(media: &Path, buckets: usize) -> Option<std::path::PathBuf> {
    let meta = std::fs::metadata(media).ok()?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&media.to_string_lossy(), &mut hasher);
    std::hash::Hash::hash(&size, &mut hasher);
    std::hash::Hash::hash(&mtime, &mut hasher);
    std::hash::Hash::hash(&buckets, &mut hasher);
    let hash_val = std::hash::Hasher::finish(&hasher);

    let cache_dir = crate::utils::AppConfig::resolve_path("cache/waveforms");
    let _ = std::fs::create_dir_all(&cache_dir);
    Some(cache_dir.join(format!("{:016x}.bin", hash_val)))
}

/// 尝试从磁盘加载预先提取好的波形包络数据
fn try_read_waveform_cache(media: &Path, buckets: usize) -> Option<WaveformData> {
    let cache_file = waveform_cache_path(media, buckets)?;
    let bytes = std::fs::read(&cache_file).ok()?;
    if bytes.len() < 20 || &bytes[0..8] != b"V2WWF001" {
        return None;
    }
    let duration = f64::from_le_bytes(bytes[8..16].try_into().ok()?);
    let peak_count = u32::from_le_bytes(bytes[16..20].try_into().ok()?) as usize;
    if bytes.len() != 20 + peak_count * 4 {
        return None;
    }
    let mut peaks = Vec::with_capacity(peak_count);
    for i in 0..peak_count {
        let offset = 20 + i * 4;
        peaks.push(f32::from_le_bytes(
            bytes[offset..offset + 4].try_into().ok()?,
        ));
    }
    tracing::info!(
        path = %cache_file.display(),
        peaks = peaks.len(),
        "命中磁盘波形包络缓存，0ms 秒开"
    );
    Some(WaveformData {
        peaks,
        duration,
        source: media.to_path_buf(),
    })
}

/// 将提取好的波形包络持久化到磁盘缓存文件 (约 8KB)
fn try_write_waveform_cache(data: &WaveformData, buckets: usize) {
    if let Some(cache_file) = waveform_cache_path(&data.source, buckets) {
        let mut bytes = Vec::with_capacity(20 + data.peaks.len() * 4);
        bytes.extend_from_slice(b"V2WWF001");
        bytes.extend_from_slice(&data.duration.to_le_bytes());
        bytes.extend_from_slice(&(data.peaks.len() as u32).to_le_bytes());
        for &p in &data.peaks {
            bytes.extend_from_slice(&p.to_le_bytes());
        }
        let _ = std::fs::write(cache_file, bytes);
    }
}

pub fn extract(ffmpeg: &FFmpegEngine, media: &Path, buckets: usize) -> Result<WaveformData> {
    let buckets = buckets.clamp(1, MAX_WAVEFORM_BUCKETS);

    // 优先命中磁盘二进制缓存，消灭二次打开大视频时的 FFmpeg 软解高负荷脉冲
    if let Some(cached) = try_read_waveform_cache(media, buckets) {
        return Ok(cached);
    }

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
    // 写入磁盘缓存供下次秒开
    try_write_waveform_cache(&data, buckets);
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

/// 默认磁性吸附搜索半径 (秒，120ms，处于 100ms~150ms 剪辑吸附手感区间)
pub const DEFAULT_SNAP_RADIUS_SECS: f64 = 0.12;

/// 默认静音波谷检测阈值 (归一化振幅 0.0 ~ 1.0，低于此值视为静音或弱音间隙)
pub const DEFAULT_SILENCE_THRESHOLD: f32 = 0.05;

/// 单个波形采样列的时间戳与归一化峰值 (用于磁性吸附与波形几何分析)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaveformColumn {
    /// 该列对应的中心时间戳 (秒)
    pub time: f64,
    /// 归一化振幅峰值 (0.0 ~ 1.0)
    pub peak: f32,
}

impl WaveformColumn {
    #[inline]
    pub fn new(time: f64, peak: f32) -> Self {
        Self {
            time,
            peak: peak.clamp(0.0, 1.0),
        }
    }
}

/// 高精度磁性吸附计算核心算法 (Waveform Magnetic Snapping)。
///
/// # 参数说明
/// - `target_t`: 用户当前拖动/定位的目标时间戳 (秒)；
/// - `search_radius`: 吸附搜索半径 (秒，通常为 100ms ~ 150ms)；
/// - `columns`: 波形采样列数据（含时间戳与峰值振幅）；
/// - `edges`: 相邻字幕片段的边缘端点切片 (如相邻字幕的 `start` 与 `end`)；
/// - `silence_threshold`: 静音阈值，若未传入则默认使用 [`DEFAULT_SILENCE_THRESHOLD`] (0.05)。
///
/// # 算法策略
/// 1. **边缘对齐吸附优先 (Rule a)**：优先检查输入中的相邻字幕边缘端点 (`edges`)。
///    若存在距离 `target_t` 在 `search_radius` 内的边缘端点，返回距离最近的边缘点。
///    这能有效防止字幕重叠，消除 1~2 帧的无意义缝隙，提供卡扣式的缝合手感；
/// 2. **波形静音波谷检测 (Rule b)**：当没有边缘命中时，在波形列中检测局部极小值 (local valley)：
///    - 该采样点的振幅峰值必须低于静音阈值 (`silence_threshold`)；
///    - 该采样点是局部的波谷（其振幅不大于左右相邻采样）；
///    - 该采样点的时间位于 `target_t ± search_radius` 范围内。
///
///    在所有符合条件的候选波谷中，优先选择振幅更低（静音更彻底）且距 `target_t` 最近的波谷时刻；
/// 3. 若均未命中，返回 `None`。
pub fn find_magnetic_snap_point(
    target_t: f64,
    search_radius: f64,
    columns: &[WaveformColumn],
    edges: &[f64],
    silence_threshold: Option<f32>,
) -> Option<f64> {
    if !target_t.is_finite() || !search_radius.is_finite() || search_radius <= 0.0 {
        return None;
    }

    // (a) 优先检查相邻字幕边缘端点
    let mut closest_edge = None;
    let mut min_edge_dist = f64::INFINITY;
    for &edge in edges {
        if !edge.is_finite() || edge < 0.0 {
            continue;
        }
        let dist = (edge - target_t).abs();
        if dist <= search_radius && dist < min_edge_dist {
            min_edge_dist = dist;
            closest_edge = Some(edge);
        }
    }

    if let Some(edge) = closest_edge {
        return Some(edge);
    }

    // (b) 其次在波形采样点中检测局部极小值 / 静音谷底
    if columns.is_empty() {
        return None;
    }

    let threshold = silence_threshold.unwrap_or(DEFAULT_SILENCE_THRESHOLD);
    let mut best_valley: Option<(f64, f32, f64)> = None;

    let len = columns.len();
    for i in 0..len {
        let col = &columns[i];
        if !col.time.is_finite() || !col.peak.is_finite() {
            continue;
        }
        let dist = (col.time - target_t).abs();
        if dist > search_radius {
            continue;
        }
        if col.peak > threshold {
            continue;
        }

        // 局部波谷检测：
        // 边界处理：len == 1 算波谷；len > 1 时，边界点单侧比较，内部点双侧比较
        let is_valley = match len {
            1 => true,
            _ if i == 0 => col.peak <= columns[1].peak,
            _ if i == len - 1 => col.peak <= columns[len - 2].peak,
            _ => col.peak <= columns[i - 1].peak && col.peak <= columns[i + 1].peak,
        };

        if !is_valley {
            continue;
        }

        // 候选评估：振幅更低的静音谷底更具吸引力；振幅相近（<= 0.005）时选离拖动点更近的
        match best_valley {
            None => {
                best_valley = Some((col.time, col.peak, dist));
            }
            Some((_b_time, b_peak, b_dist)) => {
                let peak_diff = col.peak - b_peak;
                let is_better = if peak_diff.abs() <= 0.005 {
                    dist < b_dist || (dist == b_dist && col.peak < b_peak)
                } else {
                    col.peak < b_peak
                };

                if is_better {
                    best_valley = Some((col.time, col.peak, dist));
                }
            }
        }
    }

    best_valley.map(|(t, _, _)| t)
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

    /// 窗口重采样：柱子数恒定；整段窗口取全量；窗口内取的是 max（放大不矮化）。
    #[test]
    fn peaks_in_window_resamples_to_fixed_bar_count() {
        let data = WaveformData {
            peaks: vec![0.1, 0.9, 0.2, 0.8, 0.3, 0.7],
            duration: 6.0,
            source: PathBuf::from("D:/m.mp4"),
        };
        // 整段窗口、3 根柱子 → 每根吃 2 个桶，取 max
        let bars = data.peaks_in_window(0.0, 6.0, 3);
        assert_eq!(bars.len(), 3);
        assert_eq!(bars, vec![0.9, 0.8, 0.7]);
        // 只放大到前半段：仍然给固定柱数，值取该段内的 max
        // 放大到前 2 个桶（窗口 0~2s）：仍然给固定柱数。桶数少于柱数时按整数分配，
        // 前两根落在第 0 个桶上（平顶），第三根落在第 1 个桶——刻意不插值。
        let zoomed = data.peaks_in_window(0.0, 2.0, 3);
        assert_eq!(zoomed.len(), 3, "放大后柱子数不许变");
        assert_eq!(zoomed, vec![0.1, 0.1, 0.9]);
        // 反向：整段窗口的柱数也一样是 3（缩放不改变柱数，只改变每根覆盖的范围）
        assert_eq!(data.peaks_in_window(0.0, 6.0, 3).len(), 3);
    }

    /// bars 多于桶数时重复采样同一桶（表现为平顶），不插值、不造假。
    #[test]
    fn peaks_in_window_repeats_buckets_when_bars_exceed_data() {
        let data = WaveformData {
            peaks: vec![0.5],
            duration: 1.0,
            source: PathBuf::from("D:/m.mp4"),
        };
        assert_eq!(data.peaks_in_window(0.0, 1.0, 4), vec![0.5, 0.5, 0.5, 0.5]);
    }

    /// 退化输入一律返回空表：调用方据此不画波形，而不是画一根假的。
    #[test]
    fn peaks_in_window_degenerate_inputs_are_empty() {
        let data = WaveformData {
            peaks: vec![0.4, 0.6],
            duration: 2.0,
            source: PathBuf::from("D:/m.mp4"),
        };
        assert!(data.peaks_in_window(0.0, 2.0, 0).is_empty(), "0 根柱子");
        assert!(data.peaks_in_window(1.5, 0.5, 8).is_empty(), "窗口反向");
        assert!(
            data.peaks_in_window(f64::NAN, 1.0, 8).is_empty(),
            "NaN 起点"
        );
        assert!(
            data.peaks_in_window(5.0, 9.0, 8).is_empty(),
            "窗口在音频之外"
        );
        let empty = WaveformData {
            peaks: Vec::new(),
            duration: 2.0,
            source: PathBuf::from("D:/m.mp4"),
        };
        assert!(empty.peaks_in_window(0.0, 2.0, 8).is_empty(), "没有峰值");
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

    #[test]
    fn magnetic_snap_edge_takes_priority_over_waveform() {
        let cols = vec![
            WaveformColumn::new(1.00, 0.40),
            WaveformColumn::new(1.02, 0.01), // 静音波谷 (低于 0.05)
            WaveformColumn::new(1.04, 0.50),
        ];
        let edges = vec![1.05]; // 相邻字幕端点
        let target = 1.01;
        let radius = 0.12;

        let snapped = find_magnetic_snap_point(target, radius, &cols, &edges, None);
        // 虽然 1.02 离 1.01 更近且是波谷，但 Rule (a) 规定边缘端点优先吸附（防止接缝重叠）
        assert_eq!(snapped, Some(1.05));
    }

    #[test]
    fn magnetic_snap_edge_picks_closest_within_radius() {
        let edges = vec![1.00, 1.25, 2.50];
        let radius = 0.12;

        // 离 1.00 仅 0.04s，在 0.12 范围内
        assert_eq!(
            find_magnetic_snap_point(1.04, radius, &[], &edges, None),
            Some(1.00)
        );
        // 离 1.25 仅 0.03s
        assert_eq!(
            find_magnetic_snap_point(1.22, radius, &[], &edges, None),
            Some(1.25)
        );
        // 离所有边缘都远超 radius
        assert_eq!(
            find_magnetic_snap_point(1.50, radius, &[], &edges, None),
            None
        );
    }

    #[test]
    fn magnetic_snap_waveform_silence_valley() {
        let cols = vec![
            WaveformColumn::new(0.10, 0.80),
            WaveformColumn::new(0.20, 0.40),
            WaveformColumn::new(0.30, 0.01), // 明显波谷且 < 0.05
            WaveformColumn::new(0.40, 0.35),
            WaveformColumn::new(0.50, 0.90),
        ];
        let radius = 0.12;

        // 拖动到 0.28s，附近 0.30s 为静音谷底
        let snapped = find_magnetic_snap_point(0.28, radius, &cols, &[], None);
        assert_eq!(snapped, Some(0.30));

        // 拖动到 0.12s，附近波形虽然有局部低点但振幅为 0.40/0.80，均高于阈值 0.05
        assert_eq!(
            find_magnetic_snap_point(0.12, radius, &cols, &[], None),
            None
        );
    }

    #[test]
    fn magnetic_snap_rejects_above_silence_threshold() {
        let cols = vec![
            WaveformColumn::new(0.10, 0.50),
            WaveformColumn::new(0.20, 0.08), // 虽然是局部波谷，但 0.08 > 0.05
            WaveformColumn::new(0.30, 0.60),
        ];
        let snapped = find_magnetic_snap_point(0.20, 0.12, &cols, &[], None);
        assert_eq!(snapped, None, "高于静音阈值的波谷不应被误判为吸附点");

        // 若传入自定义宽松阈值 0.10，则应当命中
        let custom_snapped = find_magnetic_snap_point(0.20, 0.12, &cols, &[], Some(0.10));
        assert_eq!(custom_snapped, Some(0.20));
    }

    #[test]
    fn magnetic_snap_flat_silence_picks_closest_point() {
        // 一段连续静音区
        let cols = vec![
            WaveformColumn::new(0.10, 0.60),
            WaveformColumn::new(0.20, 0.00),
            WaveformColumn::new(0.30, 0.00),
            WaveformColumn::new(0.40, 0.00),
            WaveformColumn::new(0.50, 0.70),
        ];
        // 用户拖到 0.22s，离 0.20s 最近
        assert_eq!(
            find_magnetic_snap_point(0.22, 0.12, &cols, &[], None),
            Some(0.20)
        );
        // 用户拖到 0.39s，离 0.40s 最近
        assert_eq!(
            find_magnetic_snap_point(0.39, 0.12, &cols, &[], None),
            Some(0.40)
        );
    }

    #[test]
    fn waveform_data_snap_time_matches_columns_algorithm() {
        let data = WaveformData {
            peaks: vec![0.8, 0.5, 0.02, 0.4, 0.9, 0.01, 0.7],
            duration: 7.0, // 每桶恰好 1.0 秒，中心分别在 0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5
            source: PathBuf::from("dummy.wav"),
        };
        let edges = vec![0.0];

        // 拖拽到 2.4s，目标附近的桶中心在 2.5s (peak = 0.02)
        let res_struct = data.snap_time(2.4, 0.25, &edges, None);
        let res_func = find_magnetic_snap_point(2.4, 0.25, &data.columns(), &edges, None);
        assert_eq!(res_struct, Some(2.5));
        assert_eq!(res_struct, res_func);

        // 拖拽到 0.08s，边缘 0.0 优先触发
        assert_eq!(data.snap_time(0.08, 0.12, &edges, None), Some(0.0));
    }

    #[test]
    fn magnetic_snap_degenerate_inputs_return_none() {
        assert_eq!(
            find_magnetic_snap_point(f64::NAN, 0.12, &[], &[], None),
            None
        );
        assert_eq!(find_magnetic_snap_point(1.0, 0.0, &[], &[], None), None);
        assert_eq!(find_magnetic_snap_point(1.0, -0.5, &[], &[], None), None);
        assert_eq!(
            find_magnetic_snap_point(1.0, 0.12, &[], &[f64::NAN, -1.0], None),
            None
        );

        let data = WaveformData {
            peaks: Vec::new(),
            duration: 0.0,
            source: PathBuf::from("dummy.wav"),
        };
        assert_eq!(data.snap_time(1.0, 0.12, &[], None), None);
    }
}
