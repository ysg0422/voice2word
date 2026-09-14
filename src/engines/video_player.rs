//! 内嵌实时视频播放器
//!
//! FFmpeg packed BGRA pipe → 环形帧。硬解失败自动切软解。
//! 严格匹配 GPUI RenderImage (Bgra8Unorm) 纹理格式，杜绝红蓝色彩颠倒。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{error, info, warn};

use super::media_pipeline::{apply_no_window, DecodePolicy};

pub const PLAYER_WIDTH: u32 = 1280;
pub const PLAYER_HEIGHT: u32 = 720;
pub const FRAME_BYTES: usize = (PLAYER_WIDTH * PLAYER_HEIGHT * 4) as usize;

pub struct VideoPlayerEngine {
    ffmpeg_path: PathBuf,
    ffplay_path: PathBuf,
    policy: DecodePolicy,

    current_frame: Arc<Mutex<Option<Vec<u8>>>>,
    is_playing: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    video_proc: Arc<Mutex<Option<Child>>>,
    audio_proc: Arc<Mutex<Option<Child>>>,
    audio_feeder_proc: Arc<Mutex<Option<Child>>>,
    play_start_instant: Arc<Mutex<Option<Instant>>>,
    play_start_seconds: Arc<Mutex<f64>>,
    current_frame_index: Arc<AtomicU64>,
}

impl VideoPlayerEngine {
    pub fn new<P: AsRef<Path>>(ffmpeg_path: P) -> Self {
        Self::with_policy(
            ffmpeg_path,
            DecodePolicy {
                try_hwaccel: false,
                software_threads: 0,
            },
            false,
        )
    }

    pub fn with_policy<P: AsRef<Path>>(
        ffmpeg_path: P,
        policy: DecodePolicy,
        _prefer_gpu: bool,
    ) -> Self {
        let ffmpeg = ffmpeg_path.as_ref().to_path_buf();
        let ffplay = ffmpeg.with_file_name("ffplay.exe");
        Self {
            ffmpeg_path: ffmpeg,
            ffplay_path: ffplay,
            policy,
            current_frame: Arc::new(Mutex::new(None)),
            is_playing: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
            video_proc: Arc::new(Mutex::new(None)),
            audio_proc: Arc::new(Mutex::new(None)),
            audio_feeder_proc: Arc::new(Mutex::new(None)),
            play_start_instant: Arc::new(Mutex::new(None)),
            play_start_seconds: Arc::new(Mutex::new(0.0)),
            current_frame_index: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// 最新一帧 BGRA（播放中或暂停后都可用，严格匹配 GPUI Bgra8Unorm，避免红蓝颠倒与暂停闪黑）。
    pub fn get_frame(&self) -> Option<Vec<u8>> {
        self.current_frame.lock().ok()?.clone()
    }

    pub fn is_playing(&self) -> bool {
        self.is_playing.load(Ordering::SeqCst)
    }

    pub fn current_play_time(&self) -> f64 {
        let base = *self
            .play_start_seconds
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !self.is_playing() {
            return base;
        }
        let frame_idx = self.current_frame_index.load(Ordering::SeqCst);
        if let Some(instant) = *self
            .play_start_instant
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            let wall_sec = instant.elapsed().as_secs_f64();
            let frame_sec = (frame_idx as f64) / 25.0;
            // 毫秒级锁定画面与字幕：以真实出帧进度为基准，杜绝启动延迟导致的跑飞
            base + frame_sec.min(wall_sec)
        } else {
            base
        }
    }

    /// 暂停时钟但不杀解码进程（保留当前帧）。
    pub fn pause_clock(&self) {
        let now = self.current_play_time();
        self.is_playing.store(false, Ordering::SeqCst);
        *self
            .play_start_seconds
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = now;
        *self
            .play_start_instant
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.kill_procs();
    }

    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.is_playing.store(false, Ordering::SeqCst);
        *self
            .play_start_instant
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.kill_procs();
    }

    fn kill_procs(&self) {
        if let Ok(mut lock) = self.video_proc.lock() {
            if let Some(mut child) = lock.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        if let Ok(mut lock) = self.audio_proc.lock() {
            if let Some(mut child) = lock.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        if let Ok(mut lock) = self.audio_feeder_proc.lock() {
            if let Some(mut child) = lock.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    pub fn play<P: AsRef<Path>>(&self, video_path: P, start_seconds: f64) {
        self.stop();

        let video_path = video_path.as_ref().to_path_buf();
        if !video_path.exists() {
            warn!("播放失败：文件不存在 {:?}", video_path);
            return;
        }

        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let start_sec_str = format!("{:.3}", start_seconds.max(0.0));
        *self
            .play_start_seconds
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = start_seconds;
        *self
            .play_start_instant
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.current_frame_index.store(0, Ordering::SeqCst);
        self.is_playing.store(true, Ordering::SeqCst);

        // 启动音频管线：通过 FFmpeg 容器级关键帧快速寻轨 (100ms 响应，杜绝原生 ffplay 耗费 9.7 秒解码卡死的严重音画脱节)
        if self.ffplay_path.exists() {
            let mut feeder_cmd = Command::new(&self.ffmpeg_path);
            apply_no_window(&mut feeder_cmd);
            feeder_cmd.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-ss",
                &start_sec_str,
                "-i",
            ])
            .arg(&video_path)
            .args([
                "-vn",
                "-sn",
                "-dn",
                "-acodec",
                "pcm_s16le",
                "-ar",
                "44100",
                "-ac",
                "2",
                "-f",
                "wav",
                "pipe:1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

            if let Ok(mut feeder_child) = feeder_cmd.spawn() {
                if let Some(feeder_stdout) = feeder_child.stdout.take() {
                    let mut player_cmd = Command::new(&self.ffplay_path);
                    apply_no_window(&mut player_cmd);
                    player_cmd.args([
                        "-nodisp",
                        "-autoexit",
                        "-loglevel",
                        "quiet",
                        "-nostats",
                        "-f",
                        "wav",
                        "-i",
                        "pipe:0",
                    ])
                    .stdin(feeder_stdout)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());

                    if let Ok(player_child) = player_cmd.spawn() {
                        if let Ok(mut lock) = self.audio_proc.lock() {
                            *lock = Some(player_child);
                        }
                        if let Ok(mut lock) = self.audio_feeder_proc.lock() {
                            *lock = Some(feeder_child);
                        }
                    }
                }
            }
        }

        let mut used_hwaccel = self.policy.try_hwaccel;
        let mut video_child = match self.spawn_video(&video_path, &start_sec_str, used_hwaccel) {
            Ok(child) => child,
            Err(e) if used_hwaccel => {
                warn!(error = %e, "硬件解码启动失败，回退软解");
                used_hwaccel = false;
                match self.spawn_video(&video_path, &start_sec_str, false) {
                    Ok(child) => child,
                    Err(e) => {
                        error!("启动 FFmpeg 视频流失败: {}", e);
                        self.is_playing.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }
            Err(e) => {
                error!("启动 FFmpeg 视频流失败: {}", e);
                self.is_playing.store(false, Ordering::SeqCst);
                return;
            }
        };

        let mut stdout = match video_child.stdout.take() {
            Some(s) => s,
            None => return,
        };

        if let Ok(mut lock) = self.video_proc.lock() {
            *lock = Some(video_child);
        }

        let frame_target = self.current_frame.clone();
        let is_playing_flag = self.is_playing.clone();
        let generation = self.generation.clone();
        let play_instant_target = self.play_start_instant.clone();
        let frame_index_target = self.current_frame_index.clone();

        std::thread::Builder::new()
            .name(format!("v2w-preview-{gen}"))
            .spawn(move || {
                let mut buf = vec![0u8; FRAME_BYTES];
                let mut frame_index: u64 = 0;
                let mut active_start: Option<Instant> = None;
                const FRAME_DURATION: std::time::Duration = std::time::Duration::from_millis(40);

                while is_playing_flag.load(Ordering::SeqCst)
                    && generation.load(Ordering::SeqCst) == gen
                {
                    match stdout.read_exact(&mut buf) {
                        Ok(()) => {
                            if generation.load(Ordering::SeqCst) != gen {
                                break;
                            }
                            if active_start.is_none() {
                                let now = Instant::now();
                                active_start = Some(now);
                                if let Ok(mut lock) = play_instant_target.lock() {
                                    *lock = Some(now);
                                }
                            }
                            let start_inst = active_start.unwrap();
                            let target_time = start_inst + FRAME_DURATION * (frame_index as u32);
                            let now = Instant::now();
                            if target_time > now {
                                std::thread::sleep(target_time - now);
                            } else if now.saturating_duration_since(target_time)
                                > FRAME_DURATION * 2
                            {
                                frame_index += 1;
                                frame_index_target.store(frame_index, Ordering::SeqCst);
                                continue;
                            }
                            if let Ok(mut lock) = frame_target.lock() {
                                *lock = Some(buf.clone());
                            }
                            frame_index += 1;
                            frame_index_target.store(frame_index, Ordering::SeqCst);
                        }
                        Err(_) => break,
                    }
                }
            })
            .ok();

            info!(
                start = start_seconds,
                hwaccel = used_hwaccel,
                gen,
                "内嵌播放器已启动 (BGRA 1280x720 @ 25fps)"
            );
    }

    fn spawn_video(
        &self,
        video_path: &Path,
        start_sec_str: &str,
        hwaccel: bool,
    ) -> std::io::Result<Child> {
        let mut video_cmd = Command::new(&self.ffmpeg_path);
        apply_no_window(&mut video_cmd);
        video_cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
        ]);
        if hwaccel {
            video_cmd.args(["-hwaccel", "auto"]);
        }
        // 先粗略 seek 再打开，暂停续播启动更快。
        video_cmd
            .arg("-ss")
            .arg(start_sec_str)
            .arg("-i")
            .arg(video_path)
            .args([
                "-an",
                "-sn",
                "-vf",
                &format!(
                    "scale={}:{}:flags=fast_bilinear,format=bgra",
                    PLAYER_WIDTH, PLAYER_HEIGHT
                ),
                "-pix_fmt",
                "bgra",
                "-f",
                "rawvideo",
                "-r",
                "25",
                "-vsync",
                "cfr",
                "-threads",
            ]);
        let threads = if hwaccel {
            "1".to_string()
        } else if self.policy.software_threads == 0 {
            "0".to_string()
        } else {
            self.policy.software_threads.to_string()
        };
        video_cmd
            .arg(threads)
            .arg("pipe:1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = video_cmd.spawn()?;
        if hwaccel {
            std::thread::sleep(std::time::Duration::from_millis(60));
            match child.try_wait() {
                Ok(Some(status)) if !status.success() => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "hwaccel ffmpeg exited",
                    ));
                }
                _ => {}
            }
        }
        Ok(child)
    }
}

impl Drop for VideoPlayerEngine {
    fn drop(&mut self) {
        self.stop();
    }
}
