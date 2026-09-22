//! 异步任务流水线：调度 FFmpeg -> Whisper -> LLM -> SubtitleWriter

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::engines::{
    transcribe_chunked, FFmpegEngine, LLMEngine, PunctuationEngine, SenseVoiceEngine, WhisperEngine,
};
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
    sensevoice: Option<Arc<SenseVoiceEngine>>,
    llm: Arc<LLMEngine>,
    punc: Option<Arc<PunctuationEngine>>,
    cancelled: Arc<AtomicBool>,
}

impl TaskPipeline {
    pub fn new(
        ffmpeg: Arc<FFmpegEngine>,
        whisper: Arc<WhisperEngine>,
        sensevoice: Option<Arc<SenseVoiceEngine>>,
        llm: Arc<LLMEngine>,
        punc: Option<Arc<PunctuationEngine>>,
    ) -> Self {
        Self {
            ffmpeg,
            whisper,
            sensevoice,
            llm,
            punc,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // 同步强杀正在运行的识别子进程，让「终止转写」即时生效而非等阶段结束
        self.whisper.cancel();
        if let Some(sv) = self.sensevoice.as_ref() {
            sv.cancel();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// 运行完整流水线：提取音频 -> Whisper 转写 -> 置信度救场 -> 标点/LLM 润色 -> 写入字幕
    #[allow(clippy::too_many_arguments)]
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
        rescue_logprob: f64,             // 置信度救场阈值 (avg_logprob 下限，<0 启用，0 关闭)
        tx: UnboundedSender<PipelineEvent>,
    ) -> Result<Vec<Segment>> {

        self.cancelled.store(false, Ordering::Relaxed);
        self.whisper.reset();
        if let Some(sv) = self.sensevoice.as_ref() {
            sv.reset();
        }
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
            let _ = tx.send(PipelineEvent::Finished(Vec::new(), Default::default()));
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

        let is_sensevoice = model_override
            .as_ref()
            .map(|p| p.to_string_lossy().to_lowercase().contains("sensevoice"))
            .unwrap_or(false);

        // 判断是否启用纯内存管道推流 (In-Memory PCM Streaming Pipe):
        // 当使用 SenseVoice、启用 GPU 推理，或音频时长无需多进程切块时，直接以 0 磁盘 I/O 管道推流
        let can_stream = is_sensevoice || use_gpu || duration_for_chunks <= 405.0;

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
            let _ = tx.send(PipelineEvent::Finished(Vec::new(), Default::default()));
            return Ok(Vec::new());
        }

        let asr_engine_name = if is_sensevoice {
            "SenseVoice 极速转写".to_string()
        } else {
            "Whisper 神经转写".to_string()
        };

        let _ = tx.send(PipelineEvent::StageChanged("语音识别".into()));
        let _ = tx.send(PipelineEvent::Progress {
            stage: "语音识别".into(),
            progress: 0.15,
            detail: if is_sensevoice {
                "SenseVoice 正在通过纯内存管道极速转写语音...".into()
            } else if can_stream {
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
        let (mut segments, vad_sec, pure_whisper_sec) = if is_sensevoice && self.sensevoice.is_some() {
            let sv_engine = self.sensevoice.as_ref().unwrap().clone();
            let in_file_stream = input_file.clone();
            let ffmpeg_stream = ffmpeg.clone();
            let tx_sv = tx.clone();
            let lang_sv = lang_clone.clone();

            let (segs, elapsed) = tokio::task::spawn_blocking(move || -> Result<(Vec<Segment>, f64)> {
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

                let tx_cb = tx_sv.clone();
                let res = sv_engine.transcribe_stream(
                    Box::new(ffmpeg_stdout),
                    lang_sv.as_deref(),
                    threads,
                    if duration_for_chunks > 0.0 { Some(duration_for_chunks) } else { None },
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
            .await??;
            (segs, 0.0, elapsed)
        } else if can_stream {
            let in_file_stream = input_file.clone();
            let ffmpeg_stream = ffmpeg.clone();
            let whisper_engine = whisper.clone();
            let model_override_stream = model_override.clone();

            let (segs, v_sec) = tokio::task::spawn_blocking(move || -> Result<(Vec<Segment>, f64)> {
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
            .await??;
            let total_elapsed = transcription_started.elapsed().as_secs_f64();
            let pure_sec = (total_elapsed - v_sec).max(0.0);
            (segs, v_sec, pure_sec)
        } else {
            let wav_path = temp_wav_path.as_ref().unwrap().clone();
            let ffmpeg_chunks = ffmpeg.clone();
            let whisper_engine = whisper.clone();
            let model_override_clone = model_override.clone();

            let (segs, v_sec) = tokio::task::spawn_blocking(move || {
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
            .await??;
            let total_elapsed = transcription_started.elapsed().as_secs_f64();
            let pure_sec = (total_elapsed - v_sec).max(0.0);
            (segs, v_sec, pure_sec)
        };

        info!(
            elapsed = ?transcription_started.elapsed(),
            vad_sec,
            pure_whisper_sec,
            segments = segments.len(),
            engine = %asr_engine_name,
            "语音转写完成"
        );

        // 清理临时 wav (仅当存在时清理)
        if let Some(ref p) = temp_wav_path {
            let _ = std::fs::remove_file(p);
        }

        if self.is_cancelled() {
            let _ = tx.send(PipelineEvent::Finished(segments.clone(), Default::default()));
            return Ok(segments);
        }

        // ── 阶段 2.5: 置信度二段重解码（快速通场 -nf 的质量兜底）──
        // 快速通场全程禁用温度回退提速；这里只对 avg_logprob 垫底的少数窗口
        // 切出音频、带回退重新解码一次，用几个百分点的时间换回质量。
        let rescue_started = Instant::now();
        let mut rescue_span_count = 0usize;
        let rescue_enabled = rescue_logprob < 0.0
            && !is_sensevoice
            && segments.iter().any(|s| s.confidence.is_some());

        if rescue_enabled {
            let spans = plan_rescue_spans(&segments, rescue_logprob);
            let total_span_sec: f64 = spans.iter().map(|(s, e)| e - s).sum();
            let media_sec = if duration_for_chunks > 0.0 { duration_for_chunks } else { 0.0 };

            if spans.is_empty() {
                info!("置信度救场：全程置信度良好，无需二次解码");
            } else if media_sec > 0.0 && total_span_sec > media_sec * 0.35 {
                // 低置信占比过高说明是整体难度问题（噪音/口音），逐窗救场不划算，保留原文
                warn!(
                    spans = spans.len(),
                    total_span_sec, media_sec,
                    "低置信片段占比超过 35%，跳过置信度救场以避免耗时失控"
                );
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "语音识别".into(),
                    progress: if enable_polish { 0.60 } else { 0.80 },
                    detail: "低置信片段较多，已跳过二次解码".into(),
                });
            } else {
                let _ = tx.send(PipelineEvent::StageChanged("置信度救场".into()));
                info!(
                    spans = spans.len(),
                    total_span_sec = format!("{total_span_sec:.1}"),
                    "快速通场存在低置信片段，启动置信度救场重解码"
                );

                let whisper_r = whisper.clone();
                let ffmpeg_r = ffmpeg.clone();
                let in_file_r = input_file.clone();
                let lang_r = language.clone();
                let model_r = model_override.clone();
                let cancel_r = self.cancelled.clone();
                let tx_r = tx.clone();
                let segs_snapshot = segments.clone();
                let spans_r = spans.clone();
                let progress_base = if enable_polish { 0.60 } else { 0.80 };
                let threads_r = threads;
                let spans_len = spans_r.len();

                let rescued = tokio::task::spawn_blocking(move || {
                    let mut working = segs_snapshot;
                    for (idx, &(s0, s1)) in spans_r.iter().enumerate() {
                        if cancel_r.load(Ordering::Relaxed) {
                            break;
                        }
                        let span_dur = s1 - s0;
                        let cb_tx = tx_r.clone();
                        let outcome = (|| -> Result<Vec<Segment>> {
                            let mut child = ffmpeg_r.spawn_audio_stream_at(&in_file_r, Some(s0), Some(span_dur))?;
                            let stdout = child.stdout.take().context("获取救场窗口音频管道失败")?;

                            struct ChildReaper(std::process::Child);
                            impl Drop for ChildReaper {
                                fn drop(&mut self) {
                                    let _ = self.0.kill();
                                    let _ = self.0.wait();
                                }
                            }
                            let mut reaper = ChildReaper(child);

                            let res = whisper_r.transcribe_stream_with_fallback(
                                Box::new(stdout),
                                lang_r.as_deref(),
                                threads_r,
                                Some(span_dur),
                                model_r.as_deref(),
                                Some(Box::new(move |p, info, opt_seg| {
                                    if let Some(seg) = opt_seg {
                                        let _ = cb_tx.send(PipelineEvent::SegmentStream(seg));
                                    }
                                    let _ = cb_tx.send(PipelineEvent::Progress {
                                        stage: "置信度救场".into(),
                                        progress: progress_base
                                            + ((idx as f64 + p) / spans_len as f64).min(1.0) * 0.10,
                                        detail: format!("窗口 {}/{}: {}", idx + 1, spans_len, info),
                                    });
                                })),
                            )?;
                            let _ = reaper.0.wait();
                            Ok(res.0)
                        })();

                        match outcome {
                            Ok(new_segs) if !new_segs.is_empty() => {
                                apply_rescued_segments(&mut working, (s0, s1), new_segs);
                            }
                            Ok(_) => {
                                warn!(span_start = s0, span_end = s1, "救场窗口未产出文本，保留原片段");
                            }
                            Err(err) => {
                                warn!(error = %err, span_start = s0, "救场窗口解码失败，保留原片段");
                            }
                        }
                    }
                    working
                })
                .await
                .unwrap_or_else(|err| {
                    warn!(error = %err, "置信度救场任务异常，保留快速通场结果");
                    segments.clone()
                });

                rescue_span_count = spans.len();
                segments = rescued;
                crate::subtitle::optimize_segments(&mut segments);
            }
        }
        let rescue_sec = rescue_started.elapsed().as_secs_f64();

        // ── 阶段 3: 标点与语法润色（可跳过）──
        let mut qwen_sec = 0.0;
        let mut polish_engine_name = None;
        if enable_polish && !segments.is_empty() {
            let mode = polish_mode.unwrap_or_else(|| "punc".to_string());
            let use_punc = (mode == "punc") && self.punc.as_ref().map(|p| p.is_available()).unwrap_or(false);

            if is_sensevoice && mode == "punc" {
                polish_engine_name = Some("SenseVoice 原生标点".to_string());
                info!("SenseVoice 原生自带高精度标点与数字规范化(ITN)，跳过 CT-Punc 阶段以实现极致速度");
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "极速标点".into(),
                    progress: 0.95,
                    detail: "SenseVoice 原生自带高精度标点与 ITN，已瞬时就绪".into(),
                });
            } else if use_punc {
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
        crate::subtitle::optimize_segments(&mut segments);
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

        let audio_process_name = if can_stream {
            "纯内存 PCM 匿名管道推流 (0 物理磁盘 I/O)".to_string()
        } else {
            "FFmpeg 抽取临时 WAV 文件".to_string()
        };

        let vad_engine_name = if is_sensevoice {
            "SenseVoice 融合流式 VAD".to_string()
        } else if vad_sec > 0.0 {
            "Silero VAD 毫秒级语音切片压实".to_string()
        } else {
            "无独立 VAD (全音频直接输入)".to_string()
        };

        let asr_detail_name = if is_sensevoice {
            format!("SenseVoice-Small INT8 (非自回归单次前向 · {}线程)", threads.unwrap_or(8))
        } else {
            let model_name = model_override
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("ggml-model.bin");
            let dev_str = if use_gpu { "GPU加速" } else { "CPU" };
            format!("Whisper {model_name} ({dev_str} · {}线程)", threads.unwrap_or(8))
        };

        let export_name = format!("{} 标准字幕格式写出", output_format.to_uppercase());

        let metrics = crate::core::PipelinePerformanceMetrics {
            video_duration,
            ffmpeg_audio_sec,
            audio_process_name: Some(audio_process_name),
            vad_sec,
            vad_engine_name: Some(vad_engine_name),
            whisper_sec: pure_whisper_sec,
            rescue_sec,
            rescue_span_count,
            asr_engine_name: Some(asr_detail_name),
            qwen_sec,
            polish_engine_name,
            srt_export_sec,
            export_name: Some(export_name),
            segment_count: segments.len(),
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

/// 规划置信度救场窗口：把置信度低于阈值的连续片段合并为重解码窗口
/// （外扩 0.5s 缓冲，窗口间 2s 内的间隙并入同一窗口，减少进程启动次数）。
/// 返回 (窗口起点, 窗口终点) 列表，单位秒，针对原始媒体时间轴。
pub fn plan_rescue_spans(segments: &[Segment], threshold: f64) -> Vec<(f64, f64)> {
    let mut spans: Vec<(f64, f64)> = Vec::new();
    for seg in segments {
        let is_bad = seg.confidence.map_or(false, |c| c < threshold);
        if !is_bad {
            continue;
        }
        let s = (seg.start - 0.5).max(0.0);
        let e = seg.end + 0.5;
        if let Some(last) = spans.last_mut() {
            if s <= last.1 + 2.0 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        spans.push((s, e));
    }
    spans
}

/// 把救场窗口重解码出的片段回写进字幕列表：
/// 替换中点落在窗口内的全部原片段（含窗口缓冲边缘的健康片段，整体重解码质量更高），
/// 随后按时间排序并重新编号。新片段为空时不动原片段，避免内容丢失。
pub fn apply_rescued_segments(segments: &mut Vec<Segment>, span: (f64, f64), new_segs: Vec<Segment>) {
    if new_segs.is_empty() {
        return;
    }
    let (s0, s1) = span;
    let lo = s0 + 0.1;
    let hi = (s1 - 0.1).max(lo);
    segments.retain(|seg| {
        let mid = (seg.start + seg.end) / 2.0;
        !(mid >= lo && mid <= hi)
    });
    segments.extend(new_segs.into_iter().map(|mut seg| {
        seg.start += s0;
        seg.end += s0;
        seg
    }));
    segments.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    for (i, seg) in segments.iter_mut().enumerate() {
        seg.index = i + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle::Segment;

    fn seg(index: usize, start: f64, end: f64, confidence: Option<f64>) -> Segment {
        let mut s = Segment::new(index, start, end, "测试文本");
        s.confidence = confidence;
        s
    }

    #[test]
    fn rescue_spans_merge_close_bad_segments() {
        let segs = vec![
            seg(1, 0.0, 2.0, Some(-0.1)),
            seg(2, 10.0, 12.0, Some(-0.9)),  // 坏
            seg(3, 12.5, 14.0, Some(-0.8)),  // 坏，与上者间隙 < 2s，应并入
            seg(4, 20.0, 22.0, Some(-0.2)),
            seg(5, 30.0, 31.0, Some(-1.2)),  // 坏，远离，独立窗口
        ];
        let spans = plan_rescue_spans(&segs, -0.65);
        assert_eq!(spans.len(), 2, "应为两个独立窗口: {spans:?}");
        assert!((spans[0].0 - 9.5).abs() < 1e-9);
        assert!((spans[0].1 - 14.5).abs() < 1e-9);
        assert!((spans[1].0 - 29.5).abs() < 1e-9);
        assert!((spans[1].1 - 31.5).abs() < 1e-9);
    }

    #[test]
    fn rescue_spans_empty_when_all_confident() {
        let segs = vec![seg(1, 0.0, 2.0, Some(-0.2)), seg(2, 3.0, 5.0, Some(-0.4))];
        assert!(plan_rescue_spans(&segs, -0.65).is_empty());
        // 无置信度数据（非 Whisper 引擎）也不触发
        let segs_none = vec![seg(1, 0.0, 2.0, None)];
        assert!(plan_rescue_spans(&segs_none, -0.65).is_empty());
    }

    #[test]
    fn apply_rescued_replaces_window_and_keeps_outside() {
        let mut segs = vec![
            seg(1, 0.0, 2.0, Some(-0.1)),
            seg(2, 10.0, 12.0, Some(-0.9)),
            seg(3, 12.5, 14.0, Some(-0.8)),
            seg(4, 20.0, 22.0, Some(-0.2)),
        ];
        let rescued = vec![
            Segment::new(1, 0.2, 2.0, "窗口前半句"),
            Segment::new(2, 2.5, 4.2, "窗口后半句"),
        ];
        apply_rescued_segments(&mut segs, (9.5, 14.5), rescued);
        assert_eq!(segs.len(), 4, "窗口内 2 条被替换为 2 条新片段");
        assert_eq!(segs[0].text, "测试文本");
        assert_eq!(segs[1].text, "窗口前半句");
        assert!((segs[1].start - 9.7).abs() < 1e-9, "新片段应偏移到原始媒体时间轴");
        assert_eq!(segs[3].text, "测试文本");
        assert_eq!(segs[3].index, 4, "应重新连续编号");
    }

    #[test]
    fn apply_rescued_keeps_originals_when_decode_empty() {
        let mut segs = vec![seg(1, 10.0, 12.0, Some(-0.9))];
        apply_rescued_segments(&mut segs, (9.5, 12.5), Vec::new());
        assert_eq!(segs.len(), 1, "空结果不得删掉原片段");
    }
}
