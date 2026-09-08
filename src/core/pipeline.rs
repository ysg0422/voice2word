//! 异步任务流水线：调度 FFmpeg -> Whisper -> LLM -> SubtitleWriter

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;

use crate::engines::{FFmpegEngine, LLMEngine, WhisperEngine};
use crate::subtitle::{Segment, SubtitleWriter};

#[derive(Debug, Clone)]
pub enum PipelineEvent {
    StageChanged(String),
    Progress { stage: String, progress: f64, detail: String },
    SegmentStream(Segment),
    Finished(Vec<Segment>),
    Error(String),
}

pub struct TaskPipeline {
    ffmpeg: Arc<FFmpegEngine>,
    whisper: Arc<WhisperEngine>,
    llm: Arc<LLMEngine>,
    cancelled: Arc<AtomicBool>,
}

impl TaskPipeline {
    pub fn new(
        ffmpeg: Arc<FFmpegEngine>,
        whisper: Arc<WhisperEngine>,
        llm: Arc<LLMEngine>,
    ) -> Self {
        Self {
            ffmpeg,
            whisper,
            llm,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// 执行完整任务管线
    pub async fn run(
        &self,
        input_file: PathBuf,
        output_file: Option<PathBuf>,
        language: Option<String>,
        output_format: String,
        enable_polish: bool,
        threads: Option<u32>,
        tx: UnboundedSender<PipelineEvent>,
    ) -> Result<Vec<Segment>> {
        self.cancelled.store(false, Ordering::SeqCst);
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

        // ── 阶段 1: 音频提取 ──
        if self.is_cancelled() {
            return Ok(Vec::new());
        }
        let _ = tx.send(PipelineEvent::StageChanged("提取音频".into()));
        let _ = tx.send(PipelineEvent::Progress {
            stage: "提取音频".into(),
            progress: 0.1,
            detail: "正在使用 FFmpeg 抽取 16kHz 音频...".into(),
        });

        let ffmpeg = self.ffmpeg.clone();
        let in_file = input_file.clone();
        let extraction_started = Instant::now();
        let wav_path = tokio::task::spawn_blocking(move || ffmpeg.extract_audio(&in_file, None))
            .await??;
        info!(elapsed = ?extraction_started.elapsed(), "FFmpeg 音频提取完成");

        let _ = tx.send(PipelineEvent::Progress {
            stage: "提取音频".into(),
            progress: 1.0,
            detail: "音频提取成功".into(),
        });

        // ── 阶段 2: 语音转写 ──
        if self.is_cancelled() {
            let _ = std::fs::remove_file(&wav_path);
            return Ok(Vec::new());
        }

        let _ = tx.send(PipelineEvent::StageChanged("语音识别".into()));
        let _ = tx.send(PipelineEvent::Progress {
            stage: "语音识别".into(),
            progress: 0.15,
            detail: "Whisper 模型正在转写语音...".into(),
        });

        let ffmpeg_for_dur = self.ffmpeg.clone();
        let in_file_for_dur = input_file.clone();
        let total_dur = tokio::task::spawn_blocking(move || {
            let d = ffmpeg_for_dur.get_duration(&in_file_for_dur);
            if d > 0.0 { Some(d) } else { None }
        })
        .await
        .unwrap_or(None);

        let whisper = self.whisper.clone();
        let wav_clone = wav_path.clone();
        let lang_clone = language.clone();
        let tx_whisper = tx.clone();

        let transcription_started = Instant::now();
        let mut segments = tokio::task::spawn_blocking(move || {
            whisper.transcribe(
                &wav_clone,
                lang_clone.as_deref(),
                threads,
                total_dur,
                Some(Box::new(move |p, info, opt_seg| {
                    let _ = tx_whisper.send(PipelineEvent::Progress {
                        stage: "语音识别".into(),
                        progress: 0.15 + p * 0.45,
                        detail: info.to_string(),
                    });
                    if let Some(seg) = opt_seg {
                        let _ = tx_whisper.send(PipelineEvent::SegmentStream(seg));
                    }
                })),
            )
        })
        .await??;
        info!(elapsed = ?transcription_started.elapsed(), segments = segments.len(), "Whisper 转写完成");

        // 清理临时 wav
        let _ = std::fs::remove_file(&wav_path);

        if self.is_cancelled() {
            return Ok(segments);
        }

        // ── 阶段 3: LLM 润色 ──
        if enable_polish && !segments.is_empty() {
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
            segments = tokio::task::spawn_blocking(move || {
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
            .await??;
            info!(elapsed = ?polishing_started.elapsed(), segments = segments.len(), "Qwen 润色完成");
        }

        if self.is_cancelled() {
            return Ok(segments);
        }

        // ── 阶段 4: 输出字幕文件 ──
        let _ = tx.send(PipelineEvent::StageChanged("生成字幕".into()));
        let writing_started = Instant::now();
        SubtitleWriter::write_to_file(&segments, &out_path, &output_format)?;
        info!(elapsed = ?writing_started.elapsed(), "字幕文件写入完成");

        let _ = tx.send(PipelineEvent::Progress {
            stage: "生成字幕".into(),
            progress: 1.0,
            detail: format!("字幕已写入 {:?}", out_path.file_name().unwrap_or_default()),
        });

        let _ = tx.send(PipelineEvent::Finished(segments.clone()));
        info!(elapsed = ?pipeline_started.elapsed(), "管线全部执行完成，输出文件: {:?}", out_path);
        Ok(segments)
    }
}
