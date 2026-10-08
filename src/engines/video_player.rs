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

/// 播放位置 = 本次播放 / 恢复的基准秒 + 已流逝的墙钟秒。
///
/// 抽成纯函数便于单测（不依赖真实播放）。时钟**只**由墙钟推进：视频帧落后时
/// 允许出帧线程丢帧追上，而不是让时钟等帧（旧实现见 `current_play_time`）。
fn play_time_at(base: f64, wall_sec: f64) -> f64 {
    base + wall_sec.max(0.0)
}

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
        if let Some(instant) = *self
            .play_start_instant
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            // 以墙钟（≈音频进度）为唯一基准推进播放位置，不再拿视频帧号兜底。
            // 旧实现取 `min(frame_sec, wall_sec)`：解码一旦落后，视频帧号停在旧帧，
            // 该表达式退化成 frame_sec，时钟被帧号拖住；而音频（ffplay 管道）仍按
            // 墙钟前进，长片就会持续累积音画漂移。视频帧只是画面，跟不上时应由出帧
            // 线程丢帧追上（见 `play` 内出帧线程），时钟不该等它。
            play_time_at(base, instant.elapsed().as_secs_f64())
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
        // 连同最后一帧一起丢弃。`stop()` 的三个调用点（开始转写、时间轴跳转/上下句、
        // 引擎析构）语义都是「结束这次预览」，而跳转路径紧接着会抽一张**新位置**的
        // 静态帧（`trigger_extract_frame`）。若这里留着旧的实时帧，`render_monitor_picture`
        // 只要 `get_frame()` 有值就永远走实时帧分支、静态帧分支根本到不了，用户跳转后
        // 看到的是跳转前那张画面，与时间轴/字幕完全对不上。
        // 注意：暂停（`pause_clock`）**不**清帧——那里要保留最后一帧作定格。
        if let Ok(mut lock) = self.current_frame.lock() {
            *lock = None;
        }
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
        self.is_playing.store(true, Ordering::SeqCst);

        // 启动音频管线：通过 FFmpeg 容器级关键帧快速寻轨 (100ms 响应，杜绝原生 ffplay 耗费 9.7 秒解码卡死的严重音画脱节)
        if self.ffplay_path.exists() {
            let mut feeder_cmd = Command::new(&self.ffmpeg_path);
            apply_no_window(&mut feeder_cmd);
            feeder_cmd
                .args([
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
                    player_cmd
                        .args([
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
        let mut video_child =
            match self.spawn_video(&video_path, &start_sec_str, used_hwaccel, frame_w, frame_h) {
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
                            // 墙钟锚点取「第一帧真正到达」的时刻（音频管线先行启动），
                            // 避免启动延迟被算进时钟造成初始跳变；出帧节奏与时钟同源，
                            // 之后的落后只可能来自解码跟不上。
                            if active_start.is_none() {
                                let now = Instant::now();
                                active_start = Some(now);
                                if let Ok(mut lock) = play_instant_target.lock() {
                                    *lock = Some(now);
                                }
                            }
                            let start_inst = active_start.unwrap_or_else(Instant::now);
                            let target_time = start_inst + FRAME_DURATION * (frame_index as u32);
                            let now = Instant::now();
                            if target_time > now {
                                std::thread::sleep(target_time - now);
                            } else if now.saturating_duration_since(target_time)
                                > FRAME_DURATION * 2
                            {
                                // 解码落后于墙钟：丢掉这一帧直接追上，而不是让时钟等帧。
                                frame_index += 1;
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
                    return Err(std::io::Error::other("hwaccel ffmpeg exited"));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_struct() -> Arc<PlayerFrame> {
        Arc::new(PlayerFrame {
            data: vec![0u8; 4 * 4 * 4],
            width: 4,
            height: 4,
        })
    }

    /// `stop()` 必须清掉最后一帧，否则时间轴跳转后监视器仍显示跳转前的旧画面。
    #[test]
    fn stop_discards_last_frame() {
        let engine = VideoPlayerEngine::new("ffmpeg");
        *engine.current_frame.lock().unwrap() = Some(frame_struct());
        engine.stop();
        assert!(engine.get_frame().is_none(), "stop() 后不应残留上一帧");
    }

    /// 暂停要保留定格画面（只是停时钟），不能顺手把帧清掉。
    #[test]
    fn pause_keeps_last_frame_as_still() {
        let engine = VideoPlayerEngine::new("ffmpeg");
        *engine.current_frame.lock().unwrap() = Some(frame_struct());
        engine.pause_clock();
        assert!(engine.get_frame().is_some(), "暂停应保留定格帧");
    }

    /// 缺陷 1 回归：播放位置必须只由墙钟推进。解码落后时视频帧号停在旧帧，
    /// 旧实现 `min(frame_sec, wall_sec)` 会让时钟被帧号拖住，而音频继续前进。
    #[test]
    fn play_time_follows_wall_clock_not_frame_index() {
        // 帧只走到第 25 帧（1.0s），墙钟已到 3.0s —— 模拟解码落后 2 秒
        let frame_lagging_sec = 25.0 / 25.0;
        let wall_sec = 3.0;
        assert!(
            play_time_at(0.0, wall_sec) > frame_lagging_sec,
            "时钟必须按墙钟推进，不能被落后的帧号拖住"
        );
        // 与旧实现对比：旧值会退化成 frame_lagging_sec，新值等于墙钟
        assert!((play_time_at(0.0, wall_sec) - wall_sec).abs() < 1e-9);
        assert!((play_time_at(120.0, 0.5) - 120.5).abs() < 1e-9);
        // 暂停后恢复（重锚 120.5s）不应跳变：新基线 + 新的墙钟流逝
        assert!((play_time_at(play_time_at(120.0, 0.5), 0.25) - 120.75).abs() < 1e-9);
        // 负的墙钟流逝（时钟被回拨）不得把进度往回带
        assert!((play_time_at(5.0, -1.0) - 5.0).abs() < 1e-9);
    }

    /// 缺陷 1 回归（时钟集成）：播放位置随时间推进，且暂停后停在基线位置，
    /// 恢复播放从该位置继续、不产生跳变。用真实时钟但只断言单调/基线语义。
    #[test]
    fn clock_advances_and_pause_resume_does_not_jump() {
        let engine = VideoPlayerEngine::new("ffmpeg");
        // 未播放：时钟就是基线，不推进
        assert_eq!(engine.current_play_time(), 0.0);

        // 模拟 play() 已设置起点：is_playing=true + 锚点已落地
        engine.is_playing.store(true, Ordering::SeqCst);
        *engine.play_start_seconds.lock().unwrap() = 10.0;
        *engine.play_start_instant.lock().unwrap() = Some(Instant::now());

        let t0 = engine.current_play_time();
        assert!((10.0..11.0).contains(&t0), "应从 10.0s 起推进: {t0}");

        // 暂停：时钟停在当前值，不再推进
        engine.pause_clock();
        let paused = engine.current_play_time();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(engine.current_play_time(), paused, "暂停后时钟不得推进");

        // 恢复：play_start_seconds 已经重锚为 paused，新的锚点从这里继续
        engine.is_playing.store(true, Ordering::SeqCst);
        *engine.play_start_instant.lock().unwrap() = Some(Instant::now());
        let resumed = engine.current_play_time();
        assert!(
            resumed >= paused && resumed < paused + 0.5,
            "恢复应从暂停点继续、不得跳变: paused={paused} resumed={resumed}"
        );

        engine.stop();
    }
}
