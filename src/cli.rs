//! CLI / 无头模式入口。
//!
//! 复用 GUI 侧同一套库内核（`AppConfig` + `TaskPipeline`），把「转写并导出字幕」
//! 暴露成可脚本化的命令行：
//!
//! ```text
//! voice2word transcribe <输入文件> [--output <文件>] [--format <格式>] ...
//! ```
//!
//! 设计约束：
//! - 参数解析手写，不引入新依赖；
//! - 无参数或 `--gui` 时 [`run`] 返回 `None`，由 `main` 继续走原有 GPUI 启动路径，
//!   桌面行为与加入 CLI 之前完全一致；
//! - 进度与诊断写 stderr，最终结果路径写 stdout，便于重定向只取结果；
//! - 落盘由管线负责；本模块只在管线未产出目标文件时做一次兜底重写。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::sync::mpsc;

use crate::core::pipeline::{DiarizationOptions, PreprocessOptions, WhisperRuntimeOptions};
use crate::core::{PipelineEvent, TaskPipeline};
use crate::engines::{
    self, FFmpegEngine, HardwareProfile, LLMEngine, PunctuationEngine, SenseVoiceEngine,
    WhisperEngine,
};
use crate::subtitle::{Segment, SubtitleWriter};
use crate::utils::AppConfig;

/// 允许导出的字幕格式；顺序与 [`usage`] 中展示的一致。
pub const SUPPORTED_FORMATS: [&str; 7] = ["srt", "ass", "vtt", "json", "txt", "ttml", "ttal"];

/// 未指定 `--format` 时使用的格式。
const DEFAULT_FORMAT: &str = "srt";

/// 转写子命令解析后的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscribeArgs {
    /// 待转写的音视频文件。
    pub input: PathBuf,
    /// 输出字幕路径；`None` 表示与输入同目录、同名的 `.<format>`。
    pub output: Option<PathBuf>,
    /// 输出格式，恒为 [`SUPPORTED_FORMATS`] 中的小写项。
    pub format: String,
    /// 源语言；`None` 表示沿用 `config.toml` 的 `pipeline.language`。
    pub language: Option<String>,
    /// 目标语言。无头模式当前只记录，不接入翻译阶段。
    pub translate: Option<String>,
    /// 覆盖 Whisper 模型文件。
    pub model: Option<PathBuf>,
    /// 覆盖推理线程数。
    pub threads: Option<u32>,
    /// 强制开启润色/标点阶段。
    pub polish: bool,
    /// 复用阶段产物目录中已有的识别结果（断点续跑）。
    pub resume: bool,
    /// 阶段产物落盘目录；`None` 表示不落盘（默认，与历史行为逐字节一致）。
    pub artifact_dir: Option<PathBuf>,
}

/// 命令行解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    /// 进入图形界面（无参数或 `--gui`）。
    Gui,
    /// 打印帮助。
    Help,
    /// 打印版本号。
    Version,
    /// 无头转写。
    Transcribe(Box<TranscribeArgs>),
}

/// 命令行解析错误（面向用户的一段中文说明）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    message: String,
}

impl CliError {
    /// 用一条说明构造错误。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 取说明文本。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// 把 `--threads` 收敛到 `[2, 本机逻辑核数]`。
///
/// 命令行是唯一可以绕过设置页滑条量程的入口：传 `--threads 256` 时 whisper-cli 会真的
/// 开出 256 个线程互相抢 CPU，转写反而更慢。收敛口径复用 GUI 的
/// [`crate::app::AppState::clamp_threads_to_cores`]，保证两端上限只此一处、不会打架。
/// 生效值与请求值不同时打印一行提示，方便用户理解实际用的是多少线程。
fn bound_cli_threads(requested: u32) -> u32 {
    let cores = crate::app::AppState::logical_cores();
    let bounded = crate::app::AppState::clamp_threads_to_cores(requested, cores);
    if bounded != requested {
        eprintln!("提示: --threads {requested} 已收敛为 {bounded}（本机逻辑核心数 {cores}）");
    }
    bounded
}

/// 帮助文本。
pub fn usage() -> String {
    format!(
        r#"Voice2Word {version} — 音视频智能字幕生成器（无头模式）

用法:
  voice2word [--gui]                               启动图形界面（默认，无参数等同）
  voice2word transcribe <输入文件> [选项]           转写并导出字幕
  voice2word --help | -h                           显示本帮助
  voice2word --version | -V                        显示版本号

transcribe 选项:
  -o, --output <文件>          输出字幕路径（默认：与输入同目录、同名的 .<格式>）
      --format <格式>          输出格式，可选 {formats}（默认 {default}）
      --lang, --language <代码> 指定源语言（如 zh / en / ja / auto；默认取 config.toml）
      --translate <语言>       目标语言（当前仅记录，翻译阶段未接入）
      --model <路径>           覆盖 Whisper 模型文件
      --threads <数量>         推理线程数（正整数，覆盖 config.toml；超过本机逻辑核数会自动收敛）
      --polish                 强制开启润色/标点（默认沿用 config.toml）
      --resume                 复用阶段产物目录中已有的识别结果（断点续跑）
      --artifact-dir <路径>     指定阶段产物目录（默认 <输入目录>/.voice2word-stages/<文件名>）
  -h, --help                   显示本帮助

示例:
  voice2word transcribe talk.mp4
  voice2word transcribe talk.mp4 --format vtt --lang zh -o out/talk.vtt"#,
        version = env!("CARGO_PKG_VERSION"),
        formats = SUPPORTED_FORMATS.join("|"),
        default = DEFAULT_FORMAT,
    )
}

/// 解析命令行（`args` 含 argv[0]）。
pub fn parse<I, S>(args: I) -> std::result::Result<CliAction, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let argv: Vec<String> = args.into_iter().map(Into::into).collect();
    let rest: &[String] = if argv.is_empty() { &[] } else { &argv[1..] };

    let first = match rest.first() {
        None => return Ok(CliAction::Gui),
        Some(first) => first.as_str(),
    };

    match first {
        "--help" | "-h" | "help" => Ok(CliAction::Help),
        "--version" | "-V" | "version" => Ok(CliAction::Version),
        "--gui" => Ok(CliAction::Gui),
        "transcribe" => parse_transcribe(&rest[1..]),
        other if other.starts_with('-') => Err(CliError::new(format!("未知选项: {other}"))),
        other => Err(CliError::new(format!("未知子命令: {other}"))),
    }
}

/// 解析 `transcribe` 子命令的参数。
fn parse_transcribe(args: &[String]) -> std::result::Result<CliAction, CliError> {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut format = DEFAULT_FORMAT.to_string();
    let mut language: Option<String> = None;
    let mut translate: Option<String> = None;
    let mut model: Option<PathBuf> = None;
    let mut threads: Option<u32> = None;
    let mut polish = false;
    let mut resume = false;
    let mut artifact_dir: Option<PathBuf> = None;

    // `--` 之后一律按位置参数处理（文件名以 '-' 开头时的标准转义手段）。
    let mut positional_only = false;

    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].as_str();
        if positional_only {
            if input.is_some() {
                return Err(CliError::new(format!("多余的参数: {arg}")));
            }
            input = Some(PathBuf::from(arg));
            i += 1;
            continue;
        }
        if arg == "--" {
            positional_only = true;
            i += 1;
            continue;
        }
        if arg == "-h" || arg == "--help" {
            return Ok(CliAction::Help);
        }
        let (flag, inline) = split_flag(arg);
        match flag {
            "-o" | "--output" => {
                output = Some(PathBuf::from(take_value(flag, inline, args, &mut i)?));
            }
            "--format" => {
                let value = take_value(flag, inline, args, &mut i)?.to_ascii_lowercase();
                if !SUPPORTED_FORMATS.contains(&value.as_str()) {
                    return Err(CliError::new(format!(
                        "不支持的输出格式: {value}（可选：{}）",
                        SUPPORTED_FORMATS.join("|")
                    )));
                }
                format = value;
            }
            "--lang" | "--language" => {
                language = Some(take_value(flag, inline, args, &mut i)?);
            }
            "--translate" => {
                translate = Some(take_value(flag, inline, args, &mut i)?);
            }
            "--model" => {
                model = Some(PathBuf::from(take_value(flag, inline, args, &mut i)?));
            }
            "--threads" => {
                let value = take_value(flag, inline, args, &mut i)?;
                let parsed: u32 = value
                    .parse()
                    .map_err(|_| CliError::new(format!("--threads 需要正整数，收到: {value}")))?;
                if parsed == 0 {
                    return Err(CliError::new("--threads 需要大于 0 的整数"));
                }
                threads = Some(parsed);
            }
            "--polish" => {
                if inline.is_some() {
                    return Err(CliError::new("--polish 不接受取值"));
                }
                polish = true;
            }
            "--resume" => {
                if inline.is_some() {
                    return Err(CliError::new("--resume 不接受取值"));
                }
                resume = true;
            }
            "--artifact-dir" => {
                artifact_dir = Some(PathBuf::from(take_value(flag, inline, args, &mut i)?));
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(CliError::new(format!("未知选项: {other}")));
            }
            _ => {
                if input.is_some() {
                    return Err(CliError::new(format!("多余的参数: {arg}")));
                }
                input = Some(PathBuf::from(arg));
            }
        }
        i += 1;
    }

    let input = input.ok_or_else(|| CliError::new("缺少输入文件"))?;
    Ok(CliAction::Transcribe(Box::new(TranscribeArgs {
        input,
        output,
        format,
        language,
        translate,
        model,
        threads,
        polish,
        resume,
        artifact_dir,
    })))
}

/// 拆出 `--opt=value` 形式里的取值；无 `=` 时取值为 `None`。
fn split_flag(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((flag, value)) => (flag, Some(value)),
        None => (arg, None),
    }
}

/// 取选项取值：优先用内联的 `=value`，否则消费下一个参数。
fn take_value(
    flag: &str,
    inline: Option<&str>,
    args: &[String],
    i: &mut usize,
) -> std::result::Result<String, CliError> {
    if let Some(value) = inline {
        if value.is_empty() {
            return Err(CliError::new(format!("选项 {flag} 缺少取值")));
        }
        return Ok(value.to_string());
    }
    *i += 1;
    let value = args
        .get(*i)
        .cloned()
        .ok_or_else(|| CliError::new(format!("选项 {flag} 缺少取值")))?;
    // 裸 `-` 是「标准输入 / 标准输出」的约定（如 `-o -`），不算选项前缀。
    if value != "-" && value.starts_with('-') {
        return Err(CliError::new(format!(
            "选项 {flag} 缺少取值：下一个参数“{value}”看起来是另一个选项（若取值本身以 '-' 开头，请写成 {flag}={value}）"
        )));
    }
    Ok(value)
}

/// CLI 入口。返回 `None` 表示应继续启动图形界面；`Some(code)` 表示以该退出码结束进程。
pub fn run<I, S>(args: I) -> Option<i32>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    match parse(args) {
        Ok(CliAction::Gui) => None,
        Ok(CliAction::Help) => {
            println!("{}", usage());
            Some(0)
        }
        Ok(CliAction::Version) => {
            println!("voice2word {}", env!("CARGO_PKG_VERSION"));
            Some(0)
        }
        Ok(CliAction::Transcribe(args)) => Some(run_transcribe(&args)),
        Err(err) => {
            eprintln!("参数错误: {}", err.message());
            eprintln!("{}", usage());
            Some(2)
        }
    }
}

/// 执行一次无头转写，返回进程退出码（成功 0，失败 1）。
fn run_transcribe(args: &TranscribeArgs) -> i32 {
    match transcribe(args) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("转写失败: {err:#}");
            1
        }
    }
}

/// 加载配置、装配管线、驱动事件循环并确认字幕落盘。
fn transcribe(args: &TranscribeArgs) -> Result<()> {
    let config = AppConfig::load_from_file("config.toml").context("加载 config.toml 失败")?;

    if !args.input.is_file() {
        bail!("输入文件不存在或不是普通文件: {}", args.input.display());
    }
    if let Some(target) = &args.translate {
        eprintln!("提示: --translate {target} 已解析；无头模式当前只做转写与导出，翻译阶段未接入");
    }

    let pipeline = build_pipeline(&config);
    if let Some(dir) = resolve_artifact_dir(args) {
        eprintln!("提示: 阶段产物目录 {}（存在缓存时断点续跑）", dir.display());
        pipeline.set_artifact_dir(Some(dir));
    }
    let out_path = output_path(args);
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建输出目录失败: {}", parent.display()))?;
        }
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("创建异步运行时失败")?;
    let (tx, mut rx) = mpsc::unbounded_channel::<PipelineEvent>();

    let enable_polish = args.polish || config.pipeline.enable_polish;
    let polish_mode = enable_polish.then(|| config.pipeline.polish_mode.clone());
    let whisper_options = build_runtime_options(&config);
    let diarization = DiarizationOptions {
        enabled: config.pipeline.enable_diarization,
        speakers: config.pipeline.speaker_count,
    };

    let task_pipeline = pipeline.clone();
    let task_tx = tx.clone();
    let task_out = out_path.clone();
    let input = args.input.clone();
    // 省略 --lang 时回落到 config.toml 的 pipeline.language；uto 视为未指定，
    // 与 GUI（state.rs 读取同一字段）保持一致的默认语言行为。
    let language = args
        .language
        .clone()
        .or_else(|| Some(config.pipeline.language.clone()).filter(|l| l != "auto"));
    let format = args.format.clone();
    // `--threads` 允许任意正整数，但推理线程数超过本机逻辑核数只会互相抢占 CPU，
    // 反而拖慢转写，因此在这里按与 GUI 完全相同的口径收敛（见 `bound_cli_threads`）。
    let threads = args.threads.map(bound_cli_threads);
    let model = args.model.clone();
    let rescue_logprob = config.pipeline.whisper_rescue_logprob;

    let handle = rt.spawn(async move {
        let result = task_pipeline
            .run_with_options(
                input,
                Some(task_out),
                language,
                format,
                enable_polish,
                polish_mode,
                threads,
                model,
                rescue_logprob,
                whisper_options,
                diarization,
                task_tx.clone(),
            )
            .await;
        if let Err(err) = result {
            let _ = task_tx.send(PipelineEvent::Error(err.to_string()));
        }
    });

    // 事件循环必须在本线程持有 runtime 时驱动；先丢掉本地发送端，否则
    // `rx.recv()` 永远等不到 None——通道里始终存在一个未释放的发送者。
    drop(tx);

    let mut segments: Vec<Segment> = Vec::new();
    let mut failure: Option<String> = None;
    let mut last_percent = -1i32;

    rt.block_on(async {
        while let Some(event) = rx.recv().await {
            match event {
                PipelineEvent::StageChanged(stage) => {
                    eprintln!("[阶段] {stage}");
                    last_percent = -1;
                }
                PipelineEvent::Progress {
                    stage,
                    progress,
                    detail,
                } => {
                    let percent = (progress.clamp(0.0, 1.0) * 100.0).round() as i32;
                    if percent != last_percent {
                        last_percent = percent;
                        if detail.is_empty() {
                            eprintln!("[{stage}] {percent}%");
                        } else {
                            eprintln!("[{stage}] {percent}% {detail}");
                        }
                    }
                }
                PipelineEvent::SegmentStream(_) => {}
                PipelineEvent::Finished(list, _metrics) => segments = list,
                PipelineEvent::Error(message) => failure = Some(message),
            }
        }
    });

    if let Err(join_err) = rt.block_on(handle) {
        if failure.is_none() {
            failure = Some(format!("转写任务异常结束: {join_err}"));
        }
    }
    if let Some(message) = failure {
        bail!("{message}");
    }

    // 管线在「生成字幕」阶段已按目标路径落盘，但**写盘失败只记日志、不改变事件流**
    // （见 `TaskPipeline::run_with_options`：`warn!` 之后照常发 `Finished`）。
    // 因此不能用 `out_path.exists()` 判定本次是否成功——目标路径上可能残留上一次导出的
    // 旧文件，会把一次失败的导出报成成功、并返回 0。这里由 CLI 自己按最终片段重写一次并
    // 检查返回值：成功即把结果覆盖成本次内容，失败则带原因以非零码退出。
    // 该写入与管线 `style == None` 分支调用的是同一个 `SubtitleWriter::write_to_file`，
    // 字节一致，因此不改变正常路径的产物。
    if segments.is_empty() {
        bail!("转写未产生任何字幕片段，未写入文件: {}", out_path.display());
    }
    SubtitleWriter::write_to_file(&segments, &out_path, &args.format)
        .with_context(|| format!("写入字幕失败: {}", out_path.display()))?;
    println!(
        "字幕已写入: {}（共 {} 段）",
        out_path.display(),
        segments.len()
    );
    Ok(())
}

/// 解析阶段产物目录：`--artifact-dir` 优先；仅 `--resume` 时回落到
/// `<输入目录>/.voice2word-stages/<文件名>`；两者都未给时为 `None`（不落盘）。
fn resolve_artifact_dir(args: &TranscribeArgs) -> Option<PathBuf> {
    if let Some(dir) = &args.artifact_dir {
        return Some(dir.clone());
    }
    if !args.resume {
        return None;
    }
    let base = match args.input.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let stem = args
        .input
        .file_stem()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("output"));
    Some(base.join(".voice2word-stages").join(stem))
}

/// 目标字幕路径：`--output` 优先，否则与输入同目录、同名、换成目标格式后缀。
fn output_path(args: &TranscribeArgs) -> PathBuf {
    if let Some(output) = &args.output {
        return output.clone();
    }
    let mut file_name = args
        .input
        .file_stem()
        .map(|stem| stem.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("output"));
    file_name.push(".");
    file_name.push(&args.format);
    match args.input.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(file_name),
        _ => PathBuf::from(file_name),
    }
}

/// 按配置装配运行时参数（音频预处理 + VAD + 语速），与 GUI 侧保持一致。
fn build_runtime_options(config: &AppConfig) -> WhisperRuntimeOptions {
    let preprocess = PreprocessOptions {
        enabled: config.pipeline.preprocess_enabled,
        filters: engines::SpeechFilterOptions {
            highpass_hz: config.pipeline.preprocess_highpass_hz,
            denoise: config.pipeline.preprocess_denoise,
            normalize: config.pipeline.preprocess_normalize,
            target_rate: engines::ASR_SAMPLE_RATE,
        },
        compaction: engines::CompactionConfig::default(),
        compact: config.pipeline.preprocess_compact,
        min_saving: config.pipeline.preprocess_min_saving,
    };
    WhisperRuntimeOptions {
        audio_speed: config.pipeline.whisper_audio_speed,
        vad_threshold: config.pipeline.whisper_vad_threshold,
        preprocess,
    }
}

/// 装配完整管线：FFmpeg + Whisper + （可选）SenseVoice + LLM + （可选）标点。
fn build_pipeline(config: &AppConfig) -> Arc<TaskPipeline> {
    let ffmpeg = Arc::new(FFmpegEngine::new(AppConfig::resolve_path(
        &config.paths.ffmpeg,
    )));
    let python_path = AppConfig::resolve_command(&config.paths.python);

    let default_vad = AppConfig::resolve_path("models/whisper/ggml-silero-v6.2.0.bin");
    let vad_model_path = if config.pipeline.enable_vad {
        config
            .paths
            .vad_model
            .as_ref()
            .map(|p| AppConfig::resolve_path(p))
            .or_else(|| default_vad.exists().then(|| default_vad.clone()))
    } else {
        None
    };

    let mut hardware = if !config.gpu.hwaccel_decode && !config.gpu.whisper_offload {
        HardwareProfile::cpu_only()
    } else {
        HardwareProfile::detect()
    };
    if !config.gpu.hwaccel_decode {
        hardware.hardware_decode = false;
    }
    let whisper_gpu = hardware.use_gpu_pipeline() && config.gpu.whisper_offload;
    engines::media_pipeline::set_yield_to_desktop(config.gpu.yield_to_desktop);
    let gpu_limit_percent = if whisper_gpu {
        config.gpu.effective_gpu_limit()
    } else {
        100
    };

    let whisper = Arc::new(WhisperEngine::with_device(
        AppConfig::resolve_path(&config.paths.whisper_cli),
        AppConfig::resolve_path(&config.paths.whisper_model),
        vad_model_path,
        config.pipeline.whisper_threads,
        config.pipeline.whisper_processors,
        whisper_gpu,
        config.pipeline.whisper_no_fallback,
        config.pipeline.whisper_max_context,
        gpu_limit_percent,
    ));

    let llm = Arc::new(LLMEngine::new(
        AppConfig::resolve_path(&config.paths.llama_cli),
        AppConfig::resolve_path(&config.paths.llm_model),
        config.pipeline.llm_ctx,
        config.pipeline.llm_threads,
    ));

    let sensevoice = build_sensevoice(config, &python_path);
    let punc = build_punc(config, &python_path);

    Arc::new(TaskPipeline::new(ffmpeg, whisper, sensevoice, llm, punc))
}

/// 可选 SenseVoice 引擎：runner / 模型 / tokens / VAD 四项齐全才装配。
fn build_sensevoice(config: &AppConfig, python_path: &Path) -> Option<Arc<SenseVoiceEngine>> {
    let runner = AppConfig::resolve_path("tools/sensevoice_runner.py");
    let model = config
        .paths
        .sensevoice_model
        .as_ref()
        .map(|p| AppConfig::resolve_path(p))
        .unwrap_or_else(|| AppConfig::resolve_path("models/sensevoice/model.int8.onnx"));
    let tokens = config
        .paths
        .sensevoice_tokens
        .as_ref()
        .map(|p| AppConfig::resolve_path(p))
        .unwrap_or_else(|| AppConfig::resolve_path("models/sensevoice/tokens.txt"));
    let vad = config
        .paths
        .sensevoice_vad
        .as_ref()
        .map(|p| AppConfig::resolve_path(p))
        .unwrap_or_else(|| AppConfig::resolve_path("models/sensevoice/silero_vad.onnx"));

    if runner.exists() && model.exists() && tokens.exists() && vad.exists() {
        let provider = config.gpu.resolve_onnx_provider();
        Some(Arc::new(SenseVoiceEngine::with_python_and_provider(
            runner,
            model,
            tokens,
            vad,
            config.pipeline.whisper_threads,
            python_path,
            provider,
        )))
    } else {
        eprintln!("提示: SenseVoice 引擎未就绪，转写回退到 Whisper");
        None
    }
}

/// 可选标点引擎：runner 与模型都存在才装配。
fn build_punc(config: &AppConfig, python_path: &Path) -> Option<Arc<PunctuationEngine>> {
    let runner = AppConfig::resolve_path("tools/punc_runner.py");
    let model = config
        .paths
        .punc_model
        .as_ref()
        .map(|p| AppConfig::resolve_path(p))
        .unwrap_or_else(|| AppConfig::resolve_path("models/punc/model.int8.onnx"));

    if runner.exists() && model.exists() {
        let provider = config.gpu.resolve_onnx_provider();
        Some(Arc::new(PunctuationEngine::with_python_and_provider(
            runner,
            model,
            4,
            python_path,
            provider,
        )))
    } else {
        eprintln!("提示: 标点引擎未就绪，润色阶段将跳过标点恢复");
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 解析一段 `transcribe` 命令行并取出参数，失败即 panic。
    fn transcribe_args(argv: &[&str]) -> TranscribeArgs {
        match parse(argv.iter().copied()) {
            Ok(CliAction::Transcribe(args)) => *args,
            other => panic!("期望 transcribe 动作，得到 {other:?}"),
        }
    }

    /// 解析失败时取出错误说明。
    fn parse_error(argv: &[&str]) -> String {
        match parse(argv.iter().copied()) {
            Err(err) => err.message().to_string(),
            Ok(action) => panic!("期望解析失败，得到 {action:?}"),
        }
    }

    #[test]
    fn no_args_or_gui_flag_keeps_gui_mode() {
        assert_eq!(parse(["voice2word"]), Ok(CliAction::Gui));
        assert_eq!(parse(["voice2word", "--gui"]), Ok(CliAction::Gui));
        assert_eq!(parse(Vec::<String>::new()), Ok(CliAction::Gui));
    }

    #[test]
    fn run_returns_none_for_gui_mode() {
        assert_eq!(run(["voice2word"]), None);
        assert_eq!(run(["voice2word", "--gui"]), None);
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(parse(["voice2word", "help"]), Ok(CliAction::Help));
        assert_eq!(parse(["voice2word", "-h"]), Ok(CliAction::Help));
        assert_eq!(parse(["voice2word", "version"]), Ok(CliAction::Version));
        assert_eq!(parse(["voice2word", "-V"]), Ok(CliAction::Version));
        assert_eq!(run(["voice2word", "--help"]), Some(0));
        assert_eq!(run(["voice2word", "--version"]), Some(0));
    }

    #[test]
    fn transcribe_uses_documented_defaults() {
        let args = transcribe_args(&["voice2word", "transcribe", "a.mp3"]);
        assert_eq!(args.input, PathBuf::from("a.mp3"));
        assert_eq!(args.output, None);
        assert_eq!(args.format, "srt");
        assert_eq!(args.language, None);
        assert_eq!(args.translate, None);
        assert_eq!(args.model, None);
        assert_eq!(args.threads, None);
        assert!(!args.polish);
        assert!(!args.resume);
        assert_eq!(args.artifact_dir, None);
    }

    #[test]
    fn transcribe_parses_full_option_set() {
        let args = transcribe_args(&[
            "voice2word",
            "transcribe",
            "talk.mp4",
            "-o",
            "out/talk.ass",
            "--format",
            "ass",
            "--lang",
            "zh",
            "--translate",
            "en",
            "--model",
            "models/big.bin",
            "--threads",
            "8",
            "--polish",
        ]);
        assert_eq!(args.input, PathBuf::from("talk.mp4"));
        assert_eq!(args.output, Some(PathBuf::from("out/talk.ass")));
        assert_eq!(args.format, "ass");
        assert_eq!(args.language.as_deref(), Some("zh"));
        assert_eq!(args.translate.as_deref(), Some("en"));
        assert_eq!(args.model, Some(PathBuf::from("models/big.bin")));
        assert_eq!(args.threads, Some(8));
        assert!(args.polish);
    }

    #[test]
    fn options_accept_inline_equals_form() {
        let args = transcribe_args(&[
            "voice2word",
            "transcribe",
            "a.mp3",
            "--output=x.srt",
            "--format=VTT",
            "--threads=4",
        ]);
        assert_eq!(args.output, Some(PathBuf::from("x.srt")));
        assert_eq!(args.format, "vtt");
        assert_eq!(args.threads, Some(4));
    }

    #[test]
    fn language_alias_is_accepted() {
        let args = transcribe_args(&["voice2word", "transcribe", "a.mp3", "--language", "ja"]);
        assert_eq!(args.language.as_deref(), Some("ja"));
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        let message = parse_error(&["voice2word", "frobnicate"]);
        assert!(message.contains("未知子命令"), "{message}");
    }

    #[test]
    fn unknown_top_level_option_is_rejected() {
        let message = parse_error(&["voice2word", "--frobnicate"]);
        assert!(message.contains("未知选项"), "{message}");
    }

    #[test]
    fn missing_input_is_rejected() {
        let message = parse_error(&["voice2word", "transcribe"]);
        assert!(message.contains("缺少输入文件"), "{message}");
    }

    #[test]
    fn invalid_format_is_rejected() {
        let message = parse_error(&["voice2word", "transcribe", "a.mp3", "--format", "doc"]);
        assert!(message.contains("不支持的输出格式"), "{message}");
    }

    #[test]
    fn unknown_option_is_rejected() {
        let message = parse_error(&["voice2word", "transcribe", "a.mp3", "--bogus"]);
        assert!(message.contains("未知选项"), "{message}");
    }

    #[test]
    fn option_without_value_is_rejected() {
        let message = parse_error(&["voice2word", "transcribe", "a.mp3", "--output"]);
        assert!(message.contains("缺少取值"), "{message}");
        let message = parse_error(&["voice2word", "transcribe", "a.mp3", "--threads"]);
        assert!(message.contains("缺少取值"), "{message}");
    }

    #[test]
    fn invalid_threads_is_rejected() {
        let message = parse_error(&["voice2word", "transcribe", "a.mp3", "--threads", "0"]);
        assert!(message.contains("大于 0"), "{message}");
        let message = parse_error(&["voice2word", "transcribe", "a.mp3", "--threads", "many"]);
        assert!(message.contains("正整数"), "{message}");
    }

    #[test]
    fn cli_threads_override_is_bounded() {
        // 上限：超过本机逻辑核数的一律收敛到核数（与 GUI 的滑条量程同口径），
        // 免得 whisper-cli 开出上百个线程互相抢 CPU。
        let cores = crate::app::AppState::logical_cores();
        assert_eq!(
            bound_cli_threads(cores + 128),
            cores.max(2),
            "超上限应收敛到核数"
        );
        assert_eq!(
            bound_cli_threads(cores.max(2)),
            cores.max(2),
            "等于上限时保持原值"
        );
        // 下限：0 / 1 都抬到 2，whisper-cli 不接受 0 线程（解析期已挡 0，这里兜底）。
        assert_eq!(bound_cli_threads(0), 2);
        assert_eq!(bound_cli_threads(1), 2);
        // 区间内原样保留，不能顺手改动用户显式给的合法值。
        assert_eq!(bound_cli_threads(2), 2);
    }

    #[test]
    fn duplicate_positional_is_rejected() {
        let message = parse_error(&["voice2word", "transcribe", "a.mp3", "b.mp3"]);
        assert!(message.contains("多余的参数"), "{message}");
    }

    #[test]
    fn default_output_path_follows_input_stem() {
        let args = transcribe_args(&["voice2word", "transcribe", "clips/talk.mp4"]);
        assert_eq!(output_path(&args), PathBuf::from("clips/talk.srt"));

        let args = transcribe_args(&["voice2word", "transcribe", "talk.mp4", "--format", "vtt"]);
        assert_eq!(output_path(&args), PathBuf::from("talk.vtt"));

        let args = transcribe_args(&[
            "voice2word",
            "transcribe",
            "talk.mp4",
            "-o",
            "custom/name.txt",
            "--format",
            "vtt",
        ]);
        assert_eq!(output_path(&args), PathBuf::from("custom/name.txt"));
    }

    #[test]
    fn resume_flag_enables_default_artifact_dir() {
        let args = transcribe_args(&["voice2word", "transcribe", "clips/talk.mp4", "--resume"]);
        assert!(args.resume);
        assert_eq!(args.artifact_dir, None);
        assert_eq!(
            resolve_artifact_dir(&args),
            Some(PathBuf::from("clips/.voice2word-stages/talk"))
        );
    }

    #[test]
    fn artifact_dir_option_overrides_default() {
        let args = transcribe_args(&[
            "voice2word",
            "transcribe",
            "talk.mp4",
            "--artifact-dir",
            "stages",
        ]);
        assert_eq!(args.artifact_dir, Some(PathBuf::from("stages")));
        assert!(!args.resume);
        assert_eq!(resolve_artifact_dir(&args), Some(PathBuf::from("stages")));
    }

    #[test]
    fn caching_is_disabled_by_default() {
        let args = transcribe_args(&["voice2word", "transcribe", "talk.mp4"]);
        assert_eq!(resolve_artifact_dir(&args), None);
    }

    #[test]
    fn resume_without_parent_falls_back_to_local_stages_dir() {
        let args = transcribe_args(&["voice2word", "transcribe", "talk.mp4", "--resume"]);
        assert_eq!(
            resolve_artifact_dir(&args),
            Some(PathBuf::from("./.voice2word-stages/talk"))
        );
    }

    #[test]
    fn resume_flag_rejects_inline_value() {
        let message = parse_error(&["voice2word", "transcribe", "a.mp3", "--resume=1"]);
        assert!(message.contains("不接受取值"), "{message}");
    }

    #[test]
    fn artifact_dir_accepts_inline_equals_form() {
        let args = transcribe_args(&[
            "voice2word",
            "transcribe",
            "a.mp3",
            "--artifact-dir=cache/x",
        ]);
        assert_eq!(args.artifact_dir, Some(PathBuf::from("cache/x")));
    }

    #[test]
    fn usage_mentions_core_commands() {
        let text = usage();
        assert!(text.contains("transcribe"), "{text}");
        assert!(text.contains("--output"), "{text}");
        assert!(text.contains("--format"), "{text}");
        assert!(text.contains("srt"), "{text}");
        assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
    }

    #[test]
    fn supported_formats_are_all_lowercase() {
        for format in SUPPORTED_FORMATS {
            assert_eq!(format, format.to_ascii_lowercase(), "{format}");
        }
    }

    /// 取值位置的下一个参数看起来是另一个选项时，必须报「缺少取值」，
    /// 而不是把它吞成取值：修前 `--lang --polish` 会把 "--polish" 当语言，
    /// 用户既没得到语言、也没得到润色，还毫无提示。
    #[test]
    fn option_value_starting_with_dash_is_rejected() {
        let message = parse_error(&["voice2word", "transcribe", "a.mp4", "--lang", "--polish"]);
        assert!(message.contains("缺少取值"), "{message}");

        // 取值本身确实以 '-' 开头（合法但罕见）时，`=value` 形式仍可用。
        let args = transcribe_args(&["voice2word", "transcribe", "a.mp4", "--lang=--weird"]);
        assert_eq!(args.language.as_deref(), Some("--weird"));

        // 裸 '-' 是标准输入/输出约定（`-o -`），不能当成选项拦截。
        let args = transcribe_args(&["voice2word", "transcribe", "a.mp4", "-o", "-"]);
        assert_eq!(args.output, Some(PathBuf::from("-")));
    }

    /// `--` 之后停止解析选项：文件名以 '-' 开头时也能当输入传入。
    #[test]
    fn double_dash_terminates_option_parsing() {
        let args = transcribe_args(&["voice2word", "transcribe", "--", "-h"]);
        assert_eq!(args.input, PathBuf::from("-h"));

        let message = parse_error(&["voice2word", "transcribe", "a.mp3", "--", "b.mp3"]);
        assert!(message.contains("多余的参数"), "{message}");
    }
}
