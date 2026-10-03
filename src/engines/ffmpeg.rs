//! FFmpeg 引擎 — 音视频处理与音频提取

use anyhow::{Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tracing::info;

use crate::utils::TempPathGuard;

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
        self.extract_audio_filtered(input_path, output_wav, None)
    }

    /// 带自定义 FFmpeg 音频滤镜链（`-af`）的整轨音频提取。
    ///
    /// 语音增强链（高通 / 谱减降噪 / 电平归一 / 重采样）在这里就地生效，
    /// 因此「只做增强、不做停顿压实」时不必额外单跑一遍解码：这次提取
    /// 同时就是增强。`filter` 为 `None` 时与 [`Self::extract_audio`] 等价。
    pub fn extract_audio_filtered<P: AsRef<Path>>(
        &self,
        input_path: P,
        output_wav: Option<P>,
        filter: Option<&str>,
    ) -> Result<PathBuf> {
        let input_path = input_path.as_ref();
        // 自带临时路径时挂一个守卫：FFmpeg 中途失败（`?` 提前返回）或返回非零退出码
        // （下面的 bail!）都会在磁盘上留下一段截断的 WAV，过去没有任何代码负责删它。
        // 只有「路径由我们自己生成」时才守卫；调用方传入 output_wav 时所有权归调用方。
        let mut owned_guard = None;
        let target_wav = match output_wav {
            Some(out) => out.as_ref().to_path_buf(),
            None => {
                let temp_dir = std::env::temp_dir();
                let stem = input_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("audio");
                let path = temp_dir.join(format!("{}_voice2word.wav", stem));
                owned_guard = Some(TempPathGuard::file(path.clone()));
                path
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
        cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(input_path)
            .args(["-map", "0:a:0?", "-vn", "-sn", "-dn"]);
        if let Some(chain) = filter {
            if !chain.is_empty() {
                cmd.arg("-af").arg(chain);
            }
        }
        let status = cmd
            .args([
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
        // 提取成功：取消自动清理，把文件所有权交给调用方（由管线负责在转写后删除）
        if let Some(g) = owned_guard {
            g.disarm();
        }
        Ok(target_wav)
    }

    /// 抽取指定时间窗的音频为 16kHz 单声道 PCM WAV 文件。
    ///
    /// 与 [`Self::extract_audio`] 的区别：`-ss`/`-t` 放在 `-i` 之前做输入侧快速
    /// seek，因此只解码目标窗口，不读取整片。基准测试裁样本片段时用它避免为
    /// 一个 10 分钟窗口解码整段两小时的视频。
    pub fn extract_audio_window<P1: AsRef<Path>, P2: AsRef<Path>>(
        &self,
        input_path: P1,
        start_sec: f64,
        duration_sec: f64,
        out_wav: P2,
    ) -> Result<PathBuf> {
        let input_path = input_path.as_ref();
        let out_wav = out_wav.as_ref().to_path_buf();
        if let Some(parent) = out_wav.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut cmd = Command::new(&self.ffmpeg_path);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        if start_sec > 0.0 {
            cmd.arg("-ss").arg(format!("{:.3}", start_sec));
        }
        if duration_sec > 0.0 {
            cmd.arg("-t").arg(format!("{:.3}", duration_sec));
        }
        let status = cmd
            .arg("-i")
            .arg(input_path)
            .args([
                "-map", "0:a:0?", "-vn", "-sn", "-dn", "-acodec", "pcm_s16le", "-ar", "16000", "-ac", "1",
            ])
            .arg(&out_wav)
            .status()
            .with_context(|| format!("执行 FFmpeg 窗口抽音频失败: {:?}", self.ffmpeg_path))?;
        if !status.success() {
            anyhow::bail!("FFmpeg 窗口抽音频返回非零退出码");
        }
        Ok(out_wav)
    }

    /// 纯内存管道推流：以 16kHz 单声道 s16le PCM WAV 格式将音频输出到 stdout 匿名管道 (0 磁盘 I/O)
    pub fn spawn_audio_stream<P: AsRef<Path>>(&self, input_path: P) -> Result<std::process::Child> {
        self.spawn_audio_stream_at(input_path, None, None)
    }

    /// 带时间窗的纯内存管道推流（用于置信度救场：只重解码指定的低置信窗口）。
    /// `start_sec`/`duration_sec` 为 None 时等价于 spawn_audio_stream 全片推流。
    pub fn spawn_audio_stream_at<P: AsRef<Path>>(
        &self,
        input_path: P,
        start_sec: Option<f64>,
        duration_sec: Option<f64>,
    ) -> Result<std::process::Child> {
        self.spawn_audio_stream_at_rate(input_path, start_sec, duration_sec, 1.0)
    }

    /// 临时改变音频语速（保持音高），输出仍为 16kHz PCM；原视频文件不变。
    pub fn spawn_audio_stream_at_rate<P: AsRef<Path>>(
        &self,
        input_path: P,
        start_sec: Option<f64>,
        duration_sec: Option<f64>,
        speed: f64,
    ) -> Result<std::process::Child> {
        anyhow::ensure!(speed.is_finite() && (1.0..=1.5).contains(&speed), "音频加速倍率须在 1.0～1.5 之间");
        let chain = if speed > 1.0 {
            Some(format!("atempo={speed:.3}"))
        } else {
            None
        };
        self.spawn_audio_stream_filtered(input_path, start_sec, duration_sec, chain.as_deref())
    }

    /// 带自定义 FFmpeg 音频滤镜链（`-af`）的窗口化内存推流。
    ///
    /// `filter` 为 `None` 时与 [`Self::spawn_audio_stream_at`] 等价。语音增强链
    /// （高通/谱减降噪/电平归一/重采样）在推流阶段就地生效，不额外多跑一遍解码。
    pub fn spawn_audio_stream_filtered<P: AsRef<Path>>(
        &self,
        input_path: P,
        start_sec: Option<f64>,
        duration_sec: Option<f64>,
        filter: Option<&str>,
    ) -> Result<std::process::Child> {
        let input_path = input_path.as_ref();
        info!(
            "FFmpeg 启动纯内存音频推流管道: {:?} (窗口: {:?})",
            input_path.file_name().unwrap_or_default(),
            match (start_sec, duration_sec) {
                (Some(s), Some(d)) => format!("{:.2}s ~ {:.2}s", s, s + d),
                _ => "全片".to_string(),
            }
        );

        let mut cmd = Command::new(&self.ffmpeg_path);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        if let Some(s) = start_sec {
            cmd.arg("-ss").arg(format!("{:.3}", s.max(0.0)));
        }
        if let Some(d) = duration_sec {
            cmd.arg("-t").arg(format!("{:.3}", d.max(0.1)));
        }
        cmd.arg("-i")
            .arg(input_path)
            .args([
                "-map", "0:a:0?",
                "-vn", "-sn", "-dn",
            ]);
        if let Some(chain) = filter {
            if !chain.is_empty() {
                cmd.arg("-af").arg(chain);
            }
        }
        cmd.args([
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
        // 并入全局作业对象：本进程若被强杀/崩溃，这个 ffmpeg 会被系统一并清掉
        crate::utils::child_registry::adopt(&child);
        Ok(child)
    }

    /// 纯内存解码整轨音频为单声道 s16le PCM。
    ///
    /// 波形峰值包络（F-014）与说话人特征提取（F-015）共用这一条解码路径：
    /// 两者都只需要低频包络与音高/过零率级别的信息，8kHz 足够，
    /// 数据量还只有 16kHz 的一半（1 小时片 ≈ 57MB）。
    pub fn decode_pcm_mono<P: AsRef<Path>>(
        &self,
        input_path: P,
        sample_rate: u32,
    ) -> Result<Vec<i16>> {
        self.decode_pcm_mono_filtered(input_path, sample_rate, None)
    }

    /// 带自定义滤镜链的整轨 PCM 解码，供 ASR 前端预处理使用。
    ///
    /// 只跑一次 FFmpeg 就同时完成「语音增强」与「解码到内存」：增强链里已经含
    /// 重采样到 16 kHz 的 `aresample`，因此这里是全链路唯一一次音频解码，
    /// 不会因为做预处理而多付一遍 I/O。
    pub fn decode_pcm_mono_filtered<P: AsRef<Path>>(
        &self,
        input_path: P,
        sample_rate: u32,
        filter: Option<&str>,
    ) -> Result<Vec<i16>> {
        let input_path = input_path.as_ref();
        let mut cmd = Command::new(&self.ffmpeg_path);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(input_path)
            .args(["-map", "0:a:0?", "-vn", "-sn", "-dn"]);
        if let Some(chain) = filter {
            if !chain.is_empty() {
                cmd.arg("-af").arg(chain);
            }
        }
        let mut child = cmd
            .args(["-acodec", "pcm_s16le", "-ac", "1"])
            .arg("-ar")
            .arg(sample_rate.to_string())
            .args(["-f", "s16le", "pipe:1"])
            .stdout(Stdio::piped())
            // stderr 必须丢弃，否则管道写满会与 stdout 读取互相阻塞
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("启动 FFmpeg 音频解码失败: {:?}", self.ffmpeg_path))?;
        crate::utils::child_registry::adopt(&child);

        let mut raw = Vec::new();
        if let Some(mut stdout) = child.stdout.take() {
            stdout
                .read_to_end(&mut raw)
                .context("读取 FFmpeg 音频管道失败")?;
        }
        let status = child.wait().context("等待 FFmpeg 音频解码进程失败")?;
        if !status.success() && raw.is_empty() {
            anyhow::bail!("FFmpeg 音频解码返回非零退出码（可能该文件没有音轨）");
        }
        // s16le 小端逐样本还原；奇数尾巴（半个样本）直接丢弃
        Ok(raw
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect())
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

    /// 探测视频真实分辨率 (宽, 高)。解析 `ffmpeg -i` 元数据输出中首个视频流的尺寸；
    /// 探测失败返回 None（调用方回退 16:9）。
    pub fn get_resolution<P: AsRef<Path>>(&self, input_path: P) -> Option<(u32, u32)> {
        let mut cmd = Command::new(&self.ffmpeg_path);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let output = cmd.arg("-i").arg(input_path.as_ref()).output().ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            let Some(video_pos) = line.find("Video:") else { continue };
            for token in line[video_pos..].split(&[',', ' ', '(', ')']) {
                let Some((w, h)) = token.split_once('x') else { continue };
                let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>())
                else {
                    continue;
                };
                // 过滤 0x001B 之类的十六进制杂讯与异常值，只接受合理分辨率
                if w >= 16 && h >= 16 && w <= 16384 && h <= 16384 {
                    return Some((w, h));
                }
            }
        }
        None
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
