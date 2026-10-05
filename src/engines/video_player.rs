//! 内嵌实时视频播放器
//!
//! FFmpeg packed BGRA pipe → 环形帧。硬解失败自动切软解。
//! 严格匹配 GPUI RenderImage (Bgra8Unorm) 纹理格式，杜绝红蓝色彩颠倒。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{error, info, warn};

use super::media_pipeline::{apply_background_priority, apply_no_window, DecodePolicy};

pub const PLAYER_WIDTH: u32 = 1280;
pub const PLAYER_HEIGHT: u32 = 720;
pub const FRAME_BYTES: usize = (PLAYER_WIDTH * PLAYER_HEIGHT * 4) as usize;

/// 一帧解码输出：BGRA 像素 + 本帧实际尺寸（随视频真实宽高比动态变化，UI 据此构建纹理）
pub struct PlayerFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct VideoPlayerEngine {
    ffmpeg_path: PathBuf,
    ffplay_path: PathBuf,
    policy: DecodePolicy,

    // 以 Arc 共享最新帧：出帧线程写入，UI 只克隆指针，杜绝每帧 3.68MB 深拷贝
    current_frame: Arc<Mutex<Option<Arc<PlayerFrame>>>>,
    /// 出帧尺寸（随视频真实分辨率等比适配，set_frame_dimensions 驱动）
    frame_width: AtomicU32,
    frame_height: AtomicU32,
    /// 帧版本号：每写入一帧自增，UI 据此判断画面是否变化（永不回退，与 generation 解耦）
    frame_version: Arc<AtomicU64>,
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
            frame_width: AtomicU32::new(PLAYER_WIDTH),
            frame_height: AtomicU32::new(PLAYER_HEIGHT),
            frame_version: Arc::new(AtomicU64::new(0)),
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
    /// 返回 Arc 指针克隆而非深拷贝，UI 层配合 frame_version 缓存 RenderImage 实现零拷贝复用。
    pub fn get_frame(&self) -> Option<Arc<PlayerFrame>> {
        self.current_frame.lock().ok()?.clone()
    }

    /// 设定视频源分辨率：在 1280x720 预算内等比适配出帧尺寸（偶数对齐）。
    /// scale 滤镜、帧缓冲读取与 UI 画面比例全部由此驱动，保证任意宽高比下画面不拉伸。
    pub fn set_frame_dimensions(&self, src_w: u32, src_h: u32) {
        let (sw, sh) = (src_w.max(2), src_h.max(2));
        let scale = (PLAYER_WIDTH as f32 / sw as f32).min(PLAYER_HEIGHT as f32 / sh as f32);
        let mut w = ((sw as f32 * scale).round() as u32).clamp(2, PLAYER_WIDTH);
        let mut h = ((sh as f32 * scale).round() as u32).clamp(2, PLAYER_HEIGHT);
        w -= w % 2;
        h -= h % 2;
        self.frame_width.store(w, Ordering::SeqCst);
        self.frame_height.store(h, Ordering::SeqCst);
    }

    /// 当前出帧尺寸 (宽, 高)
    pub fn frame_dimensions(&self) -> (u32, u32) {
        (
            self.frame_width.load(Ordering::SeqCst),
            self.frame_height.load(Ordering::SeqCst),
        )
    }

    /// 当前帧版本号：每写入一帧自增一次。UI 版本未变时可直接复用已构建的 RenderImage。
    pub fn frame_version(&self) -> u64 {
        self.frame_version.load(Ordering::SeqCst)
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
                crate::utils::child_registry::adopt(&feeder_child);
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
                        crate::utils::child_registry::adopt(&player_child);
                        if let Ok(mut lock) = self.audio_proc.lock() {
                            *lock = Some(player_child);
                        }
                        if let Ok(mut lock) = self.audio_feeder_proc.lock() {
                            *lock = Some(feeder_child);
                        }
                    } else {
                        // ffplay 起不来：feeder 已经在往一个没人读的管道里灌 WAV。
                        // `Child` 的 Drop 不会杀进程，放任下去会留下一个阻塞在满管道上的
                        // 孤儿 ffmpeg —— 一直占着视频文件句柄，反复预览会越积越多，
                        // 用户连改名/删除这个视频都会失败。
                        warn!("启动 ffplay 失败，就地回收音频馈送子进程");
                        let _ = feeder_child.kill();
                        let _ = feeder_child.wait();
                    }
                }
            }
        }

        // 出帧尺寸：按视频真实宽高比等比适配（探测未完成时回退 1280x720）
        let (frame_w, frame_h) = self.frame_dimensions();
        let frame_bytes = frame_w as usize * frame_h as usize * 4;

        let mut used_hwaccel = self.policy.try_hwaccel;
        let mut video_child = match self.spawn_video(&video_path, &start_sec_str, used_hwaccel, frame_w, frame_h) {
            Ok(child) => child,
            Err(e) if used_hwaccel => {
                warn!(error = %e, "硬件解码启动失败，回退软解");
                used_hwaccel = false;
                match self.spawn_video(&video_path, &start_sec_str, false, frame_w, frame_h) {
                    Ok(child) => child,
                    Err(e) => {
                        error!("启动 FFmpeg 视频流失败: {}", e);
                        self.is_playing.store(false, Ordering::SeqCst);
                        // 音频管线（ffplay + ffmpeg feeder）在视频流之前就已启动并
                        // 存进 audio_proc / audio_feeder_proc。这里只置 is_playing
                        // 而不回收，界面显示「未播放」却仍有声音，且两进程一直占着
                        // 视频文件句柄。必须与 `stop()` 同源地把整条管线收掉。
                        self.kill_procs();
                        return;
                    }
                }
            }
            Err(e) => {
                error!("启动 FFmpeg 视频流失败: {}", e);
                self.is_playing.store(false, Ordering::SeqCst);
                self.kill_procs();
                return;
            }
        };

        let mut stdout = match video_child.stdout.take() {
            Some(s) => s,
            None => {
                // 同上：拿不到出帧管道就没法播，音频管线一并收掉，别留半条在响。
                self.is_playing.store(false, Ordering::SeqCst);
                let _ = video_child.kill();
                let _ = video_child.wait();
                self.kill_procs();
                return;
            }
        };

        if let Ok(mut lock) = self.video_proc.lock() {
            *lock = Some(video_child);
        }

        let frame_target = self.current_frame.clone();
        let frame_version_target = self.frame_version.clone();
        let is_playing_flag = self.is_playing.clone();
        let generation = self.generation.clone();
        let play_instant_target = self.play_start_instant.clone();
        let frame_index_target = self.current_frame_index.clone();

        std::thread::Builder::new()
            .name(format!("v2w-preview-{gen}"))
            .spawn(move || {
                let mut frame_index: u64 = 0;
                let mut active_start: Option<Instant> = None;
                const FRAME_DURATION: std::time::Duration = std::time::Duration::from_millis(40);

                while is_playing_flag.load(Ordering::SeqCst)
                    && generation.load(Ordering::SeqCst) == gen
                {
                    // 每帧独立分配并直接装入 Arc：读取即所有权，整条链路零深拷贝。
                    // 分配器会复用上一帧归还的同尺寸内存块，稳态下无页错误开销。
                    let mut buf = vec![0u8; frame_bytes];
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
                                *lock = Some(Arc::new(PlayerFrame {
                                    data: buf,
                                    width: frame_w,
                                    height: frame_h,
                                }));
                                frame_version_target.fetch_add(1, Ordering::SeqCst);
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
                width = frame_w,
                height = frame_h,
                "内嵌播放器已启动 (BGRA 等比出帧 @ 25fps)"
            );
    }

    fn spawn_video(
        &self,
        video_path: &Path,
        start_sec_str: &str,
        hwaccel: bool,
        frame_w: u32,
        frame_h: u32,
    ) -> std::io::Result<Child> {
        let mut video_cmd = Command::new(&self.ffmpeg_path);
        // 让路是**进程级**策略：读全局开关而不是启动时快照的 `policy.yield_to_desktop`，
        // 否则用户在设置页把「GPU 让路」改成「全速」后，本进程内的预览解码仍按旧值跑。
        let yield_now = super::media_pipeline::yield_to_desktop();
        if yield_now {
            // 预览出帧是可延迟的后台工作：让路时降到低于正常优先级，
            // 避免与桌面合成器、Whisper 转写抢 GPU/CPU 时间片。
            apply_background_priority(&mut video_cmd);
        } else {
            apply_no_window(&mut video_cmd);
        }
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
                    frame_w, frame_h
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
        crate::utils::child_registry::adopt(&child);
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
