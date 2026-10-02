pub mod ffmpeg;
pub mod llm;
pub mod punc;
pub mod sensevoice;
pub mod video_player;
pub mod whisper;
pub mod media_pipeline;
pub mod chunked_whisper;
pub mod translate;
pub mod waveform;
pub mod diarization;
pub mod audio_prep;

pub use audio_prep::{
    plan_compaction, write_wav_mono16, CompactionConfig, CompactionPlan, SpeechFilterOptions,
    TimePiece, SAMPLE_RATE as ASR_SAMPLE_RATE,
};
pub use ffmpeg::FFmpegEngine;
pub use llm::LLMEngine;
pub use punc::PunctuationEngine;
pub use sensevoice::SenseVoiceEngine;
pub use translate::{OnlineApiConfig, TranslateEngine, TranslateMode};
pub use video_player::{PlayerFrame, VideoPlayerEngine, PLAYER_HEIGHT, PLAYER_WIDTH};
pub use whisper::WhisperEngine;
pub use chunked_whisper::{
    sensevoice_worker_count, transcribe_chunked, transcribe_chunked_sensevoice,
};
pub use media_pipeline::{
    convert_nv12_frame, nv12_to_rgba, DecodePolicy, DecodedFrame, FrameRing, HardwareProfile,
    Nv12Renderer, ProxyManager, RenderBackend, NV12_WGSL_SHADER,
};
pub use waveform::{WaveformData, WAVEFORM_SAMPLE_RATE};
pub use diarization::{detect_speakers, MAX_SPEAKERS};
