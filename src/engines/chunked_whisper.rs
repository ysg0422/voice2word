//! Whisper 切块并行：6 分钟一块、1.5 秒重叠，多进程推理后按时间戳拼接。
//! 课程设计仍走本地 whisper-cli，不换模型栈。

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

use super::{FFmpegEngine, SenseVoiceEngine, WhisperEngine};
use crate::subtitle::Segment;
use crate::utils::TempPathGuard;

const CHUNK_SEC: f64 = 360.0;
const OVERLAP_SEC: f64 = 1.5;
const MIN_CHUNK_SEC: f64 = 45.0;
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

pub fn worker_count(chunk_n: usize, requested_threads: u32, use_gpu: bool) -> usize {    if chunk_n <= 1 {
        return 1;
    }
    // 核显/单 GPU 上两个 whisper-cli 会抢同一块显存，识别直接变糊。
    if use_gpu {
        return 1;
    }
    // 每个 whisper-cli 进程至少给 8 个线程，避免 16 线程机器被拆成
    // 4 个 4 线程进程后互相争抢内存带宽。超过 16 线程再增加进程数。
    let by_cpu = (requested_threads.max(8) / 8) as usize;
    by_cpu.clamp(2, MAX_WORKERS).min(chunk_n).max(DEFAULT_WORKERS.min(chunk_n))
}

/// 去掉切块重叠区里的重复句：后一块落在上一块尾部的字幕丢弃。
pub fn stitch_chunks(mut parts: Vec<(usize, f64, Vec<Segment>)>) -> Vec<Segment> {
    parts.sort_by_key(|(idx, _, _)| *idx);
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
    let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    long.contains(short.as_str()) && short.chars().count() >= 4
}

/// 单块转写回调：传入（切片 WAV 路径, 该块时长秒, 建议线程数），
/// 返回块内相对时间戳的片段与（可选的）该块 VAD 耗时。
pub type ChunkTranscriber<'a> =
    dyn Fn(&Path, f64, u32) -> Result<(Vec<Segment>, f64)> + Send + Sync + 'a;

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
    progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    transcribe_one: &ChunkTranscriber<'_>,
) -> Result<(Vec<Segment>, f64)> {
    let workers = workers.max(1);
    info!(
        chunks = chunks.len(),
        workers,
        per_proc_threads,
        "切块并行转写启动"
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
        ffmpeg.slice_wav(wav_path, chunk.start, chunk.duration, out.as_path())?;
        slice_paths.push((*chunk, out));
    }

    let progress_cb = progress_cb.map(Arc::new);
    let done = Arc::new(AtomicUsize::new(0));
    let total_chunks = slice_paths.len();

    let pool = rayon_pool(workers);
    let results: Vec<Result<(usize, f64, Vec<Segment>, f64)>> = pool.install(|| {
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
                                finished,
                                total_chunks,
                                seg.end as u32
                            ),
                            Some(seg),
                        );
                    }
                    if segs.is_empty() {
                        cb(ratio, &format!("块 {}/{} 无语音", finished, total_chunks), None);
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
    let merged = stitch_chunks(parts);
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
pub fn transcribe_chunked_sensevoice(
    ffmpeg: &FFmpegEngine,
    sensevoice: &SenseVoiceEngine,
    wav_path: &Path,
    total_duration: f64,
    language: Option<&str>,
    threads_hint: u32,
    // 用户在设置页显式指定的并行进程数；None = 按核数自动推导
    workers_override: Option<usize>,
    progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
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
    progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
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

fn rayon_pool(workers: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(workers.max(1))
        .thread_name(|i| format!("v2w-whisper-{i}"))
        .build()
        .expect("创建 Whisper 切块线程池失败")
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
        assert!(longest - shortest < OVERLAP_SEC + 0.1, "unbalanced chunks: {:?}", c);
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
        assert!((last.start + last.duration - 1000.0).abs() < 0.05, "末块应贴住片尾");
        for w in chunks.windows(2) {
            assert!(
                w[0].start + w[0].duration > w[1].start,
                "相邻块必须重叠，否则边界语音会被切断"
            );
        }
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
        let transcribe_one = |_p: &Path, _dur: f64, _th: u32| -> Result<(Vec<Segment>, f64)> {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            // 每块都返回一个块内 0~1s 的片段，用于验证外部偏移是否正确施加
            Ok((vec![Segment::new(1, 0.0, 1.0, format!("块{n}"))], 0.0))
        };
        let (segs, _vad) =
            run_chunked_parallel(&ffmpeg, &wav, chunks, 2, 2, None, &transcribe_one)
                .expect("切块并行应成功");

        assert_eq!(segs.len(), 2, "两块各产出一条片段: {segs:?}");
        assert!((segs[0].start - 0.0).abs() < 1e-6);
        assert!(segs[1].start > 45.0, "第二块应被偏移到时间轴后段: {}", segs[1].start);
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[1].index, 2, "拼接后应重新连续编号");

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

        let wav = std::env::temp_dir().join(format!("v2w_test_chunk_fail_{}.wav", std::process::id()));
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
        assert!(leaked.is_empty(), "失败后切块目录必须被删除，残留: {leaked:?}");

        let _ = std::fs::remove_file(&wav);
    }
}
