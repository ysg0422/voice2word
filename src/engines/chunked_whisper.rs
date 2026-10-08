//! Whisper 切块并行：6 分钟一块、1.5 秒重叠，多进程推理后按时间戳拼接。
//! 课程设计仍走本地 whisper-cli，不换模型栈。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

use super::ffmpeg::FFmpegEngine;
use super::{SenseVoiceEngine, WhisperEngine};
use crate::subtitle::Segment;
use crate::utils::TempPathGuard;

const CHUNK_SEC: f64 = 360.0;
const OVERLAP_SEC: f64 = 1.5;
const MIN_CHUNK_SEC: f64 = 45.0;
/// 末段兜底延展的硬上限（秒）。见 [`stitch_chunks`] 末尾。
///
/// 依据：兜底本意是「补回最后一句被重叠判定误丢的收尾语音」，真实漏删只有几秒；
/// 项目内单条字幕的时长上限是 6s（`subtitle::segment::MAX_SEGMENT_DUR`），下游
/// `optimize_segments` 还会再拆。取 2 条标准字幕的长度（12s）足以覆盖真实漏删，
/// 又能在「末块整块被丢弃」时挡住把一条字幕拉长到几十秒 / 整块时长的错误行为。
/// （不取 `MIN_CHUNK_SEC`=45s：那等于允许单条字幕被拉到接近一个整块，正是要挡的场景。）
const TAIL_EXTEND_MAX_SEC: f64 = 12.0;
const DEFAULT_WORKERS: usize = 2;
const MAX_WORKERS: usize = 4;
/// SenseVoice 多进程并行的 worker 上限。
/// 16 核机器实测：6 进程 × 2 线程（12 线程）最快（6 分钟样本 14.5s）；
/// 8 进程反而回落到 17.3s —— 并发模型加载的磁盘争用与内存带宽成为新瓶颈。
const MAX_SV_WORKERS: usize = 6;

#[derive(Clone, Copy, Debug)]
pub struct AudioChunk {
    pub index: usize,
    pub start: f64,
    pub duration: f64,
}

pub fn plan_chunks(total_duration: f64) -> Vec<AudioChunk> {
    plan_chunks_target(total_duration, CHUNK_SEC)
}

/// 按指定目标块长切块（Whisper 用 360s；SenseVoice 进程并行用更短的块以获得
/// 更好的负载均衡——块数应为 worker 数的整数倍附近，避免最后一轮只有少数
/// worker 在跑、其余空转）。
pub fn plan_chunks_target(total_duration: f64, target_sec: f64) -> Vec<AudioChunk> {
    let target = target_sec.max(MIN_CHUNK_SEC);
    if total_duration <= target + MIN_CHUNK_SEC {
        return vec![AudioChunk {
            index: 0,
            start: 0.0,
            duration: total_duration.max(0.1),
        }];
    }
    // Keep the historical six-minute target for the chunk count, but divide
    // the media evenly. A fixed step leaves a short tail chunk, making the
    // final parallel round wait on one long worker while the other is idle.
    let chunk_count = (total_duration / target).ceil().max(2.0) as usize;
    let nominal = total_duration / chunk_count as f64;
    let half_overlap = OVERLAP_SEC * 0.5;
    (0..chunk_count)
        .map(|index| {
            let boundary_start = index as f64 * nominal;
            let boundary_end = ((index + 1) as f64 * nominal).min(total_duration);
            let start = if index == 0 {
                0.0
            } else {
                (boundary_start - half_overlap).max(0.0)
            };
            let end = if index + 1 == chunk_count {
                total_duration
            } else {
                (boundary_end + half_overlap).min(total_duration)
            };
            AudioChunk {
                index,
                start,
                duration: (end - start).max(0.1),
            }
        })
        .collect()
}

/// SenseVoice 多进程并行的 worker 数：每 2 个核心一个进程，上限 6。
/// 抽成独立函数以便管线展示与基准测试复用同一份策略。
pub fn sensevoice_worker_count(cores: u32) -> usize {
    (cores / 2).clamp(2, MAX_SV_WORKERS as u32) as usize
}

pub fn worker_count(chunk_n: usize, requested_threads: u32, use_gpu: bool) -> usize {
    if chunk_n <= 1 {
        return 1;
    }
    // 核显/单 GPU 上两个 whisper-cli 会抢同一块显存，识别直接变糊。
    if use_gpu {
        return 1;
    }
    // 每个 whisper-cli 进程至少给 8 个线程，避免 16 线程机器被拆成
    // 4 个 4 线程进程后互相争抢内存带宽。超过 16 线程再增加进程数。
    let by_cpu = (requested_threads.max(8) / 8) as usize;
    by_cpu
        .clamp(2, MAX_WORKERS)
        .min(chunk_n)
        .max(DEFAULT_WORKERS.min(chunk_n))
}

/// 去掉切块重叠区里的重复句：后一块落在上一块尾部的字幕丢弃。
pub fn stitch_chunks(mut parts: Vec<(usize, f64, Vec<Segment>)>) -> Vec<Segment> {
    parts.sort_by_key(|(idx, _, _)| *idx);
    // 所有块的最大结束时间（按各自偏移还原到全局时间轴）。用于兜住「末块整体落在
    // 已接受字幕之后」这一漏删场景——见下方对末条接受片段的处理。
    let global_end = parts
        .iter()
        .map(|(_, offset, segs)| segs.iter().map(|s| s.end + offset).fold(0.0f64, f64::max))
        .fold(0.0f64, f64::max);
    let mut out: Vec<Segment> = Vec::new();
    for (_idx, offset, segs) in parts {
        for mut seg in segs {
            seg.start += offset;
            seg.end += offset;
            if let Some(prev) = out.last() {
                let gap = seg.start - prev.end;
                let similar = text_similar(prev.display_text(), seg.display_text());
                // 切块有 1.5s 重叠：时间交叉或紧挨着的重复句都丢掉后一块。
                if similar && gap < OVERLAP_SEC + 0.4 {
                    continue;
                }
                if gap < -0.25 {
                    if similar || seg.end <= prev.end + 0.15 {
                        continue;
                    }
                    if seg.start < prev.end {
                        seg.start = prev.end;
                    }
                    if seg.end - seg.start < 0.12 {
                        continue;
                    }
                }
            }
            out.push(seg);
        }
    }
    // 末块整体被上面的重叠判定吞掉时的兜底：若已接受字幕整体提前于全部块的真实
    // 结束时间收尾，说明末尾那段语音被判成了「重叠重复」而丢弃，而它其实是片尾
    // 最后一句——直接丢掉会让字幕凭空少一截。末条正是被压得最长的那一句
    // （相邻重叠触发 `seg.end <= prev.end + 0.15` 而 continue 时，它的 end 至少
    // 比真实结尾短 0.15s），因此把它的终点拉回到全局结尾即可，不新增片段。
    //
    // 但延展必须有上限：`global_end` 是「全部块的最大结束时间」，若整个末块都被
    // 判成重复而丢弃，`global_end - last.end` 会是整块的跨度（Whisper 目标块长
    // 360s），把一条字幕无上限地拉长几十秒到几分钟。真实漏删只是末尾一两句
    // （见 `TAIL_EXTEND_MAX_SEC`），超过上限就说明不是「漏了最后一句」而是末块
    // 整体失效，此时保留原 `end` 更诚实（不伪造时间轴），交给下游/人工处理。
    if let Some(last) = out.last_mut() {
        let extension = global_end - last.end;
        if extension > 0.05 && extension <= TAIL_EXTEND_MAX_SEC {
            last.end = global_end;
        }
    }
    for (i, seg) in out.iter_mut().enumerate() {
        seg.index = i + 1;
    }
    out
}

fn text_similar(a: &str, b: &str) -> bool {
    let a = a.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let b = b.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    long.contains(short.as_str()) && short.chars().count() >= 4
}

/// 单块转写回调：传入（切片 WAV 路径, 该块时长秒, 建议线程数），
/// 返回块内相对时间戳的片段与（可选的）该块 VAD 耗时。
pub type ChunkTranscriber<'a> =
    dyn Fn(&Path, f64, u32) -> Result<(Vec<Segment>, f64)> + Send + Sync + 'a;

/// 单块转写结果：(块下标, 块起始秒, 块内片段, 该块 VAD 耗时秒)。
type ChunkResult = (usize, f64, Vec<Segment>, f64);

/// 通用切块并行执行器：切片 → 多 worker 并行转写 → 去重叠拼接。
///
/// 与具体引擎解耦，Whisper 与 SenseVoice 共用同一套切块 / 拼接 / 进度逻辑，
/// 保证两者的重叠去重与时间轴还原行为完全一致。
#[allow(clippy::too_many_arguments)]
pub fn run_chunked_parallel(
    ffmpeg: &FFmpegEngine,
    wav_path: &Path,
    chunks: Vec<AudioChunk>,
    workers: usize,
    per_proc_threads: u32,
    progress_cb: Option<crate::engines::SegmentProgressCb>,
    transcribe_one: &ChunkTranscriber<'_>,
) -> Result<(Vec<Segment>, f64)> {
    let workers = workers.max(1);
    info!(
        chunks = chunks.len(),
        workers, per_proc_threads, "切块并行转写启动"
    );

    let work_dir_path = std::env::temp_dir().join(format!(
        "v2w_chunks_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    // 切块目录用守卫持有：切片阶段任何一个 `slice_wav(...)?` 失败都会直接返回，
    // 而删除代码写在函数末尾，于是整个目录（每块 30~60s 的 16kHz WAV，可达数百 MB）
    // 就永久留在 TEMP 里。守卫让它在任意出口（含切片失败、worker panic）都被递归删掉。
    let mut work_dir_guard = TempPathGuard::dir(&work_dir_path);
    let work_dir = work_dir_path.as_path();
    std::fs::create_dir_all(work_dir)?;

    let mut slice_paths: Vec<(AudioChunk, PathBuf)> = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let out = work_dir.join(format!("c{}.wav", chunk.index));
        // 带上块号与起止时间：切片失败（源 WAV 损坏 / 时长与实测不符）时，光看
        // ffmpeg 的原始报错分不清是哪一块，也没法对照是「切到片尾之外」还是别的。
        ffmpeg
            .slice_wav(wav_path, chunk.start, chunk.duration, out.as_path())
            .with_context(|| {
                format!(
                    "切分第 {} 块失败 (start={:.2}s, duration={:.2}s)",
                    chunk.index, chunk.start, chunk.duration
                )
            })?;
        slice_paths.push((*chunk, out));
    }

    let progress_cb = progress_cb.map(Arc::new);
    let done = Arc::new(AtomicUsize::new(0));
    let total_chunks = slice_paths.len();

    let pool = rayon_pool(workers)?;
    let results: Vec<Result<ChunkResult>> = pool.install(|| {
        use rayon::prelude::*;
        slice_paths
            .par_iter()
            .map(|(chunk, path)| {
                let cb = progress_cb.clone();
                let done = done.clone();
                let (segs, chunk_vad_sec) = transcribe_one(path, chunk.duration, per_proc_threads)?;
                let finished = done.fetch_add(1, Ordering::SeqCst) + 1;
                let ratio = finished as f64 / total_chunks as f64;
                if let Some(cb) = cb.as_ref() {
                    for mut seg in segs.iter().cloned() {
                        seg.start += chunk.start;
                        seg.end += chunk.start;
                        cb(
                            ratio,
                            &format!(
                                "块 {}/{} 完成 · {}s",
                                finished, total_chunks, seg.end as u32
                            ),
                            Some(seg),
                        );
                    }
                    if segs.is_empty() {
                        cb(
                            ratio,
                            &format!("块 {}/{} 无语音", finished, total_chunks),
                            None,
                        );
                    }
                }
                Ok((chunk.index, chunk.start, segs, chunk_vad_sec))
            })
            .collect()
    });

    let mut parts = Vec::new();
    let mut total_vad_sec = 0.0;
    for item in results {
        match item {
            Ok((idx, start, segs, vad_sec)) => {
                parts.push((idx, start, segs));
                total_vad_sec += vad_sec;
            }
            Err(err) => {
                warn!(error = %err, "切块转写失败");
                // 失败即清理：守卫在 return 时也会兜底，这里提前删是为了尽快释放磁盘
                work_dir_guard.remove_now();
                return Err(err);
            }
        }
    }

    // 全部切片已转写完，立刻删掉整个切块目录释放磁盘
    work_dir_guard.remove_now();
    let mut merged = stitch_chunks(parts);
    // 与整段路径（`whisper.transcribe_with_model` / `sensevoice.transcribe` 末尾）
    // 同一个收尾：切块路径此前跳过了 `optimize_segments`，于是「并行」与「整段」
    // 两种模式产出的字幕粒度不一致——并行模式下跨块拼起来的超长句不会被拆到
    // 6 秒内、短句也不平滑，同一台机器换个开关就换一套字幕形态。这里补齐，
    // 让两条路径的最终字幕同形（该函数幂等，管线若再调一次结果不变）。
    crate::subtitle::optimize_segments(&mut merged);
    info!(segments = merged.len(), total_vad_sec, "切块字幕已拼接");
    if let Some(cb) = progress_cb {
        cb(1.0, &format!("转写完成，共 {} 个片段", merged.len()), None);
    }
    Ok((merged, total_vad_sec))
}

/// SenseVoice 长音频多进程切块并行。
///
/// 实测（16 核 AMD、6 分钟样本）：单进程 8 线程 32.6s、16 线程反而 41.6s；而
/// 6 进程 × 2 线程仅需 14.5s。原因是 sherpa-onnx 单会话的 ONNX intra-op 线程
/// 扩展性很差（线程越多越慢），进程级并行才能真正吃满多核。因此长音频改为
/// 按块切分、每块独立起一个 runner 进程，再按时间戳拼接。
// 参数是「引擎 + 输入 + 线程/并行提示 + 进度回调」的完整既有签名，调用点分布在
// `core/pipeline.rs` 与集成测试；拆成参数结构体属于超出本轮范围的接口改动。
#[allow(clippy::too_many_arguments)]
pub fn transcribe_chunked_sensevoice(
    ffmpeg: &FFmpegEngine,
    sensevoice: &SenseVoiceEngine,
    wav_path: &Path,
    total_duration: f64,
    language: Option<&str>,
    threads_hint: u32,
    // 用户在设置页显式指定的并行进程数；None = 按核数自动推导
    workers_override: Option<usize>,
    progress_cb: Option<crate::engines::SegmentProgressCb>,
) -> Result<(Vec<Segment>, f64)> {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4) as u32;
    // 每 2 个核心一个 worker，上限 6 个进程（实测 16 核下 6 进程 × 2 线程最优）；
    // 用户在设置页显式指定时以其为准，但不超过实测上限。
    let workers = match workers_override {
        Some(w) if w >= 1 => w.clamp(1, MAX_SV_WORKERS),
        _ => sensevoice_worker_count(cores),
    };
    // 单会话线程数固定 2：实测每进程 2 线程的吞吐优于 4 线程（线程越多单会话越慢），
    // 多进程才是提升方向。总占用 = workers × 2，16 核下 12 线程，留余量给 UI 与系统。
    let per_proc_threads = 2u32;

    // 单进程时切块只会反复冷启动加载模型，比整段识别更慢，直接走整段路径
    if workers <= 1 {
        return sensevoice.transcribe(
            wav_path,
            language,
            Some(threads_hint.clamp(2, 8)),
            Some(total_duration),
            progress_cb,
        );
    }

    // 块数取 worker 数的整数倍，保证每一轮都满载（否则最后一轮只有少数 worker
    // 在跑、其余空转）；同时保证单块不超过 360s，避免单块过长拖尾。
    let mut chunk_count = workers;
    while total_duration / chunk_count as f64 > CHUNK_SEC {
        chunk_count += workers;
    }
    let target = total_duration / chunk_count as f64;
    let chunks = plan_chunks_target(total_duration, target);

    if chunks.len() <= 1 {
        // 音频太短：单进程更划算（省去多份模型加载），线程数按实测饱和点收敛
        return sensevoice.transcribe(
            wav_path,
            language,
            Some(threads_hint.clamp(2, 8)),
            Some(total_duration),
            progress_cb,
        );
    }

    let lang = language.map(|s| s.to_string());
    let transcribe_one = |path: &Path, dur: f64, th: u32| -> Result<(Vec<Segment>, f64)> {
        sensevoice.transcribe(path, lang.as_deref(), Some(th), Some(dur), None)
    };
    // 实际块数可能少于 worker 数：`plan_chunks_target` 在多数情况下会按 target 重算
    // 块数（target 恰好等于 total/chunk_count 时通常保持一致，但短音频被 `MIN_CHUNK_SEC`
    // 兜成单块、或 target 被下限抬高时块数会变少）。多出来的线程只会空转，且进度
    // 分母取的是实际块数，因此这里收敛到实际块数。
    let workers = workers.min(chunks.len());
    // 收尾的 `optimize_segments` 已在 `run_chunked_parallel` 内统一执行
    run_chunked_parallel(
        ffmpeg,
        wav_path,
        chunks,
        workers,
        per_proc_threads,
        progress_cb,
        &transcribe_one,
    )
}

// 同上：参数逐层透传到 `run_chunked_parallel`，就地 allow 而不改公开签名。
#[allow(clippy::too_many_arguments)]
pub fn transcribe_chunked(
    ffmpeg: &FFmpegEngine,
    whisper: &WhisperEngine,
    wav_path: &Path,
    total_duration: f64,
    language: Option<&str>,
    threads: Option<u32>,
    model_override: Option<&Path>,
    use_gpu: bool,
    // 用户在设置页显式指定的并行进程数；None = 按核数自动推导
    workers_override: Option<usize>,
    progress_cb: Option<crate::engines::SegmentProgressCb>,
) -> Result<(Vec<Segment>, f64)> {
    // GPU 单进程切块只会反复加载模型并丢掉跨块上下文，又慢又不准。
    if use_gpu || total_duration <= CHUNK_SEC + MIN_CHUNK_SEC {
        return whisper.transcribe_with_model(
            wav_path,
            language,
            threads,
            Some(total_duration),
            model_override,
            progress_cb,
        );
    }

    let chunks = plan_chunks(total_duration);
    if chunks.len() <= 1 {
        return whisper.transcribe_with_model(
            wav_path,
            language,
            threads,
            Some(total_duration),
            model_override,
            progress_cb,
        );
    }

    let workers = match workers_override {
        Some(w) if w >= 1 => w.min(chunks.len()),
        _ => worker_count(chunks.len(), threads.unwrap_or(8), use_gpu),
    };
    // 单进程时切块只会反复冷启动加载模型，比整段识别更慢，直接走整段路径
    if workers <= 1 {
        return whisper.transcribe_with_model(
            wav_path,
            language,
            threads,
            Some(total_duration),
            model_override,
            progress_cb,
        );
    }
    let per_proc_threads = if use_gpu {
        threads.unwrap_or(8).max(4)
    } else {
        (threads.unwrap_or(8) / workers as u32).max(4)
    };
    let lang = language.map(|s| s.to_string());
    let model = model_override.map(|p| p.to_path_buf());
    let transcribe_one = |path: &Path, dur: f64, th: u32| -> Result<(Vec<Segment>, f64)> {
        whisper.transcribe_with_model_single_processor(
            path,
            lang.as_deref(),
            Some(th),
            Some(dur),
            model.as_deref(),
            None,
        )
    };
    let (merged, total_vad_sec) = run_chunked_parallel(
        ffmpeg,
        wav_path,
        chunks,
        workers,
        per_proc_threads,
        progress_cb,
        &transcribe_one,
    )?;
    // 收尾的 `optimize_segments` 已在 `run_chunked_parallel` 内统一执行
    Ok((merged, total_vad_sec))
}

fn rayon_pool(workers: usize) -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(workers.max(1))
        .thread_name(|i| format!("v2w-whisper-{i}"))
        .build()
        .with_context(|| format!("创建 Whisper 切块线程池失败 (workers={workers})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_audio_is_single_chunk() {
        let c = plan_chunks(120.0);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].start, 0.0);
    }

    #[test]
    fn long_audio_is_chunked_with_overlap() {
        let c = plan_chunks(32.0 * 60.0);
        assert!(c.len() >= 5, "32min -> {:?}", c.len());
        assert!(c[1].start < 32.0 * 60.0 / c.len() as f64);
        let longest = c.iter().map(|x| x.duration).fold(0.0, f64::max);
        let shortest = c.iter().map(|x| x.duration).fold(f64::INFINITY, f64::min);
        assert!(
            longest - shortest < OVERLAP_SEC + 0.1,
            "unbalanced chunks: {:?}",
            c
        );
        let last = c.last().unwrap();
        assert!((last.start + last.duration - 32.0 * 60.0).abs() < 0.05);
    }

    #[test]
    fn stitch_drops_overlap_duplicate() {
        let a = vec![Segment::new(1, 350.0, 358.0, "切比雪夫不等式")];
        let b = vec![Segment::new(1, 0.2, 8.0, "切比雪夫不等式")];
        let out = stitch_chunks(vec![(0, 0.0, a), (1, 358.5, b)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "切比雪夫不等式");
    }

    /// 末块唯一一条句子因「与上一块尾部重复」被判丢时，音频的真实结尾不能一起丢：
    /// 末条已接受片段的终点要拉回到全部块的真实结束时间（不新增片段）。
    #[test]
    fn stitch_recovers_media_end_when_tail_duplicate_is_dropped() {
        let head = vec![Segment::new(1, 250.0, 300.0, "切比雪夫不等式")];
        // 末块（偏移 300s）只剩这一句，文本与上一块尾部相同：按重叠去重规则会被丢弃
        let tail = vec![Segment::new(1, 0.1, 8.0, "切比雪夫不等式")];
        let out = stitch_chunks(vec![(0, 0.0, head), (1, 300.0, tail)]);
        assert_eq!(out.len(), 1, "重复句仍应被丢掉: {out:?}");
        assert_eq!(out[0].text, "切比雪夫不等式");
        assert!(
            (out[0].end - 308.0).abs() < 1e-9,
            "末条应延展到音频真实结尾，而不是停在 300s: {out:?}"
        );
    }

    /// 上限内的正常兜底：末尾被误丢的一句只有几秒时仍应补回真实结尾。
    #[test]
    fn stitch_tail_recovery_within_cap_extends() {
        let head = vec![Segment::new(1, 250.0, 300.0, "切比雪夫不等式")];
        let tail = vec![Segment::new(1, 0.1, 4.0, "切比雪夫不等式")];
        let out = stitch_chunks(vec![(0, 0.0, head), (1, 300.0, tail)]);
        assert_eq!(out.len(), 1);
        assert!(
            (out[0].end - 304.0).abs() < 1e-9,
            "上限内的漏删仍应补回结尾: {out:?}"
        );
    }

    /// 末段兜底必须封顶：整块被判为重复丢弃时 `global_end - last.end` 会接近一个
    /// 整块（Whisper 360s），旧实现会把一条字幕直接拉长几十秒。超限时应保留原 end。
    #[test]
    fn stitch_tail_recovery_is_capped() {
        let head = vec![Segment::new(1, 250.0, 300.0, "切比雪夫不等式")];
        // 末块内唯一的句子长 30s，整体被判为与上一块尾部重复而丢弃：
        // 不封顶时 out[0].end 会被拉到 330s（单条字幕凭空多出 30s）。
        let tail = vec![Segment::new(1, 0.1, 30.0, "切比雪夫不等式")];
        let out = stitch_chunks(vec![(0, 0.0, head), (1, 300.0, tail)]);
        assert_eq!(out.len(), 1, "重复句仍应被丢掉: {out:?}");
        assert!(
            (out[0].end - 300.0).abs() < 1e-9,
            "延展超限时必须保留原 end，而不是把单条字幕拉长: {out:?}"
        );
    }

    /// 反向断言：末块是不同的新句、整体落在上一块之后时必须原样保留，
    /// 不能被去重规则误删、也不该被上一条兜底逻辑吞掉。
    #[test]
    fn stitch_keeps_distinct_tail_segment() {
        let head = vec![Segment::new(1, 300.0, 358.5, "前面这半句话")];
        let tail = vec![Segment::new(1, 0.1, 4.0, "后面这半句是新的内容")];
        let out = stitch_chunks(vec![(0, 0.0, head), (1, 358.5, tail)]);
        assert_eq!(out.len(), 2, "不同文本的新句不得被去重规则误删: {out:?}");
        assert!(out[1].start > out[0].end, "新句应落在上一句之后: {out:?}");
    }

    #[test]
    fn cpu_workers_keep_eight_threads_per_process() {
        assert_eq!(worker_count(6, 8, false), 2);
        assert_eq!(worker_count(6, 16, false), 2);
        assert_eq!(worker_count(6, 32, false), 4);
    }

    #[test]
    fn target_chunk_len_controls_chunk_count() {
        // 32 分钟样片：360s 目标 -> 6 块；更短的目标块长应切出更多块
        assert_eq!(plan_chunks_target(1942.0, 360.0).len(), 6);
        assert!(plan_chunks_target(1942.0, 150.0).len() >= 12);
        // 太短的音频仍是单块
        assert_eq!(plan_chunks_target(60.0, 45.0).len(), 1);
    }

    #[test]
    fn target_chunks_cover_timeline_and_overlap() {
        let chunks = plan_chunks_target(1000.0, 120.0);
        assert_eq!(chunks[0].start, 0.0);
        let last = chunks.last().unwrap();
        assert!(
            (last.start + last.duration - 1000.0).abs() < 0.05,
            "末块应贴住片尾"
        );
        for w in chunks.windows(2) {
            assert!(
                w[0].start + w[0].duration > w[1].start,
                "相邻块必须重叠，否则边界语音会被切断"
            );
        }
    }

    /// 4 小时长音频：步长按实际块数均分，末块必须贴住片尾、相邻块严格重叠，
    /// 否则后半段音频会无人转写。
    #[test]
    fn plan_chunks_target_long_audio_still_covers_tail() {
        let chunks = plan_chunks_target(4.0 * 3600.0, 45.0);
        let last = chunks.last().unwrap();
        assert!(
            (last.start + last.duration - 4.0 * 3600.0).abs() < 0.05,
            "末块必须贴住片尾，否则后半段音频无人转写: {last:?}"
        );
        for w in chunks.windows(2) {
            assert!(
                w[0].start + w[0].duration > w[1].start,
                "相邻块必须重叠，否则边界语音会被切断"
            );
        }
    }

    /// 拼接结果里不得残留相邻的「时间几乎重叠」字幕：这是块边界被同一句切两半时
    /// 最常见的形态，也是下游 `optimize_segments` 会当成鬼影剔掉的那一类。
    #[test]
    fn stitch_output_has_no_overlapping_neighbours() {
        let a = vec![
            Segment::new(1, 0.0, 3.0, "第一句内容"),
            Segment::new(2, 3.0, 8.0, "第二句内容比较长"),
        ];
        let b = vec![
            Segment::new(1, 0.1, 4.0, "第三句内容"),
            Segment::new(2, 4.0, 9.0, "第四句内容比较长"),
        ];
        let out = stitch_chunks(vec![(0, 0.0, a), (1, 358.0, b)]);
        for w in out.windows(2) {
            assert!(w[0].end <= w[1].start + 1e-9, "拼接后仍存在交叉区间: {w:?}");
        }
        let last = out.last().unwrap();
        assert!(
            (last.end - 367.0).abs() < 0.6,
            "末句终点应接近音频真实结尾（367s）: {last:?}"
        );
    }

    fn write_silent_wav(path: &Path, seconds: u32, rate: u32) {
        let samples = seconds * rate;
        let data_len = samples * 2;
        let mut buf = Vec::with_capacity(44 + data_len as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(36 + data_len).to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&rate.to_le_bytes());
        buf.extend_from_slice(&(rate * 2).to_le_bytes());
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(&16u16.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.resize(44 + data_len as usize, 0);
        std::fs::write(path, &buf).unwrap();
    }

    /// 通用切块执行器：用假转写器验证时间轴偏移、去重叠拼接与重新编号。
    /// 仅切片需要真实 ffmpeg，缺失时自动跳过。
    #[test]
    fn run_chunked_parallel_offsets_and_stitches() {
        let cfg: toml::Value = std::fs::read_to_string("config.toml")
            .ok()
            .and_then(|c| toml::from_str(&c).ok())
            .unwrap_or_else(|| toml::Value::Table(Default::default()));
        let Some(ffmpeg_path) = cfg["paths"]["ffmpeg"].as_str() else {
            eprintln!("跳过：config.toml 未配置 ffmpeg");
            return;
        };
        if !Path::new(ffmpeg_path).exists() {
            eprintln!("跳过：ffmpeg 不存在");
            return;
        }

        let wav = std::env::temp_dir().join("v2w_test_chunk_src.wav");
        write_silent_wav(&wav, 100, 16000);
        // 与泄漏用例串行：本用例调用期间会新建 `v2w_chunks_*` 目录，若与泄漏用例的
        // 前后快照窗口重叠，会让对方把这里的目录误判成自己的残留。
        let _serial = chunk_test_guard();

        let ffmpeg = FFmpegEngine::new(ffmpeg_path);
        let chunks = plan_chunks_target(100.0, 50.0);
        assert_eq!(chunks.len(), 2);

        let calls = std::sync::atomic::AtomicUsize::new(0);
        let transcribe_one = |p: &Path, dur: f64, _th: u32| -> Result<(Vec<Segment>, f64)> {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            assert!(p.exists(), "传给转写器的切片必须已落在磁盘上: {:?}", p);
            assert!(dur > 0.0, "块时长必须为正: {dur}");
            // 每块都返回一个块内 0~1s 的片段，用于验证外部偏移是否正确施加
            Ok((vec![Segment::new(1, 0.0, 1.0, format!("块{n}"))], 0.0))
        };
        let (segs, _vad) = run_chunked_parallel(&ffmpeg, &wav, chunks, 2, 2, None, &transcribe_one)
            .expect("切块并行应成功");

        assert_eq!(segs.len(), 2, "两块各产出一条片段: {segs:?}");
        assert!((segs[0].start - 0.0).abs() < 1e-6);
        assert!(
            segs[1].start > 45.0,
            "第二块应被偏移到时间轴后段: {}",
            segs[1].start
        );
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[1].index, 2, "拼接后应重新连续编号");

        let _ = std::fs::remove_file(&wav);
    }

    /// 执行器收尾必须与整段路径同形：跨块拼出的超长句要在 `run_chunked_parallel`
    /// 出口就被拆到 6 秒内。此前切块路径整个跳过了 `optimize_segments`，
    /// 于是「开并行」与「不开并行」得到两套不同粒度的字幕。
    #[test]
    fn run_chunked_parallel_optimizes_merged_output() {
        let cfg: toml::Value = std::fs::read_to_string("config.toml")
            .ok()
            .and_then(|c| toml::from_str(&c).ok())
            .unwrap_or_else(|| toml::Value::Table(Default::default()));
        let Some(ffmpeg_path) = cfg["paths"]["ffmpeg"].as_str() else {
            eprintln!("跳过：config.toml 未配置 ffmpeg");
            return;
        };
        if !Path::new(ffmpeg_path).exists() {
            eprintln!("跳过：ffmpeg 不存在");
            return;
        }

        let wav =
            std::env::temp_dir().join(format!("v2w_test_chunk_opt_{}.wav", std::process::id()));
        write_silent_wav(&wav, 100, 16000);
        let _serial = chunk_test_guard();

        let ffmpeg = FFmpegEngine::new(ffmpeg_path);
        let chunks = plan_chunks_target(100.0, 50.0);
        assert_eq!(chunks.len(), 2);
        // 带标点的 10 秒长句：`optimize_segments` 应把它拆成 6 秒以内的多条
        let long = "前面这半句讲的是背景，后面这半句讲的是结论。";
        let transcribe_one = |_p: &Path, _dur: f64, _th: u32| -> Result<(Vec<Segment>, f64)> {
            Ok((vec![Segment::new(1, 0.0, 10.0, long)], 0.0))
        };
        let (segs, _vad) = run_chunked_parallel(&ffmpeg, &wav, chunks, 2, 2, None, &transcribe_one)
            .expect("切块并行应成功");

        assert!(segs.len() >= 2, "10 秒长句应在执行器出口被拆短: {segs:?}");
        for s in &segs {
            assert!(s.duration() <= 6.0 + 1e-9, "执行器出口仍残留超长句: {s:?}");
        }
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[1].index, 2, "拆分后应重新连续编号");

        let _ = std::fs::remove_file(&wav);
    }

    /// 本模块内所有会调用 `run_chunked_parallel` 的用例共用一把锁。
    ///
    /// 该函数会在 `%TEMP%` 下按 `v2w_chunks_{pid}_{millis}` 建目录，而泄漏用例靠
    /// 「调用前后目录集合的差」判定残留。若两个用例并行，兄弟用例在这段窗口里新建的
    /// 目录会被算成泄漏用例的残留（假失败）。串行化后，窗口内只可能留下本用例自己
    /// 创建的目录，集合差才真正等价于「本用例是否泄漏」。
    static CHUNK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 取得串行锁；前一个用例若 panic 会毒化锁，这里恢复内部值以免连累后续用例。
    fn chunk_test_guard() -> std::sync::MutexGuard<'static, ()> {
        CHUNK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 快照 `%TEMP%` 下「本进程」创建的切块目录名，用于断言「失败后没有新增残留」。
    ///
    /// 过滤 `v2w_chunks_{pid}_` 前缀：其它测试二进制（各自独立进程）并发跑时也会建
    /// 同前缀目录，但 pid 不同，据此把它们排除，只对本进程的目录负责。
    fn chunk_dirs_in_temp() -> std::collections::HashSet<String> {
        let prefix = format!("v2w_chunks_{}_", std::process::id());
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return Default::default();
        };
        entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect()
    }

    /// 验证泄漏修复：切片已建好、转写中途失败时，切块临时目录必须被清掉。
    ///
    /// 这条路径过去会直接 `return Err(err)` 跳过函数末尾的 `remove_dir_all`，
    /// 把整个目录（每块数十 MB 的 WAV）永久留在 TEMP 里。
    #[test]
    fn chunk_failure_leaves_no_temp_dir() {
        let cfg: toml::Value = std::fs::read_to_string("config.toml")
            .ok()
            .and_then(|c| toml::from_str(&c).ok())
            .unwrap_or_else(|| toml::Value::Table(Default::default()));
        let Some(ffmpeg_path) = cfg["paths"]["ffmpeg"].as_str() else {
            eprintln!("跳过：config.toml 未配置 ffmpeg");
            return;
        };
        if !Path::new(ffmpeg_path).exists() {
            eprintln!("跳过：ffmpeg 不存在");
            return;
        }

        let wav =
            std::env::temp_dir().join(format!("v2w_test_chunk_fail_{}.wav", std::process::id()));
        write_silent_wav(&wav, 100, 16000);
        let ffmpeg = FFmpegEngine::new(ffmpeg_path);
        let chunks = plan_chunks_target(100.0, 50.0);

        // 只比较「调用后新增」的目录：本模块其它用例也会建 `v2w_chunks_*`，所以先拿
        // 串行锁保证窗口内没有别的用例在并发建目录，再按本进程 pid 过滤，把集合差
        // （after - before 必须为空）限定在「本次调用自己可能创建的目录」上。
        let _serial = chunk_test_guard();
        let before = chunk_dirs_in_temp();
        let failing = |_p: &Path, _dur: f64, _th: u32| -> Result<(Vec<Segment>, f64)> {
            anyhow::bail!("模拟块转写失败")
        };
        let res = run_chunked_parallel(&ffmpeg, &wav, chunks, 2, 2, None, &failing);
        let after = chunk_dirs_in_temp();

        assert!(res.is_err(), "转写器返回 Err 时执行器必须把错误上抛");
        let leaked: Vec<&String> = after.difference(&before).collect();
        assert!(
            leaked.is_empty(),
            "失败后切块目录必须被删除，残留: {leaked:?}"
        );

        let _ = std::fs::remove_file(&wav);
    }
}
