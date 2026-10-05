//! 异步任务流水线：调度 FFmpeg -> Whisper -> LLM -> SubtitleWriter

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::engines::{
    plan_compaction, transcribe_chunked, transcribe_chunked_sensevoice, write_wav_mono16,
    CompactionConfig, CompactionPlan, FFmpegEngine, LLMEngine, PunctuationEngine, SenseVoiceEngine,
    SpeechFilterOptions, WhisperEngine, ASR_SAMPLE_RATE,
};
use crate::subtitle::{Segment, SubtitleWriter};
use crate::utils::TempPathGuard;

#[derive(Debug, Clone)]
pub enum PipelineEvent {
    StageChanged(String),
    Progress { stage: String, progress: f64, detail: String },
    SegmentStream(Segment),
    Finished(Vec<Segment>, crate::core::PipelinePerformanceMetrics),
    Error(String),
}

#[derive(Debug, Clone, Copy)]
pub struct WhisperRuntimeOptions {
    pub audio_speed: f64,
    pub vad_threshold: f64,
    pub preprocess: PreprocessOptions,
}

impl Default for WhisperRuntimeOptions {
    fn default() -> Self {
        Self { audio_speed: 1.0, vad_threshold: 0.50, preprocess: PreprocessOptions::default() }
    }
}

/// 音频前端预处理参数（仅 Whisper 路径）。
///
/// # 预处理负责「精度」，不负责「速度」
///
/// 曾经的设计假设是：Whisper 的耗时正比于喂进去的音频时长，所以切掉静音
/// 就能等量省下编码器窗口与解码 token。这个假设**不成立**，因为
/// whisper.cpp 自带的 Silero VAD（`--vad`）已经做了同一件事——它切掉静音、
/// 只对语音段建窗口，并把时间戳映射回原始时间轴。在外面再压一遍属于重复劳动，
/// 实测（10 分钟片段）编码窗口数与解码 token 数都没有下降，反而因为
/// 「每个 VAD 切片各自向上取整到 30 秒窗口」而略微增加，端到端更慢。
///
/// 因此当前默认只保留**语音增强**（高通 / 谱减降噪 / 电平归一）：它的收益在
/// 精度侧——抑制静音处的幻觉 token 与 `no_speech` 误判，从而减少碎段
/// （每段都要重付一轮 SOT 与时间戳解码）。停顿压实默认关闭，见
/// [`PipelineConfig::preprocess_compact`]。
#[derive(Debug, Clone, Copy)]
pub struct PreprocessOptions {
    pub enabled: bool,
    pub filters: SpeechFilterOptions,
    pub compaction: CompactionConfig,
    pub compact: bool,
    /// 最小收益阈值：可切除静音占比低于该值时放弃压实（仍保留语音增强）
    pub min_saving: f64,
}

impl Default for PreprocessOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            filters: SpeechFilterOptions::default(),
            compaction: CompactionConfig::default(),
            compact: false,
            min_saving: 0.10,
        }
    }
}

/// 预处理产物：交给 ASR 的临时 WAV + 时间轴回映射计划。
///
/// `wav` 用 [`TempPathGuard`] 而不是裸 `PathBuf`：文件是在 `spawn_blocking` 闭包里
/// 写的，而「写盘」和「删除」之间隔着好几条可能提前退出的路径——
/// `write_wav_mono16` 写一半失败、闭包内 panic、调用方拿到 Err 后直接返回，
/// 以及守卫持有期间上游的任何 `?`。把文件的生命周期交给守卫，
/// 删除就不再依赖调用方记得清理，这几条路径一次性全覆盖。
struct PreparedAudio {
    wav: TempPathGuard,
    /// `Some` = 确实做了停顿压实，需要回映射；`None` = 只做了语音增强
    plan: Option<CompactionPlan>,
    /// 增强后的完整时长（秒，已含变速）
    decoded_sec: f64,
    /// 压实后的时长（秒）
    asr_sec: f64,
}

/// 预处理时间轴 → 原始媒体时间轴的回映射器。
///
/// 预处理按「增强 → 变速(atempo) → 切除静音」的顺序改造音频，模型吐出的
/// 时间戳都落在最后那条轴上，还原必须按相反顺序做：
///
/// ```text
/// 模型时间轴 t --(压实映射表)--> 变速后时间轴 --(× speed)--> 真实时间轴
/// ```
///
/// 压实映射与变速是两个独立的仿射/分段线性变换，且压实是在变速后的 PCM 上
/// 规划的，因此先查映射表再乘倍率即可，不需要把两者合成一张表。
#[derive(Clone)]
struct TimelineRestorer {
    plan: Option<Arc<CompactionPlan>>,
    speed: f64,
    original_duration: f64,
}

impl TimelineRestorer {
    fn to_original(&self, t: f64) -> f64 {
        let t = match self.plan.as_deref() {
            Some(p) if !p.is_identity() => p.to_original(t),
            _ => t,
        };
        let t = (t * self.speed).max(0.0);
        if self.original_duration > 0.0 {
            t.min(self.original_duration)
        } else {
            t
        }
    }

    fn restore(&self, seg: &mut Segment) {
        let start = self.to_original(seg.start);
        let end = self.to_original(seg.end).max(start);
        seg.start = start;
        seg.end = end;
    }

    fn restore_all(&self, segments: &mut [Segment]) {
        for seg in segments.iter_mut() {
            self.restore(seg);
        }
    }
}

/// 说话人分离的运行参数（F-015）。
///
/// 分离在转写与润色之后、写出之前执行：它只读字幕时间轴与音频本身，
/// 不改文本，因此放在最后一步既不会干扰 ASR，也能让导出直接拿到标签。
#[derive(Debug, Clone, Copy)]
pub struct DiarizationOptions {
    pub enabled: bool,
    /// 期望说话人数（会被收敛到 2..=MAX_SPEAKERS）
    pub speakers: u32,
}

impl Default for DiarizationOptions {
    fn default() -> Self {
        Self { enabled: false, speakers: 2 }
    }
}

pub struct TaskPipeline {
    ffmpeg: Arc<FFmpegEngine>,
    whisper: Arc<WhisperEngine>,
    sensevoice: Option<Arc<SenseVoiceEngine>>,
    llm: Arc<LLMEngine>,
    punc: Option<Arc<PunctuationEngine>>,
    cancelled: Arc<AtomicBool>,
    /// 并行进程数：0 = 自动（按 CPU 核数推导引擎内调优默认值）；>0 = 用户在设置页显式指定
    parallel_workers: AtomicUsize,
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
            parallel_workers: AtomicUsize::new(0),
        }
    }

    /// 设置并行进程数（0 = 自动）。设置页滑条调用；对下一次启动的转写任务生效。
    pub fn set_parallel_workers(&self, n: usize) {
        self.parallel_workers.store(n, Ordering::Relaxed);
    }

    /// 当前并行进程数设置（0 = 自动）
    pub fn parallel_workers(&self) -> usize {
        self.parallel_workers.load(Ordering::Relaxed)
    }

    /// 设置润色（Qwen）推理线程数，对下一次润色生效
    pub fn set_llm_threads(&self, n: u32) {
        self.llm.set_threads(n);
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
        self.run_with_options(
            input_file, output_file, language, output_format, enable_polish, polish_mode,
            threads, model_override, rescue_logprob, WhisperRuntimeOptions::default(),
            DiarizationOptions::default(), tx,
        ).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run_with_options(
        &self,
        input_file: PathBuf,
        output_file: Option<PathBuf>,
        language: Option<String>,
        output_format: String,
        enable_polish: bool,
        polish_mode: Option<String>,
        threads: Option<u32>,
        model_override: Option<PathBuf>,
        rescue_logprob: f64,
        options: WhisperRuntimeOptions,
        diarization: DiarizationOptions,
        tx: UnboundedSender<PipelineEvent>,
    ) -> Result<Vec<Segment>> {

        self.cancelled.store(false, Ordering::Relaxed);
        self.whisper.reset();
        let audio_speed = if options.audio_speed.is_finite() {
            options.audio_speed.clamp(1.0, 1.5)
        } else {
            1.0
        };
        self.whisper.set_vad_threshold(options.vad_threshold);
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

        // SenseVoice 在纯 CPU 多核机器上的长音频改走多进程切块并行：
        // 实测单进程 16 线程 41.6s，而 4 进程 × 2~4 线程仅 18.6~20.2s（快约 2 倍）。
        // 该路径需要完整临时 WAV 以便随机切块，因此不再走纯内存推流。
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4) as u32;
        let sv_parallel = is_sensevoice
            && self.sensevoice.is_some()
            && !use_gpu
            && audio_speed <= 1.0
            && duration_for_chunks >= 180.0
            && cores >= 4;
        // SenseVoice 单会话在 8 线程附近已饱和，继续加线程反而更慢
        // （实测 16 线程 41.6s vs 8 线程 32.6s），故对单会话线程数设上限。
        let sv_threads = threads.unwrap_or(8).clamp(2, 8);
        // 并行进程数：0 = 自动（沿用引擎内按核数推导的调优默认值）
        let workers_override = match self.parallel_workers.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        };

        // 判断是否启用纯内存管道推流 (In-Memory PCM Streaming Pipe):
        // 当使用 SenseVoice、启用 GPU 推理，或音频时长无需多进程切块时，直接以 0 磁盘 I/O 管道推流
        let mut can_stream = !sv_parallel
            && (is_sensevoice || use_gpu || duration_for_chunks <= 405.0 || audio_speed > 1.0);

        let mut ffmpeg_audio_sec = 0.0;
        // 临时 WAV 用守卫持有：本函数后续有多个 `?` / 取消提前返回点，
        // 只要守卫在作用域内，无论从哪条路径退出都会被删掉。
        let mut temp_wav_path: Option<TempPathGuard> = None;

        // ── 阶段 1.5: 音频前端预处理（语音增强 + 可选停顿压实）──
        //
        // 语音增强的收益在精度侧：抑制静音处的幻觉 token 与 no_speech 误判，
        // 从而减少碎段（每个碎段都要重付一轮 SOT 与时间戳解码）。
        //
        // 停顿压实默认关闭：whisper.cpp 自带的 Silero VAD 已经做了同一件事，
        // 外置再压一遍不会减少编码窗口与解码 token，却要多付一次整轨解码、
        // 多写一个临时 WAV，还会放弃 0 磁盘 I/O 的内存推流通道。
        //
        // 只做增强时不再单独跑一遍解码：增强链里已经含 `aresample` 到 16 kHz，
        // 交给推流或临时 WAV 提取阶段就地生效即可。
        let preprocess = options.preprocess;
        let mut preprocess_sec = 0.0;
        let mut preprocess_note: Option<String> = None;
        let mut compaction: Option<CompactionPlan> = None;
        let mut asr_duration = duration_for_chunks;
        // 音频通道滤镜链：语音增强（若启用）+ 变速，留给推流 / 抽音阶段就地生效。
        // 变速必须在这里兜底——关闭预处理时音频加速仍要生效。
        let mut enhance_chain: Option<String> = if audio_speed > 1.0 {
            Some(format!("atempo={audio_speed:.3}"))
        } else {
            None
        };

        if preprocess.enabled && !is_sensevoice && duration_for_chunks > 0.0 {
            let chain = preprocess.filters.chain_with_speed(audio_speed);

            if !preprocess.compact {
                // 只做语音增强：不需要随机访问，保持纯内存推流通道
                enhance_chain = chain;
                // 增强链里含 atempo，切块规划与进度必须按变速后的时长算
                asr_duration = if audio_speed > 1.0 {
                    duration_for_chunks / audio_speed
                } else {
                    duration_for_chunks
                };
                preprocess_note = Some(if enhance_chain.is_some() {
                    "语音增强（降噪 / 电平归一，在音频通道就地生效）".to_string()
                } else {
                    "音频预处理未产生有效滤镜".to_string()
                });
                info!("音频前端预处理：仅语音增强（停顿压实已关闭），保持纯内存推流通道");
            } else {
                let _ = tx.send(PipelineEvent::StageChanged("音频预处理".into()));
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "音频预处理".into(),
                    progress: 0.12,
                    detail: "正在做语音增强（降噪 / 电平归一）与停顿压实...".into(),
                });

                let started = Instant::now();
                let ffmpeg_prep = ffmpeg.clone();
                let in_file_prep = input_file.clone();
                let chain_for_task = chain.clone();
                let cfg = preprocess.compaction;
                let min_saving = preprocess.min_saving;

                let outcome = tokio::task::spawn_blocking(move || -> Result<PreparedAudio> {
                    let pcm = ffmpeg_prep.decode_pcm_mono_filtered(
                        &in_file_prep,
                        ASR_SAMPLE_RATE,
                        chain_for_task.as_deref(),
                    )?;
                    if pcm.is_empty() {
                        anyhow::bail!("预处理解码得到空音频（该文件可能没有音轨）");
                    }
                    let decoded_sec = pcm.len() as f64 / ASR_SAMPLE_RATE as f64;
                    let plan = plan_compaction(&pcm, ASR_SAMPLE_RATE, &cfg);
                    // 收益太小就退回未压实音频：多写一个临时 WAV 却换不到多少模型工作量，
                    // 不如把这一遍省下来的时间还给用户。
                    let use_plan = !plan.is_identity() && plan.is_worthwhile(min_saving);
                    let kept: Vec<i16> = if use_plan { plan.materialize(&pcm) } else { pcm };
                    let wav_path = std::env::temp_dir().join(format!(
                        "v2w_prep_{}_{}.wav",
                        std::process::id(),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0)
                    ));
                    // 先建守卫再落盘：write_wav_mono16 写一半失败时 `?` 直接返回，
                    // 没有守卫就会留下一段截断的 WAV（正是实测到的 13.8 MB 残留）。
                    let wav = TempPathGuard::file(wav_path);
                    write_wav_mono16(wav.path(), &kept, ASR_SAMPLE_RATE)?;
                    Ok(PreparedAudio {
                        wav,
                        plan: use_plan.then_some(plan),
                        decoded_sec,
                        asr_sec: kept.len() as f64 / ASR_SAMPLE_RATE as f64,
                    })
                })
                .await;

                match outcome {
                    Ok(Ok(prep)) => {
                        preprocess_sec = started.elapsed().as_secs_f64();
                        asr_duration = prep.asr_sec;
                        // 解构出来：wav 是守卫，plan 决定它该被保留还是随作用域销毁
                        let PreparedAudio { wav, plan, decoded_sec, asr_sec } = prep;
                        match plan.as_ref() {
                            Some(_) => {
                                // 确实压实了：时间轴不再连续，必须走临时 WAV 随机访问通道
                                can_stream = false;
                                preprocess_note = Some(format!(
                                    "语音增强 + 停顿压实 ({:.1}s → {:.1}s，切除 {:.0}% 静音)",
                                    decoded_sec,
                                    asr_sec,
                                    (1.0 - asr_sec / decoded_sec.max(0.001)) * 100.0
                                ));
                            }
                            None => {
                                // 压实收益不足：让守卫在本分支结束时删掉刚写的临时 WAV，
                                // 增强改由音频通道就地生效，把 0 磁盘 I/O 的推流路径还回来。
                                enhance_chain = chain;
                                preprocess_note = Some(if enhance_chain.is_some() {
                                    format!("语音增强（{:.1}s，静音占比不足未压实）", decoded_sec)
                                } else {
                                    format!("音频预处理无有效滤镜（{:.1}s）", decoded_sec)
                                });
                            }
                        }
                        info!(
                            elapsed = ?started.elapsed(),
                            decoded_sec,
                            asr_sec,
                            compacted = plan.is_some(),
                            "音频前端预处理完成"
                        );
                        if let Some(p) = plan {
                            compaction = Some(p);
                            // 把守卫的所有权移交给 temp_wav_path：转写结束后由它负责删除
                            temp_wav_path = Some(wav);
                        }
                    }
                    Ok(Err(err)) => {
                        warn!(error = %err, "音频预处理失败，回退到原始音频通道");
                        enhance_chain = chain;
                    }
                    Err(err) => {
                        warn!(error = %err, "音频预处理任务异常，回退到原始音频通道");
                        enhance_chain = chain;
                    }
                }
            }
        }

        if temp_wav_path.is_none() {
            if can_stream {
                info!("音频通道就绪：启用纯内存管道推流 (In-Memory PCM Streaming Pipe，0 磁盘 I/O)");
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "提取音频".into(),
                    progress: 1.0,
                    detail: "已建立纯内存音频推流管道 (0 磁盘 I/O，毫秒级就绪)".into(),
                });
            } else {
                // CPU 多核切块模式回退：提前抽取完整临时 WAV 以供多进程随机 seek 切块。
                // 只做语音增强（未压实）时让增强链在这里就地生效，省掉单独一遍解码。
                let in_file = input_file.clone();
                let ffmpeg_extract = ffmpeg.clone();
                let chain_extract = enhance_chain.clone();
                let extraction_started = Instant::now();
                let wav_path = tokio::task::spawn_blocking(move || {
                    ffmpeg_extract.extract_audio_filtered(&in_file, None, chain_extract.as_deref())
                })
                .await??;
                ffmpeg_audio_sec = extraction_started.elapsed().as_secs_f64();
                info!(elapsed = ?extraction_started.elapsed(), "FFmpeg 临时 WAV 提取完成 (CPU 切块模式)");
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "提取音频".into(),
                    progress: 1.0,
                    detail: "音频提取成功".into(),
                });
                temp_wav_path = Some(TempPathGuard::file(wav_path));
            }
        }

        // 预处理把音频改造成了「增强 + 变速 + 压实」后的新时间轴，
        // 后续所有时间戳都必须经它还原回原始媒体时间轴。
        let restorer = TimelineRestorer {
            plan: compaction.clone().map(Arc::new),
            speed: audio_speed,
            original_duration: duration_for_chunks,
        };
        let prep_active = compaction.is_some() || preprocess_note.is_some();

        // ── 阶段 2: 语音转写 ──
        if self.is_cancelled() {
            // 取消：清掉临时 WAV。这里显式 take + drop 是为了让删除发生在 send 之前，
            // 但即使漏写这一行，函数返回时守卫的 Drop 也会兜住。
            drop(temp_wav_path.take());
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

        // JSON 体积开关：救场开启（阈值<0 且非 SenseVoice）才需要 -ojf 全量 token 概率，
        // 救场关闭时降级轻量 -oj，省掉全量 JSON 体积与解析成本
        self.whisper
            .set_need_token_probs(rescue_logprob < 0.0 && !is_sensevoice);

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
                    Some(sv_threads),
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
            // 语音增强链在这里就地生效；`chain_with_speed` 已含 atempo，
            // 因此它同时取代了原先单独的变速推流，输出时长仍为 dur / speed。
            let chain_stream = enhance_chain.clone();

            let (segs, v_sec) = tokio::task::spawn_blocking(move || -> Result<(Vec<Segment>, f64)> {
                let mut ffmpeg_child = ffmpeg_stream.spawn_audio_stream_filtered(
                    &in_file_stream, None, None, chain_stream.as_deref(),
                )?;
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
                    if duration_for_chunks > 0.0 { Some(duration_for_chunks / audio_speed) } else { None },
                    model_override_stream.as_deref(),
                    Some(Box::new(move |p, info, opt_seg| {
                        if let Some(mut seg) = opt_seg {
                            scale_segment_to_original_time(&mut seg, audio_speed, duration_for_chunks);
                            let _ = tx_cb.send(PipelineEvent::SegmentStream(seg));
                        }
                        let _ = tx_cb.send(PipelineEvent::Progress {
                            stage: "语音识别".into(),
                            progress: 0.15 + p * whisper_span,
                            detail: if audio_speed > 1.0 {
                                format!("音频 {audio_speed:.2}x 加速识别：{:.0}%", p * 100.0)
                            } else {
                                info.to_string()
                            },
                        });
                    })),
                );
                let _ = reaper.0.wait();
                let (segments, vad_sec) = res?;
                // 返回的最终片段保持「变速后」时间轴，统一交给下方唯一入口还原。
                // 这里若再自行 scale 一次，就会与统一还原叠加成二次缩放。
                Ok((segments, vad_sec))
            })
            .await??;
            let total_elapsed = transcription_started.elapsed().as_secs_f64();
            let pure_sec = (total_elapsed - v_sec).max(0.0);
            (segs, v_sec, pure_sec)
        } else {
            let wav_path = temp_wav_path.as_ref().unwrap().path().to_path_buf();
            let ffmpeg_chunks = ffmpeg.clone();
            let whisper_engine = whisper.clone();
            let sv_engine = self.sensevoice.clone();
            let model_override_clone = model_override.clone();
            let chunk_threads = threads.unwrap_or(8);
            // 压实后音频变短，切块与进度都必须按压实后的时长规划，
            // 否则会把短音频误判成长音频、切出一堆无语音的碎块。
            let plan_duration = asr_duration;
            let cb_restorer = restorer.clone();

            let (segs, v_sec) = tokio::task::spawn_blocking(move || {
                let cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>> =
                    Some(Box::new(move |p, info, opt_seg| {
                        if let Some(mut seg) = opt_seg {
                            // 流式预览也必须回映射，否则界面上会看到压实时间轴上的错位时间
                            cb_restorer.restore(&mut seg);
                            let _ = tx_whisper.send(PipelineEvent::SegmentStream(seg));
                        }
                        let _ = tx_whisper.send(PipelineEvent::Progress {
                            stage: "语音识别".into(),
                            progress: 0.15 + p * whisper_span,
                            detail: info.to_string(),
                        });
                    }));

                if sv_parallel {
                    let sv = sv_engine.context("SenseVoice 引擎未就绪，无法执行切块并行")?;
                    transcribe_chunked_sensevoice(
                        ffmpeg_chunks.as_ref(),
                        sv.as_ref(),
                        &wav_path,
                        plan_duration,
                        lang_clone.as_deref(),
                        chunk_threads,
                        workers_override,
                        cb,
                    )
                } else {
                    transcribe_chunked(
                        ffmpeg_chunks.as_ref(),
                        whisper_engine.as_ref(),
                        &wav_path,
                        plan_duration,
                        lang_clone.as_deref(),
                        threads,
                        model_override_clone.as_deref(),
                        use_gpu,
                        workers_override,
                        cb,
                    )
                }
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

        // 时间轴回映射：模型看到的是「增强 + 变速 + 压实」后的音频，
        // 这里把全部片段一次性还原回原始媒体时间轴。必须放在置信度救场之前——
        // 救场按时间窗从原始媒体重新切片，只有拿到原始时间轴才能切对位置。
        // 这是**唯一**的最终还原入口：各转写分支返回的片段都必须原样交到这里，
        // 分支内部不得再对返回片段自行还原（详见 `restore_final_timeline`）。
        restore_final_timeline(&mut segments, &restorer, is_sensevoice);
        if prep_active {
            crate::subtitle::optimize_segments(&mut segments);
            info!(
                segments = segments.len(),
                ratio = restorer.plan.as_deref().map(|p| p.ratio()).unwrap_or(1.0),
                "预处理时间轴已还原到原始媒体"
            );
        }

        // 转写已完成，临时 wav 不再需要：显式释放守卫，尽早把磁盘空间还给用户。
        // 注意这里不是「唯一的删除点」——取消 / 报错提前返回时守卫的 Drop 会同样生效。
        drop(temp_wav_path.take());

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
                                    if let Some(mut seg) = opt_seg {
                                        // 救场窗口是从原始媒体切出来的：模型时间轴以窗口起点为 0，
                                        // 预览必须加回窗口起点，否则实时字幕会显示成 00:0x 的错位时间。
                                        shift_window_to_media_timeline(
                                            std::slice::from_mut(&mut seg),
                                            s0,
                                        );
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
                // 把管线的取消标志交给 LLM：润色是分批长循环，不接这根线的话
                // 用户点「终止转写」只杀掉 ASR，润色会继续把几百条字幕跑完。
                llm.install_cancel_flag(self.cancelled.clone());

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

        // ── 阶段 3.5: 说话人分离（F-015，可跳过）──
        // 放在润色之后：分离只读时间轴与音频、不改文本，最后一步做最省事，
        // 也让「导出带说话人前缀」与「界面标签」用的是同一份结果。
        let mut diarization_sec = 0.0;
        if diarization.enabled && !segments.is_empty() {
            let _ = tx.send(PipelineEvent::StageChanged("说话人分离".into()));
            let _ = tx.send(PipelineEvent::Progress {
                stage: "说话人分离".into(),
                progress: 0.97,
                detail: "正在提取基频/能量特征并聚类说话人...".into(),
            });
            let started = Instant::now();
            let ffmpeg_d = self.ffmpeg.clone();
            let in_file_d = input_file.clone();
            let snapshot = segments.clone();
            let k = diarization.speakers;

            let outcome = tokio::task::spawn_blocking(move || -> Result<Vec<Option<u32>>> {
                let samples =
                    ffmpeg_d.decode_pcm_mono(&in_file_d, crate::engines::WAVEFORM_SAMPLE_RATE)?;
                Ok(crate::engines::detect_speakers(
                    &samples,
                    crate::engines::WAVEFORM_SAMPLE_RATE,
                    &snapshot,
                    k,
                ))
            })
            .await;

            match outcome {
                Ok(Ok(labels)) => {
                    for (seg, label) in segments.iter_mut().zip(labels) {
                        seg.speaker = label;
                    }
                    diarization_sec = started.elapsed().as_secs_f64();
                    let tagged = segments.iter().filter(|s| s.speaker.is_some()).count();
                    // 分别记录「请求人数」与「实际聚出的人数」：k-means 可能收敛到
                    // 比请求更少的簇（例如全程只有一个人在说话），只打请求值会让人
                    // 以为分离按预期生效了。
                    let mut found: Vec<u32> =
                        segments.iter().filter_map(|s| s.speaker).collect();
                    found.sort_unstable();
                    found.dedup();
                    info!(
                        elapsed = ?started.elapsed(),
                        requested = k,
                        detected = found.len(),
                        tagged,
                        total = segments.len(),
                        "说话人分离完成"
                    );
                }
                Ok(Err(err)) => {
                    warn!(error = %err, "说话人分离解码失败，已跳过并保留无标签结果");
                }
                Err(err) => {
                    warn!(error = %err, "说话人分离任务异常，已跳过并保留无标签结果");
                }
            }
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
        // 由字幕时间区间求并集，得到有声覆盖时长；不能使用 vad_sec，后者只是 VAD 算法耗时。
        let voiced_duration_sec = covered_duration(&segments);

        let audio_process_name = if let Some(note) = preprocess_note.as_deref() {
            format!("FFmpeg 单遍解码 + {note}")
        } else if can_stream {
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
            if sv_parallel {
                let workers = crate::engines::sensevoice_worker_count(cores);
                format!("SenseVoice-Small INT8 (多进程切块并行 · {workers} 进程 × 2 线程)")
            } else {
                format!("SenseVoice-Small INT8 (非自回归单次前向 · {sv_threads}线程)")
            }
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
            voiced_duration_sec,
            ffmpeg_audio_sec: ffmpeg_audio_sec + preprocess_sec,
            audio_process_name: Some(audio_process_name),
            vad_sec,
            vad_engine_name: Some(vad_engine_name),
            whisper_sec: pure_whisper_sec,
            rescue_sec,
            rescue_span_count,
            asr_engine_name: Some(asr_detail_name),
            qwen_sec,
            polish_engine_name,
            diarization_sec,
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

fn scale_segment_to_original_time(segment: &mut Segment, speed: f64, original_duration: f64) {
    if speed <= 1.0 {
        return;
    }
    segment.start *= speed;
    segment.end *= speed;
    if original_duration > 0.0 {
        segment.start = segment.start.clamp(0.0, original_duration);
        segment.end = segment.end.clamp(segment.start, original_duration);
    }
}

/// 转写完成后，把最终片段从「预处理后时间轴」还原回原始媒体时间轴。
///
/// 这是**唯一**的最终还原入口，且只还原一次。各转写分支内部的流式预览回调
/// 会对另外的片段对象各自还原一次（预览与最终结果不是同一份数据），但分支
/// **返回的最终片段**必须原样交到这里——分支内若再自行还原一遍，就会与这里
/// 叠加成二次缩放：1.25x 会被放大到 1.5625x，字幕越走越偏并在片尾被钳到总时长。
///
/// SenseVoice 推流既不增强也不变速（`FFmpegEngine::spawn_audio_stream` 恒为
/// 1.0 倍速），模型时间轴本身就是原始媒体时间轴，因此跳过还原。
fn restore_final_timeline(segments: &mut [Segment], restorer: &TimelineRestorer, is_sensevoice: bool) {
    if is_sensevoice {
        return;
    }
    restorer.restore_all(segments);
}

/// 计算字幕/语音区间的并集时长，避免相邻或重叠片段重复计时。
/// 这是“识别到的有声覆盖时长”的可复现近似值，不把静音间隔算进去。
pub fn covered_duration(segments: &[Segment]) -> f64 {
    let mut ranges: Vec<(f64, f64)> = segments
        .iter()
        .filter_map(|s| {
            let start = s.start.max(0.0);
            let end = s.end.max(start);
            (end > start).then_some((start, end))
        })
        .collect();
    ranges.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut total = 0.0;
    let mut current: Option<(f64, f64)> = None;
    for (start, end) in ranges {
        match current {
            Some((cs, ce)) if start <= ce => current = Some((cs, ce.max(end))),
            Some((cs, ce)) => {
                total += ce - cs;
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((start, end)) = current {
        total += end - start;
    }
    total
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
        shift_window_to_media_timeline(std::slice::from_mut(&mut seg), s0);
        seg
    }));
    segments.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    for (i, seg) in segments.iter_mut().enumerate() {
        seg.index = i + 1;
    }
}

/// 把「以窗口起点为 0」的片段时间戳平移回原始媒体时间轴。
///
/// 置信度救场从原始媒体切出窗口 `[window_start, window_start + dur]` 后交给模型，
/// 模型吐出的时间戳以窗口起点为 0。最终回写（[`apply_rescued_segments`]）与实时
/// 预览推流都必须经过这里——预览若漏了这一步，界面上的时间徽章会在救场阶段
/// 显示成 `00:0x`，与实际位置（可能在 25 分钟处）完全对不上。
fn shift_window_to_media_timeline(segments: &mut [Segment], window_start: f64) {
    for seg in segments.iter_mut() {
        seg.start += window_start;
        seg.end += window_start;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle::Segment;

    #[test]
    fn accelerated_audio_timestamps_restore_original_timeline() {
        let mut seg = Segment::new(1, 10.0, 12.0, "测试");
        scale_segment_to_original_time(&mut seg, 1.25, 600.0);
        assert!((seg.start - 12.5).abs() < 1e-9);
        assert!((seg.end - 15.0).abs() < 1e-9);
    }

    #[test]
    fn final_timeline_restore_scales_exactly_once() {
        // 场景：语音增强默认开启（preprocess_enabled=true, compact=false），
        // 用户把音频加速设成 1.25x → 喂给模型的音频被 atempo 压缩，模型时间轴
        // 比原始媒体短 1.25 倍。返回的最终片段必须且只能还原一次：40s → 50s。
        //
        // 修复前：can_stream 分支先调用 scale_segment_to_original_time 还原一遍，
        // 汇总处又由 restorer 再还原一遍，40s 被放大到 62.5s（= 40 × 1.25²），
        // 字幕随片长越走越偏、片尾被钳到总时长。本用例钉死「只还原一次」。
        let restorer = TimelineRestorer { plan: None, speed: 1.25, original_duration: 600.0 };
        let mut segs = vec![Segment::new(1, 40.0, 44.0, "测试")];
        restore_final_timeline(&mut segs, &restorer, false);
        assert!((segs[0].start - 50.0).abs() < 1e-9, "40s 应还原为 50s，实得 {}", segs[0].start);
        assert!((segs[0].end - 55.0).abs() < 1e-9, "44s 应还原为 55s，实得 {}", segs[0].end);
    }

    #[test]
    fn final_timeline_restore_skips_sensevoice() {
        // SenseVoice 推流不做变速（spawn_audio_stream 恒 1.0 倍速），
        // 其时间轴已是原始媒体时间轴，不能再乘 speed，否则会凭空拉长字幕。
        let restorer = TimelineRestorer { plan: None, speed: 1.25, original_duration: 600.0 };
        let mut segs = vec![Segment::new(1, 40.0, 44.0, "测试")];
        restore_final_timeline(&mut segs, &restorer, true);
        assert!((segs[0].start - 40.0).abs() < 1e-9, "SenseVoice 不应被缩放: {}", segs[0].start);
        assert!((segs[0].end - 44.0).abs() < 1e-9);
    }

    #[test]
    fn rescue_window_segments_are_shifted_to_media_timeline() {
        // 救场窗口从原始媒体 1500s 处切出：模型时间轴 0.4s 的句子真实位置是 1500.4s。
        // 预览推流与最终回写共用这个平移函数；漏掉它界面会显示 00:00 的错位时间。
        let mut segs = vec![seg(1, 0.4, 2.1, None)];
        shift_window_to_media_timeline(&mut segs, 1500.0);
        assert!((segs[0].start - 1500.4).abs() < 1e-9, "实得 {}", segs[0].start);
        assert!((segs[0].end - 1502.1).abs() < 1e-9, "实得 {}", segs[0].end);
    }

    #[test]
    fn covered_duration_merges_overlapping_segments() {
        let mut a = Segment::new(1, 0.0, 2.0, "a");
        let mut b = Segment::new(2, 1.5, 4.0, "b");
        let c = Segment::new(3, 8.0, 9.0, "c");
        a.start = 0.0;
        b.start = 1.5;
        assert!((covered_duration(&[a, b, c]) - 5.0).abs() < 1e-9);
    }

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
