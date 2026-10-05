//! LLM 引擎 — 基于 llama.cpp 的字幕文本润色 (标点恢复、错别字修正、智能断句)

use anyhow::{anyhow, Result};
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

// 每批最多 24 条字幕：条数是「模型加载 / 请求次数」的节流阀，批量越大越快。
const MAX_SEGMENTS_PER_BATCH: usize = 24;

/// 输入侧 token/字符 经验系数。
///
/// 实测 Qwen2.5 分词器：中文字幕约 0.72~0.80 token/字符。取 0.85 留余量，
/// 避免英文混排、数字、符号偏多时低估输入长度。
const PROMPT_TOKENS_PER_CHAR: f64 = 0.85;
/// 输出侧 token/字符 经验系数。
///
/// 译文长度可能**超过**原文（中→英实测约 0.84 token/字符），按 1.0 封顶，
/// 给膨胀型目标语言留足空间。
const OUTPUT_TOKENS_PER_CHAR: f64 = 1.0;
/// 上下文里留给系统提示词、对话模板与特殊 token 的余量。
const CTX_MARGIN: u32 = 192;

/// 按上下文窗口推导「一批源文本的字符预算」。
///
/// 一批要同时装下「输入 prompt」与「模型输出」，两者都随源字符数线性增长，
/// 因此预算 = (ctx - 余量) / (输入系数 + 输出系数)。硬编码 3600 字符在
/// 4096 上下文下会让「长行 + 输出膨胀」的组合越界，表现为最后几行译文被截断。
fn batch_char_budget(ctx_size: u32) -> usize {
    let usable = ctx_size.saturating_sub(CTX_MARGIN) as f64;
    let per_char = PROMPT_TOKENS_PER_CHAR + OUTPUT_TOKENS_PER_CHAR;
    ((usable / per_char) as usize).max(64)
}

/// 一批译文的输出 token 上限（`-n` / `n_predict`）。
///
/// 此前按「条数 × 48」估算并夹在 768：那是 8 条/批时代的经验值，假定每行都短。
/// 而批次实际按**字符**预算切分，长行批次（如 24 行 × 150 字）需要上千 token
/// 才能译完，768 会**静默截断**——实测 24 行只译出 14 行（模型撞上 767 步上限），
/// 剩下 10 行原样留空，用户看到的是「翻译莫名其妙少了几句」。
///
/// 改为按源字符数给预算，再用上下文剩余空间封顶，保证 prompt + 输出不越界。
fn output_token_budget(source_chars: usize, ctx_size: u32) -> u32 {
    let want = (source_chars as f64 * OUTPUT_TOKENS_PER_CHAR) as u32;
    let prompt_est = (source_chars as f64 * PROMPT_TOKENS_PER_CHAR) as u32 + 64;
    let remaining = ctx_size.saturating_sub(prompt_est).saturating_sub(64);
    want.max(128).min(remaining.max(128))
}

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
/// 字符预算：一批的源文本总长超过 `char_budget` 就提前收口。
///
/// `char_budget` 由 [`batch_char_budget`] 按上下文窗口推导，不再硬编码——
/// 硬编码值在「长行 + 译文膨胀」时会越界，输出被截断且无法察觉。
///
/// 单条自身就超预算时仍单独成批——否则会陷入「永远切不出合法批次」的死循环，
/// 交回模型截断处理比在这里卡死更合理。
fn plan_translate_batches(
    pending: &[usize],
    segments: &[Segment],
    char_budget: usize,
) -> Vec<Vec<usize>> {
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_chars = 0usize;

    for &pos in pending {
        // 序号文本（"[123] "）也算输入，粗估 6 字符
        let cost = segments[pos].translate_source().chars().count() + 6;
        let would_overflow = !current.is_empty()
            && (current.len() >= MAX_SEGMENTS_PER_BATCH
                || current_chars + cost > char_budget);
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

    /// 离线链路前置检查：模型与推理程序缺一不可。
    ///
    /// 为什么必须显式报错而不是「静默跑出 0 句」：`polish` / `translate` 走的是
    /// 「先起常驻服务、失败再回退命令行」两条路。模型文件缺失时两条路都起不来，
    /// 循环里每批都返回空映射，最后 `Ok(segments)` 原样返回——用户看到的是
    /// 「翻译已完成（0 句）」这种既没报错、也没任何结果的诡异状态，根本不知道
    /// 是「没下模型」。这里在开工前就给出可操作的中文提示，
    /// 两者都指向「性能设置 → 模型与组件」——模型与 llama.cpp 现在都可一键下载。
    fn ensure_ready(&self) -> Result<()> {
        if !self.model_path.exists() {
            return Err(anyhow!(
                "本地 Qwen 模型未就位：{}。请到「性能设置 → 模型与组件」点击下载，或改用「在线 API」翻译。",
                self.model_path.display()
            ));
        }
        // 命令行回退要用的主程序；常驻服务是它的同目录兄弟（llama-server.exe）。
        // llama.cpp 现在在下载清单里（同一个 zip 会把
        // llama-completion.exe / llama-server.exe 与各 DLL 一并解压到 tools/），
        // 因此提示只指向界面下载即可，不再要求用户手动放文件。
        if !self.cli_path.exists() {
            return Err(anyhow!(
                "本地推理程序未就位：{}。请到「性能设置 → 模型与组件」下载「llama.cpp 推理程序」（会自动解压到 tools/）；若你把它装在别处，也可在 config.toml 的 paths.llama_cli 指向实际位置，或改用「在线 API」翻译。",
                self.cli_path.display()
            ));
        }
        Ok(())
    }

    /// 批量润色字幕片段
    pub fn polish(
        &self,
        mut segments: Vec<Segment>,
        progress_cb: Option<Box<dyn Fn(f64, &str) + Send>>,
    ) -> Result<Vec<Segment>> {
        let total = segments.len();
        // 先做前置检查：模型/程序缺失时立刻给出可操作的中文提示，
        // 而不是跑完一圈「0 句」让用户以为翻译成功了。
        self.ensure_ready()?;
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
            // 长字幕按**字符预算**进一步拆开：预算随上下文窗口推导（见
            // `batch_char_budget`），不再靠「超过阈值就把批数减半」这种粗估。
            let budget = batch_char_budget(self.ctx_size);
            let mut start = 0usize;
            while start < group.len() {
                if self.is_cancelled() {
                    break;
                }
                let mut end = start;
                let mut chars = 0usize;
                while end < group.len() {
                    let cost = group[end].text.chars().count() + 6;
                    if end > start && chars + cost > budget {
                        break;
                    }
                    chars += cost;
                    end += 1;
                }
                let results = self.polish_batch(&group[start..end], server.as_mut());
                for seg in group[start..end].iter_mut() {
                    seg.polished = results
                        .get(&seg.index)
                        .cloned()
                        .unwrap_or_else(|| seg.text.clone());
                }
                start = end;
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
        // 同 polish：模型/程序缺失时 fail-fast，避免「翻译完成 0 句」的假成功。
        self.ensure_ready()?;
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

        let budget = batch_char_budget(self.ctx_size);
        let batches = plan_translate_batches(&pending, &segments, budget);
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
            let mut missed: Vec<usize> = Vec::new();
            for &pos in &chunk {
                match results.get(&segments[pos].index) {
                    Some(trans) => {
                        segments[pos].translation = Some(trans.clone());
                        segments[pos].translation_lang = Some(target_lang.to_string());
                    }
                    // 本批没译出这一句：收集起来，稍后**对半重试**。
                    None => missed.push(pos),
                }
            }

            // 模型偶发漏行（输出截断、行格式串味）时，整批丢弃会让用户看到
            // 「翻译莫名其妙少了几句」。把漏掉的句子按更小的批（对半）再试一轮，
            // 小批更不容易撞上输出长度上限，通常一轮就能补齐。
            if !missed.is_empty() {
                self.retry_missing(&mut segments, &missed, target_lang, server.as_mut());
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

        // 一句都没译出、又没被取消：多半是模型/程序异常（常驻服务起不来、命令行回退
        // 也失败）。此时 `Ok` 会让上层显示「翻译已完成（0 句）」——彻头彻尾的假成功，
        // 用户以为译完了、实际全空。这里改成显式报错，让失败在界面上看得见。
        if !self.is_cancelled() {
            let ok = pending
                .iter()
                .filter(|&&pos| segments[pos].translation_matches(target_lang))
                .count();
            if ok == 0 {
                return Err(anyhow!(
                    "Qwen 未能译出任何一句（共 {pending_total} 句待译）。请检查「模型与组件」里的 Qwen 模型与 llama.cpp 是否就位，或改用「在线 API」翻译。"
                ));
            }
        }
        info!("LLM 全部字幕翻译完成");
        Ok(segments)
    }

    /// 把上一轮**没译出**的句子按更小的批重试一轮，直到补全或批次缩到单句为止。
    ///
    /// 为什么要专门做这件事：模型输出被截断 / 行格式串味时，`translate_batch` 只
    /// 返回能解析出的行，剩下的**静默丢失**——用户看到的是「翻译莫名其妙少了几句」，
    /// 而且没有任何提示。这里把漏掉的句子折半再问一次：批越小，输出越不容易撞上
    /// 长度上限，一轮下来通常就补齐了；实在补不上的（如某个序号模型就是吐不出）
    /// 才放弃，并留一条日志。
    fn retry_missing(
        &self,
        segments: &mut [Segment],
        missed: &[usize],
        target_lang: &str,
        mut server: Option<&mut LlamaServer>,
    ) {
        let mut todo: Vec<usize> = missed.to_vec();
        // 每轮把待补清单**对半**拆成更小的批（单条时 half=1，即「单句重试一次」）。
        // `guard` 只防极端情况下的死循环；正常 24 条最多 5 轮就收敛。
        let mut guard = 0;
        while !todo.is_empty() && guard < 8 {
            guard += 1;
            let half = todo.len().div_ceil(2);
            let mut still_missing: Vec<usize> = Vec::new();
            for sub in todo.chunks(half) {
                if self.is_cancelled() {
                    return;
                }
                let batch: Vec<Segment> = sub.iter().map(|&p| segments[p].clone()).collect();
                let results = self.translate_batch(&batch, target_lang, server.as_deref_mut());
                for &pos in sub {
                    match results.get(&segments[pos].index) {
                        Some(trans) => {
                            segments[pos].translation = Some(trans.clone());
                            segments[pos].translation_lang = Some(target_lang.to_string());
                        }
                        None => still_missing.push(pos),
                    }
                }
            }
            // 这一轮没有任何进展：模型对这几个序号就是不出，收手，免得空转。
            if still_missing.len() >= todo.len() {
                todo = still_missing;
                break;
            }
            todo = still_missing;
        }
        if !todo.is_empty() {
            warn!(
                count = todo.len(),
                "Qwen 翻译对 {} 句折半重试后仍未译出，保持未翻译（可再次点击「开始翻译」补译）",
                todo.len()
            );
        }
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
        // 源语言线索：ASR 已检测到整批的语言（Segment::language）。多给这一条，
        // 模型在「日→中」「中英混排」这类方向上更少漏译、更少误翻专名。
        let src_hint = crate::subtitle::dominant_language(source_segments.iter().copied())
            .map(|code| format!("源语言为{}；", crate::subtitle::language_name(&code)))
            .unwrap_or_default();
        let prompt = format!(
            "<|im_start|>system\n你是一个专业字幕翻译专家。{}将给出的字幕文本准确翻译为地道的{}。保持原意，语言通顺紧凑。必须逐行输出，格式为 [序号] 翻译文本；不解释，不合并，不遗漏。<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n",
            src_hint,
            target_lang,
            source
        );
        let source_chars: usize = source_segments
            .iter()
            .map(|s| s.translate_source().chars().count())
            .sum();
        let max_tokens = output_token_budget(source_chars, self.ctx_size);
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
        let source_chars: usize = source_segments.iter().map(|s| s.text.chars().count()).sum();
        let max_tokens = output_token_budget(source_chars, self.ctx_size);
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

    /// 解析一行「序号 + 译文」，兼容模型常见的几种序号写法。
    ///
    /// 主格式是提示词要求的 `[序号] 译文`。但小模型（尤其 Qwen 0.5B）偶尔会写成
    /// `1. 译文` / `1) 译文` / `1、译文` / `1: 译文`——只认方括号的话，整行会被判为
    /// 解析失败而**丢失**（配合重试也只是反复失败）。这里按「前导数字 + 分隔符」
    /// 宽容识别；调用方再用 `expected` 集合过滤，因此译文正文里恰好以数字开头的
    /// 情况（如「2024 年」）不会误命中，除非该数字正好是某个待译序号。
    fn parse_indexed_line(line: &str) -> Option<(usize, String)> {
        let line = line.trim();
        // 主格式：[123] 译文
        if let Some(rest) = line.strip_prefix('[') {
            let (index, text) = rest.split_once(']')?;
            let index = index.trim().parse::<usize>().ok()?;
            let text = text.trim_start_matches([':', '：', '-', ' ']).trim();
            return Some((index, text.to_string()));
        }
        // 回退格式：123. / 123) / 123、/ 123: 后跟译文
        //
        // 必须紧跟一个**分隔标点**（不是纯空格）才算序号：否则「2 个苹果」这种
        // 恰好以数字开头的普通行会被误判成「序号 2 的译文」，一旦 2 正好是待译
        // 序号就会张冠李戴。宁可漏认（下一轮重试仍会补），不可错认。
        let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        let rest_raw = &line[digits.len()..];
        let sep_ok = rest_raw
            .chars()
            .next()
            .map(|c| matches!(c, '.' | ')' | '）' | '、' | ':' | '：' | '-' | '—' | '|'))
            .unwrap_or(false);
        if !sep_ok {
            return None;
        }
        let index = digits.parse::<usize>().ok()?;
        let rest = rest_raw
            .trim_start_matches(['.', ')', '）', '、', ':', '：', '-', '—', '|', ' '])
            .trim();
        if rest.is_empty() {
            return None;
        }
        Some((index, rest.to_string()))
    }

    pub(crate) fn parse_batch_response(
        raw: &str,
        expected: &std::collections::HashSet<usize>,
    ) -> std::collections::HashMap<usize, String> {
        let mut result = std::collections::HashMap::new();
        for line in raw.lines() {
            let Some((index, text)) = Self::parse_indexed_line(line) else { continue };
            let text = Self::strip_stop_tags(&text);
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
    use super::{
        batch_char_budget, output_token_budget, plan_translate_batches, LLMEngine,
        MAX_SEGMENTS_PER_BATCH, OUTPUT_TOKENS_PER_CHAR, PROMPT_TOKENS_PER_CHAR,
    };
    use crate::subtitle::Segment;
    use std::collections::HashSet;

    #[test]
    fn parses_indexed_batch_response() {
        let expected = HashSet::from([1, 2]);
        let result = LLMEngine::parse_batch_response("[1] 第一条。\n[2] 第二条！", &expected);
        assert_eq!(result.get(&1).map(String::as_str), Some("第一条。"));
        assert_eq!(result.get(&2).map(String::as_str), Some("第二条！"));
    }

    /// 解析器必须宽容小模型常见的几种序号写法，否则整行会被判为失败而丢失。
    #[test]
    fn parses_tolerant_index_formats() {
        let expected = HashSet::from([1, 2, 3, 4, 5]);
        let raw = "[1] 方括号\n2. 点号\n3) 右括号\n4、顿号\n5：全角冒号";
        let result = LLMEngine::parse_batch_response(raw, &expected);
        assert_eq!(result.get(&1).map(String::as_str), Some("方括号"));
        assert_eq!(result.get(&2).map(String::as_str), Some("点号"));
        assert_eq!(result.get(&3).map(String::as_str), Some("右括号"));
        assert_eq!(result.get(&4).map(String::as_str), Some("顿号"));
        assert_eq!(result.get(&5).map(String::as_str), Some("全角冒号"));
    }

    /// 回退格式（`2. 译文`）必须带**分隔标点**：纯空格开头的「2 个苹果」不算序号，
    /// 否则会把它误当成序号 2 的译文，一旦 2 恰是待译序号就张冠李戴。
    #[test]
    fn fallback_index_requires_separator_punctuation() {
        assert_eq!(
            LLMEngine::parse_indexed_line("2. Hello"),
            Some((2, "Hello".to_string()))
        );
        assert_eq!(
            LLMEngine::parse_indexed_line("3、你好"),
            Some((3, "你好".to_string()))
        );
        assert_eq!(LLMEngine::parse_indexed_line("2 个苹果"), None);
        assert_eq!(LLMEngine::parse_indexed_line("Hello world"), None);
    }

    /// 宽容解析**不能**把译文正文里以数字开头的句子误当成序号。
    /// 靠 `expected` 集合过滤：正文里的数字几乎不会是「另一个待译序号」。
    #[test]
    fn tolerant_parse_does_not_steal_numbered_body() {
        let expected = HashSet::from([1]);
        // 第 1 句的译文正文恰好以「2024」开头——不该被当成序号 2024 的条目
        let raw = "[1] 2024 年是很特别的一年。";
        let result = LLMEngine::parse_batch_response(raw, &expected);
        assert_eq!(
            result.get(&1).map(String::as_str),
            Some("2024 年是很特别的一年。"),
            "正文里的数字不该被误切"
        );
        assert_eq!(result.len(), 1);
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
        let batches = plan_translate_batches(&pending, &segs, batch_char_budget(4096));
        assert!(
            batches.iter().all(|b| b.len() <= MAX_SEGMENTS_PER_BATCH),
            "每批不得超过 MAX_SEGMENTS_PER_BATCH"
        );
        // 所有下标都要被覆盖且不重复
        let mut all: Vec<usize> = batches.iter().flatten().copied().collect();
        all.sort_unstable();
        assert_eq!(all, pending);
    }

    /// 长句必须按**字符预算**提前收口——原先只按条数切，一条长字幕就能把整批
    /// 顶出模型上下文，表现为最后几行译文缺失。
    #[test]
    fn plan_batches_splits_on_char_budget() {
        let budget = batch_char_budget(4096);
        let long = "字".repeat(500);
        let segs: Vec<Segment> = (1..=24).map(|i| seg_at(i, &long)).collect();
        let pending: Vec<usize> = (0..24).collect();
        let batches = plan_translate_batches(&pending, &segs, budget);
        assert!(batches.len() > 1, "长句应被拆成多批");
        for b in &batches {
            let chars: usize = b
                .iter()
                .map(|&p| segs[p].translate_source().chars().count() + 6)
                .sum();
            assert!(chars <= budget, "批次字符数超预算: {chars} > {budget}");
        }
        let mut all: Vec<usize> = batches.iter().flatten().copied().collect();
        all.sort_unstable();
        assert_eq!(all, pending, "不得丢句或重复");
    }

    /// 单条就超过字符预算时仍要单独成批：否则会切不出合法批次而死循环。
    #[test]
    fn plan_batches_keeps_oversized_single_segment() {
        let budget = batch_char_budget(4096);
        let huge = "字".repeat(budget * 2);
        let segs = vec![seg_at(1, &huge)];
        let batches = plan_translate_batches(&[0], &segs, budget);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![0]);
    }

    /// 空输入不该产出空批次。
    #[test]
    fn plan_batches_empty_input() {
        let segs: Vec<Segment> = Vec::new();
        assert!(plan_translate_batches(&[], &segs, batch_char_budget(4096)).is_empty());
    }

    /// 回归：输出 token 预算必须**随批次源字符数增长**。
    ///
    /// 修前按「条数 × 48」夹在 768：24 行 × 150 字的批次实际需要 1300+ token 才能
    /// 译完，768 会在第 14 行撞上限后**静默截断**（实测复现），用户看到的是
    /// 「翻译莫名其妙少了几句」。这个断言锁死「长批不再被 768 卡死」。
    #[test]
    fn output_budget_scales_with_source_length() {
        let ctx = 4096;
        let short = output_token_budget(200, ctx);
        let long = output_token_budget(3_000, ctx);
        assert!(
            long > 768,
            "长批的输出预算必须突破旧的 768 上限，实际 {long}"
        );
        assert!(long > short, "源越长，输出预算应越大");
        // 无论多长，prompt + 输出都不该超过上下文窗口
        assert!(
            (3_000f64 * PROMPT_TOKENS_PER_CHAR) as u32 + long <= ctx,
            "prompt + 输出越界: {long}"
        );
        // 极短批也要给足最小生成空间
        assert!(output_token_budget(10, ctx) >= 128);
    }

    /// 字符预算必须随上下文窗口缩放，且给输入 + 输出都留出空间。
    #[test]
    fn char_budget_scales_with_context() {
        let small = batch_char_budget(2048);
        let large = batch_char_budget(8192);
        assert!(large > small, "上下文越大，单批字符预算应越大");
        // 4096 上下文下的预算 ×（输入+输出系数）不得超过窗口
        let per = PROMPT_TOKENS_PER_CHAR + OUTPUT_TOKENS_PER_CHAR;
        assert!((small as f64 * per) <= 2048.0);
        assert!((large as f64 * per) <= 8192.0);
    }
    /// 离线链路前置检查：模型或程序缺失时必须报错（而非静默跑出 0 句的假成功）。
    #[test]
    fn ensure_ready_fails_when_model_or_cli_missing() {
        let engine = LLMEngine::new(
            "definitely-not-a-real-cli-path.exe",
            "definitely-not-a-real-model.gguf",
            4096,
            4,
        );
        let err = engine.ensure_ready().expect_err("缺模型/程序时应报错");
        let msg = err.to_string();
        assert!(
            msg.contains("模型未就位") || msg.contains("推理程序未就位"),
            "错误信息应说明缺什么: {msg}"
        );
    }
}
