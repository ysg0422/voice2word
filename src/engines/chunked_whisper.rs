//! Whisper 切块并行：6 分钟一块、1.5 秒重叠，多进程推理后按时间戳拼接。
//! 课程设计仍走本地 whisper-cli，不换模型栈。

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

use super::{FFmpegEngine, WhisperEngine};
use crate::subtitle::Segment;

const CHUNK_SEC: f64 = 360.0;
const OVERLAP_SEC: f64 = 1.5;
const MIN_CHUNK_SEC: f64 = 45.0;
const DEFAULT_WORKERS: usize = 2;
const MAX_WORKERS: usize = 4;

#[derive(Clone, Copy)]
pub struct AudioChunk {
    pub index: usize,
    pub start: f64,
    pub duration: f64,
}

pub fn plan_chunks(total_duration: f64) -> Vec<AudioChunk> {
    if total_duration <= CHUNK_SEC + MIN_CHUNK_SEC {
        return vec![AudioChunk {
            index: 0,
            start: 0.0,
            duration: total_duration.max(0.1),
        }];
    }
    let mut chunks = Vec::new();
    let mut start = 0.0;
    let mut index = 0usize;
    while start < total_duration - 0.05 {
        let remaining = total_duration - start;
        let duration = if remaining <= CHUNK_SEC + MIN_CHUNK_SEC {
            remaining
        } else {
            CHUNK_SEC
        };
        chunks.push(AudioChunk {
            index,
            start,
            duration,
        });
        index += 1;
        if start + duration >= total_duration - 0.05 {
            break;
        }
        start += CHUNK_SEC - OVERLAP_SEC;
    }
    chunks
}

pub fn worker_count(chunk_n: usize, requested_threads: u32, use_gpu: bool) -> usize {
    if chunk_n <= 1 {
        return 1;
    }
    // 核显/单 GPU 上两个 whisper-cli 会抢同一块显存，识别直接变糊。
    if use_gpu {
        return 1;
    }
    let by_cpu = (requested_threads.max(4) / 4) as usize;
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

pub fn transcribe_chunked(
    ffmpeg: &FFmpegEngine,
    whisper: &WhisperEngine,
    wav_path: &Path,
    total_duration: f64,
    language: Option<&str>,
    threads: Option<u32>,
    model_override: Option<&Path>,
    use_gpu: bool,
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

    let workers = worker_count(chunks.len(), threads.unwrap_or(8), use_gpu);
    info!(
        chunks = chunks.len(),
        workers,
        duration = total_duration,
        "Whisper 切块并行转写"
    );

    let work_dir = std::env::temp_dir().join(format!(
        "v2w_chunks_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&work_dir)?;

    let mut slice_paths: Vec<(AudioChunk, PathBuf)> = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let out = work_dir.join(format!("c{}.wav", chunk.index));
        ffmpeg.slice_wav(wav_path, chunk.start, chunk.duration, out.as_path())?;
        slice_paths.push((*chunk, out));
    }

    let progress_cb = progress_cb.map(Arc::new);
    let done = Arc::new(AtomicUsize::new(0));
    let total_chunks = slice_paths.len();
    let lang = language.map(|s| s.to_string());
    let model = model_override.map(|p| p.to_path_buf());
    let per_proc_threads = if use_gpu {
        threads.unwrap_or(8).max(4)
    } else {
        (threads.unwrap_or(8) / workers as u32).max(4)
    };

    let pool = rayon_pool(workers);
    let results: Vec<Result<(usize, f64, Vec<Segment>, f64)>> = pool.install(|| {
        use rayon::prelude::*;
        slice_paths
            .par_iter()
            .map(|(chunk, path)| {
                let cb = progress_cb.clone();
                let done = done.clone();
                let lang_ref = lang.as_deref();
                let model_ref = model.as_deref();
                let (segs, chunk_vad_sec) = whisper.transcribe_with_model(
                    path,
                    lang_ref,
                    Some(per_proc_threads),
                    Some(chunk.duration),
                    model_ref,
                    None,
                )?;
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
                let _ = std::fs::remove_dir_all(&work_dir);
                return Err(err);
            }
        }
    }

    let _ = std::fs::remove_dir_all(&work_dir);
    let merged = stitch_chunks(parts);
    info!(segments = merged.len(), total_vad_sec, "切块字幕已拼接");
    if let Some(cb) = progress_cb {
        cb(1.0, &format!("转写完成，共 {} 个片段", merged.len()), None);
    }
    Ok((merged, total_vad_sec))
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
        assert!(c[1].start < CHUNK_SEC);
        assert!((c[1].start - (CHUNK_SEC - OVERLAP_SEC)).abs() < 0.01);
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
}
