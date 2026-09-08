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
        Self {
            cli_path: cli_path.as_ref().to_path_buf(),
            model_path: model_path.as_ref().to_path_buf(),
            vad_model_path,
            threads,
            processors,
        }
    }

    /// 转写音频文件，返回 Segment 列表 (支持实时流式时间戳解析、逐句吐出与精确进度)
    pub fn transcribe<P: AsRef<Path>>(
        &self,
        audio_path: P,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<Vec<Segment>> {
        let audio_path = audio_path.as_ref();
        let temp_dir = std::env::temp_dir();
        let prefix = temp_dir.join(format!("v2w_whisper_{}", std::process::id()));

        let lang = language.unwrap_or("auto");
        let th = threads.unwrap_or(self.threads);
        // 在 16 逻辑核心机器上以 2 个 8 线程处理器并行执行，避免单个
        // Whisper 上下文限制整机吞吐量。最多两个以限制模型内存占用。
        let processors = if self.processors > 0 {
            self.processors as usize
        } else {
            std::thread::available_parallelism()
                .map(|count| (count.get() / th.max(1) as usize).clamp(1, 2))
                .unwrap_or(1)
        };
        info!(
            "Whisper 开始转写: {:?}, 语言: {}, 线程数: {}, 并行处理器: {}, 模型: {:?}, VAD: {:?}",
            audio_path, lang, th, processors, self.model_path, self.vad_model_path
        );

        if !self.model_path.exists() {
            anyhow::bail!(
                "Whisper 模型文件未找到: {:?}。请检查模型是否放置在正确目录。",
                self.model_path
            );
        }

        if let Some(ref cb) = progress_cb {
            cb(0.0, "Whisper 正在加载模型并开始逐句识别...", None);
        }

        let mut cmd = Command::new(&self.cli_path);
        cmd.arg("-m")
            .arg(&self.model_path)
            .arg("-f")
            .arg(audio_path);

        if let Some(ref vad_path) = self.vad_model_path {
            if vad_path.exists() {
                info!("Whisper 启用 Silero VAD 静音切片: {:?}", vad_path);
                cmd.arg("--vad")
                    .arg("-vm")
                    .arg(vad_path)
                    .arg("-vt")
                    .arg("0.50");
            }
        }

        cmd.arg("-l")
            .arg(lang)
            .arg("-t")
            .arg(th.to_string())
            .arg("-p")
            .arg(processors.to_string())
            .arg("-bo")
            .arg("1") // 仅保留最佳候选，配合单束搜索降低解码计算量
            .arg("-bs")
            .arg("1") // 单束搜索：快速模式
            .arg("-oj") // 输出 JSON 结果
            .arg("-of")
            .arg(&prefix);

        // 中文普通话强提示词与上下文携带：锚定中文词表，严禁漂移幻读为英文
        if lang == "zh" || lang == "auto" {
            cmd.arg("--prompt")
                .arg("以下是普通话中文语音识别，包含学术概念与数学公式，请全部使用简体中文输出。")
                .arg("--carry-initial-prompt");
        }

        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().with_context(|| {
            format!("调用 whisper-cli 失败: {:?}", self.cli_path)
        })?;

        // 逐行异步读取 stdout，实时提取时间戳回传进度与字幕片段
        let stdout = child.stdout.take();
        let cb_shared = std::sync::Arc::new(progress_cb);
        let cb_clone = cb_shared.clone();

        let stdout_handle = std::thread::spawn(move || {
            let mut captured_lines = Vec::new();
            let mut seg_index = 1usize;
            if let Some(out) = stdout {
                use std::io::BufRead;
                let reader = std::io::BufReader::new(out);
                for line_res in reader.lines() {
                    if let Ok(line) = line_res {
                        let trimmed = line.trim();
                        if trimmed.contains("-->") {
                            // 示例: [00:01:23.450 --> 00:01:28.900]   切比雪夫不等式
                            let parts: Vec<&str> = trimmed.split(']').collect();
                            let time_info = parts.first().map(|s| s.trim_start_matches('[')).unwrap_or("");
                            let text_part = parts.get(1).map(|s| s.trim()).unwrap_or("");
                            let time_parts: Vec<&str> = time_info.split("-->").collect();
                            let (start_sec, end_sec) = if time_parts.len() == 2 {
                                (Self::parse_time_str(time_parts[0].trim()), Self::parse_time_str(time_parts[1].trim()))
                            } else {
                                (0.0, 0.0)
                            };

                            let ratio = if let Some(tot) = total_duration {
                                if tot > 0.0 { (end_sec / tot).clamp(0.0, 1.0) } else { 0.5 }
                            } else {
                                0.5
                            };

                            let opt_seg = if !text_part.is_empty() {
                                let seg = Segment {
                                    index: seg_index,
                                    start: start_sec,
                                    end: end_sec,
                                    text: text_part.to_string(),
                                    polished: String::new(),
                                    language: None,
                                };
                                seg_index += 1;
                                Some(seg)
                            } else {
                                None
                            };

                            if let Some(cb) = cb_clone.as_ref() {
                                cb(ratio, &format!("[{}] {}", time_info, text_part), opt_seg);
                            }
                        }
                        captured_lines.push(line);
                    }
                }
            }
            captured_lines.join("\n")
        });

        let output_status = child.wait().with_context(|| "等待 whisper-cli 进程结束失败")?;
        let stdout_str = stdout_handle.join().unwrap_or_default();

        if !output_status.success() {
            let mut stderr_str = String::new();
            if let Some(mut err) = child.stderr.take() {
                use std::io::Read;
                let _ = err.read_to_string(&mut stderr_str);
            }
            error!("whisper-cli 运行报错 (退出码 {:?}): {}", output_status.code(), stderr_str);
            anyhow::bail!("Whisper 转写失败: {}", stderr_str.trim());
        }

        let json_file = PathBuf::from(format!("{}.json", prefix.display()));
        let mut segments = Vec::new();

        if json_file.exists() {
            let json_str = std::fs::read_to_string(&json_file)?;
            let parsed: Result<WhisperJsonOutput, _> = serde_json::from_str(&json_str);
            let _ = std::fs::remove_file(&json_file); // 清理临时 json

            if let Ok(data) = parsed {
                let detected_lang = data.result.and_then(|r| r.language);
                if let Some(trans) = data.transcription {
                    for (i, item) in trans.into_iter().enumerate() {
                        let text = item.text.unwrap_or_default().trim().to_string();
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

        info!("Whisper 转写完成，生成 {} 条字幕", segments.len());
        Ok(segments)
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
                    let text = line[end_bracket + 1..].trim().to_string();
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
