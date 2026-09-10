//! PunctuationEngine — 基于 CT-Transformer (ONNX) 的超轻量极速标点恢复引擎
//! 纯 CPU 单次推理毫秒级，比大模型快 50~100 倍

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;
use tracing::{info, warn};

use crate::subtitle::Segment;

#[derive(Clone)]
pub struct PunctuationEngine {
    runner_script: PathBuf,
    model_path: PathBuf,
    threads: u32,
}

impl PunctuationEngine {
    pub fn new<P1: AsRef<Path>, P2: AsRef<Path>>(
        runner_script: P1,
        model_path: P2,
        threads: u32,
    ) -> Self {
        Self {
            runner_script: runner_script.as_ref().to_path_buf(),
            model_path: model_path.as_ref().to_path_buf(),
            threads,
        }
    }

    pub fn is_available(&self) -> bool {
        self.model_path.exists() && self.runner_script.exists()
    }

    pub fn add_punctuation(
        &self,
        mut segments: Vec<Segment>,
        progress_cb: Option<Box<dyn Fn(f64, &str) + Send>>,
    ) -> Result<Vec<Segment>> {
        if segments.is_empty() {
            return Ok(segments);
        }

        if !self.is_available() {
            warn!("CT-Punc 模型或脚本未就绪: {:?}, 跳过极速标点", self.model_path);
            return Ok(segments);
        }

        if let Some(ref cb) = progress_cb {
            cb(0.1, "CT-Transformer 极速标点引擎加载中...");
        }

        let started = Instant::now();

        #[derive(serde::Serialize)]
        struct InputItem<'a> {
            index: usize,
            text: &'a str,
        }

        let items: Vec<InputItem> = segments
            .iter()
            .map(|s| InputItem {
                index: s.index,
                text: &s.text,
            })
            .collect();

        let input_json = serde_json::to_vec(&items)
            .context("序列化字幕输入失败")?;

        let mut cmd = Command::new("python");
        cmd.arg(&self.runner_script)
            .arg("--model")
            .arg(&self.model_path)
            .arg("--threads")
            .arg(self.threads.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }

        let mut child = cmd.spawn().context("启动 CT-Transformer 标点恢复子进程失败")?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&input_json)?;
            stdin.flush()?;
        }

        if let Some(ref cb) = progress_cb {
            cb(0.5, "正在进行毫秒级标点预测与断句...");
        }

        let output = child.wait_with_output().context("等待标点子进程退出失败")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("CT-Punc 执行报错: {}", stderr);
            anyhow::bail!("CT-Punc 执行失败: {}", stderr);
        }

        #[derive(serde::Deserialize)]
        struct OutputResult {
            index: usize,
            polished: String,
        }

        #[derive(serde::Deserialize)]
        struct OutputPayload {
            #[allow(dead_code)]
            count: usize,
            #[allow(dead_code)]
            elapsed_sec: f64,
            results: Vec<OutputResult>,
        }

        let payload: OutputPayload = serde_json::from_slice(&output.stdout)
            .context("解析标点输出 JSON 失败")?;

        let map: HashMap<usize, String> = payload
            .results
            .into_iter()
            .map(|r| (r.index, r.polished))
            .collect();

        for seg in segments.iter_mut() {
            if let Some(pol) = map.get(&seg.index) {
                seg.polished = pol.clone();
            } else {
                seg.polished = seg.text.clone();
            }
        }

        if let Some(ref cb) = progress_cb {
            cb(1.0, "标点恢复完成");
        }

        info!(
            elapsed = ?started.elapsed(),
            count = segments.len(),
            "CT-Transformer 标点恢复完成"
        );

        Ok(segments)
    }
}
