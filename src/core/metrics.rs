//! 转写流水线阶段性能耗时统计与 Benchmark 基准数据模型

use serde::{Deserialize, Serialize};
use crate::utils::time::format_duration_short;

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct PipelinePerformanceMetrics {
    /// 媒体文件时长 (秒)
    pub video_duration: f64,
    /// 阶段 1: FFmpeg 提取音频耗时 (秒)
    pub ffmpeg_audio_sec: f64,
    /// 阶段 2: Silero VAD 语音活性检测耗时 (秒)
    pub vad_sec: f64,
    /// 阶段 3: Whisper 神经网络纯推理耗时 (秒)
    pub whisper_sec: f64,
    /// 阶段 4: 字幕润色/标点恢复耗时 (秒，未启用时为 0.0)
    pub qwen_sec: f64,
    /// 阶段 4 引擎说明（如 "CT-Punc 极速标点" 或 "Qwen 字幕润色"）
    #[serde(default)]
    pub polish_engine_name: Option<String>,
    /// 阶段 5: 字幕写出与导出耗时 (秒)
    pub srt_export_sec: f64,
    /// 全流程总耗时 (秒)
    pub total_elapsed_sec: f64,
}

impl PipelinePerformanceMetrics {
    /// 生成标准终端性能报告
    pub fn format_summary_block(&self) -> String {
        let dur_str = format_duration_short(self.video_duration);
        let polish_title = self.polish_engine_name.as_deref().unwrap_or("标点/AI润色：   ");
        format!(
r#"
========== 性能统计 ==========
视频总时长：     {dur_str}

FFmpeg 音频处理：   {:>7.1} 秒
VAD 语音检测：      {:>7.1} 秒
Whisper 转写：      {:>7.1} 秒
{:<16} {:>7.1} 秒
字幕导出：           {:>7.1} 秒

总耗时：            {:>7.1} 秒
==============================
"#,
            self.ffmpeg_audio_sec,
            self.vad_sec,
            self.whisper_sec,
            polish_title,
            self.qwen_sec,
            self.srt_export_sec,
            self.total_elapsed_sec,
        )
    }
}

