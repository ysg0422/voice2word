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

/// 启动「靠 Python 跑 runner 脚本」的引擎（SenseVoice / 标点）失败时的可操作报错。
///
/// # 为什么要单独翻一遍
///
/// 最常见的一种失败是「解释器不在」：Windows 上的原文是「系统找不到指定的文件。」，
/// 完全没提 Python——用户只会以为程序坏了，而实际只需装个 Python 3 或改一行配置。
/// 这与「ffmpeg 找不到就告诉用户放哪」是同一条经验。
///
/// 抽成纯函数是为了能单测：真正去 spawn 一个不存在的解释器，各平台错误文本不同，
/// 而「该告诉用户什么」这件事与平台无关。
pub(crate) fn python_spawn_error(
    err: &std::io::Error,
    python: &std::path::Path,
    what: &str,
) -> anyhow::Error {
    if err.kind() == std::io::ErrorKind::NotFound {
        anyhow::anyhow!(
            "找不到 Python 解释器（配置里是 {}）：{what}靠 Python 3 跑 runner 脚本。\n\
             请安装 Python 3 并确保它在系统 PATH 上，或把 config.toml 里 [paths] python 改成解释器的绝对路径。",
            python.display()
        )
    } else {
        anyhow::anyhow!("启动{what}失败（{}）：{err}", python.display())
    }
}

#[cfg(test)]
mod python_spawn_error_tests {
    use super::python_spawn_error;
    use std::path::Path;

    /// 解释器缺失：报错必须提到 Python、给出配置项名与两种做法。
    #[test]
    fn missing_interpreter_points_at_python_and_config() {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        let msg = python_spawn_error(&err, Path::new("python"), "SenseVoice 识别").to_string();
        assert!(msg.contains("Python"), "{msg}");
        assert!(msg.contains("[paths] python"), "要指出改哪个配置项：{msg}");
        assert!(msg.contains("PATH"), "要给出「装到 PATH」这条做法：{msg}");
        assert!(msg.contains("SenseVoice 识别"), "要说明是谁需要它：{msg}");
    }

    /// 其他失败（权限、杀软拦截…）：如实带上原文与解释器路径，不冒充「缺 Python」。
    #[test]
    fn other_failures_keep_the_original_reason() {
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let msg = python_spawn_error(&err, Path::new("D:/py/python.exe"), "标点恢复").to_string();
        assert!(msg.contains("标点恢复"), "{msg}");
        assert!(
            msg.contains("D:/py/python.exe"),
            "要带上配置里的解释器路径：{msg}"
        );
        assert!(
            !msg.contains("请安装 Python 3"),
            "不要把所有失败都说成缺解释器：{msg}"
        );
    }
}
