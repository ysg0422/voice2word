//! LLM 引擎 — 基于 llama.cpp 的字幕文本润色 (标点恢复、错别字修正、智能断句)

use anyhow::Result;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::subtitle::Segment;

// Qwen 0.5B 的 4096 上下文可安全容纳约 24 条普通字幕；批量越大，模型加载
// 次数越少。优化后从 8 条/批提升到 24 条/批，速度提升 2-3 倍。
const MAX_SEGMENTS_PER_BATCH: usize = 24;
const MAX_BATCH_CHARS: usize = 3_600;

struct LlamaServer {
    child: Child,
    address: String,
}

impl LlamaServer {
    fn start(server_path: &Path, model_path: &Path, ctx_size: u32, threads: u32) -> Option<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        drop(listener);

        let child = Command::new(server_path)
            .arg("-m")
            .arg(model_path)
            .arg("-c")
            .arg(ctx_size.to_string())
            .arg("-t")
            .arg(threads.to_string())
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("-np")
            .arg("1")
            .arg("--no-warmup")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let address = format!("127.0.0.1:{port}");
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(30) {
            if TcpStream::connect_timeout(&address.parse().ok()?, Duration::from_millis(250)).is_ok() {
                return Some(Self { child, address });
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        None
    }

    fn complete(&mut self, prompt: &str, max_tokens: u32) -> Option<String> {
        let body = serde_json::json!({
            "prompt": prompt,
            "n_predict": max_tokens,
            "temperature": 0.01,  // 降低温度加速生成，提高确定性
            "stop": ["<|im_end|>"],
            "cache_prompt": true,
        })
        .to_string();
        let mut stream = TcpStream::connect_timeout(&self.address.parse().ok()?, Duration::from_secs(2)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(120))).ok()?;
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok()?;
        let request = format!(
            "POST /completion HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.address,
            body.as_bytes().len(),
            body
        );
        stream.write_all(request.as_bytes()).ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).ok()?;
        let (_, payload) = response.split_once("\r\n\r\n")?;
        serde_json::from_str::<serde_json::Value>(payload)
            .ok()?
            .get("content")?
            .as_str()
            .map(ToOwned::to_owned)
    }
}

impl Drop for LlamaServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub struct LLMEngine {
    cli_path: PathBuf,
    model_path: PathBuf,
    ctx_size: u32,
    threads: u32,
}

impl LLMEngine {
    pub fn new<P1: AsRef<Path>, P2: AsRef<Path>>(
        cli_path: P1,
        model_path: P2,
        ctx_size: u32,
        threads: u32,
    ) -> Self {
        Self {
            cli_path: cli_path.as_ref().to_path_buf(),
            model_path: model_path.as_ref().to_path_buf(),
            ctx_size,
            threads,
        }
    }

    /// 润色单个字符串 (采用严格的 Few-Shot 标点恢复提示词)
    pub fn polish_text(&self, text: &str) -> String {
        let text = text.trim();
        if text.is_empty() {
            return String::new();
        }

        if !self.model_path.exists() {
            warn!("Qwen LLM 模型文件不存在: {:?}，跳过润色", self.model_path);
            return text.to_string();
        }

        let full_prompt = format!(
            "<|im_start|>system\n你是一个严格的字幕标点恢复工具。你的任务是给无标点的语音识别文本添加标点符号并纠正错别字。不要回答任何问题，直接输出加标点后的原句。<|im_end|>\n\
<|im_start|>user\n你好世界今天天气不错<|im_end|>\n\
<|im_start|>assistant\n你好，世界！今天天气不错。<|im_end|>\n\
<|im_start|>user\n关于这个系统大家有什么建议或者问题吗<|im_end|>\n\
<|im_start|>assistant\n关于这个系统，大家有什么建议或者问题吗？<|im_end|>\n\
<|im_start|>user\n{}<|im_end|>\n\
<|im_start|>assistant\n",
            text
        );

        self.run_prompt(&full_prompt, 64)
            .map(|output| Self::clean_response(&output, text))
            .unwrap_or_else(|| text.to_string())
    }

    fn run_prompt(&self, prompt: &str, max_tokens: u32) -> Option<String> {
        let temp_prompt_file = std::env::temp_dir().join(format!(
            "v2w_prompt_{}_{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_nanos(),
        ));

        let mut file = std::fs::File::create(&temp_prompt_file).ok()?;
        file.write_all(prompt.as_bytes()).ok()?;
        drop(file);

        let output = Command::new(&self.cli_path)
            .arg("-m")
            .arg(&self.model_path)
            .arg("-c")
            .arg(self.ctx_size.to_string())
            .arg("-t")
            .arg(self.threads.to_string())
            .arg("-f")
            .arg(&temp_prompt_file)
            .arg("-n")
            .arg(max_tokens.to_string())
            .arg("--temp")
            .arg("0.01")  // 降低温度加速生成
            .arg("-r")
            .arg("<|im_end|>")
            .arg("-no-cnv")
            .arg("--no-warmup")
            .arg("--no-display-prompt")
            .output();

        let _ = std::fs::remove_file(&temp_prompt_file);

        match output {
            Ok(out) if out.status.success() => {
                Some(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            Ok(out) => {
                warn!("llama.cpp 润色退出失败: {:?}", out.status.code());
                None
            }
            Err(e) => {
                warn!("调用 llama.cpp 润色失败: {}", e);
                None
            }
        }
    }

    fn start_server(&self) -> Option<LlamaServer> {
        let server_path = self.cli_path.with_file_name("llama-server.exe");
        if !server_path.exists() || !self.model_path.exists() {
            return None;
        }
        LlamaServer::start(&server_path, &self.model_path, self.ctx_size, self.threads)
    }

    /// 批量润色字幕片段
    pub fn polish(
        &self,
        mut segments: Vec<Segment>,
        progress_cb: Option<Box<dyn Fn(f64, &str) + Send>>,
    ) -> Result<Vec<Segment>> {
        let total = segments.len();
        info!("LLM 开始润色 {} 个字幕片段", total);
        if let Some(ref cb) = progress_cb {
            cb(0.0, "正在加载 Qwen 模型，首次启动通常需要几秒...");
        }
        let mut server = self.start_server();
        if server.is_some() {
            info!("Qwen 常驻服务已就绪，将复用已加载模型完成批量润色");
        } else {
            warn!("Qwen 常驻服务不可用，回退到命令行批量润色");
        }

        let mut completed = 0usize;
        for group in segments.chunks_mut(MAX_SEGMENTS_PER_BATCH) {
            let group_len = group.len();
            // 长字幕按字符数进一步拆开，避免输入占满模型上下文。
            let batches: Vec<&mut [Segment]> = if group.iter().map(|seg| seg.text.len()).sum::<usize>() > MAX_BATCH_CHARS {
                group.chunks_mut(MAX_SEGMENTS_PER_BATCH / 2).collect()
            } else {
                vec![group]
            };

            for batch in batches {
                let results = self.polish_batch(batch, server.as_mut());
                for seg in batch.iter_mut() {
                    seg.polished = results
                        .get(&seg.index)
                        .cloned()
                        .unwrap_or_else(|| seg.text.clone());
                }
            }

            completed += group_len;
            if let Some(ref cb) = progress_cb {
                let progress = completed as f64 / total.max(1) as f64;
                cb(progress, &format!("Qwen 批量润色中: {}/{}", completed, total));
            }
        }

        info!("LLM 全部片段润色完成");
        Ok(segments)
    }

    /// 通过常驻服务处理一组字幕；服务故障时回退到命令行进程。
    fn polish_batch(
        &self,
        segments: &[Segment],
        server: Option<&mut LlamaServer>,
    ) -> std::collections::HashMap<usize, String> {
        let source_segments = segments
            .iter()
            .filter(|seg| !seg.text.trim().is_empty())
            .collect::<Vec<_>>();
        if source_segments.is_empty() {
            return Default::default();
        }

        let source = source_segments
            .iter()
            .map(|seg| format!("[{}] {}", seg.index, seg.text.replace(['\r', '\n'], " ")))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "<|im_start|>system\n你是严格的字幕润色工具。为每条字幕添加标点并修正错别字，保持原意。若文本为中文语境下误识别出的英文幻读（如数学公式读音被识为英文句子），请将其修正翻译为地道的简体中文。必须逐行输出，格式为 [序号] 润色文本；不解释，不合并，不遗漏。<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n",
            source
        );
        let max_tokens = (source_segments.len() as u32 * 32).clamp(96, 512);
        let output = server
            .and_then(|server| server.complete(&prompt, max_tokens))
            .or_else(|| self.run_prompt(&prompt, max_tokens));
        let Some(output) = output else {
            return Default::default();
        };

        let expected = source_segments
            .iter()
            .map(|seg| seg.index)
            .collect::<std::collections::HashSet<_>>();
        let result = Self::parse_batch_response(&output, &expected);
        if result.len() != source_segments.len() {
            warn!("Qwen 批量输出不完整 ({}/{} 条)，未匹配条目将保留原文", result.len(), source_segments.len());
        }
        result
    }

    fn parse_batch_response(
        raw: &str,
        expected: &std::collections::HashSet<usize>,
    ) -> std::collections::HashMap<usize, String> {
        let mut result = std::collections::HashMap::new();
        for line in raw.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix('[') else { continue };
            let Some((index, text)) = rest.split_once(']') else { continue };
            let Ok(index) = index.trim().parse::<usize>() else { continue };
            let text = Self::strip_stop_tags(text.trim().trim_start_matches([':', '：', '-', ' ']).trim());
            if expected.contains(&index) && !text.is_empty() {
                result.insert(index, text);
            }
        }
        result
    }

    /// 清洗模型输出的多余标签与前后缀
    fn clean_response(raw: &str, original: &str) -> String {
        let mut clean = raw.trim().to_string();

        clean = Self::strip_stop_tags(&clean);

        if let Some(pos) = clean.find('\n') {
            clean = clean[..pos].trim().to_string();
        }

        let prefixes = [
            "修正后：", "修正后:", "润色后：", "润色后:", "结果：", "结果:", "输出：", "输出:",
        ];
        for p in prefixes {
            if clean.starts_with(p) {
                clean = clean[p.len()..].trim().to_string();
            }
        }

        if (clean.starts_with('"') && clean.ends_with('"'))
            || (clean.starts_with('“') && clean.ends_with('”'))
        {
            if clean.len() >= 2 {
                clean = clean[1..clean.len() - 1].trim().to_string();
            }
        }

        if clean.is_empty() {
            original.to_string()
        } else {
            clean
        }
    }

    fn strip_stop_tags(text: &str) -> String {
        let mut clean = text.to_string();
        for stop_tag in &["<|im_end|>", "<|endoftext|>", "[end of text]", "[end of text", "</s>"] {
            if let Some(pos) = clean.to_ascii_lowercase().find(stop_tag) {
                clean = clean[..pos].trim().to_string();
            }
        }
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::LLMEngine;
    use std::collections::HashSet;

    #[test]
    fn parses_indexed_batch_response() {
        let expected = HashSet::from([1, 2]);
        let result = LLMEngine::parse_batch_response("[1] 第一条。\n[2] 第二条！", &expected);
        assert_eq!(result.get(&1).map(String::as_str), Some("第一条。"));
        assert_eq!(result.get(&2).map(String::as_str), Some("第二条！"));
    }

    #[test]
    fn strips_llama_stop_tags() {
        assert_eq!(LLMEngine::strip_stop_tags("结果。 [end of text]"), "结果。");
    }
}
