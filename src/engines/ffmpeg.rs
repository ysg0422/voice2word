//! FFmpeg 引擎 — 音视频处理与音频提取

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::info;

pub struct FFmpegEngine {
    ffmpeg_path: PathBuf,
}

impl FFmpegEngine {
    pub fn new<P: AsRef<Path>>(ffmpeg_path: P) -> Self {
        Self {
            ffmpeg_path: ffmpeg_path.as_ref().to_path_buf(),
        }
    }

    /// 提取音频为 16kHz, 单声道, s16le PCM WAV 文件
    pub fn extract_audio<P: AsRef<Path>>(
        &self,
        input_path: P,
        output_wav: Option<P>,
    ) -> Result<PathBuf> {
        let input_path = input_path.as_ref();
        let target_wav = match output_wav {
            Some(out) => out.as_ref().to_path_buf(),
            None => {
                let temp_dir = std::env::temp_dir();
                let stem = input_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("audio");
                temp_dir.join(format!("{}_voice2word.wav", stem))
            }
        };

        info!(
            "FFmpeg 提取音频: {:?} -> {:?}",
            input_path.file_name().unwrap_or_default(),
            target_wav.file_name().unwrap_or_default()
        );

        let status = Command::new(&self.ffmpeg_path)
            .arg("-i")
            .arg(input_path)
            .arg("-vn") // 去除视频流
            .arg("-acodec")
            .arg("pcm_s16le")
            .arg("-ar")
            .arg("16000") // 16kHz 适合 Whisper
            .arg("-ac")
            .arg("1") // 单声道
            .arg("-y") // 覆盖输出
            .arg(&target_wav)
            .status()
            .with_context(|| format!("执行 FFmpeg 失败: {:?}", self.ffmpeg_path))?;

        if !status.success() {
            anyhow::bail!("FFmpeg 音频提取返回非零退出码");
        }

        info!("音频提取完成: {:?}", target_wav);
        Ok(target_wav)
    }

    /// 获取视频/音频的时长 (秒)
    pub fn get_duration<P: AsRef<Path>>(&self, input_path: P) -> f64 {
        let output = Command::new(&self.ffmpeg_path)
            .arg("-i")
            .arg(input_path.as_ref())
            .output();

        if let Ok(out) = output {
            let stderr = String::from_utf8_lossy(&out.stderr);
            for line in stderr.lines() {
                if let Some(pos) = line.find("Duration:") {
                    let dur_part = &line[pos + 9..];
                    if let Some(comma_pos) = dur_part.find(',') {
                        let dur_str = dur_part[..comma_pos].trim();
                        let parts: Vec<&str> = dur_str.split(':').collect();
                        if parts.len() == 3 {
                            let h: f64 = parts[0].parse().unwrap_or(0.0);
                            let m: f64 = parts[1].parse().unwrap_or(0.0);
                            let s: f64 = parts[2].parse().unwrap_or(0.0);
                            return h * 3600.0 + m * 60.0 + s;
                        }
                    }
                }
            }
        }
        0.0
    }

    /// 根据时间点快速抽取视频某一帧画面 (用于视频编辑监视器预览)
    pub fn extract_frame<P1: AsRef<Path>, P2: AsRef<Path>>(
        &self,
        video_path: P1,
        time_sec: f64,
        out_jpg: P2,
    ) -> Result<PathBuf> {
        let video_path = video_path.as_ref();
        let out_jpg = out_jpg.as_ref();
        let time_str = format!("{:.3}", time_sec.max(0.0));

        if let Some(parent) = out_jpg.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let status = Command::new(&self.ffmpeg_path)
            .arg("-ss")
            .arg(&time_str)
            .arg("-i")
            .arg(video_path)
            .arg("-vframes")
            .arg("1")
            .arg("-q:v")
            .arg("2")
            .arg("-y")
            .arg(out_jpg)
            .status()
            .with_context(|| format!("执行 FFmpeg 抽帧失败: {:?}", self.ffmpeg_path))?;

        if !status.success() {
            anyhow::bail!("FFmpeg 抽取视频帧返回非零状态");
        }

        Ok(out_jpg.to_path_buf())
    }
}
