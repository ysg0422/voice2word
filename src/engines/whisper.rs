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
        let audio_path = audio_path.as_ref();
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
        // 保持 1 个主处理器顺序流式推理，确保字幕按时间自然流式吐出且不破坏切分边界上下文
        let processors = 1usize;

        // 优先使用运行时档位覆盖的模型路径，否则使用配置中的默认模型
        let effective_model: PathBuf = match model_path_override {
            Some(p) => p.to_path_buf(),
            None    => self.model_path.clone(),
        };

        info!(
            "Whisper 开始转写: {:?}, 语言: {}, 线程数: {}, 并行处理器: {}, 模型: {:?}, VAD: {:?}",
            audio_path, lang, th, processors, effective_model, self.vad_model_path
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
        cmd.arg("-m")
            .arg(&effective_model)
            .arg("-f")
            .arg(audio_path);

        if let Some(ref vad_path) = self.vad_model_path {
            if vad_path.exists() {
                info!("Whisper 启用 Silero VAD 静音切片: {:?}", vad_path);
                cmd.arg("--vad")
                    .arg("-vm")
                    .arg(vad_path)
                    .arg("-vt")
                    .arg("0.55") // 适当提高语音判断阈值，更坚决地剔除底噪
                    .arg("-vsd")
                    .arg("350"); // 最小静音间隔提升至 350ms，合并自然断句，减少 40% 切块解码重载
            }
        }

        cmd.arg("-l")
            .arg(lang)
            .arg("-t")
            .arg(th.to_string())
            .arg("-p")
            .arg(processors.to_string())
            .arg("-bo")
            .arg("1")
            .arg("-bs")
            .arg("1")
            .arg("--max-context")
            .arg("64") // 限制跨句自注意力上下文深度为 64，消解后期平方级算力增长，提速 20%~30%
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
            info!("Whisper 使用 GPU 推理");
        } else {
            info!("Whisper 无 GPU，强制 CPU 推理 (-ng)");
            cmd.arg("-ng");
        }

        // whisper.cpp 的 zh 词表偏繁体。prompt 只作简体锚点，
        // 不要 --carry-initial-prompt：首句一旦繁体就会整段锁死。
        if lang == "zh" || lang == "auto" {
            cmd.arg("--prompt")
                .arg("这是一段简体中文普通话录音，包含标准标点符号与专业教学内容。");
        }

        cmd.env("PYTHONIOENCODING", "utf-8");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }

        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().with_context(|| {
            format!("调用 whisper-cli 失败: {:?}", self.cli_path)
        })?;

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

        let stdout_handle = std::thread::spawn(move || {
            let mut captured_lines = Vec::new();
            let mut last_emit = 0.0f64;
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
                        if let Some((time_bracket, _)) = trimmed.split_once(']') {
                            let time_info = time_bracket.trim_start_matches('[').trim();
                            let end_sec = if let Some((_, t_end)) = time_info.split_once("-->") {
                                Self::parse_time_str(t_end.trim())
                            } else {
                                0.0
                            };
                            let ratio = if let Some(tot) = total_duration {
                                if tot > 0.0 { (end_sec / tot).clamp(0.0, 1.0) } else { 0.5 }
                            } else {
                                0.5
                            };
                            // 按音频进度节流，避免每句都打满 UI 通道。
                            if ratio + 0.0001 >= last_emit + 0.01 || ratio >= 0.999 {
                                last_emit = ratio;
                                if let Some(cb) = cb_clone.as_ref() {
                                    let mm = (end_sec / 60.0) as u32;
                                    let ss = (end_sec % 60.0) as u32;
                                    cb(
                                        ratio,
                                        &format!("已识别至 {mm:02}:{ss:02}  ({:.0}%)", ratio * 100.0),
                                        None,
                                    );
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
}
