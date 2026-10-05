//! LLM 引擎 — 基于 llama.cpp 的字幕文本润色 (标点恢复、错别字修正、智能断句)

use anyhow::Result;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::subtitle::Segment;
use crate::utils::TempPathGuard;

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

        let mut cmd = Command::new(server_path);
        // 让路模式：llama-server 是常驻进程，CPU 占用高，必须跟随全局设置
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        let child = cmd
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
        crate::utils::child_registry::adopt(&child);

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

/// 把待译下标切成批次：**同时**受「条数」与「字符数」约束。
///
/// 原先只按条数切（24 条一批），一条长字幕就能把整批输入顶到模型上下文之外，
/// 表现为最后几条译文缺失（`parse_batch_response` 只认能解析出的行）。这里补上
/// 字符预算：一批的源文本总长超过 [`MAX_BATCH_CHARS`] 就提前收口。
///
/// 单条自身就超预算时仍单独成批——否则会陷入「永远切不出合法批次」的死循环，
/// 交回模型截断处理比在这里卡死更合理。
fn plan_translate_batches(pending: &[usize], segments: &[Segment]) -> Vec<Vec<usize>> {
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_chars = 0usize;

    for &pos in pending {
        // 序号文本（"[123] "）也算输入，粗估 6 字符
        let cost = segments[pos].translate_source().chars().count() + 6;
        let would_overflow = !current.is_empty()
            && (current.len() >= MAX_SEGMENTS_PER_BATCH
                || current_chars + cost > MAX_BATCH_CHARS);
        if would_overflow {
            batches.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        current.push(pos);
        current_chars += cost;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

pub struct LLMEngine {
    cli_path: PathBuf,
    model_path: PathBuf,
    ctx_size: u32,
    /// 推理线程数：设置页可运行时调整，故用原子量而非普通字段
    threads: std::sync::atomic::AtomicU32,
    /// 取消标志。润色 / 翻译都是**分批长循环**，用户点「终止」后不能等整批跑完
    /// 才生效（长视频几百条字幕要等几分钟）。每个批次开始前检查一次，置位即返回。
    ///
    /// 用 `Mutex<Arc<..>>` 而不是裸 `Arc`：标志由**外部**（管线的 cancelled、
    /// 翻译面板的取消位）在任务开始时注入，而 `LLMEngine` 在 `Arc` 后面被共享、
    /// 拿不到 `&mut self`。每批读一次锁，开销可忽略。
    ///
    /// 与 `TaskPipeline::cancelled` 共用同一个 `Arc`：转写的取消与润色的取消是
    /// 同一件事，各自维护一份标志会导致「终止转写」只杀掉 ASR、润色照跑。
    cancel: Mutex<Arc<AtomicBool>>,
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
            threads: std::sync::atomic::AtomicU32::new(threads.max(1)),
            cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
        }
    }

    /// 注入外部取消标志（管线 / 翻译面板在任务开始前调用）。
    ///
    /// **只安装、不复位**：复位是「任务开始」的职责，由调用方在起手时做
    /// （`TaskPipeline::run` 复位自己的 `cancelled`；翻译面板在起手处复位
    /// `translate_cancel`）。在这里顺手复位是危险的——润色阶段才安装标志，
    /// 若此时把标志清零，用户在此之前按下的「终止」就被无声撤销了。
    pub fn install_cancel_flag(&self, cancel: Arc<AtomicBool>) {
        *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = cancel;
    }

    fn is_cancelled(&self) -> bool {
        self.cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 运行时调整推理线程数（设置页滑条调用，对下一次润色生效）
    pub fn set_threads(&self, threads: u32) {
        self.threads
            .store(threads.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn threads(&self) -> u32 {
        self.threads.load(std::sync::atomic::Ordering::Relaxed)
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

        // 先挂守卫再落盘：create / write_all 任一步失败都会用 `.ok()?` 提前返回，
        // 过去那两行 `?` 会跳过函数末尾的 remove_file，留下一个半截提示词文件。
        let mut prompt_guard = TempPathGuard::file(&temp_prompt_file);
        let mut file = std::fs::File::create(&temp_prompt_file).ok()?;
        file.write_all(prompt.as_bytes()).ok()?;
        drop(file);

        let mut cli_cmd = Command::new(&self.cli_path);
        // 让路模式同样适用于命令行回退路径
        super::media_pipeline::apply_default_child_flags(&mut cli_cmd);
        let output = cli_cmd
            .arg("-m")
            .arg(&self.model_path)
            .arg("-c")
            .arg(self.ctx_size.to_string())
            .arg("-t")
            .arg(self.threads().to_string())
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

        // 进程已退出，提示词文件不再需要：立刻删除（守卫仍会在返回时兜底）
        prompt_guard.remove_now();

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
        LlamaServer::start(&server_path, &self.model_path, self.ctx_size, self.threads())
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
            if self.is_cancelled() {
                info!("润色被取消，保留已完成的部分（{completed}/{total}）");
                break;
            }
            let group_len = group.len();
            // 长字幕按字符数进一步拆开，避免输入占满模型上下文。
            let batches: Vec<&mut [Segment]> = if group.iter().map(|seg| seg.text.len()).sum::<usize>() > MAX_BATCH_CHARS {
                group.chunks_mut(MAX_SEGMENTS_PER_BATCH / 2).collect()
            } else {
                vec![group]
            };

            for batch in batches {
                if self.is_cancelled() {
                    break;
                }
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

    /// 批量翻译字幕片段（从外语/多语言翻译为目标语言，例如简体中文）
    pub fn translate(
        &self,
        mut segments: Vec<Segment>,
        target_lang: &str,
        progress_cb: Option<Box<dyn Fn(f64, &str) + Send>>,
    ) -> Result<Vec<Segment>> {
        let total = segments.len();
        info!("LLM 开始将 {} 个字幕片段翻译为 {}", total, target_lang);
        if let Some(ref cb) = progress_cb {
            cb(0.0, "正在加载 Qwen 翻译模型，首次启动需要几秒...");
        }
        let mut server = self.start_server();
        if server.is_some() {
            info!("Qwen 常驻服务已就绪，复用模型进行批量字幕翻译");
        } else {
            warn!("Qwen 常驻服务不可用，回退到命令行批量翻译");
        }

        // 增量：只译「还没有目标语言译文」的句子。
        //
        // 翻译是可以重入的——中途取消、换目标语言、或补译后来新增的句子，
        // 都不该把已经译好的部分再烧一遍。判定用 `translation_matches` 精确比对
        // 目标语言，因此「从 English 改译 日本語」会正确地整篇重译，
        // 而「上次取消了、这次继续」只补剩下的。
        let pending: Vec<usize> = segments
            .iter()
            .enumerate()
            .filter(|(_, seg)| {
                !seg.translate_source().trim().is_empty() && !seg.translation_matches(target_lang)
            })
            .map(|(pos, _)| pos)
            .collect();

        let pending_total = pending.len();
        if pending_total == 0 {
            info!("所有片段均已是 {target_lang} 译文，无需翻译");
            if let Some(ref cb) = progress_cb {
                cb(1.0, &format!("全部 {total} 句已是{target_lang}译文"));
            }
            return Ok(segments);
        }
        if pending_total < total {
            info!("增量翻译：{total} 句中有 {pending_total} 句需要翻译为 {target_lang}");
        }

        let batches = plan_translate_batches(&pending, &segments);
        let mut completed = 0usize;
        for chunk in batches {
            if self.is_cancelled() {
                info!("翻译被取消，保留已完成的部分（{completed}/{pending_total}）");
                break;
            }
            // 克隆这一小批（≤24 条）交给批量函数：`translate_batch` 要的是
            // `&[Segment]`，而这里必须按下标写回原数组，克隆是唯一不用和借用
            // 检查器搏斗的写法。相比一次模型推理，这点拷贝可忽略。
            let batch: Vec<Segment> = chunk.iter().map(|&pos| segments[pos].clone()).collect();
            let results = self.translate_batch(&batch, target_lang, server.as_mut());
            for &pos in &chunk {
                if let Some(trans) = results.get(&segments[pos].index) {
                    segments[pos].translation = Some(trans.clone());
                    segments[pos].translation_lang = Some(target_lang.to_string());
                }
            }

            completed += chunk.len();
            if let Some(ref cb) = progress_cb {
                let progress = completed as f64 / pending_total.max(1) as f64;
                cb(
                    progress,
                    &format!("Qwen 批量翻译中: {completed}/{pending_total} 句（共 {total} 句）"),
                );
            }
        }

        info!("LLM 全部字幕翻译完成");
        Ok(segments)
    }

    fn translate_batch(
        &self,
        segments: &[Segment],
        target_lang: &str,
        server: Option<&mut LlamaServer>,
    ) -> std::collections::HashMap<usize, String> {
        let source_segments = segments
            .iter()
            .filter(|seg| !seg.translate_source().trim().is_empty())
            .collect::<Vec<_>>();
        if source_segments.is_empty() {
            return Default::default();
        }

        let source = source_segments
            .iter()
            .map(|seg| format!("[{}] {}", seg.index, seg.translate_source()))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "<|im_start|>system\n你是一个专业字幕翻译专家。将给出的字幕文本准确翻译为地道的{}。保持原意，语言通顺紧凑。必须逐行输出，格式为 [序号] 翻译文本；不解释，不合并，不遗漏。<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n",
            target_lang,
            source
        );
        let max_tokens = (source_segments.len() as u32 * 48).clamp(128, 768);
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
            warn!("Qwen 翻译批量输出不完整 ({}/{} 条)，未匹配条目保持未翻译", result.len(), source_segments.len());
        }
        result
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

    pub(crate) fn parse_batch_response(
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
            // 弯引号 “ ” 是多字节字符，按字节切片会 panic，必须用 strip 按字符边界剥壳
            if let Some(inner) = clean
                .strip_prefix('"')
                .or_else(|| clean.strip_prefix('“'))
                .and_then(|s| s.strip_suffix('"').or_else(|| s.strip_suffix('”')))
            {
                clean = inner.trim().to_string();
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
    use super::{plan_translate_batches, LLMEngine, MAX_BATCH_CHARS, MAX_SEGMENTS_PER_BATCH};
    use crate::subtitle::Segment;
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

    #[test]
    fn clean_response_strips_curly_quotes_without_panic() {
        // 回归：弯引号 “ ” 是多字节字符，按字节切片会 panic（not a char boundary）
        assert_eq!(LLMEngine::clean_response("“你好世界”", "原文"), "你好世界");
        assert_eq!(LLMEngine::clean_response("\"你好世界\"", "原文"), "你好世界");
        // 纯中文内容超长时也不 panic
        let long_zh = "式".repeat(50);
        assert_eq!(LLMEngine::clean_response(&format!("“{long_zh}”"), "原文"), long_zh);
    }
    // ─────────── 翻译批次规划 ───────────

    fn seg_at(index: usize, text: &str) -> Segment {
        Segment::new(index, 0.0, 1.0, text)
    }

    /// 批次必须同时受条数约束。
    #[test]
    fn plan_batches_respects_max_segments() {
        let segs: Vec<Segment> = (1..=50).map(|i| seg_at(i, "短句")).collect();
        let pending: Vec<usize> = (0..50).collect();
        let batches = plan_translate_batches(&pending, &segs);
        assert!(
            batches.iter().all(|b| b.len() <= MAX_SEGMENTS_PER_BATCH),
            "每批不得超过 MAX_SEGMENTS_PER_BATCH"
        );
        // 所有下标都要被覆盖且不重复
        let mut all: Vec<usize> = batches.iter().flatten().copied().collect();
        all.sort_unstable();
        assert_eq!(all, pending);
    }

    /// 长句必须按**字符数**提前收口——这是本次修复的核心：
    /// 原先只按条数切，一条长字幕就能把整批顶出模型上下文。
    #[test]
    fn plan_batches_splits_on_char_budget() {
        // 每条 500 字，MAX_BATCH_CHARS=3600 → 每批最多 7 条
        let long = "字".repeat(500);
        let segs: Vec<Segment> = (1..=24).map(|i| seg_at(i, &long)).collect();
        let pending: Vec<usize> = (0..24).collect();
        let batches = plan_translate_batches(&pending, &segs);
        assert!(batches.len() > 1, "长句应被拆成多批");
        for b in &batches {
            let chars: usize = b
                .iter()
                .map(|&p| segs[p].translate_source().chars().count() + 6)
                .sum();
            assert!(
                chars <= MAX_BATCH_CHARS,
                "批次字符数超预算: {chars} > {MAX_BATCH_CHARS}"
            );
        }
        let mut all: Vec<usize> = batches.iter().flatten().copied().collect();
        all.sort_unstable();
        assert_eq!(all, pending, "不得丢句或重复");
    }

    /// 单条就超过字符预算时仍要单独成批：否则会切不出合法批次而死循环。
    #[test]
    fn plan_batches_keeps_oversized_single_segment() {
        let huge = "字".repeat(MAX_BATCH_CHARS * 2);
        let segs = vec![seg_at(1, &huge)];
        let batches = plan_translate_batches(&[0], &segs);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![0]);
    }

    /// 空输入不该产出空批次。
    #[test]
    fn plan_batches_empty_input() {
        let segs: Vec<Segment> = Vec::new();
        assert!(plan_translate_batches(&[], &segs).is_empty());
    }
}