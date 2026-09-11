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

        let mut cmd = Command::new(&self.ffmpeg_path);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        let status = cmd
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(input_path)
            .args([
                "-map", "0:a:0?",
                "-vn", "-sn", "-dn",
                "-acodec", "pcm_s16le",
                "-ar", "16000",
                "-ac", "1",
                "-threads", "0",
            ])
            .arg(&target_wav)
            .status()
            .with_context(|| format!("执行 FFmpeg 失败: {:?}", self.ffmpeg_path))?;

        if !status.success() {
            anyhow::bail!("FFmpeg 音频提取返回非零退出码");
        }

        info!("音频提取完成: {:?}", target_wav);
        Ok(target_wav)
    }

    /// 纯内存管道推流：以 16kHz 单声道 s16le PCM WAV 格式将音频输出到 stdout 匿名管道 (0 磁盘 I/O)
    pub fn spawn_audio_stream<P: AsRef<Path>>(&self, input_path: P) -> Result<std::process::Child> {
        let input_path = input_path.as_ref();
        info!(
            "FFmpeg 启动纯内存音频推流管道: {:?}",
            input_path.file_name().unwrap_or_default()
        );

        let mut cmd = Command::new(&self.ffmpeg_path);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(input_path)
            .args([
                "-map", "0:a:0?",
                "-vn", "-sn", "-dn",
                "-acodec", "pcm_s16le",
                "-ar", "16000",
                "-ac", "1",
                "-threads", "0",
                "-f", "wav",
                "pipe:1",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let child = cmd.spawn().with_context(|| format!("启动 FFmpeg 内存管道失败: {:?}", self.ffmpeg_path))?;
        Ok(child)
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

        let mut cmd = Command::new(&self.ffmpeg_path);

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW: 杜绝弹出控制台窗口与系统句柄消耗
        }

        let status = cmd
            .arg("-ss")
            .arg(&time_str)
            .arg("-i")
            .arg(video_path)
            .arg("-frames:v")
            .arg("1")
            .arg("-vf")
            .arg("scale='min(960,iw)':-1") // 缩放至高清预览尺寸（960px宽），放大窗口清晰锐利，依然毫秒级响应
            .arg("-threads")
            .arg("0") // 软解多线程；代理已是 720p H.264，CPU 压力可控
            .arg("-an") // 跳过音频流解析
            .arg("-sn") // 跳过字幕流解析
            .arg("-q:v")
            .arg("3")
            .arg("-y")
            .arg(out_jpg)
            .status()
            .with_context(|| format!("执行 FFmpeg 抽帧失败: {:?}", self.ffmpeg_path))?;

        if !status.success() {
            anyhow::bail!("FFmpeg 抽取视频帧返回非零状态");
        }

        Ok(out_jpg.to_path_buf())
    }

    /// 从已提取的 16kHz WAV 切出一段（秒）。用于 Whisper 切块并行。
    pub fn slice_wav(
        &self,
        wav_path: &Path,
        start_sec: f64,
        duration_sec: f64,
        out_wav: &Path,
    ) -> Result<PathBuf> {
        let out_wav = out_wav.to_path_buf();
        if let Some(parent) = out_wav.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut cmd = Command::new(&self.ffmpeg_path);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        let status = cmd
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .arg("-ss")
            .arg(format!("{:.3}", start_sec.max(0.0)))
            .arg("-t")
            .arg(format!("{:.3}", duration_sec.max(0.1)))
            .arg("-i")
            .arg(wav_path)
            .args(["-c", "copy"]) // 原音频已经是 16kHz s16le WAV，直接流复制，毫秒级完成切片，零 CPU 损耗！
            .arg(&out_wav)
            .status()
            .with_context(|| format!("切分 WAV 失败: {:?}", self.ffmpeg_path))?;
        if !status.success() {
            anyhow::bail!("FFmpeg 切分 WAV 返回非零退出码");
        }
        Ok(out_wav)
    }
}
