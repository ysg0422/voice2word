//! 异步任务流水线：调度 FFmpeg -> Whisper -> LLM -> SubtitleWriter

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::engines::sensevoice::ProviderStatus;
use crate::engines::{
    plan_compaction, transcribe_chunked, transcribe_chunked_sensevoice, write_wav_mono16,
    CompactionConfig, CompactionPlan, FFmpegEngine, LLMEngine, PunctuationEngine, SenseVoiceEngine,
    SpeechFilterOptions, WhisperEngine, ASR_SAMPLE_RATE,
};
use crate::subtitle::{ExportMode, Segment, SubtitleWriter};
use crate::utils::{SubtitleStyleConfig, TempPathGuard};

#[derive(Debug, Clone)]
pub enum PipelineEvent {
    StageChanged(String),
    Progress {
        stage: String,
        progress: f64,
        detail: String,
    },
    SegmentStream(Segment),
    Finished(Vec<Segment>, crate::core::PipelinePerformanceMetrics),
    Error(String),
}

/// 转写引擎能力描述：把原先散落在 `run_with_options` 里的
/// `is_sensevoice / can_stream / sv_parallel` 分支样板收敛为一次结构化查询。
/// 新增引擎时只需扩展 [`EngineCapabilities::plan`]，管线其余部分统一经
/// [`EngineProfile`] 读取能力，不再各自复算布尔量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineCapabilities {
    /// 本次任务选用 SenseVoice 引擎
    pub sensevoice: bool,
    /// 走纯内存 PCM 管道推流（0 磁盘 I/O）
    pub stream: bool,
    /// SenseVoice 切块并行（多进程）
    pub parallel_chunks: bool,
    /// SenseVoice 单会话线程数（已按引擎上限 clamp）
    pub sensevoice_threads: u32,
}

impl EngineCapabilities {
    #[allow(clippy::too_many_arguments)]
    pub fn plan(
        sensevoice_selected: bool,
        sensevoice_available: bool,
        use_gpu: bool,
        audio_speed: f64,
        duration_secs: f64,
        cores: u32,
        threads: Option<u32>,
        chunk_threshold_secs: f64,
    ) -> Self {
        // 公式必须与改动前的内联逻辑逐条等价（见下方"等价公式"）
        let sensevoice = sensevoice_selected;
        let parallel_chunks = sensevoice
            && sensevoice_available
            && !use_gpu
            && audio_speed <= 1.0
            && duration_secs >= 180.0
            && cores >= 4;
        let stream = !parallel_chunks
            && (sensevoice
                || use_gpu
                || duration_secs <= chunk_threshold_secs
                || audio_speed > 1.0);
        let sensevoice_threads = threads
            .unwrap_or(crate::engines::sensevoice::MAX_THREADS)
            .clamp(
                crate::engines::sensevoice::MIN_THREADS,
                crate::engines::sensevoice::MAX_THREADS,
            );
        Self {
            sensevoice,
            stream,
            parallel_chunks,
            sensevoice_threads,
        }
    }

    /// 识别引擎展示名（转写阶段日志用）。
    fn asr_engine_name(&self) -> &'static str {
        if self.sensevoice() {
            "SenseVoice 极速转写"
        } else {
            "Whisper 神经转写"
        }
    }

    /// 转写进度文案：按通道区分 SenseVoice 推流 / Whisper 推流 / Whisper 切块。
    fn transcribe_detail(&self) -> &'static str {
        if self.sensevoice() {
            "SenseVoice 正在通过纯内存管道极速转写语音..."
        } else if self.can_stream() {
            "Whisper 正在通过纯内存管道流式转写语音..."
        } else {
            "Whisper 模型正在转写语音..."
        }
    }

    /// 音频通道展示名：预处理说明优先，其次按是否纯内存推流区分。
    fn audio_process_name(&self, preprocess_note: Option<&str>) -> String {
        if let Some(note) = preprocess_note {
            format!("FFmpeg 单遍解码 + {note}")
        } else if self.can_stream() {
            "纯内存 PCM 匿名管道推流 (0 物理磁盘 I/O)".to_string()
        } else {
            "FFmpeg 抽取临时 WAV 文件".to_string()
        }
    }

    /// VAD 展示名。
    fn vad_engine_name(&self, vad_sec: f64) -> &'static str {
        if self.sensevoice() {
            "SenseVoice 融合流式 VAD"
        } else if vad_sec > 0.0 {
            "Silero VAD 毫秒级语音切片压实"
        } else {
            "无独立 VAD (全音频直接输入)"
        }
    }

    /// 识别引擎明细展示名：SenseVoice 区分并行 / 单会话线程，Whisper 带上模型与设备。
    fn asr_detail_name(
        &self,
        cores: u32,
        model_override: Option<&Path>,
        use_gpu: bool,
        threads: Option<u32>,
    ) -> String {
        if self.sensevoice() {
            if self.parallel() {
                let workers = crate::engines::sensevoice_worker_count(cores);
                format!("SenseVoice-Small INT8 (多进程切块并行 · {workers} 进程 × 2 线程)")
            } else {
                format!(
                    "SenseVoice-Small INT8 (非自回归单次前向 · {}线程)",
                    self.sensevoice_threads
                )
            }
        } else {
            let model_name = model_override
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("ggml-model.bin");
            let dev_str = if use_gpu { "GPU加速" } else { "CPU" };
            format!(
                "Whisper {model_name} ({dev_str} · {}线程)",
                threads.unwrap_or(8)
            )
        }
    }
}

/// 引擎能力查询的收敛点：管线只经此 trait 询问能力，
/// 避免 `sensevoice / can_stream / parallel` 三处判断散落在各分支里。
trait EngineProfile {
    /// 本次任务选用 SenseVoice 引擎
    fn sensevoice(&self) -> bool;
    /// 走纯内存 PCM 管道推流
    fn can_stream(&self) -> bool;
    /// SenseVoice 切块并行（多进程）
    fn parallel(&self) -> bool;
    /// 本次转写通道：结合「SenseVoice 引擎是否就绪」一次性求解，
    /// 判定顺序与收敛前的 `is_sensevoice / can_stream / sv_parallel` 三段式链逐条等价。
    fn transcribe_route(&self, sensevoice_ready: bool) -> TranscribeRoute;
}

/// 转写通道：`is_sensevoice / can_stream / sv_parallel` 三段式分支的收敛结果。
/// 新增引擎 / 新通道时只需扩展 [`EngineProfile::transcribe_route`]，
/// 管线侧不再各自复算布尔量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscribeRoute {
    /// SenseVoice 单会话纯内存推流（不增强、不变速，模型时间轴即原始时间轴）
    SenseVoiceStream,
    /// Whisper 纯内存 PCM 管道流式转写
    WhisperStream,
    /// 临时 WAV 随机访问切块；`parallel` 为真时走 SenseVoice 多进程切块
    Chunked { parallel: bool },
    /// 用户选了 SenseVoice 但引擎/模型未就位（禁止降级到 Whisper，避免 ONNX 模型引发 Whisper 崩溃）
    SenseVoiceUnavailable,
}

impl EngineProfile for EngineCapabilities {
    fn sensevoice(&self) -> bool {
        self.sensevoice
    }

    fn can_stream(&self) -> bool {
        self.stream
    }

    fn parallel(&self) -> bool {
        self.parallel_chunks
    }

    fn transcribe_route(&self, sensevoice_ready: bool) -> TranscribeRoute {
        if self.sensevoice() {
            if sensevoice_ready {
                TranscribeRoute::SenseVoiceStream
            } else {
                TranscribeRoute::SenseVoiceUnavailable
            }
        } else if self.can_stream() {
            TranscribeRoute::WhisperStream
        } else {
            TranscribeRoute::Chunked {
                parallel: self.parallel(),
            }
        }
    }
}

/// 流水线阶段。用于替代散落的 `StageChanged("中文")` 字符串字面量，
/// 并作为「每阶段产物落盘 / 断点续跑」的键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    ExtractAudio,
    Preprocess,
    Transcribe,
    Rescue,
    Punctuate,
    Polish,
    Write,
}

impl Stage {
    pub const ALL: [Stage; 7] = [
        Stage::ExtractAudio,
        Stage::Preprocess,
        Stage::Transcribe,
        Stage::Rescue,
        Stage::Punctuate,
        Stage::Polish,
        Stage::Write,
    ];

    /// UI 显示用的中文标签，必须与改动前 `StageChanged` 里的字面量完全一致。
    pub fn label(self) -> &'static str {
        match self {
            Stage::ExtractAudio => "提取音频",
            Stage::Preprocess => "音频预处理",
            Stage::Transcribe => "语音识别",
            Stage::Rescue => "置信度救场",
            Stage::Punctuate => "极速标点",
            Stage::Polish => "智能润色",
            Stage::Write => "生成字幕",
        }
    }

    /// 该阶段产物落盘的文件名（带序号，便于人工排查）。
    pub fn artifact_file(self) -> &'static str {
        match self {
            Stage::ExtractAudio => "01_extract_audio.json",
            Stage::Preprocess => "02_preprocess.json",
            Stage::Transcribe => "03_transcribe.json",
            Stage::Rescue => "04_rescue.json",
            Stage::Punctuate => "05_punctuate.json",
            Stage::Polish => "06_polish.json",
            Stage::Write => "07_write.json",
        }
    }
}

/// 阶段产物落盘 / 读取。根目录由 [`TaskPipeline::set_artifact_dir`] 指定；
/// 未指定时管线行为与历史完全一致（不写、不读）。
///
/// # 归属校验（断点续跑的自我保护）
///
/// 可续跑的片段产物用 `OwnedSegments` 信封落盘：除片段本体外，还记下产出它的
/// 输入媒体。经 [`Self::owned_by`] 构造的句柄在读取时校验归属，不一致就当作
/// 「没有产物」（`Ok(None)`）走全流程重算。于是「同一个产物目录被复用去跑第二支
/// 片子」（`--artifact-dir` 指向固定目录，或目录名只按文件名主干去重、两支片子的
/// 主干同名）不会把前一支的识别结果贴到后一支上。
///
/// 为什么归属写在**每份产物内部**、而不是目录里单独放一份记录：各阶段产物是
/// **按条件**落盘的（例如本次没开润色就不写 `06_polish.json`）。目录级记录只要
/// 有一份旧片段没被本次覆盖，就会被误判成「本次输入所有」，从而把旧片段放行——
/// 归属必须跟着产物本身走。
#[derive(Debug, Clone)]
pub struct StageArtifacts {
    root: PathBuf,
    /// `Some` = 启用归属校验（值 = 本次任务的输入媒体标识）；`None` = 不校验。
    owner: Option<String>,
}

/// 可续跑片段产物的落盘信封（写入侧）：借用片段，避免整份克隆。
#[derive(serde::Serialize)]
struct OwnedSegmentsRef<'a> {
    /// 产出这份片段的输入媒体（落盘时刻的 `display()` 路径）
    input: &'a str,
    /// 片段本体
    segments: &'a [Segment],
}

/// 可续跑片段产物的落盘信封（读取侧）。
#[derive(serde::Deserialize)]
struct OwnedSegments {
    input: String,
    segments: Vec<Segment>,
}

/// `ExtractAudio` 阶段的产物记录：原始媒体路径与探测到的时长。
///
/// 时长是「是否切块 / 是否并行 / 进度换算」的输入；落盘后断点续跑可直接复用，
/// 省掉一次 ffprobe 探测。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ExtractAudioRecord {
    pub input: String,
    pub duration_sec: f64,
}

/// 通用「信息型」阶段留痕：只记录一段可读说明（如预处理实际生效的滤镜链）。
///
/// 这类阶段不产出可复用的数据结构，落盘的意义是让 `02_*` 等阶段文件可被人工排查，
/// 并保持「一阶段一文件」的命名约定完整；它**不参与** resume 短路。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct StageRecord {
    pub detail: String,
}

impl StageArtifacts {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            owner: None,
        }
    }

    /// 带归属的句柄：`input` 为本次任务的输入媒体标识（管线的 `input_file`）。
    ///
    /// 读取片段产物时校验「落盘时记录的输入」是否为 `input`；不一致或解析不出
    /// 归属（老格式裸数组）一律按「没有产物」处理。写入时把 `input` 记进信封。
    pub fn owned_by(root: impl Into<PathBuf>, input: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            owner: Some(input.into()),
        }
    }

    pub fn path(&self, stage: Stage) -> PathBuf {
        self.root.join(stage.artifact_file())
    }

    pub fn exists(&self, stage: Stage) -> bool {
        self.path(stage).is_file()
    }

    /// 原子写：先写 `<file>.part` 再改名，失败不 panic（best-effort）。
    fn write_bytes(&self, stage: Stage, bytes: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("创建阶段产物目录失败: {}", self.root.display()))?;
        let path = self.path(stage);
        let tmp = path.with_extension("json.part");
        std::fs::write(&tmp, bytes)
            .with_context(|| format!("写入阶段产物失败: {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("替换阶段产物失败: {}", path.display()))?;
        Ok(())
    }

    pub fn write_segments(&self, stage: Stage, segments: &[Segment]) -> Result<()> {
        let envelope = OwnedSegmentsRef {
            input: self.owner.as_deref().unwrap_or_default(),
            segments,
        };
        let data = serde_json::to_vec_pretty(&envelope).context("序列化阶段片段失败")?;
        self.write_bytes(stage, &data)
    }

    /// 泛型 JSON 落盘（与 [`Self::write_segments`] 共用同一套原子写语义）。
    pub fn write_json<T: serde::Serialize>(&self, stage: Stage, value: &T) -> Result<()> {
        let data = serde_json::to_vec_pretty(value).context("序列化阶段产物失败")?;
        self.write_bytes(stage, &data)
    }

    /// 泛型 JSON 读取：文件不存在返回 `Ok(None)`，解析失败返回 `Err`
    /// （与 [`Self::read_segments`] 的容错边界一致）。
    pub fn read_json<T: serde::de::DeserializeOwned>(&self, stage: Stage) -> Result<Option<T>> {
        let path = self.path(stage);
        if !path.is_file() {
            return Ok(None);
        }
        let data = std::fs::read(&path)
            .with_context(|| format!("读取阶段产物失败: {}", path.display()))?;
        let value: T = serde_json::from_slice(&data)
            .with_context(|| format!("解析阶段产物失败: {}", path.display()))?;
        Ok(Some(value))
    }

    pub fn read_segments(&self, stage: Stage) -> Result<Option<Vec<Segment>>> {
        let Some(envelope) = self.read_json::<OwnedSegments>(stage)? else {
            return Ok(None);
        };
        if let Some(owner) = self.owner.as_deref() {
            if !resume_owner_matches(&envelope.input, owner) {
                warn!(
                    stage = stage.artifact_file(),
                    recorded = %envelope.input,
                    current = %owner,
                    "断点续跑：阶段产物不属于本次输入，忽略该产物并重算"
                );
                return Ok(None);
            }
        }
        Ok(Some(envelope.segments))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WhisperRuntimeOptions {
    pub audio_speed: f64,
    pub vad_threshold: f64,
    pub preprocess: PreprocessOptions,
}

impl Default for WhisperRuntimeOptions {
    fn default() -> Self {
        Self {
            audio_speed: 1.0,
            vad_threshold: 0.50,
            preprocess: PreprocessOptions::default(),
        }
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
        // 词级时间戳与段同属一条时间轴：段边界映射回真实媒体时间后，词时间必须
        // 做同一映射。否则「段在真实时间、词在模型时间」两套坐标混用，逐词高亮 /
        // 卡拉OK 滚动会整体错位。to_original 单调不减，因此映射后仍落在段区间内。
        for w in seg.words.iter_mut() {
            let ws = self.to_original(w.start);
            let we = self.to_original(w.end).max(ws);
            w.start = ws;
            w.end = we;
        }
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
        Self {
            enabled: false,
            speakers: 2,
        }
    }
}

/// 收尾统计块写到哪里：`false` = stdout（默认，图形界面与人用终端），`true` = stderr。
///
/// 无头模式必须把 stdout 让给结果：`--json` 时 stdout 只有一行 JSON，混进多行统计块
/// 会让调用方解析失败。统计块仍要打给人看，于是改道 stderr，而不是干脆不打。
/// 用进程级开关而不是参数：管线由库内多处构造，且这个决定对一次进程运行是常量。
///
/// 插在这里（而不是 metrics.rs）是因为只有管线收尾这一处会真的打印它。
static SUMMARY_TO_STDERR: AtomicBool = AtomicBool::new(false);

/// 把收尾统计块改写到 stderr（命令行入口在开始任务前调用一次）。
pub fn set_summary_to_stderr(enabled: bool) {
    SUMMARY_TO_STDERR.store(enabled, Ordering::Relaxed);
}

/// 当前统计块去向（测试与调用方自查用）。
pub fn summary_to_stderr() -> bool {
    SUMMARY_TO_STDERR.load(Ordering::Relaxed)
}

pub struct TaskPipeline {
    ffmpeg: Arc<FFmpegEngine>,
    whisper: Arc<WhisperEngine>,
    sensevoice: Option<Arc<SenseVoiceEngine>>,
    llm: Arc<LLMEngine>,
    punc: Option<Arc<PunctuationEngine>>,
    /// 取消标志：`cancel()` 置位、`begin_task()` 复位（全进程唯一的复位点，见那里的
    /// 时序说明）。它与 [`LLMEngine`] 通过 `install_cancel_flag` 共享同一个 `Arc`，
    /// 因此必须保持普通布尔量——不能升级成「代际比较」，否则润色阶段读到的语义就变了。
    cancelled: Arc<AtomicBool>,
    /// 并行进程数：0 = 自动（按 CPU 核数推导引擎内调优默认值）；>0 = 用户在设置页显式指定
    parallel_workers: AtomicUsize,
    /// 自动导出用的字幕样式快照（字号 / 折行宽度等）。
    ///
    /// 由 `AppState` 在构造与每次样式变更（设置页 / 剪辑台滑条、预设切换）时写入，
    /// 转写收尾的自动落盘据此折行——与剪辑台、库批量、库单条三条导出链路行为一致。
    /// 内部可变：UI 线程随时写、任务线程收尾时读，照 `parallel_workers` 的先例用共享
    /// 可变状态（`Mutex`），不引入新依赖。
    /// `None` = 从未写入（例如单测里裸构造的管线）→ 自动导出回落到无样式入口
    /// [`SubtitleWriter::write_to_file`]，与历史行为逐字节一致，绝不 panic。
    subtitle_style: Mutex<Option<SubtitleStyleConfig>>,
    /// 阶段产物落盘目录：`None` = 不落盘（历史行为，逐字节一致）。
    ///
    /// 由调用方通过 [`Self::set_artifact_dir`] 指定，与 `subtitle_style` 同一层：
    /// UI 线程写、任务线程读。开启后转写阶段会把最终片段原子落盘，
    /// 并在下次运行时优先复用（断点续跑）。
    artifact_dir: Mutex<Option<PathBuf>>,
}

impl TaskPipeline {
    /// 本次装配是否带上了 SenseVoice 引擎（四项齐全才会装配，见 `cli::build_sensevoice`）。
    ///
    /// 为什么要有它：无头模式要在**发起转写之前**决定用哪个引擎（见 `cli::choose_engine`），
    /// 而「可用性」的真相在装配那一刻就定了。让调用方各自去 `stat` 四个文件是第二份判断，
    /// 迟早与装配口径分叉。
    pub fn has_sensevoice(&self) -> bool {
        self.sensevoice.is_some()
    }

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
            subtitle_style: Mutex::new(None),
            artifact_dir: Mutex::new(None),
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

    /// 写入自动导出的字幕样式快照（`AppState` 在构造与每次样式变更时调用）。
    ///
    /// # 语义取舍：存「最近一次写入」而不是「任务派发瞬间」的快照
    ///
    /// 样式可能在转写期间被用户改（设置页 / 剪辑台滑条、预设切换），而写入点与派发点
    /// 并不重合（`run_with_options` 的入参里没有样式，见本文件顶部说明）。这里选择
    /// 「改样式即同步写入」：导出发生在收尾（通常已是派发后几分钟），用**当前值**与
    /// 其余三条导出链路（剪辑台 / 库批量 / 库单条都是「导出那一刻读当前 config」）
    /// 一致，也更贴近用户刚在预览里看到的排版。任务线程只读、不写，因此无竞态。
    pub fn set_subtitle_style(&self, style: SubtitleStyleConfig) {
        *self
            .subtitle_style
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(style);
    }

    /// 当前导出样式快照；`None` = 从未写入（自动导出按历史行为不折行）。
    pub fn subtitle_style(&self) -> Option<SubtitleStyleConfig> {
        self.subtitle_style
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 指定阶段产物落盘目录（`None` = 关闭落盘 / 断点续跑，保持历史行为）。
    pub fn set_artifact_dir(&self, dir: Option<PathBuf>) {
        *self.artifact_dir.lock().unwrap() = dir;
    }

    /// 当前阶段产物落盘目录；`None` = 未开启。
    pub fn artifact_dir(&self) -> Option<PathBuf> {
        self.artifact_dir.lock().unwrap().clone()
    }

    /// 设置润色（Qwen）推理线程数，对下一次润色生效
    pub fn set_llm_threads(&self, n: u32) {
        self.llm.set_threads(n);
    }

    /// 运行期改写 GPU 占用上限（百分比，100 = 不限速），转发给 whisper 引擎。
    ///
    /// 与 [`Self::set_llm_threads`] 同一层：引擎实例被管线私有持有，UI 拿不到，
    /// 凡「改完要对下一次转写立即生效」的引擎参数都在这里开一个转发口。
    /// 不转发的话，用户在性能页调整「低占用 GPU」上限后必须重启程序才生效。
    pub fn set_gpu_limit_percent(&self, percent: u32) {
        self.whisper.set_gpu_limit_percent(percent);
    }

    /// ONNX 执行后端实况：SenseVoice 优先，回退 CT-Punc；两者都没回报过则是 `None`。
    ///
    /// 供性能页把「配置了 dml 却回落 cpu」显示到界面上（引擎不感知 UI）。
    /// 只读一个 `Mutex<Option<..>>`，成本可忽略；首个回报为准（引擎侧已保证只报一次）。
    pub fn onnx_provider_status(&self) -> Option<ProviderStatus> {
        self.sensevoice
            .as_ref()
            .and_then(|e| e.provider_status())
            .or_else(|| self.punc.as_ref().and_then(|e| e.provider_status()))
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // 同步强杀正在运行的识别子进程，让「终止转写」即时生效而非等阶段结束
        self.whisper.cancel();
        if let Some(sv) = self.sensevoice.as_ref() {
            sv.cancel();
        }
        // 标点阶段同样是外部 runner 子进程：不在这里一起取消的话，用户点终止后标点会
        // 照跑到自己结束（界面表现就是「点了终止却还停在极速标点」）。取消的是引擎侧
        // 标志与在册 PID，与上面两条同一套语义。
        if let Some(punc) = self.punc.as_ref() {
            punc.cancel();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// 任务起手：把上一轮遗留的取消状态（管线侧 + 引擎侧）一并作废。
    ///
    /// **这是全进程唯一清掉取消标志的地方，且必须由调用方在「spawn 任务线程 /
    /// 把状态切成转写中」之前调用。** 早先这个复位写在 [`Self::run_with_options`]
    /// 的开头，由此产生一条「终止转写失效」的竞态：
    ///
    /// ```text
    /// UI 线程（用户与状态机）                    任务线程（刚 spawn）
    /// ─────────────────────────────────────────  ─────────────────────────────────
    /// status = Processing(准备中)                 （尚未被调度到）
    /// 用户点「终止」→ cancel():
    ///   cancelled = true                         run_with_options 开头:
    ///   whisper.cancel() 杀在册子进程                 cancelled = false   ← 抹掉这一发取消
    ///                                               whisper.reset()
    ///                                             …接着跑完整个转写，终止失效
    /// ```
    ///
    /// 把复位前移到派发之前就彻底闭合：任务线程里再不存在第二个写方，用户在线程起来
    /// 之后按下的终止不可能被任务自己抹掉。而「上一轮的取消」也不会误伤新任务——
    /// `crate::ui::actions::start_processing_for` 的派发口有 `status != Processing`
    /// 前置守卫，新任务必然在上一轮收尾（状态回 Idle）之后才派发，此处的复位严格
    /// 早于「新任务对用户可见」这一刻；那一刻之后按下的取消不会被吞掉。
    ///
    /// 为什么不改成「取消代际 / 只对当前代际生效」：引擎侧（`WhisperEngine`、
    /// `SenseVoiceEngine` 各自的 `cancel` 标志，以及 [`LLMEngine::install_cancel_flag`]
    /// 拿到的那个**裸** `Arc<AtomicBool>`）都必须在任务起手时被复位，而它们只认一个
    /// 布尔量、不认代际；要升级成代际得同时改三个引擎模块（且润色/翻译链路也共享这
    /// 根标志）。既然那份复位怎么都躲不掉，代际对管线侧就只剩冗余——把同一个复位点
    /// 挪到派发前已经闭合了竞态。
    pub fn begin_task(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
        // 引擎侧标志同批作废：登记表里的子进程早已在上一轮 cancel() 时被杀 / 被回收，
        // 这里只清标志，不会复活任何进程。
        self.whisper.reset();
        if let Some(sv) = self.sensevoice.as_ref() {
            sv.reset();
        }
        // 标点引擎的取消标志同批作废：不复位的话，上一轮的取消会把下一轮的标点阶段
        // 直接掐掉（登记表里的 runner 早在上一次 cancel() 时就被杀 / 回收了）。
        if let Some(punc) = self.punc.as_ref() {
            punc.reset();
        }
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
            input_file,
            output_file,
            language,
            output_format,
            enable_polish,
            polish_mode,
            threads,
            model_override,
            rescue_logprob,
            WhisperRuntimeOptions::default(),
            DiarizationOptions::default(),
            tx,
        )
        .await
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
        // 取消状态**不在这里复位**。复位由派发方在 spawn 任务线程之前、把状态切成
        // 「转写中」之前调用 [`Self::begin_task`] 完成（时序说明见该函数）。此处若
        // 再复位一次，就重新打开了「用户在线程刚起来时点终止、标志却被任务自己抹掉」
        // 这条竞态——本函数里不再有任何清标志的写方，正是修复本身的一部分。
        let audio_speed = if options.audio_speed.is_finite() {
            options.audio_speed.clamp(1.0, 1.5)
        } else {
            1.0
        };
        self.whisper.set_vad_threshold(options.vad_threshold);
        let pipeline_started = Instant::now();
        // 本次任务的输入标识：阶段产物的归属键（见 [`StageArtifacts`] 的说明）。
        let input_id = input_file.display().to_string();
        // 阶段产物落盘句柄：None = 历史行为（不写不读）。带上归属键后，读取任何
        // 可续跑产物都会校验「落盘时记录的输入」是否就是本次输入，不一致按「没有
        // 产物」处理（走全流程重算），因此下面几处续跑读取自带归属闸门。
        let artifacts = self
            .artifact_dir()
            .map(|dir| StageArtifacts::owned_by(dir, input_id.clone()));

        // 断点续跑检测必须早于「音频预处理」决策：命中续跑产物时要整段跳过解码 / 压实。
        // 优先复用「走完全流程」的最终产物（Stage::Write），命中即跳过 ASR + 救场 +
        // 标点/润色 + 说话人分离，直接写字幕（输出文件会被幂等重写）；退而求其次复用
        // 「已还原到原始媒体时间轴」的识别结果（Stage::Transcribe），此时仍需跑完后续阶段。
        // 两者落盘的都是最终时间轴，故下方必须跳过二次还原。
        //
        // 归属校验在 `read_segments` 内部完成：目录里的产物若属于另一支片子
        // （目录被复用 / 只按文件名主干去重而主干撞车），这里拿到的就是 `None`，
        // 于是整条链路重算，绝不会把 A 的识别结果贴到 B 上。
        let resumed_final = artifacts
            .as_ref()
            .and_then(|a| a.read_segments(Stage::Write).ok().flatten());
        let resumed_asr = artifacts
            .as_ref()
            .and_then(|a| a.read_segments(Stage::Transcribe).ok().flatten());
        let skip_post = resumed_final.is_some();
        let resumed_segments = resumed_final.or(resumed_asr);
        let resumed = resumed_segments.is_some();

        let out_path = output_file.unwrap_or_else(|| {
            let stem = input_file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("output");
            input_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(format!(
                    "{}.{}",
                    stem,
                    // 后缀取**扩展名**：`txt-ts`（带时间码纯文本）的扩展名是 `txt`，
                    // 直接用格式名会产出 `xxx.txt-ts` 这种双后缀文件名。
                    crate::subtitle::writer::export_spec_for(&output_format).0
                ))
        });

        // ── 阶段 1: 探测时长与音频管道准备 ──
        if self.is_cancelled() {
            let _ = tx.send(PipelineEvent::Finished(Vec::new(), Default::default()));
            return Ok(Vec::new());
        }
        let _ = tx.send(PipelineEvent::StageChanged(
            Stage::ExtractAudio.label().into(),
        ));
        let _ = tx.send(PipelineEvent::Progress {
            stage: "提取音频".into(),
            progress: 0.05,
            detail: "正在探测媒体时长并准备音频通道...".into(),
        });

        // 断点续跑：阶段 1 的时长探测若已有落盘记录，直接复用并跳过 ffprobe。
        let cached_duration = cached_extract_audio_duration(artifacts.as_ref(), &input_id);
        let total_dur = if let Some(d) = cached_duration {
            info!("断点续跑：复用已落盘的媒体时长（{:.1}s），跳过探测", d);
            Some(d)
        } else {
            let ffmpeg_for_dur = self.ffmpeg.clone();
            let in_file_for_dur = input_file.clone();
            tokio::task::spawn_blocking(move || {
                let d = ffmpeg_for_dur.get_duration(&in_file_for_dur);
                if d > 0.0 {
                    Some(d)
                } else {
                    None
                }
            })
            .await
            .unwrap_or(None)
        };

        let duration_for_chunks = total_dur.unwrap_or(0.0);
        if duration_for_chunks > 0.0 {
            let _ = tx.send(PipelineEvent::Progress {
                stage: "提取音频".into(),
                progress: 0.08,
                detail: format!(
                    "媒体时长探测完成（{}），正在初始化音频通道...",
                    crate::ui::format_mm_ss(duration_for_chunks)
                ),
            });
        }

        // best-effort 落盘阶段 1 产物（仅在未命中缓存时写；写失败只告警，不中断）。
        if cached_duration.is_none() {
            if let Some(a) = artifacts.as_ref() {
                let record = ExtractAudioRecord {
                    input: input_file.display().to_string(),
                    duration_sec: duration_for_chunks,
                };
                if let Err(e) = a.write_json(Stage::ExtractAudio, &record) {
                    warn!("提取音频产物落盘失败: {e:#}");
                }
            }
        }
        let whisper = self.whisper.clone();
        let ffmpeg = self.ffmpeg.clone();
        let use_gpu = whisper.uses_gpu();

        // 引擎档位是否选中 SenseVoice：仅作为能力求解的输入，最终能力统一走
        // [`EngineProfile`]，避免同一判断在求解前后各算一遍。
        let sensevoice_selected = model_override
            .as_ref()
            .map(|p| p.to_string_lossy().to_lowercase().contains("sensevoice"))
            .unwrap_or(false);

        // SenseVoice 在纯 CPU 多核机器上的长音频改走多进程切块并行：
        // 实测单进程 16 线程 41.6s，而 4 进程 × 2~4 线程仅 18.6~20.2s（快约 2 倍）。
        // 该路径需要完整临时 WAV 以便随机切块，因此不再走纯内存推流。
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4) as u32;
        // 动态按需探测/热装配 SenseVoice 引擎：
        // 若启动时尚未下载模型，但用户在运行期间完成了下载，在此处就地装配，
        // 无需强制重启应用程序即可立即享受 SenseVoice 极速转写。
        let dynamic_sensevoice = self.sensevoice.clone().or_else(|| {
            let runner = crate::utils::AppConfig::resolve_path("tools/sensevoice_runner.py");
            let model = crate::utils::AppConfig::resolve_path("models/sensevoice/model.int8.onnx");
            let tokens = crate::utils::AppConfig::resolve_path("models/sensevoice/tokens.txt");
            let vad = crate::utils::AppConfig::resolve_path("models/sensevoice/silero_vad.onnx");
            if runner.is_file() && model.is_file() && tokens.is_file() && vad.is_file() {
                let py = crate::utils::AppConfig::resolve_command("python");
                let provider = if use_gpu { "dml" } else { "cpu" };
                info!("动态热装配 SenseVoice 极速引擎 (provider={provider})");
                Some(Arc::new(
                    crate::engines::SenseVoiceEngine::with_python_and_provider(
                        runner,
                        model,
                        tokens,
                        vad,
                        threads.unwrap_or(8),
                        py,
                        provider,
                    ),
                ))
            } else {
                None
            }
        });
        let sensevoice_available = dynamic_sensevoice.is_some();

        // 引擎能力一次性求解：并行切块 / 纯内存推流 / 单会话线程数的判定公式
        // 集中在 [`EngineCapabilities::plan`]，此处只读结果，下游统一经 [`EngineProfile`] 取用。
        let mut caps = EngineCapabilities::plan(
            sensevoice_selected,
            sensevoice_available,
            use_gpu,
            audio_speed,
            duration_for_chunks,
            cores,
            threads,
            crate::engines::chunk_threshold_secs(),
        );
        // SenseVoice 单会话在 8 线程附近已饱和，继续加线程反而更慢
        // （实测 16 线程 41.6s vs 8 线程 32.6s），故单会话线程数统一在
        // [`EngineCapabilities::plan`] 内 clamp 到上限（`sensevoice_threads`）。
        // 并行进程数：0 = 自动（沿用引擎内按核数推导的调优默认值）
        let workers_override = match self.parallel_workers.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        };

        // 是否启用纯内存管道推流 (In-Memory PCM Streaming Pipe) 由
        // [`EngineCapabilities::plan`] 统一求解；停顿压实后由下方 `caps.stream = false` 就地关断。

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

        if preprocess.enabled && !caps.sensevoice() && duration_for_chunks > 0.0 && !resumed {
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
                let _ = tx.send(PipelineEvent::StageChanged(
                    Stage::Preprocess.label().into(),
                ));
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
                    let kept: Vec<i16> = if use_plan {
                        plan.materialize(&pcm)
                    } else {
                        pcm
                    };
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
                        let PreparedAudio {
                            wav,
                            plan,
                            decoded_sec,
                            asr_sec,
                        } = prep;
                        match plan.as_ref() {
                            Some(_) => {
                                // 确实压实了：时间轴不再连续，必须走临时 WAV 随机访问通道
                                caps.stream = false;
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

        // best-effort 留痕：记录预处理阶段实际生效的说明（信息型产物，不参与 resume）。
        if let Some(a) = artifacts.as_ref() {
            let record = StageRecord {
                detail: preprocess_note.clone().unwrap_or_default(),
            };
            if let Err(e) = a.write_json(Stage::Preprocess, &record) {
                warn!("预处理产物落盘失败: {e:#}");
            }
        }

        if temp_wav_path.is_none() {
            if caps.can_stream() {
                info!(
                    "音频通道就绪：启用纯内存管道推流 (In-Memory PCM Streaming Pipe，0 磁盘 I/O)"
                );
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "提取音频".into(),
                    progress: 0.14,
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
                    progress: 0.14,
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

        let asr_engine_name = caps.asr_engine_name();

        let _ = tx.send(PipelineEvent::StageChanged(
            Stage::Transcribe.label().into(),
        ));
        let _ = tx.send(PipelineEvent::Progress {
            stage: "语音识别".into(),
            progress: 0.15,
            detail: caps.transcribe_detail().into(),
        });

        let lang_clone = language.clone();
        let tx_whisper = tx.clone();
        // 关闭润色时，识别阶段占满进度条，避免卡在旧的 60%「等待润色」区间。
        let whisper_span = if enable_polish { 0.45 } else { 0.80 };

        // JSON 体积开关：救场开启（阈值<0 且非 SenseVoice）才需要 -ojf 全量 token 概率，
        // 救场关闭时降级轻量 -oj，省掉全量 JSON 体积与解析成本
        self.whisper
            .set_need_token_probs(rescue_logprob < 0.0 && !caps.sensevoice());
        // 质检（低置信复核）需要逐句置信度，因此即使不做救场也要让 whisper 输出 token
        // 概率。与救场标志**分开**存放：本行是显式决策点，不会与上一行的 `false` 互相覆盖。
        self.whisper
            .set_need_token_probs_for_quality(!caps.sensevoice());

        let transcription_started = Instant::now();
        let (mut segments, vad_sec, pure_whisper_sec) = if let Some(segs) = resumed_segments {
            info!("断点续跑：复用已落盘的识别结果（{} 段）", segs.len());
            (segs, 0.0, 0.0)
        } else {
            // 转写通道一次性求解：原先散落的 `is_sensevoice / can_stream / sv_parallel`
            // 三段式 if-else 链收敛到 [`EngineProfile::transcribe_route`]，此处只读结果。
            match caps.transcribe_route(sensevoice_available) {
                TranscribeRoute::SenseVoiceStream => {
                    let sv_engine = dynamic_sensevoice
                        .clone()
                        .context("SenseVoice 引擎未就绪：缺少核心模型或 Python 运行脚本")?;
                    let in_file_stream = input_file.clone();
                    let ffmpeg_stream = ffmpeg.clone();
                    let tx_sv = tx.clone();
                    let lang_sv = lang_clone.clone();

                    let (segs, elapsed) =
                        tokio::task::spawn_blocking(move || -> Result<(Vec<Segment>, f64)> {
                            let mut ffmpeg_child =
                                ffmpeg_stream.spawn_audio_stream(&in_file_stream)?;
                            let ffmpeg_stdout = ffmpeg_child
                                .stdout
                                .take()
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
                                Some(caps.sensevoice_threads),
                                if duration_for_chunks > 0.0 {
                                    Some(duration_for_chunks)
                                } else {
                                    None
                                },
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
                }
                TranscribeRoute::WhisperStream => {
                    let in_file_stream = input_file.clone();
                    let ffmpeg_stream = ffmpeg.clone();
                    let whisper_engine = whisper.clone();
                    let model_override_stream = model_override.clone();
                    // 语音增强链在这里就地生效；`chain_with_speed` 已含 atempo，
                    // 因此它同时取代了原先单独的变速推流，输出时长仍为 dur / speed。
                    let chain_stream = enhance_chain.clone();

                    let (segs, v_sec) =
                        tokio::task::spawn_blocking(move || -> Result<(Vec<Segment>, f64)> {
                            let mut ffmpeg_child = ffmpeg_stream.spawn_audio_stream_filtered(
                                &in_file_stream,
                                None,
                                None,
                                chain_stream.as_deref(),
                            )?;
                            let ffmpeg_stdout = ffmpeg_child
                                .stdout
                                .take()
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
                                if duration_for_chunks > 0.0 {
                                    Some(duration_for_chunks / audio_speed)
                                } else {
                                    None
                                },
                                model_override_stream.as_deref(),
                                Some(Box::new(move |p, info, opt_seg| {
                                    if let Some(mut seg) = opt_seg {
                                        scale_segment_to_original_time(
                                            &mut seg,
                                            audio_speed,
                                            duration_for_chunks,
                                        );
                                        let _ = tx_cb.send(PipelineEvent::SegmentStream(seg));
                                    }
                                    let _ = tx_cb.send(PipelineEvent::Progress {
                                        stage: "语音识别".into(),
                                        progress: 0.15 + p * whisper_span,
                                        detail: if audio_speed > 1.0 {
                                            format!(
                                                "音频 {audio_speed:.2}x 加速识别：{:.0}%",
                                                p * 100.0
                                            )
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
                }
                TranscribeRoute::Chunked { parallel } => {
                    let wav_path = temp_wav_path.as_ref().unwrap().path().to_path_buf();
                    let ffmpeg_chunks = ffmpeg.clone();
                    let whisper_engine = whisper.clone();
                    let sv_engine = dynamic_sensevoice.clone();
                    let model_override_clone = model_override.clone();
                    let chunk_threads = threads.unwrap_or(8);
                    // 压实后音频变短，切块与进度都必须按压实后的时长规划，
                    // 否则会把短音频误判成长音频、切出一堆无语音的碎块。
                    let plan_duration = asr_duration;
                    let cb_restorer = restorer.clone();

                    let (segs, v_sec) = tokio::task::spawn_blocking(move || {
                        let cb: Option<crate::engines::SegmentProgressCb> =
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

                        if parallel {
                            let sv = sv_engine
                                .context("SenseVoice 引擎未就绪：缺少核心模型或 Python 运行脚本")?;
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
                }
                TranscribeRoute::SenseVoiceUnavailable => {
                    anyhow::bail!(
                        "SenseVoice 极速引擎未就绪：缺少核心模型（model.int8.onnx / tokens / vad）或运行环境。\n\
                         请在「性能设置 → 模型与组件」下载 SenseVoice 组件，或在右侧切换为 Whisper 引擎转写。"
                    );
                }
            }
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
        // 断点续跑时落盘的已是最终时间轴，重复还原会二次缩放，故整段跳过。
        if !resumed {
            restore_final_timeline(&mut segments, &restorer, &caps);
            if prep_active {
                crate::subtitle::optimize_segments(&mut segments);
                info!(
                    segments = segments.len(),
                    ratio = restorer.plan.as_deref().map(|p| p.ratio()).unwrap_or(1.0),
                    "预处理时间轴已还原到原始媒体"
                );
            }
        }

        // 转写已完成，临时 wav 不再需要：显式释放守卫，尽早把磁盘空间还给用户。
        // 注意这里不是「唯一的删除点」——取消 / 报错提前返回时守卫的 Drop 会同样生效。
        drop(temp_wav_path.take());

        // best-effort 落盘转写产物（写失败只告警，绝不中断流水线）。
        // 只有真正跑了转写（未命中任一续跑产物）才需要写：命中时 `resumed` 为真，
        // 落盘的识别结果已是最终时间轴，无需覆盖。
        if !resumed {
            if let Some(a) = artifacts.as_ref() {
                if let Err(e) = a.write_segments(Stage::Transcribe, &segments) {
                    warn!("转写产物落盘失败: {e:#}");
                }
            }
        }

        if self.is_cancelled() {
            let _ = tx.send(PipelineEvent::Finished(
                segments.clone(),
                Default::default(),
            ));
            return Ok(segments);
        }

        // ── 阶段 2.5: 置信度二段重解码（快速通场 -nf 的质量兜底）──
        // 快速通场全程禁用温度回退提速；这里只对 avg_logprob 垫底的少数窗口
        // 切出音频、带回退重新解码一次，用几个百分点的时间换回质量。
        let rescue_started = Instant::now();
        let mut rescue_span_count = 0usize;
        let rescue_enabled = rescue_logprob < 0.0
            && !caps.sensevoice()
            && segments.iter().any(|s| s.confidence.is_some());

        // Resume: read the persisted rescue artifact BEFORE computing; a hit skips the compute.
        let rescue_resumed = resume_rescue_segments(artifacts.as_ref(), rescue_enabled, skip_post);
        let rescue_ran = rescue_enabled && !skip_post && rescue_resumed.is_none();
        if let Some(segs) = rescue_resumed {
            info!("断点续跑：复用已落盘的救场结果（{} 段）", segs.len());
            segments = segs;
        }

        if rescue_ran {
            let spans = plan_rescue_spans(&segments, rescue_logprob);
            let total_span_sec: f64 = spans.iter().map(|(s, e)| e - s).sum();
            let media_sec = if duration_for_chunks > 0.0 {
                duration_for_chunks
            } else {
                0.0
            };

            if spans.is_empty() {
                info!("置信度救场：全程置信度良好，无需二次解码");
            } else if media_sec > 0.0 && total_span_sec > media_sec * 0.35 {
                // 低置信占比过高说明是整体难度问题（噪音/口音），逐窗救场不划算，保留原文
                warn!(
                    spans = spans.len(),
                    total_span_sec,
                    media_sec,
                    "低置信片段占比超过 35%，跳过置信度救场以避免耗时失控"
                );
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "语音识别".into(),
                    progress: if enable_polish { 0.60 } else { 0.80 },
                    detail: "低置信片段较多，已跳过二次解码".into(),
                });
            } else {
                let _ = tx.send(PipelineEvent::StageChanged(Stage::Rescue.label().into()));
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

        if rescue_ran {
            if let Some(a) = artifacts.as_ref() {
                if let Err(e) = a.write_segments(Stage::Rescue, &segments) {
                    warn!("救场产物落盘失败: {e:#}");
                }
            }
        }

        // ── 阶段 3: 标点与语法润色（可跳过）──
        let mut qwen_sec = 0.0;
        let mut polish_engine_name = None;
        // 标点/润色是否**真的把结果写回了** `segments`。
        //
        // 与「这一跑跑过这个阶段」分开记：超时 / 失败 / 取消时 `segments` 仍是识别
        // 原文，此时若把 `06_polish.json` 落盘，续跑会把它当成润色成品复用（见
        // `resume_polish_segments`），标点阶段就被永久跳过了。
        let mut polish_applied = false;
        // Resume: only an enabled polish stage may reuse a persisted artifact;
        // a disabled run keeps its fresh segments and ignores stale files.
        let mut polish_resumed = false;
        if enable_polish && !skip_post {
            if let Some(segs) = resume_polish_segments(artifacts.as_ref()) {
                info!("断点续跑：复用已落盘的标点/润色结果（{} 段）", segs.len());
                segments = segs;
                polish_engine_name = Some("断点续跑：复用已落盘的标点/润色结果".to_string());
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "生成字幕".into(),
                    progress: 0.96,
                    detail: "断点续跑：复用已落盘的标点/润色结果".into(),
                });
                polish_resumed = true;
            }
        }
        if !polish_resumed && enable_polish && !segments.is_empty() && !skip_post {
            let mode = polish_mode.unwrap_or_else(|| "punc".to_string());
            let use_punc = (mode == "punc")
                && self
                    .punc
                    .as_ref()
                    .map(|p| p.is_available())
                    .unwrap_or(false);

            if caps.sensevoice() && mode == "punc" {
                polish_engine_name = Some("SenseVoice 原生标点".to_string());
                info!("SenseVoice 原生自带高精度标点与数字规范化(ITN)，跳过 CT-Punc 阶段以实现极致速度");
                // 标点由识别引擎自带：这一次的 `segments` 本身就是「已标点」的结果，
                // 落盘与续跑复用都是对的。
                polish_applied = true;
                let _ = tx.send(PipelineEvent::Progress {
                    stage: "极速标点".into(),
                    progress: 0.95,
                    detail: "SenseVoice 原生自带高精度标点与 ITN，已瞬时就绪".into(),
                });
            } else if use_punc && self.is_cancelled() {
                // 前查：用户在识别收尾期间就点了终止时，不再白起一个 Python 子进程
                // 跑一遍标点。语义与其余检查点一致——取消 ⇒ 不产出成品。
                info!("标点阶段开始前已取消，跳过 CT-Punc 极速标点");
            } else if use_punc {
                polish_engine_name = Some("CT-Punc 极速标点".to_string());
                let _ = tx.send(PipelineEvent::StageChanged(Stage::Punctuate.label().into()));
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
                        // 后查：标点跑完的那一刻用户可能已经终止。此时绝不写回结果
                        // （写回后会经下面的 `polish_ran` 落盘半成品），保持未标点原文。
                        if self.is_cancelled() {
                            info!("标点完成后检测到取消，丢弃本次标点结果");
                        } else {
                            segments = polished;
                            polish_applied = true;
                            qwen_sec = punc_started.elapsed().as_secs_f64();
                            info!(elapsed = ?punc_started.elapsed(), segments = segments.len(), "CT-Punc 极速标点完成");
                            if !skip_post {
                                if let Some(a) = artifacts.as_ref() {
                                    if let Err(e) = a.write_segments(Stage::Punctuate, &segments) {
                                        warn!("标点产物落盘失败: {e:#}");
                                    }
                                }
                            }
                        }
                    }
                    Ok(Err(err)) => {
                        // 取消与超时/失败要给不同文案：取消是用户预期内的静默收尾，
                        // 超时必须明确告警（字幕保持未标点，而不是把半截输出当结果）。
                        match err.downcast_ref::<crate::engines::punc::PuncStageError>() {
                            Some(crate::engines::punc::PuncStageError::Cancelled) => {
                                info!("极速标点已取消，字幕保持未标点");
                            }
                            Some(crate::engines::punc::PuncStageError::TimedOut) => {
                                warn!(error = %err, "极速标点超时，已终止；字幕保持未标点");
                                // 超时不像取消那样在后面的闸门早退，性能报告仍会打印：
                                // 名字若留在「CT-Punc 极速标点」就是在报告一份没发生的标点。
                                polish_engine_name = Some("标点超时（字幕未标点）".to_string());
                                let _ = tx.send(PipelineEvent::Progress {
                                    stage: "极速标点".into(),
                                    progress: 0.95,
                                    detail: "标点引擎超时，已终止；字幕保持未标点".into(),
                                });
                            }
                            None => {
                                warn!(error = %err, "极速标点失败，保留识别原文并继续收尾");
                            }
                        }
                    }
                    Err(err) => {
                        warn!(error = %err, "极速标点任务异常，保留识别原文");
                    }
                }
            } else if mode == "qwen" {
                polish_engine_name = Some("Qwen 字幕润色".to_string());
                let _ = tx.send(PipelineEvent::StageChanged(Stage::Polish.label().into()));
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
                        // 后查：与标点分支同一条语义——用户在润色跑完的那一刻点了终止，
                        // 就不把结果写回（写回后会经 `polish_applied` 落盘半成品）。
                        // Qwen 是分批长循环，`install_cancel_flag` 只保证它在批边界停下，
                        // 停在半路时得到的是「一半润色一半原文」，更不能当成品。
                        if self.is_cancelled() {
                            info!("润色完成后检测到取消，丢弃本次润色结果");
                        } else {
                            segments = polished;
                            polish_applied = true;
                            qwen_sec = polishing_started.elapsed().as_secs_f64();
                            info!(elapsed = ?polishing_started.elapsed(), segments = segments.len(), "Qwen 润色完成");
                        }
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
        } else if !polish_resumed {
            let _ = tx.send(PipelineEvent::StageChanged(Stage::Write.label().into()));
            let _ = tx.send(PipelineEvent::Progress {
                stage: "生成字幕".into(),
                progress: 0.96,
                detail: "已关闭 AI 润色，正在写出字幕...".into(),
            });
            info!("已跳过标点润色");
        }

        // Persist only when the punctuation/polish result was actually applied to
        // `segments` — a timed-out or failed stage leaves the raw ASR text in place,
        // and saving that as `06_polish.json` would make the next resume treat it as
        // the finished polish and skip the stage forever.
        if should_persist_polish_artifact(
            polish_applied,
            polish_resumed,
            enable_polish,
            !segments.is_empty(),
            skip_post,
        ) {
            if let Some(a) = artifacts.as_ref() {
                if let Err(e) = a.write_segments(Stage::Polish, &segments) {
                    warn!("润色产物落盘失败: {e:#}");
                }
            }
        }

        if self.is_cancelled() {
            let _ = tx.send(PipelineEvent::Finished(
                segments.clone(),
                Default::default(),
            ));
            return Ok(segments);
        }

        // ── 阶段 3.5: 说话人分离（F-015，可跳过）──
        // 放在润色之后：分离只读时间轴与音频、不改文本，最后一步做最省事，
        // 也让「导出带说话人前缀」与「界面标签」用的是同一份结果。
        let mut diarization_sec = 0.0;
        if diarization.enabled && !segments.is_empty() && !skip_post {
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
                    let mut found: Vec<u32> = segments.iter().filter_map(|s| s.speaker).collect();
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

        // ── 阶段 4 前的取消检查点 ──
        // 说话人分离（上一阶段）在 spawn_blocking 里做整轨解码 + 聚类，是最后一个
        // 「长阶段」，内部没有取消检查；再往下就直接落盘导出。缺了这道闸，用户在分离
        // 期间点「终止」仍会拿到一份字幕文件——而「终止 ⇒ 不产出成品」是全链路的契约。
        // 与转写后、润色后的两个检查点同一语义（`may_export`）。
        if !may_export(self.is_cancelled()) {
            warn!("任务已取消，跳过最终字幕导出");
            let _ = tx.send(PipelineEvent::Finished(
                segments.clone(),
                Default::default(),
            ));
            return Ok(segments);
        }

        // ── 阶段 4: 输出字幕文件（失败也不阻塞 Finished，避免界面一直转圈）──
        let _ = tx.send(PipelineEvent::StageChanged(Stage::Write.label().into()));
        crate::subtitle::optimize_segments(&mut segments);
        // 断点续跑命中最终产物时，后续阶段全被短路，这里补发一次进度让 UI 有可见反馈。
        if skip_post {
            let _ = tx.send(PipelineEvent::Progress {
                stage: "生成字幕".into(),
                progress: 0.99,
                detail: "断点续跑：复用已完成的全流程产物".into(),
            });
        }
        // best-effort 落盘最终产物：这是「走完全流程」的最终快照（含说话人标签），
        // 下次续跑命中它即可整段短路。写失败只告警。
        if !skip_post {
            if let Some(a) = artifacts.as_ref() {
                if let Err(e) = a.write_segments(Stage::Write, &segments) {
                    warn!("最终字幕产物落盘失败: {e:#}");
                }
            }
        }
        let writing_started = Instant::now();
        // 样式在收尾读出（`AppState` 写入；从未写入时为 `None`）。`None` → 走无样式
        // 入口，逐字节等同历史输出；`max_chars_per_line == 0` 时同样不折行（writer 侧
        // 已保证）。注意：转写收尾**通常还没有译文**（翻译是之后独立的 UI 动作），
        // 所以这里 `infer_export_mode` 一般得到 `RawOnly`——与旧 `write_to_file` 的
        // 内部推断完全一致，双语自动推断的行为没有丢。
        let export_style = self.subtitle_style();
        match write_export_with_style(&segments, &out_path, &output_format, export_style.as_ref()) {
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

        let audio_process_name = caps.audio_process_name(preprocess_note.as_deref());

        let vad_engine_name = caps.vad_engine_name(vad_sec);

        let asr_detail_name =
            caps.asr_detail_name(cores, model_override.as_deref(), use_gpu, threads);

        let export_name = format!("{} 标准字幕格式写出", output_format.to_uppercase());

        let metrics = crate::core::PipelinePerformanceMetrics {
            video_duration,
            voiced_duration_sec,
            ffmpeg_audio_sec: ffmpeg_audio_sec + preprocess_sec,
            audio_process_name: Some(audio_process_name),
            vad_sec,
            vad_engine_name: Some(vad_engine_name.to_string()),
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
        // 同一份报告会被两条路径各写一次：`println!` 直接写 stdout，而 `trace_backend`
        // 的日志 writer（`DualWriter`）把 `info!` 同时写进 stdout 与 logs/*.txt。
        // 结果同一条多行统计在控制台出现两遍、日志文件里也占两份，批量转写时刷屏。
        // 两条路径共用一次「打印名额」判定：先到的打印、后到的丢弃，始终只出现一份。
        if metrics.take_print_slot() {
            if summary_to_stderr() {
                eprintln!("{summary}");
            } else {
                println!("{summary}");
            }
            info!("{}", summary);
        }

        let _ = tx.send(PipelineEvent::Finished(segments.clone(), metrics));
        info!(elapsed = ?pipeline_started.elapsed(), "管线全部执行完成，输出文件: {:?}", out_path);
        Ok(segments)
    }
}

/// 自动导出的格式推断：**与 `SubtitleWriter::write_to_file` 内部的判定同源**
/// （见 `src/subtitle/writer.rs` 里那段 `has_translation()` 分支）——任一段有非空
/// 译文即双语，否则纯原文。
///
/// 为什么在这里复刻：切到带样式的入口 `SubtitleWriter::write_to_file_with_style`
/// 后，`ExportMode` 必须由调用方显式给出，而 writer 侧没有公开的推断函数（该文件不在
/// 本次改动范围）。复刻点只有这一处，且语义逐字保留：空串 / 纯空白译文不算「有译文」
/// （由 `Segment::has_translation` 收口），因此不会因为这次接线把「自动推断双语」弄丢。
fn infer_export_mode(segments: &[Segment]) -> ExportMode {
    if segments.iter().any(|s| s.has_translation()) {
        ExportMode::Bilingual
    } else {
        ExportMode::RawOnly
    }
}

/// 自动导出的落盘入口（样式缺失时的回落点，抽成纯函数便于单测）。
///
/// - `style == None`（管线从未被写入样式）→ 走历史入口 `SubtitleWriter::write_to_file`，
///   输出逐字节不变，绝不 panic；
/// - `style == Some(..)` → 走带样式入口，srt / vtt / ass 按 `max_chars_per_line` 折行，
///   其余格式（json / ttml / ttal / fcpxml / premiere / jianying / txt）在 writer
///   内部**原样回落**，字节不变。
///
/// 两条分支共用同一个 `infer_export_mode`，因此「自动推断双语」在两条路径上一致。
fn write_export_with_style<P: AsRef<Path>>(
    segments: &[Segment],
    path: P,
    format: &str,
    style: Option<&SubtitleStyleConfig>,
) -> Result<()> {
    match style {
        Some(style) => SubtitleWriter::write_to_file_with_style(
            segments,
            path,
            format,
            infer_export_mode(segments),
            style,
        ),
        None => SubtitleWriter::write_to_file(segments, path, format),
    }
}

/// 收尾闸门（纯函数）：当前片段是否允许当作成品导出 / 落盘。
///
/// 「取消 ⇒ 不产出成品」是整条链路的契约：界面侧靠 `cancel_requested` 做静默收尾，
/// 但字幕文件是管线自己写的，所以管线每个长阶段出口都必须过同一道闸。抽成判据
/// 是为了让这条契约可以被单测直接钉住，而不是散成若干个 `is_cancelled()`。
fn may_export(cancelled: bool) -> bool {
    !cancelled
}

fn scale_segment_to_original_time(segment: &mut Segment, speed: f64, original_duration: f64) {
    if speed <= 1.0 {
        return;
    }
    segment.start *= speed;
    segment.end *= speed;
    // 词级时间戳与段共用同一时间轴：段被还原后词必须同步缩放并钳到同一区间，
    // 否则加速音频的流式预览里「段在真实时间、词在模型时间」两套坐标混用，
    // 逐词高亮 / 卡拉OK 滚动会整体错位。
    for w in segment.words.iter_mut() {
        w.start *= speed;
        w.end *= speed;
        w.end = w.end.max(w.start);
        if original_duration > 0.0 {
            w.start = w.start.clamp(0.0, original_duration);
            w.end = w.end.clamp(w.start, original_duration);
        }
    }
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
fn restore_final_timeline(
    segments: &mut [Segment],
    restorer: &TimelineRestorer,
    caps: &EngineCapabilities,
) {
    if caps.sensevoice() {
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
        let is_bad = seg.confidence.is_some_and(|c| c < threshold);
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
pub fn apply_rescued_segments(
    segments: &mut Vec<Segment>,
    span: (f64, f64),
    new_segs: Vec<Segment>,
) {
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
    segments.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
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
        shift_and_clamp_words_to_segment(seg, window_start);
    }
}

/// Shift word timings with the segment and clamp them into the shifted bounds.
///
/// `shift_window_to_media_timeline` only moved segment boundaries; words must
/// follow the same shift, otherwise preview and export read out-of-segment
/// word times (the same invariant `clamp_words_to_segment` gives elsewhere).
fn shift_and_clamp_words_to_segment(seg: &mut Segment, window_start: f64) {
    if seg.words.is_empty() {
        return;
    }
    let (start, end) = (seg.start, seg.end);
    seg.words.retain_mut(|w| {
        w.start += window_start;
        w.end += window_start;
        w.end = w.end.max(w.start);
        if w.end <= start || w.start >= end {
            return false;
        }
        w.start = w.start.clamp(start, end);
        w.end = w.end.clamp(start, end);
        true
    });
}

/// Resume helper: persisted rescue artifact, when rescue is on and post stages run.
///
/// Read BEFORE the rescue compute; on a hit the caller skips the decode, so the
/// artifact must never be re-read AFTER computing (that would override freshly
/// computed segments with an older snapshot).
fn resume_rescue_segments(
    artifacts: Option<&StageArtifacts>,
    rescue_enabled: bool,
    skip_post: bool,
) -> Option<Vec<Segment>> {
    if !rescue_enabled || skip_post {
        return None;
    }
    artifacts
        .and_then(|a| a.read_segments(Stage::Rescue).ok().flatten())
        .filter(|segs| !segs.is_empty())
}

/// Resume helper: persisted punctuation/polish artifact (Polish wins over Punctuate).
fn resume_polish_segments(artifacts: Option<&StageArtifacts>) -> Option<Vec<Segment>> {
    artifacts
        .and_then(|a| a.read_segments(Stage::Polish).ok().flatten())
        .or_else(|| artifacts.and_then(|a| a.read_segments(Stage::Punctuate).ok().flatten()))
        .filter(|segs| !segs.is_empty())
}

/// 是否该把标点/润色阶段的产物落盘（纯函数，便于单测）。
///
/// 判据是「**结果真的写回了** `segments`」（`applied`），而不是「这一跑跑过这个阶段」：
/// 超时、失败、取消时 `segments` 仍是识别原文，落盘等于把一个「还没标点」的产物
/// 登记成成品——[`resume_polish_segments`] 优先复用 `06_polish.json`，下次续跑就会
/// 直接跳过标点阶段，用户永远等不到补标点。
fn should_persist_polish_artifact(
    applied: bool,
    resumed: bool,
    enabled: bool,
    has_segments: bool,
    skip_post: bool,
) -> bool {
    applied && !resumed && enabled && has_segments && !skip_post
}

/// Resume helper: cached media duration, valid only for the SAME input file.
///
/// A record describing another input is stale (task folder reused with a
/// different media) and must not steer chunking; ignoring it makes the caller
/// re-probe and rewrite the record.
fn cached_extract_audio_duration(artifacts: Option<&StageArtifacts>, input: &str) -> Option<f64> {
    let record = artifacts.and_then(|a| {
        a.read_json::<ExtractAudioRecord>(Stage::ExtractAudio)
            .ok()
            .flatten()
    })?;
    if !resume_owner_matches(&record.input, input) {
        warn!(
            artifact = %record.input,
            current = %input,
            "断点续跑：时长记录输入的媒体与本次任务不一致，忽略该产物"
        );
        return None;
    }
    (record.duration_sec > 0.0).then_some(record.duration_sec)
}

/// 媒体路径的等价归一（纯函数）：分隔符统一、折叠重复分隔符、去掉结尾分隔符、
/// 大小写折叠。
///
/// 只做**无损等价**的归一，不做软链 / 相对路径解析：后者要访问文件系统、会引入
/// 失败模式，而判错方向的代价是把 B 的产物认成 A 的。大小写折叠在 Windows（本项目
/// 目标平台）上是对的——同一路径的不同大小写写法指向同一支媒体；在区分大小写的
/// 文件系统上，只有「两支片子的路径除大小写外完全相同且共用同一产物目录」才可能
/// 被误判为同一支，这种情形可以忽略。
fn normalize_media_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        let ch = if ch == '\\' { '/' } else { ch };
        if ch == '/' && out.ends_with('/') {
            continue;
        }
        out.extend(ch.to_lowercase());
    }
    while out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

/// 归属判据（纯函数）：产物落盘时记下的输入 `recorded` 是否就是本次输入 `current`。
///
/// 同一条判据同时服务「片段产物」（[`StageArtifacts::read_segments`]）与
/// 「时长缓存」（[`cached_extract_audio_duration`]），避免两处各写一套比较口径。
fn resume_owner_matches(recorded: &str, current: &str) -> bool {
    normalize_media_path(recorded) == normalize_media_path(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle::segment::WordTiming;
    use crate::subtitle::Segment;

    #[test]
    fn accelerated_audio_timestamps_restore_original_timeline() {
        let mut seg = Segment::new(1, 10.0, 12.0, "测试");
        scale_segment_to_original_time(&mut seg, 1.25, 600.0);
        assert!((seg.start - 12.5).abs() < 1e-9);
        assert!((seg.end - 15.0).abs() < 1e-9);
    }

    /// 只有「结果真的写回了 segments」才允许落盘标点/润色产物。
    ///
    /// 回归护栏：曾经的判据是「这一跑跑过这个阶段」，标点超时/失败/取消时也成立，
    /// 于是把**未标点**的识别原文写成 `06_polish.json`；而续跑优先复用它，
    /// 标点阶段就被永久跳过（用户再点多少次续跑都不会补标点）。
    #[test]
    fn polish_artifact_is_persisted_only_when_the_result_was_applied() {
        // 正常跑完且写回：落盘。
        assert!(should_persist_polish_artifact(
            true, false, true, true, false
        ));
        // 超时 / 失败 / 被取消：segments 仍是识别原文，绝不能登记成成品。
        assert!(!should_persist_polish_artifact(
            false, false, true, true, false
        ));
        // 续跑复用了已有产物：不要覆盖它。
        assert!(!should_persist_polish_artifact(
            true, true, true, true, false
        ));
        // 关润色 / 没字幕 / 跳过收尾：都不写。
        assert!(!should_persist_polish_artifact(
            true, false, false, true, false
        ));
        assert!(!should_persist_polish_artifact(
            true, false, true, false, false
        ));
        assert!(!should_persist_polish_artifact(
            true, false, true, true, true
        ));
    }

    /// 加速音频的流式预览同样要还原词级时间戳：段被缩放、词不缩放 = 逐词高亮错位。
    #[test]
    fn accelerated_preview_scales_word_timings_with_segment() {
        let mut seg = Segment::new(1, 10.0, 12.0, "测试");
        seg.words = vec![
            WordTiming {
                word: "测".to_string(),
                start: 10.0,
                end: 11.0,
            },
            WordTiming {
                word: "试".to_string(),
                start: 11.0,
                end: 12.0,
            },
        ];
        scale_segment_to_original_time(&mut seg, 1.25, 600.0);
        assert!((seg.start - 12.5).abs() < 1e-9, "实得 {}", seg.start);
        assert!((seg.end - 15.0).abs() < 1e-9, "实得 {}", seg.end);
        assert_eq!(seg.words.len(), 2, "词不得在还原时丢失");
        assert!((seg.words[0].start - 12.5).abs() < 1e-9);
        assert!((seg.words[0].end - 13.75).abs() < 1e-9);
        assert!((seg.words[1].start - 13.75).abs() < 1e-9);
        assert!((seg.words[1].end - 15.0).abs() < 1e-9);
        for w in &seg.words {
            assert!(w.start <= w.end);
            assert!(w.start >= seg.start - 1e-9 && w.end <= seg.end + 1e-9);
        }
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
        let restorer = TimelineRestorer {
            plan: None,
            speed: 1.25,
            original_duration: 600.0,
        };
        let mut segs = vec![Segment::new(1, 40.0, 44.0, "测试")];
        restore_final_timeline(&mut segs, &restorer, &caps_fixture(false, true, false));
        assert!(
            (segs[0].start - 50.0).abs() < 1e-9,
            "40s 应还原为 50s，实得 {}",
            segs[0].start
        );
        assert!(
            (segs[0].end - 55.0).abs() < 1e-9,
            "44s 应还原为 55s，实得 {}",
            segs[0].end
        );
    }

    #[test]
    fn final_timeline_restore_skips_sensevoice() {
        // SenseVoice 推流不做变速（spawn_audio_stream 恒 1.0 倍速），
        // 其时间轴已是原始媒体时间轴，不能再乘 speed，否则会凭空拉长字幕。
        let restorer = TimelineRestorer {
            plan: None,
            speed: 1.25,
            original_duration: 600.0,
        };
        let mut segs = vec![Segment::new(1, 40.0, 44.0, "测试")];
        restore_final_timeline(&mut segs, &restorer, &caps_fixture(true, true, false));
        assert!(
            (segs[0].start - 40.0).abs() < 1e-9,
            "SenseVoice 不应被缩放: {}",
            segs[0].start
        );
        assert!((segs[0].end - 44.0).abs() < 1e-9);
    }

    /// 词级时间戳必须与段一起还原到原始时间轴：段被缩放、词不缩放 = 逐词高亮错位。
    #[test]
    fn restore_remaps_word_timings_to_original_timeline() {
        let restorer = TimelineRestorer {
            plan: None,
            speed: 1.25,
            original_duration: 600.0,
        };
        let mut seg = Segment::new(1, 10.0, 12.0, "测试");
        seg.words = vec![
            WordTiming {
                word: "测".to_string(),
                start: 10.0,
                end: 11.0,
            },
            WordTiming {
                word: "试".to_string(),
                start: 11.0,
                end: 12.0,
            },
        ];
        restorer.restore(&mut seg);
        assert!((seg.start - 12.5).abs() < 1e-9, "实得 {}", seg.start);
        assert!((seg.end - 15.0).abs() < 1e-9, "实得 {}", seg.end);
        assert_eq!(seg.words.len(), 2, "词不得在还原时丢失");
        assert!((seg.words[0].start - 12.5).abs() < 1e-9);
        assert!((seg.words[0].end - 13.75).abs() < 1e-9);
        assert!((seg.words[1].start - 13.75).abs() < 1e-9);
        assert!((seg.words[1].end - 15.0).abs() < 1e-9);
        for w in &seg.words {
            assert!(w.start <= w.end);
            assert!(w.start >= seg.start - 1e-9 && w.end <= seg.end + 1e-9);
        }
    }

    #[test]
    fn rescue_window_segments_are_shifted_to_media_timeline() {
        // 救场窗口从原始媒体 1500s 处切出：模型时间轴 0.4s 的句子真实位置是 1500.4s。
        // 预览推流与最终回写共用这个平移函数；漏掉它界面会显示 00:00 的错位时间。
        let mut segs = vec![seg(1, 0.4, 2.1, None)];
        shift_window_to_media_timeline(&mut segs, 1500.0);
        assert!(
            (segs[0].start - 1500.4).abs() < 1e-9,
            "实得 {}",
            segs[0].start
        );
        assert!((segs[0].end - 1502.1).abs() < 1e-9, "实得 {}", segs[0].end);
    }

    /// A stale rescue artifact must be consumed BEFORE the compute (resume
    /// shortcut), never after it: the old read-back overrode fresh segments.
    #[test]
    fn rescue_artifact_read_must_not_override_fresh_segments() {
        let dir = tmp_dir("rescue_resume_override");
        let artifacts = StageArtifacts::new(dir.clone());
        let stale = vec![seg(1, 0.0, 2.0, Some(-1.0))];
        artifacts.write_segments(Stage::Rescue, &stale).unwrap();

        let rescue_enabled = true;
        let skip_post = false;

        let resumed = resume_rescue_segments(Some(&artifacts), rescue_enabled, skip_post);
        let rescue_ran = rescue_enabled && !skip_post && resumed.is_none();
        assert!(resumed.is_some(), "artifact hit must be reused");
        assert!(!rescue_ran, "artifact hit must skip the compute");

        std::fs::remove_file(artifacts.path(Stage::Rescue)).unwrap();
        let resumed = resume_rescue_segments(Some(&artifacts), rescue_enabled, skip_post);
        let rescue_ran = rescue_enabled && !skip_post && resumed.is_none();
        assert!(resumed.is_none(), "no artifact must fall back to compute");
        assert!(rescue_ran);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The resume shortcut is only valid when rescue is enabled and post stages run.
    #[test]
    fn rescue_artifact_ignored_when_rescue_disabled() {
        let dir = tmp_dir("rescue_disabled_ignore");
        let artifacts = StageArtifacts::new(dir.clone());
        let stale = vec![seg(1, 0.0, 2.0, None)];
        artifacts.write_segments(Stage::Rescue, &stale).unwrap();

        assert!(resume_rescue_segments(Some(&artifacts), false, false).is_none());
        assert!(resume_rescue_segments(Some(&artifacts), true, true).is_none());
        assert!(resume_rescue_segments(None, true, false).is_none());
        assert_eq!(
            resume_rescue_segments(Some(&artifacts), true, false),
            Some(stale)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With polish disabled, a stale polish artifact must not override the fresh
    /// segments (the old gate only checked `!skip_post`).
    #[test]
    fn polish_off_ignores_stale_polish_artifact() {
        let dir = tmp_dir("polish_off_ignore");
        let artifacts = StageArtifacts::new(dir.clone());
        let stale = vec![seg(1, 0.0, 2.0, None)];
        artifacts.write_segments(Stage::Polish, &stale).unwrap();

        let enable_polish = false;
        let skip_post = false;
        let mut segments = vec![seg(1, 10.0, 11.0, None)];
        let fresh = segments.clone();
        if enable_polish && !skip_post {
            if let Some(segs) = resume_polish_segments(Some(&artifacts)) {
                segments = segs;
            }
        }
        assert_eq!(segments, fresh);

        // Control: with polish on, the very same artifact is picked up.
        assert_eq!(resume_polish_segments(Some(&artifacts)), Some(stale));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Word timings must follow the window shift and clamp into the shifted
    /// segment; out-of-segment words are dropped.
    #[test]
    fn rescue_shifts_word_timings() {
        let mut first = seg(1, 0.0, 2.0, None);
        first.words = vec![
            WordTiming {
                word: "a".to_string(),
                start: 0.2,
                end: 1.0,
            },
            WordTiming {
                word: "b".to_string(),
                start: 1.0,
                end: 1.8,
            },
            WordTiming {
                word: "c".to_string(),
                start: 1.9,
                end: 2.6,
            },
            WordTiming {
                word: "d".to_string(),
                start: 2.5,
                end: 3.0,
            },
        ];
        let mut segs = vec![first];
        shift_window_to_media_timeline(&mut segs, 100.0);
        let shifted = &segs[0];
        assert!((shifted.start - 100.0).abs() < 1e-9);
        assert!((shifted.end - 102.0).abs() < 1e-9);
        assert_eq!(
            shifted.words.len(),
            3,
            "out-of-segment words must be dropped"
        );
        assert!((shifted.words[0].start - 100.2).abs() < 1e-9);
        assert!((shifted.words[0].end - 101.0).abs() < 1e-9);
        assert!((shifted.words[1].start - 101.0).abs() < 1e-9);
        assert!((shifted.words[1].end - 101.8).abs() < 1e-9);
        assert!((shifted.words[2].start - 101.9).abs() < 1e-9);
        assert!(
            (shifted.words[2].end - 102.0).abs() < 1e-9,
            "word end must clamp to segment end"
        );
        for w in &shifted.words {
            assert!(w.start >= shifted.start - 1e-9);
            assert!(w.end <= shifted.end + 1e-9);
            assert!(w.end >= w.start - 1e-9);
        }
    }

    /// The duration cache must be invalidated when the recorded input differs
    /// from the current one, and must ignore non-positive durations.
    #[test]
    fn extract_audio_cache_invalidated_when_input_changes() {
        let dir = tmp_dir("audio_cache_input");
        let artifacts = StageArtifacts::new(dir.clone());
        let current = "C:/media/a.mp4";

        assert!(cached_extract_audio_duration(Some(&artifacts), current).is_none());

        let record = ExtractAudioRecord {
            input: current.to_string(),
            duration_sec: 123.0,
        };
        artifacts.write_json(Stage::ExtractAudio, &record).unwrap();
        assert_eq!(
            cached_extract_audio_duration(Some(&artifacts), current),
            Some(123.0)
        );
        assert!(cached_extract_audio_duration(Some(&artifacts), "C:/media/b.mp4").is_none());

        let zero = ExtractAudioRecord {
            input: current.to_string(),
            duration_sec: 0.0,
        };
        artifacts.write_json(Stage::ExtractAudio, &zero).unwrap();
        assert!(cached_extract_audio_duration(Some(&artifacts), current).is_none());

        let _ = std::fs::remove_dir_all(&dir);
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
            seg(2, 10.0, 12.0, Some(-0.9)), // 坏
            seg(3, 12.5, 14.0, Some(-0.8)), // 坏，与上者间隙 < 2s，应并入
            seg(4, 20.0, 22.0, Some(-0.2)),
            seg(5, 30.0, 31.0, Some(-1.2)), // 坏，远离，独立窗口
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
        assert!(
            (segs[1].start - 9.7).abs() < 1e-9,
            "新片段应偏移到原始媒体时间轴"
        );
        assert_eq!(segs[3].text, "测试文本");
        assert_eq!(segs[3].index, 4, "应重新连续编号");
    }

    #[test]
    fn apply_rescued_keeps_originals_when_decode_empty() {
        let mut segs = vec![seg(1, 10.0, 12.0, Some(-0.9))];
        apply_rescued_segments(&mut segs, (9.5, 12.5), Vec::new());
        assert_eq!(segs.len(), 1, "空结果不得删掉原片段");
    }

    // ── 取消竞态（终止转写失效）回归 ──
    //
    // 修复前：`run_with_options` 开头会 `cancelled.store(false)` + `whisper.reset()`。
    // 用户在线程刚 spawn、还没跑到这一行时点「终止」，标志被任务自己清零，转写照跑完。
    // 修复后：唯一复位点是派发前的 `begin_task()`，任务线程内不再有任何清标志的写方。

    /// 造一个不碰磁盘 / 显卡 / 子进程的最小管线，只用来观察取消状态机的行为。
    fn test_pipeline() -> TaskPipeline {
        use crate::engines::{FFmpegEngine, LLMEngine, WhisperEngine};
        TaskPipeline::new(
            Arc::new(FFmpegEngine::new("ffmpeg")),
            Arc::new(WhisperEngine::new("whisper-cli", "ggml-small.bin", 4, 1)),
            None,
            Arc::new(LLMEngine::new("llama-cli", "model.gguf", 2048, 4)),
            None,
        )
    }

    /// 用当前线程跑一段 future（不依赖 `#[tokio::test]`，与线程化用例共用同一套构造）。
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("单测用 tokio 运行时")
            .block_on(fut)
    }

    /// 走一次「任务线程入口」，即 `run_with_options`。传一个不存在的媒体路径：只要取消
    /// 已置位，函数会在阶段 1 的第一个检查点就返回空结果，根本不碰 ffmpeg。
    async fn run_task_entry(pipeline: &TaskPipeline) -> Result<Vec<Segment>> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        pipeline
            .run_with_options(
                PathBuf::from("__v2w_missing_media__.mp4"),
                None,
                None,
                "srt".to_string(),
                false,
                None,
                None,
                None,
                -0.65,
                WhisperRuntimeOptions::default(),
                DiarizationOptions::default(),
                tx,
            )
            .await
    }

    /// 同上，但把自动导出指向 `out`：验证「取消的任务不产出字幕文件」。
    async fn run_task_entry_exporting_to(
        pipeline: &TaskPipeline,
        out: PathBuf,
    ) -> Result<Vec<Segment>> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        pipeline
            .run_with_options(
                PathBuf::from("__v2w_missing_media__.mp4"),
                Some(out),
                None,
                "srt".to_string(),
                false,
                None,
                None,
                None,
                -0.65,
                WhisperRuntimeOptions::default(),
                DiarizationOptions::default(),
                tx,
            )
            .await
    }

    /// 场景一：先取消、后启动。上一轮留下的取消位必须被派发前的 `begin_task()` 作废，
    /// 否则新任务一开跑就自杀（进度条一闪、报「已取消」）。
    /// 统计块去向开关：必须可切换，且切换后立刻生效（调用方在任务起手前设置一次）。
    #[test]
    fn summary_destination_can_be_redirected_to_stderr() {
        let before = summary_to_stderr();
        set_summary_to_stderr(true);
        assert!(summary_to_stderr());
        set_summary_to_stderr(false);
        assert!(!summary_to_stderr());
        set_summary_to_stderr(before);
    }

    #[test]
    fn begin_task_clears_stale_cancel_so_new_task_is_not_instantly_cancelled() {
        let pipeline = test_pipeline();
        // 上一轮被用户终止：标志停在 true
        pipeline.cancel();
        assert!(pipeline.is_cancelled(), "cancel() 后应为已取消");
        // 新一轮派发：UI 线程在 spawn 任务线程之前复位
        pipeline.begin_task();
        assert!(
            !pipeline.is_cancelled(),
            "派发前复位后，新任务不得一上来就被取消"
        );
    }

    /// 场景二：任务已启动后取消，必须生效且不被任务自己抹掉。
    #[test]
    fn cancel_after_start_survives_task_entry_and_exits_early() {
        let pipeline = test_pipeline();
        pipeline.begin_task(); // 派发前的复位
        pipeline.cancel(); // 用户在线程起来之后点「终止」

        let out = block_on(run_task_entry(&pipeline)).expect("取消应走空结果的成功返回");
        assert!(out.is_empty(), "已取消的任务不得产出字幕");
        assert!(
            pipeline.is_cancelled(),
            "任务入口（run_with_options）不得抹掉已置位的取消标志"
        );
    }

    /// 场景三：「取消瞬间与任务启动竞争」里对取消最不利的交错。
    ///
    /// 精确到纳秒的「cancel 恰好插在 spawn 与读标志之间」无法在单测里构造——那是调度器
    /// 决定的时序，没有任何可注入的同步点。这里构造的是它的**等价最坏序**：让 `cancel()`
    /// 在任务线程进入 `run_with_options` 之前就完全返回（用 `Barrier` 建立 happens-before），
    /// 于是任务线程一起来读到的就是 `true`。旧实现会在入口把这发取消清零、静默跑完，
    /// 本断言就会失败；新实现里任务线程没有任何清标志的写方，取消必然保留。
    ///
    /// 推理保证（覆盖真正不可构造的那条交错）：新实现下 `cancelled` 的写方只有两个——
    /// `cancel()` 置 true（UI 线程）与 `begin_task()` 置 false（UI 线程、且严格早于
    /// 「状态切成转写中 / 任务线程 spawn」）。任务线程只读不写，因此 `cancel()` 一旦返回，
    /// 之后不存在任何能把它改回 false 的路径；「竞争」只可能表现为「取消早于 begin_task」
    /// （那属于上一轮的取消，被正确作废）或「晚于 begin_task」（被正确保留），不存在
    /// 「被任务自己抹掉」的第三种结果。
    #[test]
    fn cancel_at_task_entry_is_never_erased_by_the_task_thread() {
        use std::sync::Barrier;
        for _ in 0..16 {
            let pipeline = Arc::new(test_pipeline());
            let gate = Arc::new(Barrier::new(2));

            let worker_pipeline = pipeline.clone();
            let worker_gate = gate.clone();
            let worker = std::thread::spawn(move || {
                // 等主线程把「取消」置好再进任务入口——对取消最不利的交错
                worker_gate.wait();
                block_on(run_task_entry(&worker_pipeline))
            });

            pipeline.cancel();
            // Barrier 建立 happens-before：cancel() 一定先于任务线程进入 run_with_options
            gate.wait();
            let _ = worker.join().expect("任务线程不应 panic");

            assert!(
                pipeline.is_cancelled(),
                "任务线程入口不得抹掉已置位的取消标志"
            );
        }
    }

    /// 标点引擎必须被管线一起取消 / 复位：不然用户点「终止」后标点会照跑到自己结束
    /// （正是「标点阶段完全没接终止」这条缺陷），而上一轮的取消又会把下一轮的标点
    /// 阶段直接掐掉。这条用例钉的就是这两条接线的存在。
    #[test]
    fn cancel_and_begin_task_reach_punc_engine() {
        use crate::engines::{FFmpegEngine, LLMEngine, PunctuationEngine, WhisperEngine};
        let punc = Arc::new(PunctuationEngine::with_python_and_provider(
            "runner.py",
            "model.onnx",
            1,
            "python",
            "cpu",
        ));
        let pipeline = TaskPipeline::new(
            Arc::new(FFmpegEngine::new("ffmpeg")),
            Arc::new(WhisperEngine::new("whisper-cli", "ggml-small.bin", 4, 1)),
            None,
            Arc::new(LLMEngine::new("llama-cli", "model.gguf", 2048, 4)),
            Some(punc.clone()),
        );

        assert!(!punc.is_cancelled(), "初始应为未取消");
        pipeline.cancel();
        assert!(pipeline.is_cancelled());
        assert!(punc.is_cancelled(), "cancel() 必须把标点引擎也一起取消");

        pipeline.begin_task();
        assert!(!pipeline.is_cancelled());
        assert!(
            !punc.is_cancelled(),
            "begin_task() 必须复位标点引擎的取消标志，否则新任务的标点会被上一轮掐掉"
        );
    }

    // ── 自动导出接线：样式回落（缺省不改变历史输出）+ mode 推断（不与 writer 漂移）──

    /// 单测用的临时目录（每个用例一个，跑完自清理）。
    fn tmp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "v2w_pipeline_export_{}_{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("测试临时目录应可创建");
        dir
    }

    /// 交接前管线不持有样式：`subtitle_style()` 必须是 `None`（回落的前提）；
    /// 写入后能原样读回（`AppState` 构造 / 每次样式变更时用它写快照）。
    #[test]
    fn subtitle_style_is_none_until_set_then_round_trips() {
        let pipeline = test_pipeline();
        assert!(
            pipeline.subtitle_style().is_none(),
            "未写入样式时必须为 None，否则自动导出会偏离历史行为"
        );
        let style = SubtitleStyleConfig {
            max_chars_per_line: 12,
            ..SubtitleStyleConfig::default()
        };
        pipeline.set_subtitle_style(style.clone());
        assert_eq!(pipeline.subtitle_style().as_ref(), Some(&style));
    }

    /// mode 推断与 `SubtitleWriter::write_to_file` 同源：空串 / 纯空白译文不算「有译文」，
    /// 因此「整片还没翻译」的转写收尾一定落到 `RawOnly`。
    #[test]
    fn infer_export_mode_matches_writer_rules() {
        assert_eq!(infer_export_mode(&[]), ExportMode::RawOnly);

        let none = vec![Segment::new(1, 0.0, 1.0, "原文")];
        assert_eq!(infer_export_mode(&none), ExportMode::RawOnly);

        let mut blank = Segment::new(1, 0.0, 1.0, "原文");
        blank.translation = Some("   ".to_string());
        assert_eq!(
            infer_export_mode(&[blank]),
            ExportMode::RawOnly,
            "空白译文不算双语"
        );

        let mut translated = Segment::new(1, 0.0, 1.0, "原文");
        translated.translation = Some("translated".to_string());
        assert_eq!(infer_export_mode(&[translated]), ExportMode::Bilingual);
    }

    /// 缺省回落：`None` 与历史入口逐字节一致；`Some` 但 `max_chars_per_line == 0`
    /// 时同样逐字节一致（老配置 / 极端值不被这次接线改变）。两种 mode 都验：
    /// 转写收尾的 RawOnly，以及工作台里已有译文时的 Bilingual。
    #[test]
    fn auto_export_matches_legacy_bytes_when_unstyled_or_zero_wrap() {
        let mut bilingual = Segment::new(1, 0.0, 1.5, "今天天气很好，我们一起去公园散步。");
        bilingual.translation = Some("A fine day, let us walk in the park together.".into());
        let bilingual = vec![bilingual];
        let raw_only = vec![Segment::new(
            1,
            0.0,
            1.5,
            "今天天气很好，我们一起去公园散步。",
        )];
        let zero = SubtitleStyleConfig {
            max_chars_per_line: 0,
            ..SubtitleStyleConfig::default()
        };

        let dir = tmp_dir("fallback");
        for (tag, segs) in [("bilingual", &bilingual), ("raw", &raw_only)] {
            let legacy = dir.join(format!("legacy_{tag}.srt"));
            SubtitleWriter::write_to_file(segs, &legacy, "srt").unwrap();

            // 样式缺失：必须回落到历史入口
            let none_style = dir.join(format!("none_{tag}.srt"));
            write_export_with_style(segs, &none_style, "srt", None).unwrap();

            // 有样式但不折行：同样必须逐字节一致
            let zero_style = dir.join(format!("zero_{tag}.srt"));
            write_export_with_style(segs, &zero_style, "srt", Some(&zero)).unwrap();

            let legacy_bytes = std::fs::read(&legacy).unwrap();
            assert_eq!(
                legacy_bytes,
                std::fs::read(&none_style).unwrap(),
                "{tag}: 样式缺失必须与历史输出逐字节一致"
            );
            assert_eq!(
                legacy_bytes,
                std::fs::read(&zero_style).unwrap(),
                "{tag}: max_chars_per_line == 0 必须与历史输出逐字节一致"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 接线真的生效：管线被写入样式后，自动导出按 `max_chars_per_line` 折行
    /// （转写收尾无译文 → RawOnly，正是真实场景）。
    #[test]
    fn auto_export_wraps_when_style_present() {
        let segs = vec![Segment::new(
            1,
            0.0,
            3.0,
            "今天天气很好，我们一起去公园散步，然后回家吃饭，最后各自回家睡觉。",
        )];
        assert_eq!(infer_export_mode(&segs), ExportMode::RawOnly);

        let style = SubtitleStyleConfig {
            max_chars_per_line: 10,
            ..SubtitleStyleConfig::default()
        };
        let dir = tmp_dir("wrap");
        let plain = dir.join("plain.srt");
        SubtitleWriter::write_to_file(&segs, &plain, "srt").unwrap();
        let styled = dir.join("styled.srt");
        write_export_with_style(&segs, &styled, "srt", Some(&style)).unwrap();

        let plain_text = std::fs::read_to_string(&plain).unwrap();
        let styled_text = std::fs::read_to_string(&styled).unwrap();
        assert_ne!(plain_text, styled_text, "带样式导出必须真的折行");
        for line in styled_text.lines().filter(|l| {
            !l.is_empty() && !l.contains("-->") && !l.chars().all(|c| c.is_ascii_digit())
        }) {
            // 折行会把行尾标点并回上一行，允许 +2 的余量
            assert!(line.chars().count() <= 12, "折行后单行超长: {line:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 引擎能力抽象：真值表、通道判定与线程数 clamp ──

    /// 构造能力夹具：只关心判定字段，单会话线程数固定 8（不参与通道判定）。
    fn caps_fixture(sensevoice: bool, stream: bool, parallel_chunks: bool) -> EngineCapabilities {
        EngineCapabilities {
            sensevoice,
            stream,
            parallel_chunks,
            sensevoice_threads: 8,
        }
    }

    /// 纯 CPU、≥4 核、≥180s、1.0x 的 SenseVoice：切块并行，并放弃纯内存推流。
    #[test]
    fn engine_caps_long_sensevoice_multicore_prefers_parallel_chunks() {
        let caps = EngineCapabilities::plan(true, true, false, 1.0, 600.0, 8, None, 300.0);
        assert!(caps.sensevoice);
        assert!(caps.parallel_chunks);
        assert!(!caps.stream, "切块并行需要完整临时 WAV，不能走推流");
    }

    /// 时长不足 180s：不切块，退回 0 磁盘 I/O 的推流通道。
    #[test]
    fn engine_caps_short_sensevoice_streams_without_parallel() {
        let caps = EngineCapabilities::plan(true, true, false, 1.0, 60.0, 8, None, 300.0);
        assert!(caps.sensevoice);
        assert!(!caps.parallel_chunks);
        assert!(caps.stream);
    }

    /// 非 SenseVoice + GPU：长音频也不切块（切块只服务 SenseVoice），直接推流。
    #[test]
    fn engine_caps_gpu_streams_for_long_whisper() {
        let caps = EngineCapabilities::plan(false, false, true, 1.0, 3600.0, 2, None, 300.0);
        assert!(!caps.sensevoice);
        assert!(!caps.parallel_chunks);
        assert!(caps.stream);
    }

    /// 并行的四个否决条件各自单独生效：缺引擎实例 / 有 GPU / 变速 / 核数不足。
    #[test]
    fn engine_caps_parallel_requires_sensevoice_available_and_no_gpu() {
        let no_engine = EngineCapabilities::plan(true, false, false, 1.0, 600.0, 8, None, 300.0);
        assert!(!no_engine.parallel_chunks, "缺 SenseVoice 实例不得并行");
        assert!(no_engine.stream);

        let gpu = EngineCapabilities::plan(true, true, true, 1.0, 600.0, 8, None, 300.0);
        assert!(!gpu.parallel_chunks, "有 GPU 不得并行");
        assert!(gpu.stream);

        let sped = EngineCapabilities::plan(true, true, false, 1.25, 600.0, 8, None, 300.0);
        assert!(!sped.parallel_chunks, "音频加速 > 1.0 不得并行");
        assert!(sped.stream);

        let few_cores = EngineCapabilities::plan(true, true, false, 1.0, 600.0, 2, None, 300.0);
        assert!(!few_cores.parallel_chunks, "核数 < 4 不得并行");
        assert!(few_cores.stream);
    }

    /// 非 SenseVoice、CPU、超过阈值的长音频：既不能并行也不满足推流条件（走临时 WAV 切块）。
    #[test]
    fn engine_caps_long_whisper_cpu_does_not_stream() {
        let caps = EngineCapabilities::plan(false, false, false, 1.0, 3600.0, 8, None, 300.0);
        assert!(!caps.sensevoice);
        assert!(!caps.parallel_chunks);
        assert!(!caps.stream);
    }

    /// 单会话线程数必须按引擎上下界 clamp：`None` → 上限，过小 → 下界，过大 → 上限。
    #[test]
    fn engine_caps_threads_clamped_to_engine_bounds() {
        let max = crate::engines::sensevoice::MAX_THREADS;
        let min = crate::engines::sensevoice::MIN_THREADS;
        let default = EngineCapabilities::plan(false, false, false, 1.0, 10.0, 4, None, 300.0);
        assert_eq!(default.sensevoice_threads, max, "None 应回落到引擎上限");
        let low = EngineCapabilities::plan(false, false, false, 1.0, 10.0, 4, Some(0), 300.0);
        assert_eq!(low.sensevoice_threads, min, "下界应被 clamp 到 MIN_THREADS");
        let high = EngineCapabilities::plan(false, false, false, 1.0, 10.0, 4, Some(999), 300.0);
        assert_eq!(
            high.sensevoice_threads, max,
            "上界应被 clamp 到 MAX_THREADS"
        );
    }

    /// 转写通道的判定顺序必须与收敛前的 if-else 链逐条一致：
    /// SenseVoice 推流 → Whisper 推流 → 临时 WAV 切块。
    #[test]
    fn transcribe_route_matches_legacy_branch_order() {
        // SenseVoice 已就绪：优先走 SenseVoice 推流分支，即便能力位同时指向并行切块
        // （收敛前 `is_sensevoice && sensevoice.is_some()` 同样先于切块分支命中）。
        assert_eq!(
            caps_fixture(true, true, false).transcribe_route(true),
            TranscribeRoute::SenseVoiceStream
        );
        assert_eq!(
            caps_fixture(true, false, true).transcribe_route(true),
            TranscribeRoute::SenseVoiceStream
        );
        // 未选 SenseVoice 且可推流：走 Whisper 推流。
        assert_eq!(
            caps_fixture(false, true, false).transcribe_route(false),
            TranscribeRoute::WhisperStream
        );
        // 两条推流通道都不成立：落到临时 WAV 切块，`parallel` 沿用并行能力位。
        assert_eq!(
            caps_fixture(false, false, false).transcribe_route(false),
            TranscribeRoute::Chunked { parallel: false }
        );
        assert_eq!(
            caps_fixture(false, false, true).transcribe_route(false),
            TranscribeRoute::Chunked { parallel: true }
        );
    }

    /// 引擎展示名必须与收敛前的字符串字面量逐字一致（性能面板 / 界面直接展示）。
    #[test]
    fn engine_display_names_are_verbatim() {
        let sv = caps_fixture(true, true, false);
        let whisper_stream = caps_fixture(false, true, false);
        let whisper_chunked = caps_fixture(false, false, false);

        assert_eq!(sv.asr_engine_name(), "SenseVoice 极速转写");
        assert_eq!(whisper_stream.asr_engine_name(), "Whisper 神经转写");
        assert_eq!(
            sv.transcribe_detail(),
            "SenseVoice 正在通过纯内存管道极速转写语音..."
        );
        assert_eq!(
            whisper_stream.transcribe_detail(),
            "Whisper 正在通过纯内存管道流式转写语音..."
        );
        assert_eq!(
            whisper_chunked.transcribe_detail(),
            "Whisper 模型正在转写语音..."
        );

        assert_eq!(sv.vad_engine_name(1.0), "SenseVoice 融合流式 VAD");
        assert_eq!(
            whisper_stream.vad_engine_name(2.0),
            "Silero VAD 毫秒级语音切片压实"
        );
        assert_eq!(
            whisper_stream.vad_engine_name(0.0),
            "无独立 VAD (全音频直接输入)"
        );

        assert_eq!(
            whisper_stream.audio_process_name(Some("语音增强")),
            "FFmpeg 单遍解码 + 语音增强"
        );
        assert_eq!(
            whisper_stream.audio_process_name(None),
            "纯内存 PCM 匿名管道推流 (0 物理磁盘 I/O)"
        );
        assert_eq!(
            whisper_chunked.audio_process_name(None),
            "FFmpeg 抽取临时 WAV 文件"
        );

        assert_eq!(
            sv.asr_detail_name(8, None, false, None),
            "SenseVoice-Small INT8 (非自回归单次前向 · 8线程)"
        );
        assert_eq!(
            whisper_stream.asr_detail_name(8, None, true, Some(4)),
            "Whisper ggml-model.bin (GPU加速 · 4线程)"
        );
    }

    // ── Stage 枚举与阶段产物落盘 ──

    /// 每个阶段的中文标签与产物文件名都必须唯一且非空（UI 标签是 StageChanged 的键）。
    #[test]
    fn stage_labels_and_artifact_files_are_unique_and_non_empty() {
        let mut labels = std::collections::HashSet::new();
        let mut files = std::collections::HashSet::new();
        for stage in Stage::ALL {
            assert!(!stage.label().is_empty(), "{stage:?} 标签为空");
            assert!(!stage.artifact_file().is_empty(), "{stage:?} 产物名为空");
            assert!(labels.insert(stage.label()), "重复标签: {}", stage.label());
            assert!(
                files.insert(stage.artifact_file()),
                "重复产物名: {}",
                stage.artifact_file()
            );
        }
        assert_eq!(labels.len(), Stage::ALL.len());
        assert_eq!(files.len(), Stage::ALL.len());
    }

    /// UI 依赖的收尾阶段标签必须与历史字面量逐字一致。
    #[test]
    fn stage_write_label_matches_ui_literal() {
        assert_eq!(Stage::Write.label(), "生成字幕");
    }

    /// 落盘 → 读取 round-trip；未落盘返回 `Ok(None)`；坏 JSON 返回 `Err`；不留 `.part` 残留。
    #[test]
    fn stage_artifacts_round_trip_and_tolerates_bad_json() {
        let dir = tmp_dir("stage_artifacts");
        let artifacts = StageArtifacts::new(dir.clone());

        assert!(!artifacts.exists(Stage::Transcribe));
        assert!(
            artifacts
                .read_segments(Stage::Transcribe)
                .unwrap()
                .is_none(),
            "文件不存在必须返回 Ok(None)"
        );

        let segs = vec![
            Segment::new(1, 0.0, 1.5, "第一句"),
            Segment::new(2, 1.5, 3.0, "第二句"),
        ];
        artifacts.write_segments(Stage::Transcribe, &segs).unwrap();
        assert!(artifacts.exists(Stage::Transcribe));
        assert!(artifacts.path(Stage::Transcribe).is_file());
        assert!(
            !artifacts
                .path(Stage::Transcribe)
                .with_extension("json.part")
                .exists(),
            "原子写不应留下 .part 残留"
        );
        assert_eq!(
            artifacts.read_segments(Stage::Transcribe).unwrap().unwrap(),
            segs
        );

        // 解析失败必须是 Err（调用方自行决定容错），而不是 panic / Ok(None)
        std::fs::write(artifacts.path(Stage::Transcribe), b"{ not json").unwrap();
        assert!(artifacts.read_segments(Stage::Transcribe).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 泛型 JSON 落盘 / 读取：往返相等；文件不存在返回 `Ok(None)`；坏 JSON 返回 `Err`；不留 `.part`。
    #[test]
    fn stage_artifacts_json_round_trip() {
        let dir = tmp_dir("stage_json");
        let artifacts = StageArtifacts::new(dir.clone());

        // 文件不存在 → Ok(None)
        let missing: Option<StageRecord> = artifacts.read_json(Stage::Preprocess).unwrap();
        assert!(missing.is_none(), "文件不存在必须返回 Ok(None)");

        let record = StageRecord {
            detail: "语音增强 + 停顿压实".to_string(),
        };
        artifacts.write_json(Stage::Preprocess, &record).unwrap();
        assert!(artifacts.exists(Stage::Preprocess));
        assert!(
            !artifacts
                .path(Stage::Preprocess)
                .with_extension("json.part")
                .exists(),
            "泛型 JSON 原子写不应留下 .part 残留"
        );
        let back: StageRecord = artifacts.read_json(Stage::Preprocess).unwrap().unwrap();
        assert_eq!(back, record, "write_json/read_json 必须往返相等");

        // 坏 JSON → Err（不是 panic / Ok(None)）
        std::fs::write(artifacts.path(Stage::Preprocess), b"{ not json").unwrap();
        let bad = artifacts.read_json::<StageRecord>(Stage::Preprocess);
        assert!(bad.is_err(), "坏 JSON 必须返回 Err");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 两个阶段记录结构都能 JSON 往返。
    #[test]
    fn stage_record_types_round_trip() {
        let dir = tmp_dir("stage_records");
        let artifacts = StageArtifacts::new(dir.clone());

        let audio = ExtractAudioRecord {
            input: "C:/media/demo.mp4".to_string(),
            duration_sec: 123.456,
        };
        artifacts.write_json(Stage::ExtractAudio, &audio).unwrap();
        let audio_back: ExtractAudioRecord =
            artifacts.read_json(Stage::ExtractAudio).unwrap().unwrap();
        assert_eq!(audio_back, audio, "ExtractAudioRecord 必须往返相等");

        let note = StageRecord {
            detail: "语音增强（降噪 / 电平归一，在音频通道就地生效）".to_string(),
        };
        artifacts.write_json(Stage::Preprocess, &note).unwrap();
        let note_back: StageRecord = artifacts.read_json(Stage::Preprocess).unwrap().unwrap();
        assert_eq!(note_back, note, "StageRecord 必须往返相等");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 断点续跑：产物归属 ──
    //
    // 修复前：`Stage::Write` / `Stage::Transcribe` / `Stage::Rescue` 等片段产物只以
    // 「文件存在且能解析」为续跑判据，完全没有绑定输入文件。`--artifact-dir` 复用
    // 同一目录跑第二个文件（或目录名只按文件名主干去重、两支片子主干同名）时，
    // `07_write.json` 一命中就 `skip_post`，整条 ASR + 后处理全跳过，把 A 片子的
    // 识别结果贴到 B 片子并导出。
    //
    // 修复后：片段产物用归属信封落盘（片段 + 产出它的输入），读取时校验归属；
    // 不一致或读不出归属（老格式裸数组）一律按「没有产物」处理，走全流程重算。

    /// 归属判据是纯函数：覆盖分隔符 / 大小写 / 结尾分隔符 / 重复分隔符的等价写法，
    /// 以及「同主干不同目录」这种必须判为不同片子的情形。
    #[test]
    fn resume_owner_matches_normalizes_equivalent_paths() {
        assert!(resume_owner_matches(r"C:\media\a.mp4", "c:/media/a.mp4"));
        assert!(resume_owner_matches("c:/media/a.mp4", "C:/MEDIA/A.MP4"));
        assert!(resume_owner_matches("c:/media/a.mp4/", "c:/media/a.mp4"));
        assert!(resume_owner_matches("c://media//a.mp4", "c:/media/a.mp4"));
        assert!(resume_owner_matches(r"c:\media\a.mp4", "c:/media/a.mp4/"));

        assert!(!resume_owner_matches("c:/media/a.mp4", "c:/media/b.mp4"));
        assert!(
            !resume_owner_matches("a/talk.mp4", "b/talk.mp4"),
            "同主干不同目录是两支片子，不得互相续跑"
        );
        assert!(!resume_owner_matches("", "c:/media/a.mp4"));
    }

    /// 归属校验：A 落的产物用 B 读取必须拿不到；同一输入（等价写法）可读回；
    /// 无归属句柄保持历史行为；拒绝时不删文件（换回 A 仍能续跑）。
    #[test]
    fn stage_artifacts_owned_by_rejects_foreign_segments() {
        let dir = tmp_dir("stage_owner");
        let writer = StageArtifacts::owned_by(dir.clone(), r"C:\media\a.mp4");
        let segs = vec![Segment::new(1, 0.0, 1.5, "甲片的字幕")];
        writer.write_segments(Stage::Write, &segs).unwrap();

        // 同一输入（分隔符 + 大小写归一后等价）→ 可续跑
        assert_eq!(
            StageArtifacts::owned_by(dir.clone(), "C:/MEDIA/A.MP4")
                .read_segments(Stage::Write)
                .unwrap(),
            Some(segs.clone())
        );
        // 另一支片子 → 归属不符，按「没有产物」处理
        assert_eq!(
            StageArtifacts::owned_by(dir.clone(), "c:/media/b.mp4")
                .read_segments(Stage::Write)
                .unwrap(),
            None,
            "目录被复用跑第二支片子时，A 的产物必须被拒绝"
        );
        // 无归属句柄（单测 / 历史行为）不校验
        assert_eq!(
            StageArtifacts::new(dir.clone())
                .read_segments(Stage::Write)
                .unwrap(),
            Some(segs)
        );
        // 拒绝不能把文件删掉：下次换回同一支片子还能续跑
        assert!(writer.exists(Stage::Write));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老格式（裸片段数组）没有归属字段：保守拒绝，按「没有产物」处理并重算。
    #[test]
    fn stage_artifacts_legacy_bare_array_is_not_resumable() {
        let dir = tmp_dir("stage_legacy");
        let artifacts = StageArtifacts::owned_by(dir.clone(), "c:/media/a.mp4");
        let legacy = vec![Segment::new(1, 0.0, 1.0, "旧格式")];
        std::fs::write(
            artifacts.path(Stage::Transcribe),
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();

        assert!(
            artifacts
                .read_segments(Stage::Transcribe)
                .ok()
                .flatten()
                .is_none(),
            "老格式产物无归属信息，必须保守拒绝（走全流程重算）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 取消语义：终止 ⇒ 不产出成品 ──

    /// 收尾闸门：取消后不得导出，未取消才允许。
    #[test]
    fn may_export_blocks_delivery_after_cancel() {
        assert!(may_export(false), "未取消应允许导出");
        assert!(!may_export(true), "取消后不得导出");
    }

    /// 已取消的任务不得写出任何字幕文件。这里走的是阶段 1 的检查点；「说话人分离
    /// 期间取消」由一个独立的导出前检查点覆盖，但那条路径需要真媒体才能走到。
    #[test]
    fn cancelled_task_never_writes_output_file() {
        let dir = tmp_dir("cancel_no_export");
        let out = dir.join("out.srt");

        let pipeline = test_pipeline();
        pipeline.begin_task();
        pipeline.cancel();

        let segments = block_on(run_task_entry_exporting_to(&pipeline, out.clone()))
            .expect("取消应走空结果的成功返回");
        assert!(segments.is_empty(), "已取消的任务不得产出字幕");
        assert!(!out.exists(), "取消后不得写出字幕文件");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
