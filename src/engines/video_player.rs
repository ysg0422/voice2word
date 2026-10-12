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

    // 以 Arc 共享最新帧：出帧线程写入，UI 通过 `take_unconsumed_frame` **取走**，
    // 取走即独占（引用计数 1），杜绝每帧 3.68MB 深拷贝。
    current_frame: Arc<Mutex<Option<Arc<PlayerFrame>>>>,
    /// 播放器当前是否持有一幅可显示的实时画面：发布帧时置 true、`stop()` 置 false，
    /// 暂停（`pause_clock`）不动——定格帧仍算「有画面」。
    /// UI 用它取代旧的 `get_frame().is_some()` 判据：帧被 UI 取走后槽位本来就是空的，
    /// 但画面仍在 UI 的 RenderImage 缓存里显示，不能因此掉回静态帧分支（会闪）。
    has_live_picture: Arc<AtomicBool>,
    /// 已被 UI 消费（take）到的帧版本号：与 `frame_version` 相等即「本版本已取走」。
    /// 同一版本的第二次渲染必须拿不到帧，否则会重复重建纹理、白拷一遍像素。
    consumed_frame_version: AtomicU64,
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
            has_live_picture: Arc::new(AtomicBool::new(false)),
            consumed_frame_version: AtomicU64::new(0),
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
    /// 返回 Arc 指针克隆而非深拷贝；UI 的正常路径走 `take_unconsumed_frame`，
    /// 这里保留给「只看一眼、不接管所有权」的调用方与回归测试。
    pub fn get_frame(&self) -> Option<Arc<PlayerFrame>> {
        self.current_frame.lock().ok()?.clone()
    }

    /// 取走「尚未被 UI 消费」的当前帧：零拷贝，直接把槽位里的 `Arc<PlayerFrame>` 交出去。
    ///
    /// 返回 `Some((帧版本, 帧))` 当且仅当 `frame_version` 比已消费版本新；同一版本的
    /// 第二次调用返回 `None`——UI 据此**复用**自己缓存的 RenderImage，而不是重建纹理，
    /// 更不能因此掉回静态帧/占位（那会闪）。取走后槽位为空、Arc 引用计数为 1，
    /// 调用方 `Arc::try_unwrap` 必然成功：这就是「每帧一次整帧深拷贝」的消失点。
    pub fn take_unconsumed_frame(&self) -> Option<(u64, Arc<PlayerFrame>)> {
        let mut lock = self.current_frame.lock().ok()?;
        let version = self.frame_version.load(Ordering::SeqCst);
        if version == self.consumed_frame_version.load(Ordering::SeqCst) {
            return None;
        }
        // 版本比已消费新、槽位却为空：`stop()` 清帧会留下这种组合（版本号不回退）。
        // 没有画面可取，也不推进已消费版本，等下一帧发布。
        let frame = lock.take()?;
        self.consumed_frame_version.store(version, Ordering::SeqCst);
        Some((version, frame))
    }

    /// 播放器当前是否持有一幅可显示的实时画面（发布过帧且未 `stop()`；暂停保留定格帧）。
    ///
    /// UI 用它取代 `get_frame().is_some()`：帧被 UI 取走后槽位为空，但画面仍在 UI 的
    /// RenderImage 缓存里，此时**不能**掉回静态帧分支。`stop()` 翻回 false 之后，
    /// 监视器才能回到静态定位帧那条路（时间轴跳转、开始转写等入口都经过 `stop()`）。
    pub fn has_live_picture(&self) -> bool {
        self.has_live_picture.load(Ordering::SeqCst)
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
        // 「当前有画面」判据与槽位同源翻回 false：UI 靠它取代 `get_frame().is_some()`
        // （帧被 take 之后槽位本来就是空的，不能再当判据），回到静态定位帧那条路。
        // 暂停不走这里，定格帧的判据保持 true。
        self.has_live_picture.store(false, Ordering::SeqCst);
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

    /// 播放起点钳制：`-ss` 只接受非负起点，而 [`Self::current_play_time`] 把基线
    /// 原样当播放位置返回，两者必须同源。负起点会让「画面/音频从 0 秒起」与
    /// 「时钟停在负数」分叉：起点 -3.0 时 `-ss` 被钳成 0.000，时钟却从 -3.0 读秒。
    /// `f64::max` 在任一参数为 NaN 时返回另一个参数，顺带把 NaN 归一为 0。
    fn clamp_start_seconds(start_seconds: f64) -> f64 {
        start_seconds.max(0.0)
    }

    pub fn play<P: AsRef<Path>>(&self, video_path: P, start_seconds: f64) {
        self.stop();

        let video_path = video_path.as_ref().to_path_buf();
        if !video_path.exists() {
            warn!("播放失败：文件不存在 {:?}", video_path);
            return;
        }

        // 起点由上游播放头（`AppState::current_time`）传入，而它有一条**未钳制**的
        // 写入路径：`select_segment` 直接取 `seg.start`，导入的 JSON 字幕若带负时间轴
        // （`json_timestamp` 接受负的 JSON 数字，只拦 `end < start`）就会把播放头带成
        // 负数；而「上一句/下一句」「统计卡跳句」等入口不会再补一次 `seek_to` 的钳制。
        // 这里在引擎边界统一钳回非负，并与下面的 `-ss` 复用同一个值，杜绝两条路径分叉。
        let start_seconds = Self::clamp_start_seconds(start_seconds);
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let start_sec_str = format!("{:.3}", start_seconds);
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
        let has_live_picture_target = self.has_live_picture.clone();
        let is_playing_flag = self.is_playing.clone();
        let generation = self.generation.clone();
        let play_instant_target = self.play_start_instant.clone();

        let spawn_result = std::thread::Builder::new()
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
                            // 预检只是快速出口；真正决定写不写的是下面持锁写回点的复查，
                            // 因为从预检到写回之间隔着最长约 40ms 的让帧 sleep。
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
                                    // 锚点与帧同属一次播放会话：代次已过期就不能再写，
                                    // 否则旧线程会覆盖新会话/暂停恢复后的时钟锚点。
                                    if generation.load(Ordering::SeqCst) != gen {
                                        break;
                                    }
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
                            // 写回点必须在**持锁状态下**复查代次：从预检到这里隔着最长约
                            // 40ms 的让帧 sleep，期间 stop()/seek（play()）可能已自增
                            // generation 并清空缓冲；不复查就会把过期帧盖回新会话，表现为
                            // 跳转后监视器闪回旧画面、stop() 后 get_frame() 又有值。
                            if !try_publish_frame(
                                &frame_target,
                                &frame_version_target,
                                &has_live_picture_target,
                                &generation,
                                &is_playing_flag,
                                gen,
                                PlayerFrame {
                                    data: buf,
                                    width: frame_w,
                                    height: frame_h,
                                },
                            ) {
                                // 代次已过期或已停止：本次会话结束，退出出帧循环。
                                break;
                            }
                            frame_index += 1;
                        }
                        Err(_) => break,
                    }
                }
            });

        // 线程创建失败不能吞：`is_playing` 若停在 true，界面会一直显示「正在播放」
        // 却永远没有新帧。`apply_spawn_result` 就地复位播放状态并记录错误
        // （`play()` 返回 `()`，错误无法向上返回，就地记录是本文件既有约定，见启动
        // 视频流失败的两处处理），这里再回收已启动的音视频子进程，避免留下占着
        // 视频文件句柄的孤儿进程。
        if apply_spawn_result(spawn_result, &self.is_playing, gen).is_none() {
            self.kill_procs();
        }

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

/// 把一帧写回共享缓冲，返回是否成功写入。
///
/// 写入前**在已持有帧锁的状态下**复查代次与播放标志：`stop()` / seek（`play()`）
/// 会自增 `generation`，而出帧线程从读到数据到真正写回之间隔着最长约 40ms 的
/// 让帧 `sleep`，期间旧会话可能已被终止并清空缓冲。缺少这层复查时，旧线程睡醒后
/// 仍会把过期帧盖回去：`stop()` 后 `get_frame()` 又拿到旧画面、跳转后监视器闪回
/// 跳转前的帧。
fn try_publish_frame(
    frame_slot: &Mutex<Option<Arc<PlayerFrame>>>,
    frame_version: &AtomicU64,
    has_live_picture: &AtomicBool,
    generation: &AtomicU64,
    is_playing: &AtomicBool,
    gen: u64,
    frame: PlayerFrame,
) -> bool {
    let Ok(mut lock) = frame_slot.lock() else {
        // 锁中毒（持锁线程 panic）时直接丢弃本帧，不把状态写坏。
        return false;
    };
    if generation.load(Ordering::SeqCst) != gen || !is_playing.load(Ordering::SeqCst) {
        return false;
    }
    *lock = Some(Arc::new(frame));
    frame_version.fetch_add(1, Ordering::SeqCst);
    // 「当前有画面」判据在**锁内**置位：UI 先看到 true，再 take 必定能看到帧；
    // 反过来（先置位再写槽位）会让 UI 取到空槽位而误判没有画面。
    has_live_picture.store(true, Ordering::SeqCst);
    true
}

/// 收尾出帧线程的 `spawn` 结果：失败时立刻复位 `is_playing` 并就地记录日志，返回
/// `None`；成功时把 `JoinHandle` 原样带回（本文件不 join 出帧线程，交由调用方丢弃）。
///
/// 抽成独立函数是为了让「spawn 失败 → 状态复位」这条路径能用注入的 `Err` 直接
/// 单测，不必真的制造线程创建失败。复位与日志都必须发生：若 `is_playing` 停在
/// true，界面会永久显示「正在播放」却不会再有任何新帧。
fn apply_spawn_result<T>(
    spawn_result: std::io::Result<T>,
    is_playing: &AtomicBool,
    gen: u64,
) -> Option<T> {
    match spawn_result {
        Ok(handle) => Some(handle),
        Err(e) => {
            is_playing.store(false, Ordering::SeqCst);
            error!(gen, error = %e, "出帧线程创建失败，已复位播放状态");
            None
        }
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

    /// 播放起点必须非负，且 `-ss` 与时钟基线同源。
    ///
    /// 回归路径：导入带负时间轴的 JSON 字幕后 `select_segment` 把播放头设成负的
    /// `seg.start`，而「上一句/下一句」等入口不补 `seek_to` 的钳制 → 播放时旧实现
    /// 给 `-ss` 传 0 秒、却把负值原样存进基线，时钟与音频从一开始就错开。
    #[test]
    fn play_start_seconds_is_clamped_to_non_negative() {
        assert_eq!(VideoPlayerEngine::clamp_start_seconds(-3.0), 0.0);
        assert_eq!(VideoPlayerEngine::clamp_start_seconds(-1e-9), 0.0);
        assert_eq!(VideoPlayerEngine::clamp_start_seconds(0.0), 0.0);
        assert_eq!(VideoPlayerEngine::clamp_start_seconds(12.5), 12.5);
        // NaN 若漏进基线，时间读数与一切比较都会变成 NaN。
        assert_eq!(VideoPlayerEngine::clamp_start_seconds(f64::NAN), 0.0);
    }

    fn frame_with_marker(marker: u8) -> PlayerFrame {
        PlayerFrame {
            data: vec![marker; 4 * 4 * 4],
            width: 4,
            height: 4,
        }
    }

    /// 缺陷 2 回归：`stop()` / seek（重播）之后，旧出帧线程不得把过期帧写回缓冲。
    ///
    /// 时序：出帧线程通过预检拿到自己的代次 → 让帧 `sleep` 期间 `stop()` / `play()`
    /// 自增 `generation` 并清空缓冲 → 线程睡醒后仍尝试写帧。写回点必须在**持锁状态
    /// 下**复查代次并放弃：既不覆盖缓冲，也不推进 `frame_version`。
    #[test]
    fn stale_frame_not_written_after_stop_or_seek() {
        let engine = VideoPlayerEngine::new("ffmpeg");
        engine.is_playing.store(true, Ordering::SeqCst);
        // 出帧线程启动时记下的代次（通过预检的那一代）
        let stale_gen = engine.generation();

        // 对照组：代次未过期时写回必须生效
        assert!(try_publish_frame(
            &engine.current_frame,
            &engine.frame_version,
            &engine.has_live_picture,
            &engine.generation,
            &engine.is_playing,
            stale_gen,
            frame_with_marker(0x11),
        ));
        assert!(engine.get_frame().is_some(), "有效代次的帧应写入缓冲");
        let version_after_valid_write = engine.frame_version();

        // 场景 A：stop() 自增代次并清空缓冲，旧线程睡醒后仍尝试写回
        engine.stop();
        assert!(!try_publish_frame(
            &engine.current_frame,
            &engine.frame_version,
            &engine.has_live_picture,
            &engine.generation,
            &engine.is_playing,
            stale_gen,
            frame_with_marker(0x22),
        ));
        assert!(engine.get_frame().is_none(), "stop() 之后不得写回过期帧");
        assert_eq!(
            engine.frame_version(),
            version_after_valid_write,
            "过期帧不得推进帧版本号"
        );

        // 场景 B：seek / 重播走 play() 的代次自增（这里只模拟自增这一步）
        engine.is_playing.store(true, Ordering::SeqCst);
        let new_gen = engine.generation.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(!try_publish_frame(
            &engine.current_frame,
            &engine.frame_version,
            &engine.has_live_picture,
            &engine.generation,
            &engine.is_playing,
            stale_gen,
            frame_with_marker(0x33),
        ));
        assert!(engine.get_frame().is_none(), "seek 之后不得写回过期帧");

        // 新代次的写回必须被接受：被拒的原因是代次过期，而不是通道本身坏掉
        assert!(try_publish_frame(
            &engine.current_frame,
            &engine.frame_version,
            &engine.has_live_picture,
            &engine.generation,
            &engine.is_playing,
            new_gen,
            frame_with_marker(0x44),
        ));
        assert!(engine.get_frame().is_some(), "新代次的帧应能写入缓冲");
    }

    /// 缺陷 1 回归：出帧线程创建失败时播放标志必须复位，界面不能停在「正在播放」。
    ///
    /// 用注入的 `Err` 驱动复位路径，不真的制造线程创建失败。
    #[test]
    fn spawn_failure_clears_is_playing() {
        let is_playing = AtomicBool::new(true);
        let injected: std::io::Result<()> = Err(std::io::Error::other("注入的 spawn 失败"));
        assert!(
            apply_spawn_result(injected, &is_playing, 7).is_none(),
            "spawn 失败应返回 None"
        );
        assert!(
            !is_playing.load(Ordering::SeqCst),
            "spawn 失败后 is_playing 必须复位为 false"
        );

        // 成功路径不得误伤播放状态
        let is_playing = AtomicBool::new(true);
        let ok: std::io::Result<()> = Ok(());
        assert!(apply_spawn_result(ok, &is_playing, 7).is_some());
        assert!(
            is_playing.load(Ordering::SeqCst),
            "spawn 成功不应复位播放状态"
        );
    }

    /// 零拷贝协议：新版本帧只交出去一次，且交出去的必须是**独占** Arc。
    ///
    /// 引用计数为 1 才能让 UI 侧 `Arc::try_unwrap` 必然成功——这条断言就是
    /// 「每帧 3.7MB 深拷贝」消失的证据。同一版本第二次取帧必须拿不到：
    /// UI 拿到 None 就该复用自己缓存的 RenderImage，而不是重建/掉静态帧。
    #[test]
    fn take_unconsumed_frame_hands_over_exclusive_arc_once() {
        let engine = VideoPlayerEngine::new("ffmpeg");
        engine.is_playing.store(true, Ordering::SeqCst);
        let gen = engine.generation();
        assert!(try_publish_frame(
            &engine.current_frame,
            &engine.frame_version,
            &engine.has_live_picture,
            &engine.generation,
            &engine.is_playing,
            gen,
            frame_with_marker(0x5A),
        ));

        let (v1, f1) = engine.take_unconsumed_frame().expect("新版本帧应能取走");
        assert_eq!(v1, engine.frame_version());
        assert_eq!(f1.data[0], 0x5A, "取到的应是刚发布的那一帧");
        assert_eq!(
            Arc::strong_count(&f1),
            1,
            "取走后必须独占：UI 侧 try_unwrap 才会成功（零拷贝）"
        );
        assert!(engine.get_frame().is_none(), "帧取走后槽位应为空");
        assert!(
            engine.take_unconsumed_frame().is_none(),
            "同一版本不得二次取帧：UI 必须复用已缓存的纹理"
        );

        // 新版本发布后再次可取：版本号单调前进
        assert!(try_publish_frame(
            &engine.current_frame,
            &engine.frame_version,
            &engine.has_live_picture,
            &engine.generation,
            &engine.is_playing,
            gen,
            frame_with_marker(0x6B),
        ));
        let (v2, f2) = engine.take_unconsumed_frame().expect("新版本帧应能取走");
        assert!(v2 > v1, "帧版本必须单调前进: {v1} -> {v2}");
        assert_eq!(f2.data[0], 0x6B);
        assert!(engine.take_unconsumed_frame().is_none());
    }

    /// 会话重启回归：`stop()`（时间轴跳转 / 上下句 / 暂停都经过它）清帧之后，
    /// 新会话的首帧必须能正常取到——消费游标不能因为清帧而卡死。
    #[test]
    fn take_recovers_after_stop_once_new_session_publishes() {
        let engine = VideoPlayerEngine::new("ffmpeg");
        engine.is_playing.store(true, Ordering::SeqCst);
        let gen1 = engine.generation();
        assert!(try_publish_frame(
            &engine.current_frame,
            &engine.frame_version,
            &engine.has_live_picture,
            &engine.generation,
            &engine.is_playing,
            gen1,
            frame_with_marker(0x41),
        ));
        assert!(engine.take_unconsumed_frame().is_some());
        assert!(engine.take_unconsumed_frame().is_none());

        engine.stop();
        assert!(
            engine.take_unconsumed_frame().is_none(),
            "stop() 后槽位已清，不应有帧可取"
        );

        // 新会话：play() 会先 stop() 再自增代次（这里只模拟这两步）
        engine.is_playing.store(true, Ordering::SeqCst);
        let gen2 = engine.generation.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(try_publish_frame(
            &engine.current_frame,
            &engine.frame_version,
            &engine.has_live_picture,
            &engine.generation,
            &engine.is_playing,
            gen2,
            frame_with_marker(0x42),
        ));
        let (v, f) = engine.take_unconsumed_frame().expect("新会话首帧应能取走");
        assert!(v > 1, "版本号必须继续单调前进: {v}");
        assert_eq!(f.data[0], 0x42);
        assert!(engine.has_live_picture(), "新会话发布帧后判据应为 true");
    }

    /// 「当前有画面」判据：发布置 true、暂停保留 true、`stop()` 置 false。
    ///
    /// UI 用它取代 `get_frame().is_some()`；而帧被 take 之后槽位为空，
    /// 判据绝不能因此翻回 false（否则暂停定格时会闪回静态帧/占位）。
    #[test]
    fn live_picture_flag_tracks_publish_pause_and_stop() {
        let engine = VideoPlayerEngine::new("ffmpeg");
        assert!(!engine.has_live_picture(), "未发布过帧不该有画面");

        engine.is_playing.store(true, Ordering::SeqCst);
        let gen = engine.generation();
        assert!(try_publish_frame(
            &engine.current_frame,
            &engine.frame_version,
            &engine.has_live_picture,
            &engine.generation,
            &engine.is_playing,
            gen,
            frame_with_marker(0x21),
        ));
        assert!(engine.has_live_picture(), "发布帧后判据应为 true");

        // 帧被 UI 取走：槽位空了，但画面已交给 UI 显示，判据必须仍是 true
        let _ = engine.take_unconsumed_frame().expect("新版本帧应能取走");
        assert!(engine.get_frame().is_none(), "取走后槽位应为空");
        assert!(
            engine.has_live_picture(),
            "画面已交给 UI 缓存，判据不能因槽位为空而变 false"
        );

        // 暂停保留定格帧：判据不动
        engine.pause_clock();
        assert!(
            engine.has_live_picture(),
            "暂停必须保留定格帧的「有画面」判据"
        );

        // stop()：判据翻回 false，UI 回到静态定位帧那条路
        engine.stop();
        assert!(!engine.has_live_picture(), "stop() 后判据必须为 false");
        assert!(
            engine.take_unconsumed_frame().is_none(),
            "stop() 后不应有帧可取"
        );

        // stop() 之后过期帧的写回被拒，也不得把判据翻回 true
        engine.is_playing.store(true, Ordering::SeqCst);
        assert!(!try_publish_frame(
            &engine.current_frame,
            &engine.frame_version,
            &engine.has_live_picture,
            &engine.generation,
            &engine.is_playing,
            gen,
            frame_with_marker(0x31),
        ));
        assert!(!engine.has_live_picture(), "过期帧不得把判据翻回 true");
    }
}
