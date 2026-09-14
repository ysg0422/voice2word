//! 阿里 SenseVoice-Small 极速非自回归语音识别引擎封装
//! 通过 sherpa-onnx 驱动 INT8 模型，单次前向出字，自带标点与 ITN，速度是 Whisper 的 5~8 倍。

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tracing::{info, warn};

use crate::subtitle::Segment;

#[derive(Debug, Deserialize)]
struct SenseVoiceStreamLine {
    event: String,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    start: f64,
    #[serde(default)]
    end: f64,
    #[serde(default)]
    text: String,
    #[serde(default)]
    progress: f64,
    #[serde(default)]
    elapsed_sec: f64,
    #[serde(default)]
    segments: Vec<SenseVoiceOutputItem>,
}

#[derive(Debug, Deserialize)]
struct SenseVoiceOutputItem {
    index: usize,
    start: f64,
    end: f64,
    text: String,
    #[serde(default)]
    polished: String,
}

#[derive(Clone)]
pub struct SenseVoiceEngine {
    runner_path: PathBuf,
    model_path: PathBuf,
    tokens_path: PathBuf,
    vad_model_path: PathBuf,
    threads: u32,
}

impl SenseVoiceEngine {
    pub fn new<P1: AsRef<Path>, P2: AsRef<Path>, P3: AsRef<Path>, P4: AsRef<Path>>(
        runner_path: P1,
        model_path: P2,
        tokens_path: P3,
        vad_model_path: P4,
        threads: u32,
    ) -> Self {
        Self {
            runner_path: runner_path.as_ref().to_path_buf(),
            model_path: model_path.as_ref().to_path_buf(),
            tokens_path: tokens_path.as_ref().to_path_buf(),
            vad_model_path: vad_model_path.as_ref().to_path_buf(),
            threads: threads.max(1),
        }
    }

    /// 检查 SenseVoice 所需脚本与全部模型权重是否就位
    pub fn is_available(&self) -> bool {
        self.runner_path.exists()
            && self.model_path.exists()
            && self.tokens_path.exists()
            && self.vad_model_path.exists()
    }

    /// 转写本地 WAV 音频文件
    pub fn transcribe<P: AsRef<Path>>(
        &self,
        audio_path: P,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        let audio_path = audio_path.as_ref();
        info!(
            "SenseVoice 启动极速非自回归转写: {:?}, 语言: {:?}, 线程: {}",
            audio_path, language, threads.unwrap_or(self.threads)
        );

        let mut cmd = Command::new("python");
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }

        let th = threads.unwrap_or(self.threads);
        cmd.arg(&self.runner_path)
            .arg("--model").arg(&self.model_path)
            .arg("--tokens").arg(&self.tokens_path)
            .arg("--vad-model").arg(&self.vad_model_path)
            .arg("--input").arg(audio_path)
            .arg("--threads").arg(th.to_string())
            .arg("--language").arg(language.unwrap_or("auto"));

        if let Some(dur) = total_duration {
            if dur > 0.0 {
                cmd.arg("--total-duration").arg(dur.to_string());
            }
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        self.run_process_and_parse(cmd, None, progress_cb)
    }

    /// 纯内存管道推流转写：直接从流（Read）中泵入 PCM WAV 字节，0 磁盘 I/O 往返
    pub fn transcribe_stream(
        &self,
        stream: Box<dyn std::io::Read + Send + 'static>,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        info!("SenseVoice 启动纯内存管道非自回归推流转写 (0 磁盘 I/O)");

        let mut cmd = Command::new("python");
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }

        let th = threads.unwrap_or(self.threads);
        cmd.arg(&self.runner_path)
            .arg("--model").arg(&self.model_path)
            .arg("--tokens").arg(&self.tokens_path)
            .arg("--vad-model").arg(&self.vad_model_path)
            .arg("--input").arg("-")
            .arg("--threads").arg(th.to_string())
            .arg("--language").arg(language.unwrap_or("auto"));

        if let Some(dur) = total_duration {
            if dur > 0.0 {
                cmd.arg("--total-duration").arg(dur.to_string());
            }
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        self.run_process_and_parse(cmd, Some(stream), progress_cb)
    }

    fn run_process_and_parse(
        &self,
        mut cmd: Command,
        input_stream: Option<Box<dyn std::io::Read + Send + 'static>>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        let mut child = cmd.spawn().with_context(|| format!("启动 SenseVoice 进程失败: {:?}", self.runner_path))?;

        // 若有内存音频流输入，启动泵送线程写入子进程 stdin
        let stream_handle = if let Some(stream) = input_stream {
            let stdin = child.stdin.take().context("获取 SenseVoice 标准输入管道失败")?;
            Some(std::thread::spawn(move || {
                use std::io::{copy, BufReader, BufWriter, Write};
                let mut reader = BufReader::with_capacity(128 * 1024, stream);
                let mut writer = BufWriter::with_capacity(128 * 1024, stdin);
                let _ = copy(&mut reader, &mut writer);
                let _ = writer.flush();
            }))
        } else {
            None
        };

        let stdout = child.stdout.take().context("获取 SenseVoice 标准输出管道失败")?;
        let stderr = child.stderr.take().context("获取 SenseVoice 标准错误管道失败")?;

        let stderr_handle = std::thread::spawn(move || {
            use std::io::Read;
            let mut err_str = String::new();
            let _ = std::io::BufReader::new(stderr).read_to_string(&mut err_str);
            err_str
        });

        let mut segments = Vec::new();
        let mut inference_elapsed = 0.0f64;

        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout);
        for line_res in reader.lines() {
            let line = line_res.unwrap_or_default();
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Ok(parsed) = serde_json::from_str::<SenseVoiceStreamLine>(trimmed) {
                match parsed.event.as_str() {
                    "segment" => {
                        let seg = Segment {
                            index: parsed.index,
                            start: parsed.start,
                            end: parsed.end,
                            text: parsed.text.clone(),
                            translation: None,
                            polished: parsed.text.clone(),
                            language: None,
                        };
                        segments.push(seg.clone());
                        if let Some(ref cb) = progress_cb {
                            let label = format!("SenseVoice 极速转写中: 第 {} 句", parsed.index);
                            cb(parsed.progress, &label, Some(seg));
                        }
                    }
                    "finished" => {
                        inference_elapsed = parsed.elapsed_sec;
                        if segments.is_empty() && !parsed.segments.is_empty() {
                            for item in parsed.segments {
                                segments.push(Segment {
                                    index: item.index,
                                    start: item.start,
                                    end: item.end,
                                    text: item.text.clone(),
                                    translation: None,
                                    polished: item.text,
                                    language: None,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let status = child.wait().with_context(|| "等待 SenseVoice 进程结束失败")?;
        if let Some(h) = stream_handle {
            let _ = h.join();
        }
        let stderr_str = stderr_handle.join().unwrap_or_default();

        if !status.success() {
            warn!("SenseVoice 退出状态非 0: {:?}, 错误日志: {}", status.code(), stderr_str);
            anyhow::bail!("SenseVoice 识别失败: {}", stderr_str.trim());
        }

        // 智能优化时间轴：消除 100ms 重叠鬼影、消除时间重叠冲突、广播级短句延展平滑
        crate::subtitle::optimize_segments(&mut segments);

        if let Some(ref cb) = progress_cb {
            cb(1.0, &format!("SenseVoice 转写完成，共 {} 句", segments.len()), None);
        }

        info!(segments = segments.len(), elapsed = inference_elapsed, "SenseVoice 转写完成");
        Ok((segments, inference_elapsed))
    }
}
