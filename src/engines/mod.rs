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
    sensevoice_worker_count, transcribe_chunked, transcribe_chunked_sensevoice,
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
