//! 转写流水线阶段性能耗时统计与 Benchmark 基准数据模型

use serde::{Deserialize, Serialize};
use crate::utils::time::format_duration_short;

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct PipelinePerformanceMetrics {
    /// 媒体文件时长 (秒)
    pub video_duration: f64,
    /// 阶段 1: FFmpeg 提取音频耗时 (秒)
    pub ffmpeg_audio_sec: f64,
    /// 阶段 1 音频通道说明（如 "纯内存 PCM 匿名管道推流 (0 磁盘 I/O)" 或 "FFmpeg 抽取临时 WAV 文件"）
    #[serde(default)]
    pub audio_process_name: Option<String>,
    /// 阶段 2: Silero VAD 语音活性检测耗时 (秒)
    pub vad_sec: f64,
    /// 阶段 2 VAD 说明（如 "Silero VAD 毫秒级语音切片压实" 或 "SenseVoice 内嵌流式 VAD"）
    #[serde(default)]
    pub vad_engine_name: Option<String>,
    /// 阶段 3: 语音识别纯推理耗时 (秒)
    pub whisper_sec: f64,
    /// 阶段 3+: 置信度二段重解码耗时 (秒，未触发时为 0.0)
    #[serde(default)]
    pub rescue_sec: f64,
    /// 阶段 3+: 触发重解码救回的低置信窗口数量
    #[serde(default)]
    pub rescue_span_count: usize,
    /// 阶段 3 ASR 引擎详细说明（如 "SenseVoice-Small (非自回归 INT8 · CPU 8线程)" 或 "Whisper Turbo Q5 (ggml-large-v3-turbo-q5_0.bin)"）
    #[serde(default)]
    pub asr_engine_name: Option<String>,
    /// 阶段 4: 字幕润色/标点恢复耗时 (秒，未启用时为 0.0)
    pub qwen_sec: f64,
    /// 阶段 4 引擎说明（如 "SenseVoice 原生标点与 ITN (免二次后处理)" 或 "CT-Punc 极速标点 (INT8)" 或 "Qwen 字幕润色"）
    #[serde(default)]
    pub polish_engine_name: Option<String>,
    /// 阶段 5: 字幕写出与导出耗时 (秒)
    pub srt_export_sec: f64,
    /// 阶段 5 导出说明（如 "SRT 标准字幕写出"）
    #[serde(default)]
    pub export_name: Option<String>,
    /// 产出的字幕片段总数量
    #[serde(default)]
    pub segment_count: usize,
    /// 全流程总耗时 (秒)
    pub total_elapsed_sec: f64,
}

impl PipelinePerformanceMetrics {
    /// 生成标准终端与文件详细性能报告
    pub fn format_summary_block(&self) -> String {
        let dur_str = format_duration_short(self.video_duration);
        let audio_desc = self.audio_process_name.as_deref().unwrap_or(
            if self.ffmpeg_audio_sec == 0.0 {
                "纯内存 PCM 匿名管道推流 (0 物理磁盘 I/O)"
            } else {
                "FFmpeg 本地临时 WAV 提取"
            },
        );
        let vad_desc = self.vad_engine_name.as_deref().unwrap_or(
            if self.vad_sec == 0.0 {
                "内嵌流式 VAD 检测"
            } else {
                "Silero VAD 毫秒级语音切片压实"
            },
        );
        let asr_desc = self.asr_engine_name.as_deref().unwrap_or("Whisper 语音转写");
        let rescue_desc = if self.rescue_span_count > 0 {
            format!("{} 个低置信窗口已带回退重解码救回", self.rescue_span_count)
        } else {
            "未触发 (全程置信度良好或已关闭)".to_string()
        };
        let polish_desc = self.polish_engine_name.as_deref().unwrap_or("标点/AI润色 (跳过)");
        let export_desc = self.export_name.as_deref().unwrap_or("字幕文件写出");

        let total = self.total_elapsed_sec.max(0.001);
        let audio_pct = (self.ffmpeg_audio_sec / total * 100.0).clamp(0.0, 100.0);
        let vad_pct = (self.vad_sec / total * 100.0).clamp(0.0, 100.0);
        let asr_pct = (self.whisper_sec / total * 100.0).clamp(0.0, 100.0);
        let rescue_pct = (self.rescue_sec / total * 100.0).clamp(0.0, 100.0);
        let polish_pct = (self.qwen_sec / total * 100.0).clamp(0.0, 100.0);
        let export_pct = (self.srt_export_sec / total * 100.0).clamp(0.0, 100.0);

        let speed_ratio = if total > 0.0 && self.video_duration > 0.0 {
            format!(
                "{:.1}x 实时速度 (耗时仅为媒体时长的 {:.1}%)",
                self.video_duration / total,
                (total / self.video_duration) * 100.0
            )
        } else {
            "毫秒级".to_string()
        };

        let seg_info = if self.segment_count > 0 {
            format!("{} 句", self.segment_count)
        } else {
            "-".to_string()
        };

        format!(
r#"
======================= 全链路性能与耗时统计 =======================
媒体总时长：        {dur_str} ({:.1} 秒)
生成字幕句数：      {seg_info}
处理吞吐倍率：      {speed_ratio}

[阶段 1] 音频通道：    {:>6.1} 秒 ({:>4.1}%) | {audio_desc}
[阶段 2] 语音活性检测：{:>6.1} 秒 ({:>4.1}%) | {vad_desc}
[阶段 3] 语音转写识别：{:>6.1} 秒 ({:>4.1}%) | {asr_desc}
[阶段 3+]置信度救场：  {:>6.1} 秒 ({:>4.1}%) | {rescue_desc}
[阶段 4] 标点与语法：  {:>6.1} 秒 ({:>4.1}%) | {polish_desc}
[阶段 5] 字幕导出写出：{:>6.1} 秒 ({:>4.1}%) | {export_desc}

全流程总耗时：       {:>6.1} 秒
===================================================================
"#,
            self.video_duration,
            self.ffmpeg_audio_sec, audio_pct,
            self.vad_sec, vad_pct,
            self.whisper_sec, asr_pct,
            self.rescue_sec, rescue_pct,
            self.qwen_sec, polish_pct,
            self.srt_export_sec, export_pct,
            self.total_elapsed_sec,
        )
    }
}

