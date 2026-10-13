/// 转写进度回调：`(进度 0.0~1.0, 阶段描述, 可选的流式片段)`。
///
/// 抽出别名不是为了少打字，而是因为这条签名（含 `Option`、`Box<dyn …>`、
/// 两个 auto trait）在 whisper / sensevoice / chunked_whisper / pipeline 里
/// 重复出现近 20 次——clippy 的 `type_complexity` 会对每一处各报一条。
/// 统一到一个具名类型后，改签名也只需改这里一处。
pub type SegmentProgressCb = Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>;

/// 文本类进度回调：`(进度 0.0~1.0, 阶段描述)`，用于翻译 / 标点 / 润色。
pub type TextProgressCb = Box<dyn Fn(f64, &str) + Send>;

use crate::subtitle::Segment;

pub mod audio_prep;
pub mod chunked_whisper;
pub mod diarization;
pub mod ffmpeg;
pub mod llm;
pub mod media_pipeline;
pub mod punc;
pub mod sensevoice;
pub mod translate;
pub mod video_player;
pub mod waveform;
pub mod whisper;

pub use audio_prep::{
    plan_compaction, write_wav_mono16, CompactionConfig, CompactionPlan, SpeechFilterOptions,
    TimePiece, SAMPLE_RATE as ASR_SAMPLE_RATE,
};
pub use chunked_whisper::{
    chunk_threshold_secs, sensevoice_worker_count, transcribe_chunked,
    transcribe_chunked_sensevoice,
};
pub use diarization::{detect_speakers, MAX_SPEAKERS};
pub use ffmpeg::FFmpegEngine;
pub use llm::LLMEngine;
pub use media_pipeline::{
    convert_nv12_frame, nv12_to_rgba, DecodePolicy, DecodedFrame, FrameRing, HardwareProfile,
    Nv12Renderer, ProxyManager, RenderBackend, Vendor, NV12_WGSL_SHADER,
};
pub use punc::PunctuationEngine;
pub use sensevoice::SenseVoiceEngine;
pub use translate::{OnlineApiConfig, TranslateEngine, TranslateMode};
pub use video_player::{PlayerFrame, VideoPlayerEngine, PLAYER_HEIGHT, PLAYER_WIDTH};
pub use waveform::{WaveformData, WAVEFORM_SAMPLE_RATE};
pub use whisper::WhisperEngine;

/// 子进程 spawn 失败时的可操作报错。
///
/// # 为什么要单独翻一遍
///
/// 最常见的一种失败是「解释器不在」：Windows 上的原文是「系统找不到指定的文件。」，
/// 完全没提 Python——用户只会以为程序坏了，而实际只需装个 Python 3 或改一行配置。
/// 这与「ffmpeg 找不到就告诉用户放哪」是同一条经验。
///
/// # 为什么按「命令自己的程序」报错
///
/// `run_process_and_parse` 收的是**调用方拼好的任意命令**（测试里甚至是个哨兵 exe）。
/// 只有它真的是本引擎配置的解释器时，「去装 Python / 改 `[paths] python`」这条指引
/// 才成立；否则就该如实报那条命令的路径——不然会把「哨兵命令不存在」说成「缺 Python」。
pub(crate) fn spawn_failure_message(
    err: &std::io::Error,
    program: &std::ffi::OsStr,
    what: &str,
    python_hint: Option<&std::path::Path>,
) -> anyhow::Error {
    let shown = std::path::Path::new(program).display();
    match (err.kind(), python_hint) {
        (std::io::ErrorKind::NotFound, Some(python)) => anyhow::anyhow!(
            "找不到 Python 解释器（配置里是 {}）：{}靠 Python 3 跑 runner 脚本。\n\
             请安装 Python 3 并确保它在系统 PATH 上，或把 config.toml 里 [paths] python 改成解释器的绝对路径。",
            python.display(),
            what
        ),
        (std::io::ErrorKind::NotFound, None) => anyhow::anyhow!(
            "启动{}失败：找不到可执行文件 {shown}。\n请确认该文件存在，或把它在 config.toml 里指向正确的路径。",
            what
        ),
        _ => anyhow::anyhow!("启动{}失败（{shown}）：{err}", what),
    }
}
#[cfg(test)]
mod spawn_failure_message_tests {
    use super::spawn_failure_message;
    use std::path::Path;

    /// 解释器缺失（且确认待启动的就是配置里的解释器）：报错要提到 Python、
    /// 指出改哪个配置项、并给出「装到 PATH」这条做法。
    #[test]
    fn missing_interpreter_points_at_python_and_config() {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        let msg = spawn_failure_message(
            &err,
            std::ffi::OsStr::new("python"),
            " SenseVoice 进程",
            Some(Path::new("python")),
        )
        .to_string();
        assert!(msg.contains("Python"), "{msg}");
        assert!(msg.contains("[paths] python"), "要指出改哪个配置项：{msg}");
        assert!(msg.contains("PATH"), "要给出「装到 PATH」这条做法：{msg}");
        assert!(msg.contains("SenseVoice"), "要说明是谁需要它：{msg}");
    }

    /// 缺失的不是解释器（调用方传的是任意命令）：如实报那条命令的路径，
    /// **不能**冒充「缺 Python」——否则会把「拼错的路径」误导成「没装 Python」。
    #[test]
    fn missing_other_program_keeps_its_own_path() {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        let msg = spawn_failure_message(
            &err,
            std::ffi::OsStr::new("Z:/nope/v2w_probe_missing_runner.exe"),
            " SenseVoice 进程",
            None,
        )
        .to_string();
        assert!(msg.contains("v2w_probe_missing_runner.exe"), "{msg}");
        assert!(!msg.contains("请安装 Python 3"), "不该冒充缺解释器：{msg}");
    }

    /// 其他失败（权限、杀软拦截…）：带出原文与程序路径，不改写成因。
    #[test]
    fn other_failures_keep_the_original_reason() {
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let msg = spawn_failure_message(
            &err,
            std::ffi::OsStr::new("D:/py/python.exe"),
            " 标点恢复进程",
            Some(Path::new("D:/py/python.exe")),
        )
        .to_string();
        assert!(msg.contains("标点恢复"), "{msg}");
        assert!(msg.contains("D:/py/python.exe"), "要带上程序路径：{msg}");
        assert!(
            !msg.contains("请安装 Python 3"),
            "权限问题不是缺解释器：{msg}"
        );
    }
}
