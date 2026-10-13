//! FFmpeg 引擎 — 音视频处理与音频提取
//!
//! # 硬解边界（路线图 P0-A3，别改错文件）
//!
//! 输入侧硬解（`-hwaccel d3d11va` / `d3d12va`）**只属于预览 / 代理链路**，
//! 实现在 [`crate::engines::media_pipeline`]（`ProxyManager::ensure_proxy`）与
//! `video_player`（实时播放）。本文件负责的是**音频 / 转写链路**，全部保持纯软解：
//!
//! - [`FFmpegEngine::extract_audio`] / [`FFmpegEngine::extract_audio_filtered`]
//! - [`FFmpegEngine::extract_audio_window`]
//! - [`FFmpegEngine::spawn_audio_stream`] 及其 `_at` / `_at_rate` / `_filtered` 变体
//! - [`FFmpegEngine::decode_pcm_mono`]（波形包络 F-014 与说话人特征 F-015 共用）
//! - [`FFmpegEngine::decode_pcm_mono_chunked`]（波形包络 F-014 的流式路径）
//!
//! 为什么钉死：这几条链路只解码音频（`-vn`），视频硬解对它们**零收益**，只会
//! 白白引入一层设备初始化开销与「驱动/素材不兼容导致整段转写失败」的风险；
//! 而转写本来就是全链路 99% 的耗时，稳定性优先于任何解码加速。见
//! `ffmpeg_audio_paths_never_use_hwaccel` 单测——它同时是给后人的警告牌。
//!
//! [`FFmpegEngine::extract_frame`] 也不加：它是从**代理**（已降分辨率的
//! H.264）抽单帧，解码耗时只占端到端延迟的零头，而硬解初始化本身要几十到
//! 几百毫秒，净亏。

use anyhow::{Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tracing::info;

use crate::utils::TempPathGuard;

/// 为自动生成的整轨临时 WAV 取一个并发唯一的路径。
///
/// 只用源文件的 `file_stem` 会让「不同目录下的同名文件」（例如两门课的
/// `01.mp4`）在并发转写时算出同一个 `%TEMP%` 路径：后启动的 FFmpeg 用 `-y`
/// 覆盖掉前一个正在被读取的 WAV，两条转写互相污染。文件名加进程 id + 进程内
/// 原子自增序号即可保证唯一（跨进程靠 pid、进程内靠序号），不引入任何新依赖。
/// 仍保留 `_voice2word.wav` 后缀，让启动清扫 (`utils::temp_cleanup`) 能认出它。
fn temp_wav_path(stem: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "{}_{}_{}_voice2word.wav",
        stem,
        std::process::id(),
        seq
    ))
}

/// 短任务截止期：元数据探测 / 抽帧 / WAV 切片（秒级操作，120 秒足够覆盖冷启动抖动）。
pub(crate) const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(120);

/// 重活截止期：整轨音频提取 / 窗口提取 / 字幕烧录（耗时与素材时长成正比，10 分钟为保守上界）。
pub(crate) const SUBPROCESS_LONG_TIMEOUT: Duration = Duration::from_secs(600);

/// 截止期轮询间隔：25 毫秒对秒级任务几乎无额外延迟，又不会把等待变成忙等。
const DEADLINE_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// PCM 管道读取分片大小：32KB（8kHz 单声道约等于 2 秒音频）。
///
/// 固定缓冲循环读取是「流式解码」的关键：`read_to_end` 会把整轨 PCM 累积成
/// 一个大 Vec（1 小时 8kHz 约 57MB、16kHz 约 115MB），峰值内存与素材时长
/// 成正比；分片读取让内存只与「一批数据」有关，与素材多长无关。
const PCM_READ_CHUNK: usize = 32 * 1024;

/// 解码失败时保留的 stderr 尾部字节数。
///
/// stderr 必须持续抽干（管道写满会连 stdout 一起卡死），但没必要全留：
/// FFmpeg 的失败原因在最后几行，前面都是进度噪声。4KB 足够装下
/// 「Invalid data found」这类关键报错，又不会让坏素材把内存吃掉。
const STDERR_TAIL_CAP: usize = 4 * 1024;

/// 带进错误信息里的 stderr 摘要上限（字符数）：日志与弹窗都不该被
/// FFmpeg 的长篇报错刷屏，够定位就行。
const STDERR_MESSAGE_CAP: usize = 500;

/// 把一段字节流按小端拼成 i16 样本；凑不满一个样本的半个（1 字节）留给下次。
///
/// 管道 `read` 不保证按样本边界返回：一次读到奇数个字节很常见。直接
/// `chunks_exact(2)` 会把这个字节当尾巴永久丢掉——每个奇数分片丢一个样本，
/// 越往后相位漂移越大（波形包络与说话人特征都会跟着偏）。
/// `carry` 就是半样本寄存：只有整段音频最后一个字节凑不出样本时才真正丢弃。
fn push_pcm_bytes(chunk: &[u8], carry: &mut Option<u8>, out: &mut Vec<i16>) {
    let mut bytes = chunk;
    if let Some(first) = carry.take() {
        match bytes.split_first() {
            Some((&second, rest)) => {
                out.push(i16::from_le_bytes([first, second]));
                bytes = rest;
            }
            None => {
                *carry = Some(first);
                return;
            }
        }
    }
    let (pairs, tail) = bytes.as_chunks::<2>();
    out.extend(pairs.iter().map(|b| i16::from_le_bytes(*b)));
    *carry = tail.first().copied();
}

/// 只保留缓冲区尾部 `cap` 字节（保留失败原因，丢掉进度噪声）。
fn tail_bytes(buf: &mut Vec<u8>, cap: usize) {
    if buf.len() > cap {
        let drop = buf.len() - cap;
        buf.drain(..drop);
    }
}

/// 把 stderr 尾部整理成一行可读诊断：逐行 trim、丢掉空行、用 ` | ` 相连，
/// 超长时保留尾部——离失败点最近的那几行才是有效信息。
fn format_stderr_tail(raw: &[u8], cap: usize) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut cleaned = String::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if !cleaned.is_empty() {
            cleaned.push_str(" | ");
        }
        cleaned.push_str(line);
    }
    let chars = cleaned.chars().count();
    if chars > cap {
        cleaned = cleaned.chars().skip(chars - cap).collect();
    }
    cleaned
}

/// 在截止期内等待子进程退出；超时就强杀并报错。
///
/// 过去所有调用点都直接用 status() / output() 无限期阻塞：遇到坏素材或
/// 卡死的 FFmpeg 时，上层转写线程会一直陪葬（既无日志也没有取消手段）。统一
/// 改成「轮询 + 超时杀进程」后，坏任务会失败得可控、可定位。
fn wait_with_deadline(child: &mut Child, timeout: Duration, what: &str) -> Result<ExitStatus> {
    let started = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("等待子进程 {what} 失败"))?
        {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            // 先杀再收尸：kill 失败（进程恰好已退出）也要 wait 掉僵尸，
            // 否则重试路径会攒下一堆进程句柄。
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "子进程 {what} 解码停滞超时（{:.0} 秒没有新的音频数据，已强制终止）",
                timeout.as_secs_f64()
            );
        }
        std::thread::sleep(DEADLINE_POLL_INTERVAL);
    }
}

/// 带截止期地运行命令，并把 stdout / stderr 收齐成 Output。
///
/// 输出必须由后台线程持续抽干：等待期间子进程若把 64KB 管道写满，双方会互相
/// 等待，表现和卡死一模一样。超时路径由主线程 kill，两个读取线程等到管道
/// EOF 会自行结束。
///
/// `pub(crate)` 是给 `crate::engines::media_pipeline` 的代理转码 / 分辨率探测
/// 用的：那边只需要「带截止期 + 收齐输出」这一套行为，**不该**再抄一份轮询 +
/// kill 逻辑，超时预算也复用本文件的 `SUBPROCESS_TIMEOUT` / `SUBPROCESS_LONG_TIMEOUT`。
pub(crate) fn run_with_deadline(cmd: &mut Command, timeout: Duration) -> Result<Output> {
    let what = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("启动子进程 {what} 失败"))?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr_pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });

    let status = wait_with_deadline(&mut child, timeout, &what)?;
    // 走到这里说明子进程已正常退出，管道 EOF 后两个读取线程会立即收尾
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// 构造烧录字幕用的 `-vf` 滤镜串。
///
/// 入参必须是**纯文件名**（不含目录）：调用方需把子进程工作目录切到字幕所在
/// 目录，否则 Windows 绝对路径的盘符冒号会被滤镜解析器当成选项分隔符。
/// 抽成纯函数是为了让滤镜串可被单测直接断言，不必真的起 FFmpeg。
fn subtitle_burn_filter(subtitle_file_name: &str) -> String {
    // 文件名包在单引号里，自带单引号或反斜杠时会被滤镜解析器提前截断：
    // 单引号按滤镜语法转义成 '\''，反斜杠翻倍。
    // 顺序不能颠倒：反斜杠先翻倍，再转义单引号，否则会二次转义反斜杠。
    let escaped = subtitle_file_name
        .replace('\\', "\\\\")
        .replace('\'', r"'\''");
    format!("subtitles=filename='{escaped}',scale=trunc(iw/2)*2:trunc(ih/2)*2")
}
/// 字幕烧录（硬字幕合成）的编码参数。
///
/// 烧录是一条**纯软解 + 重编码**链路：把字幕滤镜结果与视频帧一起重新编码成新
/// 视频，因此它与音频 / 转写链路一样**不启用输入侧硬解**（见文件头「硬解边界」）。
/// 默认参数偏向「一次成功、体积可控」：H.264 + CRF 20 + veryfast + AAC 192k。
#[derive(Debug, Clone)]
pub struct BurnOptions {
    /// 视频编码器。
    pub video_codec: String,
    /// 恒定质量因子（CRF，越小越清晰、体积越大；libx264 常用 18~23）。
    pub crf: u32,
    /// 编码预设（越慢压缩率越高；`veryfast` 兼顾速度与体积）。
    pub preset: String,
    /// 音频编码器。
    pub audio_codec: String,
    /// 音频码率。
    pub audio_bitrate: String,
}

impl Default for BurnOptions {
    fn default() -> Self {
        Self {
            video_codec: "libx264".to_string(),
            crf: 20,
            preset: "veryfast".to_string(),
            audio_codec: "aac".to_string(),
            audio_bitrate: "192k".to_string(),
        }
    }
}
pub struct FFmpegEngine {
    ffmpeg_path: PathBuf,
}

impl FFmpegEngine {
    pub fn new<P: AsRef<Path>>(ffmpeg_path: P) -> Self {
        Self {
            ffmpeg_path: crate::utils::model_download::resolve_configured_executable(
                ffmpeg_path.as_ref(),
            ),
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
                let stem = input_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("audio");
                let path = temp_wav_path(stem);
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
        // 跟随全局让路设置（此前硬编码 CREATE_NO_WINDOW，忽略用户的开关）
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(input_path)
            .args(["-map", "0:a:0?", "-vn", "-sn", "-dn"]);
        if let Some(chain) = filter {
            if !chain.is_empty() {
                cmd.arg("-af").arg(chain);
            }
        }
        cmd.args([
            "-acodec",
            "pcm_s16le",
            "-ar",
            "16000",
            "-ac",
            "1",
            "-threads",
            "0",
        ])
        .arg(&target_wav);
        let status = run_with_deadline(&mut cmd, SUBPROCESS_LONG_TIMEOUT)
            .with_context(|| format!("执行 FFmpeg 失败: {:?}", self.ffmpeg_path))?
            .status;

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
        // 跟随全局让路设置（此前硬编码 CREATE_NO_WINDOW，忽略用户的开关）
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        if start_sec > 0.0 {
            cmd.arg("-ss").arg(format!("{:.3}", start_sec));
        }
        if duration_sec > 0.0 {
            cmd.arg("-t").arg(format!("{:.3}", duration_sec));
        }
        cmd.arg("-i")
            .arg(input_path)
            .args([
                "-map",
                "0:a:0?",
                "-vn",
                "-sn",
                "-dn",
                "-acodec",
                "pcm_s16le",
                "-ar",
                "16000",
                "-ac",
                "1",
            ])
            .arg(&out_wav);
        let status = run_with_deadline(&mut cmd, SUBPROCESS_LONG_TIMEOUT)
            .with_context(|| format!("执行 FFmpeg 窗口抽音频失败: {:?}", self.ffmpeg_path))?
            .status;
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
        anyhow::ensure!(
            speed.is_finite() && (1.0..=1.5).contains(&speed),
            "音频加速倍率须在 1.0～1.5 之间"
        );
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
        // 跟随全局让路设置（此前硬编码 CREATE_NO_WINDOW，忽略用户的开关）
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        if let Some(s) = start_sec {
            cmd.arg("-ss").arg(format!("{:.3}", s.max(0.0)));
        }
        if let Some(d) = duration_sec {
            cmd.arg("-t").arg(format!("{:.3}", d.max(0.1)));
        }
        cmd.arg("-i")
            .arg(input_path)
            .args(["-map", "0:a:0?", "-vn", "-sn", "-dn"]);
        if let Some(chain) = filter {
            if !chain.is_empty() {
                cmd.arg("-af").arg(chain);
            }
        }
        cmd.args([
            "-acodec",
            "pcm_s16le",
            "-ar",
            "16000",
            "-ac",
            "1",
            "-threads",
            "0",
            "-f",
            "wav",
            "pipe:1",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

        let child = cmd
            .spawn()
            .with_context(|| format!("启动 FFmpeg 内存管道失败: {:?}", self.ffmpeg_path))?;
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
    ///
    /// 内部走 [`Self::decode_pcm_mono_chunked`] 累积。相比旧实现（先把整轨字节
    /// 读成一个 `Vec<u8>` 再逐样本转成 `Vec<i16>`），少了一份与素材等大的
    /// 字节缓冲：16kHz 的 1 小时片能省下约 115MB 的瞬时峰值。
    pub fn decode_pcm_mono_filtered<P: AsRef<Path>>(
        &self,
        input_path: P,
        sample_rate: u32,
        filter: Option<&str>,
    ) -> Result<Vec<i16>> {
        let mut samples = Vec::new();
        self.decode_pcm_mono_chunked(input_path, sample_rate, filter, &mut |chunk| {
            samples.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok(samples)
    }

    /// 流式解码整轨音频：每读出一批样本就回调一次，不把整轨留在内存里。
    ///
    /// 给「只要一路扫过去、不需要回看」的用途用，波形包络（F-014）就是典型：
    /// 8kHz 的 1 小时音频是 57MB 原始 PCM，而包络最终只要 2000 个峰值。
    ///
    /// 行为约定：
    /// - `on_chunk` 在**调用线程**上执行，因此可以安全借用非 `Send` 的累积器；
    /// - `on_chunk` 返回 `Err` 时立刻杀掉 FFmpeg 子进程并原样上抛该错误
    ///   （UI 取消这类场景不该让解码继续白跑）；
    /// - 退出码非零时错误信息带 stderr 尾部，失败原因不必再靠复现去猜；
    /// - 成功但一个样本都没有（没有音轨 / 音轨损坏）按失败处理，
    ///   避免把「0 秒音频」这种更隐蔽的故障漏给下游模型。
    pub fn decode_pcm_mono_chunked<P: AsRef<Path>>(
        &self,
        input_path: P,
        sample_rate: u32,
        filter: Option<&str>,
        on_chunk: &mut dyn FnMut(&[i16]) -> Result<()>,
    ) -> Result<()> {
        let mut child = self.spawn_pcm_mono_decoder(input_path.as_ref(), sample_rate, filter)?;
        let what = self.ffmpeg_path.display().to_string();

        // stderr 必须持续抽干（子进程写满 stderr 管道时会连 stdout 一起卡住），
        // 但只留尾部：失败原因在最后几行，进度噪声没有诊断价值。
        let stderr_pipe = child.stderr.take();
        let stderr_reader = std::thread::spawn(move || -> Vec<u8> {
            let mut kept = Vec::new();
            let mut buf = [0u8; 8 * 1024];
            if let Some(mut pipe) = stderr_pipe {
                loop {
                    match pipe.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            kept.extend_from_slice(&buf[..n]);
                            tail_bytes(&mut kept, STDERR_TAIL_CAP);
                        }
                    }
                }
            }
            kept
        });

        // stdout 由读线程抽干、拼样本后经 channel 交给调用线程：`on_chunk` 不是
        // Send，不能进读线程；批次本身很小（32KB 字节 → 16K 样本），过一趟 channel
        // 的拷贝远贵不过一次 FFmpeg 解码。读取与「等退出」也因此保持并行：
        // FFmpeg 不会因为管道写满而卡死，调用线程也不会被读阻塞。
        let stdout_pipe = child.stdout.take();
        let (tx, rx) = mpsc::channel::<Vec<i16>>();
        let reader = std::thread::spawn(move || -> std::io::Result<()> {
            let Some(mut pipe) = stdout_pipe else {
                return Ok(());
            };
            let mut buf = [0u8; PCM_READ_CHUNK];
            let mut carry: Option<u8> = None;
            let mut batch: Vec<i16> = Vec::with_capacity(PCM_READ_CHUNK / 2);
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        batch.clear();
                        push_pcm_bytes(&buf[..n], &mut carry, &mut batch);
                        if batch.is_empty() {
                            continue;
                        }
                        // 接收端已放弃（回调报错 / 超时杀进程）时 send 会失败：
                        // 直接退出，让管道随 drop 关闭，FFmpeg 写不进去会自行结束。
                        if tx.send(std::mem::take(&mut batch)).is_err() {
                            break;
                        }
                        batch = Vec::with_capacity(PCM_READ_CHUNK / 2);
                    }
                    Err(err) => return Err(err),
                }
            }
            // 走到这里说明管道 EOF：奇数总长残留的半个样本凑不出完整样本，丢弃。
            Ok(())
        });

        // 判卡死看的是「多久没有新数据」，不是「总共跑了多久」：解码耗时随素材大小
        // 和磁盘速度成倍变化，超长素材正常解码也可能跑很久，用总预算去卡会把好任务
        // 误杀；而真正要抓的故障（坏文件让 ffmpeg 挂在半路）表现为管道不再吐字节。
        let mut stall_deadline = Instant::now() + SUBPROCESS_LONG_TIMEOUT;
        let mut decoded: u64 = 0;
        let mut callback_err: Option<anyhow::Error> = None;
        let mut timed_out = false;
        loop {
            match rx.recv_timeout(DEADLINE_POLL_INTERVAL) {
                Ok(chunk) => {
                    // 还在推进就顺延：有进度的一律不算超时
                    stall_deadline = Instant::now() + SUBPROCESS_LONG_TIMEOUT;
                    decoded += chunk.len() as u64;
                    if let Err(err) = on_chunk(&chunk) {
                        callback_err = Some(err);
                        break;
                    }
                }
                // 既没有新数据、子进程也没退出：到点就判卡死，不能干等
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() >= stall_deadline {
                        timed_out = true;
                        break;
                    }
                }
                // 通道断开 = stdout 已 EOF，子进程随后就该退出。
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        if let Some(err) = callback_err {
            // 调用方已经不要这批数据了：别让 FFmpeg 继续把整轨跑完。
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
        if timed_out {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "子进程 {what} 超时（限时 {:.1} 秒未退出，已强制终止）",
                SUBPROCESS_LONG_TIMEOUT.as_secs_f64()
            );
        }

        // stdout 已 EOF，剩下只是收退出码与 stderr 尾部。这一刻 FFmpeg 的产出已经
        // 结束，直接给一整份预算（解码阶段用的是停滞超时，没有可扣减的总账），
        // 与本文件其它调用点保持同一套语义。
        let status = wait_with_deadline(&mut child, SUBPROCESS_LONG_TIMEOUT, &what)?;
        reader
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("FFmpeg 音频管道读取线程 panic")))
            .context("读取 FFmpeg 音频管道失败")?;
        let stderr_tail = stderr_reader.join().unwrap_or_default();

        if !status.success() {
            // 旧实现只在「一个字节都没读到」时报错，会把「非零退出但已吐出一段
            // PCM」的截断结果当成功返回；这类半截音频会污染波形与说话人特征，
            // 必须整体丢弃，并把 stderr 尾部带出来。
            let detail = format_stderr_tail(&stderr_tail, STDERR_MESSAGE_CAP);
            tracing::warn!(
                samples = decoded,
                stderr = %detail,
                "FFmpeg 音频解码非零退出，已丢弃整段 PCM"
            );
            if detail.is_empty() {
                anyhow::bail!("FFmpeg 音频解码返回非零退出码（已解码 {decoded} 个采样，整段丢弃）");
            }
            anyhow::bail!(
                "FFmpeg 音频解码返回非零退出码（已解码 {decoded} 个采样，整段丢弃）: {detail}"
            );
        }
        if decoded == 0 {
            anyhow::bail!("FFmpeg 音频解码没有得到任何采样数据（可能没有音轨或音轨损坏）");
        }
        Ok(())
    }

    /// 构造整轨 PCM 解码命令并启动子进程。
    ///
    /// [`Self::decode_pcm_mono_filtered`] 与 [`Self::decode_pcm_mono_chunked`]
    /// 共用这一份参数拼装：两条路径的 `-af` / 采样率 / 输出格式必须永远一致，
    /// 否则「同一素材走累积路径与流式路径得到不同音频」会成为很难查的脏 bug。
    fn spawn_pcm_mono_decoder(
        &self,
        input_path: &Path,
        sample_rate: u32,
        filter: Option<&str>,
    ) -> Result<Child> {
        let mut cmd = Command::new(&self.ffmpeg_path);
        // 跟随全局让路设置（此前硬编码 CREATE_NO_WINDOW，忽略用户的开关）
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(input_path)
            .args(["-map", "0:a:0?", "-vn", "-sn", "-dn"]);
        if let Some(chain) = filter {
            if !chain.is_empty() {
                cmd.arg("-af").arg(chain);
            }
        }
        let child = cmd
            .args(["-acodec", "pcm_s16le", "-ac", "1"])
            .arg("-ar")
            .arg(sample_rate.to_string())
            .args(["-f", "s16le", "pipe:1"])
            .stdout(Stdio::piped())
            // stderr 既不能丢（失败原因全在这），也不能直接 null：
            // 改为管道交给专用线程抽干、只留尾部。
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("启动 FFmpeg 音频解码失败: {:?}", self.ffmpeg_path))?;
        // 并入全局作业对象：本进程若被强杀/崩溃，这个 ffmpeg 会被系统一并清掉
        crate::utils::child_registry::adopt(&child);
        Ok(child)
    }

    /// 获取视频/音频的时长 (秒)
    pub fn get_duration<P: AsRef<Path>>(&self, input_path: P) -> f64 {
        // 探测同样带截止期：坏文件让 ffmpeg -i 悬挂时不再拖死调用方。
        let mut cmd = Command::new(&self.ffmpeg_path);
        cmd.arg("-i").arg(input_path.as_ref());
        let output = run_with_deadline(&mut cmd, SUBPROCESS_TIMEOUT);

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
        // 跟随全局让路设置（此前硬编码 CREATE_NO_WINDOW，忽略用户的开关）
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        cmd.arg("-i").arg(input_path.as_ref());
        let output = run_with_deadline(&mut cmd, SUBPROCESS_TIMEOUT).ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            let Some(video_pos) = line.find("Video:") else {
                continue;
            };
            for token in line[video_pos..].split(&[',', ' ', '(', ')']) {
                let Some((w, h)) = token.split_once('x') else {
                    continue;
                };
                let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>()) else {
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

        // 跟随全局让路设置（此前硬编码 CREATE_NO_WINDOW，忽略用户的开关）
        super::media_pipeline::apply_default_child_flags(&mut cmd);

        cmd.arg("-ss")
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
            .arg(out_jpg);
        let status = run_with_deadline(&mut cmd, SUBPROCESS_TIMEOUT)
            .with_context(|| format!("执行 FFmpeg 抽帧失败: {:?}", self.ffmpeg_path))?
            .status;

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
        // 跟随全局让路设置（此前硬编码 CREATE_NO_WINDOW，忽略用户的开关）
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
            .arg("-ss")
            .arg(format!("{:.3}", start_sec.max(0.0)))
            .arg("-t")
            .arg(format!("{:.3}", duration_sec.max(0.1)))
            .arg("-i")
            .arg(wav_path)
            .args(["-c", "copy"]) // 原音频已经是 16kHz s16le WAV，直接流复制，毫秒级完成切片，零 CPU 损耗！
            .arg(&out_wav);
        let status = run_with_deadline(&mut cmd, SUBPROCESS_TIMEOUT)
            .with_context(|| format!("切分 WAV 失败: {:?}", self.ffmpeg_path))?
            .status;
        if !status.success() {
            anyhow::bail!("FFmpeg 切分 WAV 返回非零退出码");
        }
        Ok(out_wav)
    }

    /// 把字幕**烧录**（硬字幕合成）进视频，输出一个自带字幕的新视频文件。
    ///
    /// 与预览链路（`play_video` 的 ffplay `subtitles=` 叠加）不同：那是「看」，
    /// 这里是「焊死进画面」，用户拿到成品可直接分发，无需再进剪辑软件压一遍。
    ///
    /// # 为什么字幕只传文件名
    ///
    /// 字幕滤镜参数里的路径会被 FFmpeg 滤镜语法解析，Windows 绝对路径的盘符
    /// 冒号（`C:`）会被当成选项分隔符而报错。这里把子进程工作目录切到字幕所在
    /// 目录、只传文件名，与预览链路是同一套规避方式。
    ///
    /// # 滤镜链
    ///
    /// `subtitles=` 负责按 ASS 内嵌样式渲染字幕；其后接
    /// `scale=trunc(iw/2)*2:trunc(ih/2)*2` 把宽高对齐到偶数——libx264 的
    /// `yuv420p` 对奇数尺寸会直接编码失败，这一步只可能改动 1px，肉眼不可见。
    pub fn burn_subtitles(
        &self,
        input: &Path,
        subtitle: &Path,
        output: &Path,
        opts: &BurnOptions,
    ) -> Result<PathBuf> {
        let sub_dir = subtitle.parent().filter(|p| !p.as_os_str().is_empty());
        let sub_name = subtitle
            .file_name()
            .and_then(|n| n.to_str())
            .context("字幕文件名不是合法 UTF-8，无法构造烧录滤镜")?;

        if let Some(parent) = output.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建输出目录失败: {:?}", parent))?;
            }
        }

        let mut cmd = Command::new(&self.ffmpeg_path);
        // 跟随全局让路设置（与其它子进程一致，尊重用户的「不抢占」开关）
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        if let Some(dir) = sub_dir {
            cmd.current_dir(dir);
        }
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
            .arg("-i")
            .arg(input)
            .arg("-vf")
            .arg(subtitle_burn_filter(sub_name))
            .args(["-c:v", &opts.video_codec])
            .arg("-preset")
            .arg(&opts.preset)
            .arg("-crf")
            .arg(opts.crf.to_string())
            .args(["-pix_fmt", "yuv420p"])
            .args(["-c:a", &opts.audio_codec])
            .arg("-b:a")
            .arg(&opts.audio_bitrate)
            .arg(output);
        let status = run_with_deadline(&mut cmd, SUBPROCESS_LONG_TIMEOUT)
            .with_context(|| format!("执行 FFmpeg 字幕烧录失败: {:?}", self.ffmpeg_path))?
            .status;

        if !status.success() {
            anyhow::bail!("FFmpeg 字幕烧录返回非零退出码");
        }
        Ok(output.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同名不同目录的源文件必须拿到不同的临时 WAV：否则并发转写会互相覆盖。
    #[test]
    fn temp_wav_path_is_unique_for_same_stem() {
        let a = Path::new("course_a/01.mp4");
        let b = Path::new("course_b/01.mp4");
        assert_eq!(a.file_stem(), b.file_stem(), "前提：两个源文件同名");

        let pa = temp_wav_path(a.file_stem().unwrap().to_str().unwrap());
        let pb = temp_wav_path(b.file_stem().unwrap().to_str().unwrap());
        assert_ne!(pa, pb, "同名不同目录必须产生不同临时路径");
        assert_eq!(pa.parent().unwrap(), std::env::temp_dir());

        for p in [&pa, &pb] {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            assert!(
                name.ends_with("_voice2word.wav"),
                "必须保留项目可识别的后缀: {name}"
            );
            assert!(
                name.contains(&std::process::id().to_string()),
                "路径必须带 pid 以区分进程: {name}"
            );
        }

        // 同一 stem 连续两次调用也不能撞（进程内原子自增序号）
        assert_ne!(pa, temp_wav_path("01"));
    }

    /// 音频 / 转写链路必须保持纯软解（路线图 P0-A3 的边界）。
    ///
    /// 直接扫本文件源码而不是真跑 ffmpeg：这里要防的不是「某个素材硬解失败」，
    /// 而是「后人顺手把 `-hwaccel` 加进抽音频路径」——那种改动跑测试时未必失败，
    /// 却会让本该最稳的一条链路多出一层设备依赖。`-vn` 存在即可确认这些命令
    /// 本来就不解码视频。
    #[test]
    fn ffmpeg_audio_paths_never_use_hwaccel() {
        let src = include_str!("ffmpeg.rs");
        assert!(
            src.contains("\"-vn\""),
            "前提：本文件的音频命令应带 -vn（只解音频）"
        );
        assert!(
            !src.contains("\"-hwaccel\""),
            "抽音频 / 抽帧链路不得出现 -hwaccel：硬解只允许加在预览 / 代理链路（见文件头说明）"
        );
    }

    /// 烧录滤镜串必须只带**文件名**（不含目录），且宽高对齐到偶数：
    /// 前者防止 Windows 盘符冒号被滤镜语法误判，后者防止 libx264 因奇数尺寸失败。
    #[test]
    fn burn_filter_uses_filename_only_and_even_dimensions() {
        let f = subtitle_burn_filter("voice2word_burn_1.ass");
        assert_eq!(
            f,
            "subtitles=filename='voice2word_burn_1.ass',scale=trunc(iw/2)*2:trunc(ih/2)*2"
        );
        assert!(!f.contains("C:"), "滤镜串不应内嵌盘符绝对路径");
        // 即便调用方误传绝对路径，也应只在 file_name 处取值（此处直接验证 helper
        // 不会自作主张地拼接目录，因为目录由 current_dir 负责）。
        let abs = Path::new(r"C:\temp\sub.ass");
        let only_name = subtitle_burn_filter(abs.file_name().unwrap().to_str().unwrap());
        assert_eq!(
            only_name,
            "subtitles=filename='sub.ass',scale=trunc(iw/2)*2:trunc(ih/2)*2"
        );
    }

    /// 烧录默认参数应产出「通用播放器可放」的成品：H.264 + AAC。
    #[test]
    fn burn_options_default_is_mp4_friendly() {
        let o = BurnOptions::default();
        assert_eq!(o.video_codec, "libx264");
        assert_eq!(o.audio_codec, "aac");
        assert!((18..=23).contains(&o.crf), "CRF 应落在常见质量区间");
    }

    /// 缺陷回归：退出码非零但 stdout 已吐出一段 PCM 时，必须整体报错，
    /// 不能把截断音频当成功结果返回（旧判断是 !success() && raw.is_empty()）。
    ///
    /// 顺带钉住「错误信息带 stderr 尾部」：只报一句「退出码非零」时，
    /// 坏音轨 / 滤镜不支持的真正原因只能靠人肉复现。
    #[test]
    #[cfg(target_os = "windows")]
    fn extract_audio_rejects_truncated_pcm_on_nonzero_exit() {
        let dir = std::env::temp_dir().join(unique_temp_dir_name("v2w_fake_ffmpeg_decode"));
        std::fs::create_dir_all(&dir).expect("创建假 FFmpeg 目录失败");
        let fake = dir.join("ffmpeg.cmd");
        // Windows 下 std 会用 cmd.exe /C 执行 .cmd：用它冒充 ffmpeg 最省事。
        std::fs::write(
            &fake,
            "@echo off\r\necho partial-pcm\r\necho bad-audio-stream 1>&2\r\nexit /b 3\r\n",
        )
        .expect("写入假 FFmpeg 失败");

        let engine = FFmpegEngine::new(&fake);
        let err = engine
            .decode_pcm_mono("dummy.mp4", 16000)
            .expect_err("退出码非零必须报错，即便已经产出部分 PCM");
        let msg = err.to_string();
        assert!(msg.contains("采样"), "错误信息应说明已解码的采样数: {msg}");
        assert!(
            msg.contains("bad-audio-stream"),
            "错误信息必须带 stderr 尾部，否则失败原因只能靠复现去猜: {msg}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 缺陷回归：成功退出但一个样本都没有（没有音轨 / 音轨损坏）必须报错。
    /// 空结果流到下游会变成「模型收到 0 秒音频」这类更难查的故障。
    #[test]
    #[cfg(target_os = "windows")]
    fn decode_rejects_empty_pcm_from_successful_run() {
        let dir = std::env::temp_dir().join(unique_temp_dir_name("v2w_fake_ffmpeg_silent"));
        std::fs::create_dir_all(&dir).expect("创建假 FFmpeg 目录失败");
        let fake = dir.join("ffmpeg.cmd");
        std::fs::write(&fake, "@echo off\r\nexit /b 0\r\n").expect("写入假 FFmpeg 失败");

        let engine = FFmpegEngine::new(&fake);
        let err = engine
            .decode_pcm_mono("dummy.mp4", 16000)
            .expect_err("成功退出但零样本必须报错，不能返回空音频");
        assert!(
            err.to_string().contains("没有得到任何采样数据"),
            "错误信息应点明零样本: {err}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 回调放弃（UI 取消 / 累积失败）时必须立刻停掉解码子进程并上抛原错误，
    /// 不能让它把整轨吐完再报错——那是白烧几十秒 CPU。
    #[test]
    #[cfg(target_os = "windows")]
    fn chunked_decode_stops_when_callback_fails() {
        let dir = std::env::temp_dir().join(unique_temp_dir_name("v2w_fake_ffmpeg_cancel"));
        std::fs::create_dir_all(&dir).expect("创建假 FFmpeg 目录失败");
        // 造一段「能持续吐很多字节」的假 PCM：回调放弃后进程若不被杀，
        // 这条命令还要接着写完 2MB。
        std::fs::write(dir.join("payload.bin"), vec![b'A'; 2 * 1024 * 1024])
            .expect("写入假 PCM 失败");
        let fake = dir.join("ffmpeg.cmd");
        // %~dp0 是脚本自身目录：子进程的 cwd 继承测试进程，不能写相对路径。
        std::fs::write(&fake, "@echo off\r\ntype \"%~dp0payload.bin\"\r\n")
            .expect("写入假 FFmpeg 失败");

        let engine = FFmpegEngine::new(&fake);
        let started = Instant::now();
        let mut calls = 0usize;
        let err = engine
            .decode_pcm_mono_chunked("dummy.mp4", 16000, None, &mut |_chunk| {
                calls += 1;
                anyhow::bail!("回调主动中止")
            })
            .expect_err("回调返回 Err 时必须原样上抛");
        assert!(calls >= 1, "必须真的收到至少一批 PCM");
        assert!(
            err.to_string().contains("回调主动中止"),
            "必须原样上抛回调错误: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "回调放弃后必须立刻返回，实际耗时 {:?}",
            started.elapsed()
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 管道读取不保证按样本边界返回：奇数长度的分片必须把多出的半样本
    /// 留给下一次，否则每个奇数分片都永久丢掉一个样本（越读越偏）。
    #[test]
    fn pcm_bytes_are_reassembled_across_odd_reads() {
        let mut carry = None;
        let mut out = Vec::new();
        push_pcm_bytes(&[0x01], &mut carry, &mut out);
        assert!(out.is_empty(), "单个字节凑不出一个样本");
        assert_eq!(carry, Some(0x01), "半个样本必须被寄存");

        push_pcm_bytes(&[0x02], &mut carry, &mut out);
        assert_eq!(out, vec![0x0201], "跨分片的两半必须拼成一个样本");
        assert_eq!(carry, None);

        // 一次给 3 字节：1 个完整样本 + 1 字节留给下次
        out.clear();
        push_pcm_bytes(&[0x10, 0x20, 0x30], &mut carry, &mut out);
        assert_eq!(out, vec![0x2010]);
        assert_eq!(carry, Some(0x30));

        // 补码负数不受影响（0x8000 == i16::MIN）
        out.clear();
        carry = None;
        push_pcm_bytes(&[0x00, 0x80], &mut carry, &mut out);
        assert_eq!(out, vec![i16::MIN]);

        // 空分片不许把寄存的半个样本弄丢
        out.clear();
        push_pcm_bytes(&[], &mut carry, &mut out);
        assert!(out.is_empty());
        assert_eq!(carry, None);
    }

    /// stderr 只留尾部：失败原因在最后几行，前面全是进度噪声。
    #[test]
    fn stderr_tail_keeps_only_the_end() {
        let mut buf = b"0123456789".to_vec();
        tail_bytes(&mut buf, 4);
        assert_eq!(buf, b"6789");

        let mut short = b"ab".to_vec();
        tail_bytes(&mut short, 4);
        assert_eq!(short, b"ab", "没超上限就不该改动");

        let mut exact = b"abcd".to_vec();
        tail_bytes(&mut exact, 4);
        assert_eq!(exact, b"abcd", "正好等于上限时保留全部");
    }

    /// stderr 摘要必须是一行紧凑文本、且总长封顶：它会进日志和 UI 提示，
    /// 不能被 FFmpeg 的多行报错刷屏。
    #[test]
    fn stderr_digest_is_trimmed_and_capped() {
        let raw = b"\n  first line  \n\nsecond line\n";
        assert_eq!(format_stderr_tail(raw, 100), "first line | second line");

        let long = "x".repeat(600);
        let capped = format_stderr_tail(long.as_bytes(), 500);
        assert_eq!(capped.chars().count(), 500, "超长摘要必须截到上限");
        assert!(capped.chars().all(|c| c == 'x'), "截取后不能混入别的字符");

        // 非 UTF-8 字节（Windows 上 FFmpeg 偶尔吐 GBK）不能 panic
        let lossy = format_stderr_tail(&[0xff, 0xfe, b'o', b'k'], 100);
        assert!(lossy.ends_with("ok"), "{lossy}");
    }

    /// 缺陷回归：任何子进程调用都必须带截止期，超时要杀干净并迅速报错。
    #[test]
    #[cfg(target_os = "windows")]
    fn ffmpeg_jobs_are_killed_after_deadline() {
        // 用 ping 冒充卡死的长命令：它是直接子进程，kill 后不会留下孙子进程。
        let mut cmd = Command::new("ping");
        cmd.args(["-n", "30", "127.0.0.1"]);
        let start = Instant::now();
        let err = run_with_deadline(&mut cmd, Duration::from_millis(300))
            .expect_err("超过截止期的子进程必须报错");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "超时路径必须迅速返回，不能等子进程自然结束"
        );
        assert!(
            err.to_string().contains("超时"),
            "错误信息应说明超时: {err}"
        );
    }

    /// 缺陷回归：文件名里的单引号 / 反斜杠必须转义，否则滤镜解析器会提前截断。
    #[test]
    fn burn_filter_escapes_single_quote_in_filename() {
        assert_eq!(
            subtitle_burn_filter("it's.ass"),
            "subtitles=filename='it'\\''s.ass',scale=trunc(iw/2)*2:trunc(ih/2)*2"
        );
        assert!(
            subtitle_burn_filter(r"a\b.ass").contains(r"a\\b.ass"),
            "反斜杠必须成对转义"
        );
    }

    /// 生成互不冲突的临时目录名：进程号区分进程，时间戳区分同一进程内的多次运行。
    fn unique_temp_dir_name(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{prefix}_{}_{nanos}", std::process::id())
    }
}
