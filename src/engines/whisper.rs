//! Whisper 语音识别引擎封装 (基于 whisper-cli)

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{error, info};

use crate::subtitle::Segment;

pub struct WhisperEngine {
    cli_path: PathBuf,
    model_path: PathBuf,
    vad_model_path: Option<PathBuf>,
    threads: u32,
    processors: u32,
    use_gpu: bool,
}

#[derive(Debug, Deserialize)]
struct WhisperJsonOutput {
    result: Option<WhisperResultMeta>,
    transcription: Option<Vec<WhisperJsonSegment>>,
}

#[derive(Debug, Deserialize)]
struct WhisperResultMeta {
    language: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhisperJsonSegment {
    offsets: Option<WhisperOffsets>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhisperOffsets {
    from: Option<i64>, // 毫秒
    to: Option<i64>,   // 毫秒
}

pub enum AudioInput<'a> {
    Path(&'a Path),
    Stream(Box<dyn std::io::Read + Send + 'static>),
}

impl<'a> From<&'a Path> for AudioInput<'a> {
    fn from(p: &'a Path) -> Self {
        AudioInput::Path(p)
    }
}

impl<'a> From<&'a PathBuf> for AudioInput<'a> {
    fn from(p: &'a PathBuf) -> Self {
        AudioInput::Path(p.as_path())
    }
}

impl WhisperEngine {
    pub fn new<P1: AsRef<Path>, P2: AsRef<Path>>(
        cli_path: P1,
        model_path: P2,
        threads: u32,
        processors: u32,
    ) -> Self {
        let default_vad = PathBuf::from("models/whisper/ggml-silero-v6.2.0.bin");
        let vad = if default_vad.exists() {
            Some(default_vad)
        } else {
            None
        };
        Self::with_vad(cli_path, model_path, vad, threads, processors)
    }

    pub fn with_vad<P1: AsRef<Path>, P2: AsRef<Path>>(
        cli_path: P1,
        model_path: P2,
        vad_model_path: Option<PathBuf>,
        threads: u32,
        processors: u32,
    ) -> Self {
        Self::with_device(cli_path, model_path, vad_model_path, threads, processors, true)
    }

    pub fn with_device<P1: AsRef<Path>, P2: AsRef<Path>>(
        cli_path: P1,
        model_path: P2,
        vad_model_path: Option<PathBuf>,
        threads: u32,
        processors: u32,
        use_gpu: bool,
    ) -> Self {
        Self {
            cli_path: cli_path.as_ref().to_path_buf(),
            model_path: model_path.as_ref().to_path_buf(),
            vad_model_path,
            threads,
            processors,
            use_gpu,
        }
    }

    pub fn uses_gpu(&self) -> bool {
        self.use_gpu
    }

    pub fn transcribe<P: AsRef<Path>>(
        &self,
        audio_path: P,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        self.transcribe_with_model(audio_path, language, threads, total_duration, None, progress_cb)
    }

    /// 转写音频文件，支持通过 `model_path_override` 在运行时动态切换模型档位（极速/均衡/精准）
    /// 返回 `(segments, vad_duration_sec)`
    pub fn transcribe_with_model<P: AsRef<Path>>(
        &self,
        audio_path: P,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        model_path_override: Option<&Path>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        self.transcribe_input(
            AudioInput::Path(audio_path.as_ref()),
            language,
            threads,
            total_duration,
            model_path_override,
            progress_cb,
        )
    }

    /// 纯内存管道推流转写：直接从内存流（Read）中泵入 PCM WAV 字节，0 磁盘 I/O 往返
    pub fn transcribe_stream(
        &self,
        stream: Box<dyn std::io::Read + Send + 'static>,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        model_path_override: Option<&Path>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        self.transcribe_input(
            AudioInput::Stream(stream),
            language,
            threads,
            total_duration,
            model_path_override,
            progress_cb,
        )
    }

    /// 统一入口：根据 AudioInput 分发文件路径模式或纯内存管道推流模式
    pub fn transcribe_input<'a>(
        &self,
        audio_input: AudioInput<'a>,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        model_path_override: Option<&Path>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        let temp_dir = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let prefix = temp_dir.join(format!(
            "v2w_whisper_{}_{}",
            std::process::id(),
            unique
        ));

        let lang = language.unwrap_or("auto");
        let th = threads.unwrap_or(self.threads);
        // 读取配置的处理单元并发数，启用多处理器并行解码
        let processors = self.processors.max(1) as usize;

        // 线程保护机制：多处理器并行时，单处理器线程数等额下调，
        // 保证所有处理器的总 CPU 线程受控在配额内，防止 CPU 线程超售抢占桌面资源。
        let effective_th = if processors > 1 {
            (th / (processors as u32)).max(2)
        } else {
            th
        };

        // 优先使用运行时档位覆盖的模型路径，否则使用配置中的默认模型
        let effective_model: PathBuf = match model_path_override {
            Some(p) => p.to_path_buf(),
            None    => self.model_path.clone(),
        };

        let is_stream = matches!(audio_input, AudioInput::Stream(_));
        info!(
            "Whisper 开始转写: [模式: {}], 语言: {}, 线程数: {} (单实例 {}), 并行处理器: {}, 模型: {:?}, VAD: {:?}",
            if is_stream { "纯内存管道推流 (0 磁盘 I/O)" } else { "本地文件" },
            lang, th, effective_th, processors, effective_model, self.vad_model_path
        );

        if !effective_model.exists() {
            anyhow::bail!(
                "Whisper 模型文件未找到: {:?}。请检查模型是否放置在正确目录。",
                effective_model
            );
        }

        if let Some(ref cb) = progress_cb {
            cb(0.0, "Whisper 正在加载模型并开始逐句识别...", None);
        }

        let mut cmd = Command::new(&self.cli_path);
        cmd.arg("-m").arg(&effective_model);

        match &audio_input {
            AudioInput::Path(p) => {
                cmd.arg("-f").arg(p);
                cmd.stdin(std::process::Stdio::null());
            }
            AudioInput::Stream(_) => {
                cmd.arg("-f").arg("-");
                cmd.stdin(std::process::Stdio::piped());
            }
        }

        if let Some(ref vad_path) = self.vad_model_path {
            if vad_path.exists() {
                info!("Whisper 启用 Silero VAD 高密度语音无损压实与时间戳映射: {:?}", vad_path);
                cmd.arg("--vad")
                    .arg("-vm")
                    .arg(vad_path)
                    .arg("-vt")
                    .arg("0.50") // 标称阈值 0.50，精准识别语音并保护句末弱音
                    .arg("-vsd")
                    .arg("250"); // 最小静音间隔 250ms，剥离无效停顿，全自动时间戳映射还原
            }
        }

        cmd.arg("-l")
            .arg(lang)
            .arg("-t")
            .arg(effective_th.to_string())
            .arg("-p")
            .arg(processors.to_string())
            .arg("-bo")
            .arg("1")
            .arg("-bs")
            .arg("1")
            .arg("-mc")
            .arg("32") // 限制跨句自注意力上下文深度为 32，消解自注意力平方增长，提速 20%~30%
            .arg("-sns") // 抑制非语音标记(音乐/掌声/杂音)，杜绝自回归发散与幻读
            .arg("-oj")
            .arg("-of")
            .arg(&prefix);

        // 小模型才关温度回退换速度；turbo 保留 fallback，课堂专有名词不容易瞎编。
        let is_precise = effective_model
            .file_name()
            .and_then(|s| s.to_str())
            .map(|n| n.contains("large") || n.contains("turbo"))
            .unwrap_or(false);
        if !is_precise {
            cmd.arg("-nf");
        }

        if self.use_gpu {
            info!("Whisper 使用 GPU 推理 (启用 Flash Attention)");
            cmd.arg("-fa");
        } else {
            info!("Whisper 无 GPU，强制 CPU 推理 (-ng)");
            cmd.arg("-ng");
        }

        // whisper.cpp 的 zh 词表偏繁体。prompt 作为简体锚点。
        // 精炼提示词并启用 --carry-initial-prompt：
        // 跨段落始终固化复用 Prefix Prompt 的静态 KV-Cache 键值对，
        // 配合 -mc 32 将前序状态直接作为热启动向量，彻底杜绝窗口跳变时的重复前向冷启动！
        if lang == "zh" || lang == "auto" {
            cmd.arg("--prompt")
                .arg("以下是普通话录音。")
                .arg("--carry-initial-prompt");
        }

        cmd.env("PYTHONIOENCODING", "utf-8");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // 0x08000000 (CREATE_NO_WINDOW) | 0x00004000 (BELOW_NORMAL_PRIORITY_CLASS)
            // 降低 whisper-cli 进程优先级，确保 Windows 桌面管理器(DWM)、鼠标光标、前台窗口与输入法
            // 拥有最高响应调度优先权，并发计算跑满时桌面依然绝对流畅、丝滑不卡顿。
            cmd.creation_flags(0x08000000 | 0x00004000);
        }

        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().with_context(|| {
            format!("调用 whisper-cli 失败: {:?}", self.cli_path)
        })?;

        // 若为内存流输入，启动高效流泵送线程 (128KB 环形缓冲，0 磁盘 I/O)
        let stream_pump_handle = match audio_input {
            AudioInput::Stream(stream) => {
                let stdin = child.stdin.take().context("获取 whisper-cli 标准输入管道失败")?;
                Some(std::thread::spawn(move || {
                    use std::io::{copy, BufReader, BufWriter, Write};
                    let mut reader = BufReader::with_capacity(128 * 1024, stream);
                    let mut writer = BufWriter::with_capacity(128 * 1024, stdin);
                    let _ = copy(&mut reader, &mut writer);
                    let _ = writer.flush();
                    // writer 与 stdin 离开作用域自动 drop，向 whisper-cli 发送 EOF
                }))
            }
            AudioInput::Path(_) => None,
        };

        // 异步读取 stdout 与 stderr
        // 关键点：长视频或开启 VAD 时，whisper-cli 会输出海量日志到 stderr，
        // 必须异步并行消费 stderr，防止 Windows 64KB 管道缓冲区打满导致子进程永久死锁挂起！
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let cb_shared = std::sync::Arc::new(progress_cb);
        let cb_clone = cb_shared.clone();

        let stderr_handle = std::thread::spawn(move || {
            if let Some(err) = stderr {
                use std::io::Read;
                let mut raw = Vec::new();
                let _ = std::io::BufReader::new(err).read_to_end(&mut raw);
                decode_cli_bytes(&raw)
            } else {
                String::new()
            }
        });

        let proc_count = processors;
        let stdout_handle = std::thread::spawn(move || {
            let mut captured_lines = Vec::new();
            let mut last_emit = 0.0f64;
            let mut proc_progress = vec![0.0f64; proc_count];
            let mut streamed_count = 0usize;
            let dur = total_duration.unwrap_or(0.0);
            let chunk_dur = if dur > 0.0 && proc_count > 0 {
                dur / (proc_count as f64)
            } else {
                0.0
            };

            if let Some(out) = stdout {
                use std::io::BufRead;
                let mut reader = std::io::BufReader::new(out);
                let mut raw_line = Vec::new();
                loop {
                    raw_line.clear();
                    match reader.read_until(b'\n', &mut raw_line) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    let line = decode_cli_bytes(&raw_line);
                    let trimmed = line.trim();
                    if trimmed.contains("-->") {
                        if let Some((time_bracket, text_part)) = trimmed.split_once(']') {
                            let time_info = time_bracket.trim_start_matches('[').trim();
                            let (start_sec, end_sec) = if let Some((t_start, t_end)) = time_info.split_once("-->") {
                                (Self::parse_time_str(t_start.trim()), Self::parse_time_str(t_end.trim()))
                            } else {
                                (0.0, 0.0)
                            };
                            let ratio = if chunk_dur > 0.0 {
                                let p_idx = ((end_sec / chunk_dur) as usize).min(proc_count - 1);
                                let c_start = p_idx as f64 * chunk_dur;
                                let p = ((end_sec - c_start) / chunk_dur).clamp(0.0, 1.0);
                                if p > proc_progress[p_idx] {
                                    proc_progress[p_idx] = p;
                                }
                                proc_progress.iter().sum::<f64>() / (proc_count as f64)
                            } else if let Some(tot) = total_duration {
                                if tot > 0.0 { (end_sec / tot).clamp(0.0, 1.0) } else { 0.5 }
                            } else {
                                0.5
                            };

                            let clean_text = normalize_zh_text(text_part.trim());
                            let opt_seg = if !clean_text.is_empty() {
                                streamed_count += 1;
                                Some(Segment {
                                    index: streamed_count,
                                    start: start_sec,
                                    end: end_sec,
                                    text: clean_text,
                                    polished: String::new(),
                                    language: None,
                                })
                            } else {
                                None
                            };

                            let display_sec = if proc_count > 1 && dur > 0.0 {
                                ratio * dur
                            } else {
                                end_sec
                            };
                            let mm = (display_sec / 60.0) as u32;
                            let ss = (display_sec % 60.0) as u32;
                            let tot_mm = (dur / 60.0) as u32;
                            let tot_ss = (dur % 60.0) as u32;

                            let label = if dur > 0.0 {
                                format!("已转写至 {mm:02}:{ss:02} / {tot_mm:02}:{tot_ss:02} ({:.1}%)", ratio * 100.0)
                            } else {
                                format!("已转写至 {mm:02}:{ss:02} ({:.1}%)", ratio * 100.0)
                            };

                            // 有新句子时立即推送；无新句子时按进度推进节流推送
                            let should_emit = opt_seg.is_some() || ratio + 0.0001 >= last_emit + 0.01 || ratio >= 0.999;
                            if should_emit {
                                last_emit = ratio;
                                if let Some(cb) = cb_clone.as_ref() {
                                    cb(ratio, &label, opt_seg);
                                }
                            }
                        }
                    }
                    captured_lines.push(line);
                }
            }
            captured_lines.join("\n")
        });

        let output_status = child.wait().with_context(|| "等待 whisper-cli 进程结束失败")?;
        if let Some(h) = stream_pump_handle {
            let _ = h.join();
        }
        let stdout_str = stdout_handle.join().unwrap_or_default();
        let stderr_str = stderr_handle.join().unwrap_or_default();

        if !output_status.success() {
            error!("whisper-cli 运行报错 (退出码 {:?}): {}", output_status.code(), stderr_str);
            anyhow::bail!("Whisper 转写失败: {}", stderr_str.trim());
        }

        let json_file = PathBuf::from(format!("{}.json", prefix.display()));
        let mut segments = Vec::new();

        if json_file.exists() {
            let json_raw = std::fs::read(&json_file).unwrap_or_default();
            let json_str = decode_cli_bytes(&json_raw);
            let parsed: Result<WhisperJsonOutput, _> = serde_json::from_str(&json_str);
            let _ = std::fs::remove_file(&json_file); // 清理临时 json

            if let Ok(data) = parsed {
                let detected_lang = data.result.and_then(|r| r.language);
                if let Some(trans) = data.transcription {
                    for (i, item) in trans.into_iter().enumerate() {
                        let text = normalize_zh_text(&item.text.unwrap_or_default());
                        if text.is_empty() {
                            continue;
                        }
                        let start = item
                            .offsets
                            .as_ref()
                            .and_then(|o| o.from)
                            .unwrap_or(0) as f64
                            / 1000.0;
                        let end = item
                            .offsets
                            .as_ref()
                            .and_then(|o| o.to)
                            .unwrap_or(0) as f64
                            / 1000.0;

                        segments.push(Segment {
                            index: i + 1,
                            start,
                            end,
                            text,
                            polished: String::new(),
                            language: detected_lang.clone(),
                        });
                    }
                }
            }
        }

        // 多处理器并行后，按起始时间戳精准排序并重新编排连续序号，消除可能存在的跨块微小乱序
        segments.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
        for (i, seg) in segments.iter_mut().enumerate() {
            seg.index = i + 1;
        }

        // 回退逻辑：如果 json 没产出，从捕获的 stdout 解析
        if segments.is_empty() && !stdout_str.is_empty() {
            segments = Self::parse_stdout_segments(&stdout_str);
        }

        if let Some(ref cb) = cb_shared.as_ref() {
            cb(1.0, &format!("转写完成，共 {} 个片段", segments.len()), None);
        }

        let vad_sec = Self::parse_vad_time(&stderr_str);
        info!(segments = segments.len(), vad_sec, "Whisper 转写完成");
        Ok((segments, vad_sec))
    }

    /// 从 whisper-cli 的 stderr 日志中提取 Silero VAD 耗时 (秒)
    pub fn parse_vad_time(stderr: &str) -> f64 {
        let mut total_ms = 0.0;
        for line in stderr.lines() {
            if let Some(pos) = line.find("vad time =") {
                let rest = &line[pos + 10..];
                if let Some(end) = rest.find("ms") {
                    if let Ok(ms) = rest[..end].trim().parse::<f64>() {
                        total_ms += ms;
                    }
                }
            }
        }
        total_ms / 1000.0
    }

    /// 解析形如 [00:00:00.000 --> 00:00:02.500]  文本 的标准输出
    fn parse_stdout_segments(output: &str) -> Vec<Segment> {
        let mut segments = Vec::new();
        let mut idx = 1;

        for line in output.lines() {
            let line = line.trim();
            if line.starts_with('[') && line.contains("-->") {
                if let Some(end_bracket) = line.find(']') {
                    let time_range = &line[1..end_bracket];
                    let text = normalize_zh_text(&line[end_bracket + 1..]);
                    if text.is_empty() {
                        continue;
                    }
                    let parts: Vec<&str> = time_range.split("-->").collect();
                    if parts.len() == 2 {
                        let start = Self::parse_time_str(parts[0].trim());
                        let end = Self::parse_time_str(parts[1].trim());
                        segments.push(Segment {
                            index: idx,
                            start,
                            end,
                            text,
                            polished: String::new(),
                            language: None,
                        });
                        idx += 1;
                    }
                }
            }
        }
        segments
    }

    fn parse_time_str(s: &str) -> f64 {
        // HH:MM:SS.mmm
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() == 3 {
            let h: f64 = parts[0].parse().unwrap_or(0.0);
            let m: f64 = parts[1].parse().unwrap_or(0.0);
            let s: f64 = parts[2].parse().unwrap_or(0.0);
            h * 3600.0 + m * 60.0 + s
        } else {
            0.0
        }
    }
}

/// whisper-cli 在 Windows 上可能吐 UTF-8 或本机 ANSI/GBK。
fn decode_cli_bytes(raw: &[u8]) -> String {
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    if let Ok(s) = std::str::from_utf8(raw) {
        return trim_cli_line(s);
    }
    let (cow, _, had_errors) = encoding_rs::GBK.decode(raw);
    if !had_errors {
        return trim_cli_line(&cow);
    }
    trim_cli_line(&String::from_utf8_lossy(raw))
}

fn trim_cli_line(s: &str) -> String {
    s.trim_end_matches(['\r', '\n']).to_string()
}

/// Whisper 的 zh 词表偏繁体；落地前统一转简体。
fn normalize_zh_text(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    zhconv::zhconv(s, zhconv::Variant::ZhHans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traditional_becomes_simplified() {
        assert_eq!(normalize_zh_text("這是繁體中文語音識別"), "这是繁体中文语音识别");
        assert_eq!(normalize_zh_text("概率不等式"), "概率不等式");
    }

    #[test]
    fn gbk_bytes_decode_to_han() {
        let (bytes, _, _) = encoding_rs::GBK.encode("切比雪夫不等式");
        assert_eq!(decode_cli_bytes(&bytes), "切比雪夫不等式");
    }

    #[test]
    fn test_in_memory_stream_transcribe() {
        let cli_path = PathBuf::from("tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe");
        let model_path = PathBuf::from("models/whisper/ggml-base.bin");
        let ffmpeg_path = PathBuf::from("A:\\cppsoft\\ffmpeg-6.9\\bin\\ffmpeg.exe");
        let sample_media = PathBuf::from("resources/sample/sample.mp4");

        if cli_path.exists() && model_path.exists() && ffmpeg_path.exists() && sample_media.exists() {
            let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
            let engine = WhisperEngine::with_device(cli_path, model_path, None, 4, 1, true);

            let mut child = ffmpeg.spawn_audio_stream(&sample_media).expect("FFmpeg 内存流启动失败");
            let stdout = child.stdout.take().expect("获取 stdout 失败");

            let (segs, _) = engine
                .transcribe_stream(
                    Box::new(stdout),
                    Some("zh"),
                    Some(4),
                    Some(6.0),
                    None,
                    None,
                )
                .expect("纯内存推流转写应成功");

            let _ = child.wait();
            assert!(!segs.is_empty(), "内存推流转写应产出有效字幕");
            println!("纯内存管道推流测试成功，生成 {} 条字幕", segs.len());
        }
    }

    #[test]
    #[ignore]
    fn test_real_audio_transcribe() {
        let cli_path = PathBuf::from("tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe");
        let model_path = PathBuf::from("models/whisper/ggml-large-v3-turbo-q5_0.bin");
        let vad_path = Some(PathBuf::from("models/whisper/ggml-silero-v6.2.0.bin"));
        let engine = WhisperEngine::with_device(cli_path, model_path, vad_path, 8, 2, true);
        let mut audio = PathBuf::from("target/test_30s.wav");
        if !audio.exists() {
            audio = PathBuf::from("target/test_2min.wav");
        }
        if audio.exists() {
            let start = std::time::Instant::now();
            let streamed_segs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let streamed_clone = streamed_segs.clone();

            let (segs, vad_sec) = engine
                .transcribe_with_model(
                    &audio,
                    Some("zh"),
                    Some(8),
                    Some(30.0),
                    None,
                    Some(Box::new(move |_ratio, label, opt_seg| {
                        if let Some(s) = opt_seg {
                            println!("  [流式捕获] [{} -> {}] {} ({})", s.start, s.end, s.text, label);
                            streamed_clone.lock().unwrap().push(s);
                        }
                    })),
                )
                .expect("转写应成功");
            let elapsed = start.elapsed().as_secs_f64();
            let captured_count = streamed_segs.lock().unwrap().len();
            println!("转写完成! 耗时: {:.2}s, VAD: {:.2}s, 最终片段数: {}, 流式逐句推送数: {}", elapsed, vad_sec, segs.len(), captured_count);
            assert!(!segs.is_empty(), "应解析出有效字幕片段");
            assert!(captured_count > 0, "应通过流式回调逐句推送到字幕视窗");
            println!("第一句: [{} -> {}] {}", segs[0].start, segs[0].end, segs[0].text);
        }
    }
}
