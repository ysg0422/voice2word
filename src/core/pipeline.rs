//! 异步任务流水线：调度 FFmpeg -> Whisper -> LLM -> SubtitleWriter

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::engines::{transcribe_chunked, FFmpegEngine, LLMEngine, PunctuationEngine, WhisperEngine};
use crate::subtitle::{Segment, SubtitleWriter};

#[derive(Debug, Clone)]
pub enum PipelineEvent {
    StageChanged(String),
    Progress { stage: String, progress: f64, detail: String },
    SegmentStream(Segment),
    Finished(Vec<Segment>, crate::core::PipelinePerformanceMetrics),
    Error(String),
}

pub struct TaskPipeline {
    ffmpeg: Arc<FFmpegEngine>,
    whisper: Arc<WhisperEngine>,
    llm: Arc<LLMEngine>,
    punc: Option<Arc<PunctuationEngine>>,
    cancelled: Arc<AtomicBool>,
}

impl TaskPipeline {
    pub fn new(
        ffmpeg: Arc<FFmpegEngine>,
        whisper: Arc<WhisperEngine>,
        llm: Arc<LLMEngine>,
        punc: Option<Arc<PunctuationEngine>>,
    ) -> Self {
        Self {
            ffmpeg,
            whisper,
            llm,
            punc,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// 运行完整流水线：提取音频 -> Whisper 转写 -> 标点/LLM 润色 -> 写入字幕
    pub async fn run(
        &self,
        input_file: PathBuf,
        output_file: Option<PathBuf>,
        language: Option<String>,
        output_format: String,
        enable_polish: bool,
        polish_mode: Option<String>,
        threads: Option<u32>,
        model_override: Option<PathBuf>, // 可选：覆盖 whisper 模型路径（供 UI 档位切换传入）
        tx: UnboundedSender<PipelineEvent>,
    ) -> Result<Vec<Segment>> {

        self.cancelled.store(false, Ordering::Relaxed);
        let pipeline_started = Instant::now();

        let out_path = output_file.unwrap_or_else(|| {
            let stem = input_file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("output");
            input_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(format!("{}.{}", stem, output_format))
        });

        // ── 阶段 1: 探测时长与音频管道准备 ──
        if self.is_cancelled() {
            return Ok(Vec::new());
        }
        let _ = tx.send(PipelineEvent::StageChanged("提取音频".into()));
        let _ = tx.send(PipelineEvent::Progress {
            stage: "提取音频".into(),
            progress: 0.1,
            detail: "正在探测媒体时长并准备音频通道...".into(),
        });

        let ffmpeg_for_dur = self.ffmpeg.clone();
        let in_file_for_dur = input_file.clone();
        let total_dur = tokio::task::spawn_blocking(move || {
            let d = ffmpeg_for_dur.get_duration(&in_file_for_dur);
            if d > 0.0 { Some(d) } else { None }
        })
        .await
        .unwrap_or(None);

        let duration_for_chunks = total_dur.unwrap_or(0.0);
        let whisper = self.whisper.clone();
        let ffmpeg = self.ffmpeg.clone();
        let use_gpu = whisper.uses_gpu();

        // 判断是否启用纯内存管道推流 (In-Memory PCM Streaming Pipe):
        // 当启用 GPU 推理，或音频时长无需多进程切块时，直接以 0 磁盘 I/O 管道推流
        let can_stream = use_gpu || duration_for_chunks <= 405.0;

        let mut ffmpeg_audio_sec = 0.0;
        let mut temp_wav_path: Option<PathBuf> = None;

        if can_stream {
            info!("音频通道就绪：启用纯内存管道推流 (In-Memory PCM Streaming Pipe，0 磁盘 I/O)");
            let _ = tx.send(PipelineEvent::Progress {
                stage: "提取音频".into(),
                progress: 1.0,
                detail: "已建立纯内存音频推流管道 (0 磁盘 I/O，毫秒级就绪)".into(),
            });
        } else {
            // CPU 多核切块模式回退：提前抽取完整临时 WAV 以供多进程随机 seek 切块
            let in_file = input_file.clone();
            let ffmpeg_extract = ffmpeg.clone();
            let extraction_started = Instant::now();
            let wav_path = tokio::task::spawn_blocking(move || ffmpeg_extract.extract_audio(&in_file, None))
                .await??;
            ffmpeg_audio_sec = extraction_started.elapsed().as_secs_f64();
            info!(elapsed = ?extraction_started.elapsed(), "FFmpeg 临时 WAV 提取完成 (CPU 切块模式)");
            let _ = tx.send(PipelineEvent::Progress {
                stage: "提取音频".into(),
                progress: 1.0,
                detail: "音频提取成功".into(),
            });
            temp_wav_path = Some(wav_path);
        }

        // ── 阶段 2: 语音转写 ──
        if self.is_cancelled() {
            if let Some(ref p) = temp_wav_path {
                let _ = std::fs::remove_file(p);
            }
            return Ok(Vec::new());
        }

        let _ = tx.send(PipelineEvent::StageChanged("语音识别".into()));
        let _ = tx.send(PipelineEvent::Progress {
            stage: "语音识别".into(),
            progress: 0.15,
            detail: if can_stream {
                "Whisper 正在通过纯内存管道流式转写语音...".into()
            } else {
                "Whisper 模型正在转写语音...".into()
            },
        });

        let lang_clone = language.clone();
        let tx_whisper = tx.clone();
        // 关闭润色时，识别阶段占满进度条，避免卡在旧的 60%「等待润色」区间。
        let whisper_span = if enable_polish { 0.45 } else { 0.80 };

        let transcription_started = Instant::now();
        let (mut segments, vad_sec) = if can_stream {
            let in_file_stream = input_file.clone();
            let ffmpeg_stream = ffmpeg.clone();
            let whisper_engine = whisper.clone();
            let model_override_stream = model_override.clone();

            tokio::task::spawn_blocking(move || -> Result<(Vec<Segment>, f64)> {
                let mut ffmpeg_child = ffmpeg_stream.spawn_audio_stream(&in_file_stream)?;
                let ffmpeg_stdout = ffmpeg_child.stdout.take()
                    .context("获取 FFmpeg 内存管道输出失败")?;

                struct ChildReaper(std::process::Child);
                impl Drop for ChildReaper {
                    fn drop(&mut self) {
                        let _ = self.0.kill();
                        let _ = self.0.wait();
                    }
                }
                let mut reaper = ChildReaper(ffmpeg_child);

                let tx_cb = tx_whisper.clone();
                let res = whisper_engine.transcribe_stream(
                    Box::new(ffmpeg_stdout),
                    lang_clone.as_deref(),
                    threads,
                    if duration_for_chunks > 0.0 { Some(duration_for_chunks) } else { None },
                    model_override_stream.as_deref(),
                    Some(Box::new(move |p, info, opt_seg| {
                        if let Some(seg) = opt_seg {
                            let _ = tx_cb.send(PipelineEvent::SegmentStream(seg));
                        }
                        let _ = tx_cb.send(PipelineEvent::Progress {
                            stage: "语音识别".into(),
                            progress: 0.15 + p * whisper_span,
                            detail: info.to_string(),
                        });
                    })),
                );
                let _ = reaper.0.wait();
                res
            })
            .await??
        } else {
            let wav_path = temp_wav_path.as_ref().unwrap().clone();
            let ffmpeg_chunks = ffmpeg.clone();
            let whisper_engine = whisper.clone();
            let model_override_clone = model_override.clone();

            tokio::task::spawn_blocking(move || {
                transcribe_chunked(
                    ffmpeg_chunks.as_ref(),
                    whisper_engine.as_ref(),
                    &wav_path,
                    duration_for_chunks,
                    lang_clone.as_deref(),
                    threads,
                    model_override_clone.as_deref(),
                    use_gpu,
                    Some(Box::new(move |p, info, opt_seg| {
                        if let Some(seg) = opt_seg {
                            let _ = tx_whisper.send(PipelineEvent::SegmentStream(seg));
                        }
                        let _ = tx_whisper.send(PipelineEvent::Progress {
                            stage: "语音识别".into(),
                            progress: 0.15 + p * whisper_span,
                            detail: info.to_string(),
                        });
                    })),
                )
            })
            .await??
        };

        let total_transcribe_elapsed = transcription_started.elapsed().as_secs_f64();
        let pure_whisper_sec = (total_transcribe_elapsed - vad_sec).max(0.0);
        info!(elapsed = ?transcription_started.elapsed(), vad_sec, pure_whisper_sec, segments = segments.len(), "Whisper 转写完成");

        // 清理临时 wav (仅当存在时清理)
        if let Some(ref p) = temp_wav_path {
            let _ = std::fs::remove_file(p);
        }

        if self.is_cancelled() {
            let _ = tx.send(PipelineEvent::Finished(segments.clone(), Default::default()));
            return Ok(segments);
        }

        // ── 阶段 3: 标点与语法润色（可跳过）──
        let mut qwen_sec = 0.0;
        let mut polish_engine_name = None;
        if enable_polish && !segments.is_empty() {
            let mode = polish_mode.unwrap_or_else(|| "punc".to_string());
            let use_punc = (mode == "punc") && self.punc.as_ref().map(|p| p.is_available()).unwrap_or(false);

            if use_punc {
                polish_engine_name = Some("CT-Punc 极速标点".to_string());
                let _ = tx.send(PipelineEvent::StageChanged("极速标点".into()));
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "极速标点".into(),
                    progress: 0.85,
                    detail: "CT-Transformer 正在毫秒级恢复标点与断句...".into(),
                });

                let punc = self.punc.as_ref().unwrap().clone();
                let segs_clone = segments.clone();
                let tx_punc = tx.clone();
                let punc_started = Instant::now();

                match tokio::task::spawn_blocking(move || {
                    punc.add_punctuation(
                        segs_clone,
                        Some(Box::new(move |p, info| {
                            let _ = tx_punc.send(PipelineEvent::Progress {
                                stage: "极速标点".into(),
                                progress: 0.85 + p * 0.1,
                                detail: info.to_string(),
                            });
                        })),
                    )
                })
                .await
                {
                    Ok(Ok(polished)) => {
                        segments = polished;
                        qwen_sec = punc_started.elapsed().as_secs_f64();
                        info!(elapsed = ?punc_started.elapsed(), segments = segments.len(), "CT-Punc 极速标点完成");
                    }
                    Ok(Err(err)) => {
                        warn!(error = %err, "极速标点失败，保留识别原文并继续收尾");
                    }
                    Err(err) => {
                        warn!(error = %err, "极速标点任务异常，保留识别原文");
                    }
                }
            } else if mode == "qwen" {
                polish_engine_name = Some("Qwen 字幕润色".to_string());
                let _ = tx.send(PipelineEvent::StageChanged("智能润色".into()));
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "智能润色".into(),
                    progress: 0.6,
                    detail: "本地 Qwen 正在优化标点与语法...".into(),
                });

                let llm = self.llm.clone();
                let segs_clone = segments.clone();
                let tx_llm = tx.clone();

                let polishing_started = Instant::now();
                match tokio::task::spawn_blocking(move || {
                    llm.polish(
                        segs_clone,
                        Some(Box::new(move |p, info| {
                            let _ = tx_llm.send(PipelineEvent::Progress {
                                stage: "智能润色".into(),
                                progress: 0.6 + p * 0.35,
                                detail: info.to_string(),
                            });
                        })),
                    )
                })
                .await
                {
                    Ok(Ok(polished)) => {
                        segments = polished;
                        qwen_sec = polishing_started.elapsed().as_secs_f64();
                        info!(elapsed = ?polishing_started.elapsed(), segments = segments.len(), "Qwen 润色完成");
                    }
                    Ok(Err(err)) => {
                        warn!(error = %err, "润色失败，保留识别原文并继续收尾");
                        let _ = tx.send(PipelineEvent::Progress {
                            stage: "智能润色".into(),
                            progress: 0.95,
                            detail: format!("润色失败，已跳过：{err}"),
                        });
                    }
                    Err(err) => {
                        warn!(error = %err, "润色任务崩溃，保留识别原文");
                    }
                }
            } else {
                info!("润色模式为关闭或未知: {}", mode);
            }
        } else {
            let _ = tx.send(PipelineEvent::StageChanged("生成字幕".into()));
            let _ = tx.send(PipelineEvent::Progress {
                stage: "生成字幕".into(),
                progress: 0.96,
                detail: "已关闭 AI 润色，正在写出字幕...".into(),
            });
            info!("已跳过标点润色");
        }

        if self.is_cancelled() {
            let _ = tx.send(PipelineEvent::Finished(segments.clone(), Default::default()));
            return Ok(segments);
        }

        // ── 阶段 4: 输出字幕文件（失败也不阻塞 Finished，避免界面一直转圈）──
        let _ = tx.send(PipelineEvent::StageChanged("生成字幕".into()));
        let writing_started = Instant::now();
        match SubtitleWriter::write_to_file(&segments, &out_path, &output_format) {
            Ok(()) => {
                info!(elapsed = ?writing_started.elapsed(), "字幕文件写入完成");
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "生成字幕".into(),
                    progress: 1.0,
                    detail: format!("字幕已写入 {:?}", out_path.file_name().unwrap_or_default()),
                });
            }
            Err(err) => {
                warn!(error = %err, path = %out_path.display(), "字幕写入失败，识别结果仍会交给工作台");
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "生成字幕".into(),
                    progress: 1.0,
                    detail: format!("识别完成（自动保存失败：{err}）"),
                });
            }
        }
        let srt_export_sec = writing_started.elapsed().as_secs_f64();
        let total_elapsed_sec = pipeline_started.elapsed().as_secs_f64();

        // ── 阶段 5: 性能基准指标归档与打印 ──
        let video_duration = if let Some(d) = total_dur {
            d
        } else {
            segments.last().map(|s| s.end).unwrap_or(0.0)
        };

        let metrics = crate::core::PipelinePerformanceMetrics {
            video_duration,
            ffmpeg_audio_sec,
            vad_sec,
            whisper_sec: pure_whisper_sec,
            qwen_sec,
            polish_engine_name,
            srt_export_sec,
            total_elapsed_sec,
        };

        let summary = metrics.format_summary_block();
        println!("{summary}");
        info!("{}", summary);

        let _ = tx.send(PipelineEvent::Finished(segments.clone(), metrics));
        info!(elapsed = ?pipeline_started.elapsed(), "管线全部执行完成，输出文件: {:?}", out_path);
        Ok(segments)
    }
}
